/*
mcp/client.rs - McpClient: stdio JSON-RPC transport implementation

This module implements the MCP protocol client using stdio transport.
It handles JSON-RPC communication with MCP server processes via
tokio::process::Command, with async request/response matching using
oneshot channels and a background reader task.
*/

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, oneshot};
use serde_json::{json, Value};
use crate::error::AgentError;

/// JSON-RPC request message
#[derive(Debug)]
struct JsonRpcRequest {
    jsonrpc: &'static str,
    id: u64,
    method: String,
    params: Option<Value>,
}

/// JSON-RPC response message
#[derive(Debug)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    id: u64,
    result: Option<Value>,
    error: Option<JsonRpcError>,
}

/// JSON-RPC error
#[derive(Debug)]
struct JsonRpcError {
    code: i32,
    message: String,
}

/// MCP tool definition from server
#[derive(Debug, Clone)]
pub struct McpToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Initialize result from MCP server
#[derive(Debug)]
pub struct InitResult {
    pub capabilities: Value,
}

/// Inner state of McpClient (for interior mutability)
struct McpClientInner {
    child: Option<Child>,
    stdin: ChildStdin,
    // stdout_lines is owned by the background reader task, not stored here
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    // Background task handle for proper lifetime management
    reader_task: Option<tokio::task::JoinHandle<()>>,
}

/// MCP client for stdio JSON-RPC transport
pub struct McpClient {
    inner: Arc<Mutex<McpClientInner>>,
}

impl McpClient {
    /// Spawn child process and establish stdio connection
    pub async fn spawn(command: &str, args: &[&str]) -> Result<Self, AgentError> {
        let mut child = Command::new(command)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| AgentError::Other(format!("Failed to spawn MCP server: {}", e)))?;

        let stdin = child.stdin.take()
            .ok_or_else(|| AgentError::Other("Failed to get stdin handle".into()))?;
        let stdout = child.stdout.take()
            .ok_or_else(|| AgentError::Other("Failed to get stdout handle".into()))?;

        let stdout_reader = BufReader::new(stdout);
        let stdout_lines = stdout_reader.lines();

        let pending = Arc::new(Mutex::new(HashMap::new()));
        let pending_clone = Arc::clone(&pending);

        // Spawn background reader task and store handle for proper lifetime management
        let reader_task = tokio::spawn(async move {
            Self::read_responses(stdout_lines, pending_clone).await;
        });

        let inner = McpClientInner {
            child: Some(child),
            stdin,
            next_id: AtomicU64::new(1),
            pending,
            reader_task: Some(reader_task),
        };

        Ok(Self {
            inner: Arc::new(Mutex::new(inner)),
        })
    }

    /// Background task to read responses and match with pending requests
    async fn read_responses(
        lines: tokio::io::Lines<BufReader<ChildStdout>>,
        pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    ) {
        let mut lines = lines;
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }

            match serde_json::from_str::<Value>(&line) {
                Ok(response) => {
                    if let Some(id) = response.get("id").and_then(|v| v.as_u64()) {
                        let mut pending_guard = pending.lock().await;
                        if let Some(sender) = pending_guard.remove(&id) {
                            // Extract result or error
                            if let Some(result) = response.get("result") {
                                let _ = sender.send(result.clone());
                            } else if let Some(error) = response.get("error") {
                                // Send error as a string result
                                let error_msg = error.get("message")
                                    .and_then(|m| m.as_str())
                                    .unwrap_or("Unknown MCP error");
                                let _ = sender.send(json!({"error": error_msg}));
                            }
                        }
                    }
                    // Notifications without id are logged but not processed
                }
                Err(e) => {
                    tracing::warn!("Failed to parse MCP response: {} (line: {})", e, line);
                }
            }
        }
    }

    /// Send JSON-RPC request and wait for response
    async fn send_request(&self, method: &str, params: Option<Value>) -> Result<Value, AgentError> {
        let id = {
            let inner = self.inner.lock().await;
            inner.next_id.fetch_add(1, Ordering::SeqCst)
        };
        let (tx, rx) = oneshot::channel();

        {
            let inner = self.inner.lock().await;
            inner.pending.lock().await.insert(id, tx);
        }

        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params.unwrap_or(json!({}))
        });

        let request_str = request.to_string() + "\n";
        {
            let mut inner = self.inner.lock().await;
            inner.stdin.write_all(request_str.as_bytes()).await
                .map_err(|e| AgentError::Other(format!("Failed to write to MCP server stdin: {}", e)))?;
            inner.stdin.flush().await
                .map_err(|e| AgentError::Other(format!("Failed to flush MCP server stdin: {}", e)))?;
        }

        // Wait for response with timeout
        tokio::select! {
            result = rx => {
                match result {
                    Ok(response) => {
                        // Check if response contains an error
                        if let Some(error) = response.get("error") {
                            let error_msg = error.get("message")
                                .and_then(|m| m.as_str())
                                .unwrap_or("Unknown MCP error");
                            return Err(AgentError::Other(format!("MCP error: {}", error_msg)));
                        }
                        Ok(response)
                    }
                    Err(_) => Err(AgentError::Other("MCP response channel closed".into()))
                }
            }
            _ = tokio::time::sleep(tokio::time::Duration::from_secs(30)) => {
                Err(AgentError::Timeout { seconds: 30 })
            }
        }
    }

    /// Send initialize request to MCP server
    pub async fn initialize(&self) -> Result<InitResult, AgentError> {
        let params = json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "bytemaker",
                "version": "1.0.0"
            }
        });

        let response = self.send_request("initialize", Some(params)).await?;

        Ok(InitResult {
            capabilities: response.get("capabilities").cloned().unwrap_or(json!({})),
        })
    }

    /// List available tools from MCP server
    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>, AgentError> {
        let response = self.send_request("tools/list", None).await?;

        let tools_array = response.get("tools")
            .and_then(|v| v.as_array())
            .ok_or_else(|| AgentError::Other("Invalid tools/list response: missing 'tools' array".into()))?;

        let mut tools = Vec::new();
        for tool_value in tools_array {
            let name = tool_value.get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| AgentError::Other("Tool missing 'name' field".into()))?
                .to_string();

            let description = tool_value.get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let input_schema = tool_value.get("inputSchema")
                .cloned()
                .unwrap_or(json!({}));

            tools.push(McpToolDef {
                name,
                description,
                input_schema,
            });
        }

        Ok(tools)
    }

    /// Call a tool on the MCP server
    pub async fn call_tool(&self, name: &str, args: &Value) -> Result<String, AgentError> {
        let params = json!({
            "name": name,
            "arguments": args
        });

        let response = self.send_request("tools/call", Some(params)).await?;

        // Extract content from response
        if let Some(content) = response.get("content").and_then(|c| c.as_array()) {
            let texts: Vec<String> = content.iter()
                .filter_map(|item| {
                    if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                        Some(text.to_string())
                    } else if let Some(text) = item.get("text") {
                        Some(text.to_string())
                    } else {
                        Some(format!("{:?}", item))
                    }
                })
                .collect();

            Ok(texts.join("\n"))
        } else {
            // Fallback: try to stringify the whole response
            Ok(response.to_string())
        }
    }

    /// Shutdown the client and kill the child process
    pub async fn shutdown(self) -> Result<(), AgentError> {
        // Try to send shutdown request (best effort)
        let _ = self.send_request("shutdown", None).await;

        let mut inner = self.inner.lock().await;

        // Abort the background reader task first
        if let Some(reader_task) = inner.reader_task.take() {
            reader_task.abort();
            // Give the task a moment to clean up
            let _ = tokio::time::timeout(
                tokio::time::Duration::from_millis(100),
                reader_task
            ).await;
        }

        // Force kill the child process
        if let Some(mut child) = inner.child.take() {
            let kill_result = child.kill().await;
            let wait_result = child.wait().await;

            // Verify that process is actually killed
            match (kill_result, wait_result) {
                (Ok(()), Ok(exit_status)) => {
                    if !exit_status.success() {
                        tracing::warn!("MCP server process exited with non-zero status: {:?}", exit_status);
                    }
                }
                (Err(e), _) => {
                    return Err(AgentError::Other(format!("Failed to kill MCP server process: {}", e)));
                }
                (Ok(()), Err(e)) => {
                    return Err(AgentError::Other(format!("Failed to wait for MCP server process: {}", e)));
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod client_tests {
    use super::*;

    #[tokio::test]
    async fn client_generates_unique_ids() {
        // This test verifies the ID generation logic
        let client = create_mock_client_for_testing();

        let id1 = {
            let inner = client.inner.lock().await;
            inner.next_id.fetch_add(1, Ordering::SeqCst)
        };
        let id2 = {
            let inner = client.inner.lock().await;
            inner.next_id.fetch_add(1, Ordering::SeqCst)
        };

        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
    }

    #[test]
    fn tool_def_creation() {
        let tool = McpToolDef {
            name: "test_tool".to_string(),
            description: "A test tool".to_string(),
            input_schema: json!({"type": "object"}),
        };

        assert_eq!(tool.name, "test_tool");
        assert_eq!(tool.description, "A test tool");
        assert!(tool.input_schema.is_object());
    }

    #[test]
    fn init_result_creation() {
        let result = InitResult {
            capabilities: json!({"tools": {}}),
        };

        assert!(result.capabilities.is_object());
    }

    // Helper function to create a mock client for testing ID generation
    fn create_mock_client_for_testing() -> McpClient {
        // Create a minimal mock client just for testing ID generation
        use std::process::Stdio;

        let mut child = Command::new("echo")
            .arg("test")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Failed to spawn echo process");

        let stdin = child.stdin.take().expect("Failed to get stdin");
        let _stdout = child.stdout.take().expect("Failed to get stdout");
        let _stdout_reader = BufReader::new(_stdout);
        // We don't need to store stdout_lines since it's used by background task

        let inner = McpClientInner {
            child: Some(child),
            stdin,
            next_id: AtomicU64::new(1),
            pending: Arc::new(Mutex::new(HashMap::new())),
            reader_task: None,
        };

        McpClient {
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    #[tokio::test]
    async fn integration_test_mcp_protocol_interactions() {
        // This test exercises actual MCP protocol interactions with a simple mock server
        use std::io::Write;
        use std::process::{Command, Stdio, ChildStdin};

        // Create a simple Python script that acts as a mock MCP server
        let mock_server_script = r#"
import sys
import json

def send_response(response):
    print(json.dumps(response))
    sys.stdout.flush()

while True:
    try:
        line = sys.stdin.readline()
        if not line:
            break

        request = json.loads(line.strip())
        request_id = request.get("id", 0)
        method = request.get("method", "")

        if method == "initialize":
            send_response({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {
                    "capabilities": {
                        "tools": {}
                    }
                }
            })
        elif method == "tools/list":
            send_response({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {
                    "tools": [
                        {
                            "name": "test_tool",
                            "description": "A test tool",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "param1": {"type": "string"}
                                }
                            }
                        }
                    ]
                }
            })
        elif method == "tools/call":
            send_response({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {
                    "content": [
                        {
                            "type": "text",
                            "text": "Tool execution result"
                        }
                    ]
                }
            })
        elif method == "shutdown":
            send_response({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {}
            })
            break
        else:
            send_response({
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {
                    "code": -32601,
                    "message": "Method not found"
                }
            })
    except Exception as e:
        send_response({
            "jsonrpc": "2.0",
            "id": 0,
            "error": {
                "code": -32700,
                "message": str(e)
            }
        })
        break
"#;

        // Write the mock server script to a temporary file
        let mut temp_file = tempfile::NamedTempFile::new()
            .expect("Failed to create temp file");
        temp_file.write_all(mock_server_script.as_bytes())
            .expect("Failed to write mock server script");
        let temp_path = temp_file.path().to_str().expect("Invalid path");

        // Test 1: Spawn and initialize client
        let client = McpClient::spawn("python", &[temp_path])
            .await
            .expect("Failed to spawn MCP client");

        let init_result = client.initialize()
            .await
            .expect("Failed to initialize MCP client");

        assert!(init_result.capabilities.is_object(), "Initialize should return capabilities object");

        // Test 2: List tools
        let tools = client.list_tools()
            .await
            .expect("Failed to list tools");

        assert_eq!(tools.len(), 1, "Should have exactly one tool");
        assert_eq!(tools[0].name, "test_tool", "Tool name should be 'test_tool'");
        assert_eq!(tools[0].description, "A test tool", "Tool description should match");

        // Test 3: Call tool
        let tool_args = json!({"param1": "test_value"});
        let result = client.call_tool("test_tool", &tool_args)
            .await
            .expect("Failed to call tool");

        assert!(result.contains("Tool execution result"), "Tool result should contain expected text");

        // Test 4: Shutdown client (this tests the cleanup logic)
        client.shutdown()
            .await
            .expect("Failed to shutdown client");

        // Clean up temp file
        let _ = std::fs::remove_file(temp_path);
    }
}
