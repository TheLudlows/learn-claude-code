/*
mcp/mod.rs - MCP Manager and name normalization

This module implements:
- McpManager: Manages MCP server connections and tool registration
- Name normalization: Converts tool names to safe identifiers
- Collision detection: Prevents duplicate prefixed tool names
*/

pub mod tool;
pub use tool::{McpTool, McpClientTrait};

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use regex::Regex;
use serde_json::Value;
use crate::error::AgentError;
use crate::tools::registry::ToolRegistry;

const DISALLOWED_CHARS: &str = r"[^a-zA-Z0-9_-]";
const MAX_TOOL_NAME_LEN: usize = 64;

/// Replace characters not in [a-zA-Z0-9_-] with _
pub fn normalize_mcp_name(name: &str) -> Result<String, AgentError> {
    let re = Regex::new(DISALLOWED_CHARS).unwrap();
    let normalized = re.replace_all(name, "_").to_string();
    if normalized.is_empty() || normalized.chars().all(|c| c == '_') {
        return Err(AgentError::Validation("MCP name cannot normalize to empty".into()));
    }
    Ok(normalized)
}

/// Construct prefixed tool name: mcp__{server}__{tool}
pub fn prefixed_tool_name(server: &str, tool: &str) -> Result<String, AgentError> {
    let safe_server = normalize_mcp_name(server)?;
    let safe_tool = normalize_mcp_name(tool)?;
    let prefixed = format!("mcp__{}__{}", safe_server, safe_tool);
    if prefixed.len() > MAX_TOOL_NAME_LEN {
        return Err(AgentError::Validation(
            format!("MCP tool name exceeds {} chars: {}", MAX_TOOL_NAME_LEN, prefixed)
        ));
    }
    Ok(prefixed)
}

/// MCP tool definition from server
#[derive(Debug, Clone)]
pub struct McpToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Active MCP server connection
pub struct McpConnection {
    pub server_name: String,
    pub tools: Vec<McpToolDef>,
    // client will be added in Task 3.1
}

/// MCP server info for listing
#[derive(Debug, Clone)]
pub struct McpServerInfo {
    pub name: String,
    pub tools: Vec<String>,
}

/// Manager for MCP server connections and tool registration
pub struct McpManager {
    connections: RwLock<HashMap<String, McpConnection>>,
    origins: RwLock<HashMap<String, String>>, // Normalized name -> origin (server/tool)
    registry: Arc<ToolRegistry>,
}

impl McpManager {
    pub fn new(registry: Arc<ToolRegistry>) -> Self {
        Self {
            connections: RwLock::new(HashMap::new()),
            origins: RwLock::new(HashMap::new()),
            registry,
        }
    }

    /// List all connected MCP servers
    pub fn list(&self) -> Vec<McpServerInfo> {
        let connections = self.connections.read().unwrap();
        connections.values()
            .map(|conn| McpServerInfo {
                name: conn.server_name.clone(),
                tools: conn.tools.iter().map(|t| t.name.clone()).collect(),
            })
            .collect()
    }

    /// Generate system prompt suffix (connected server list)
    pub fn system_prompt_suffix(&self) -> String {
        let servers = self.list();
        if servers.is_empty() {
            return String::new();
        }
        let mut lines = vec!["## Connected MCP Servers".to_string()];
        for server in servers {
            lines.push(format!("- {}: {} tool(s)", server.name, server.tools.len()));
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod name_normalization_tests {
    use super::*;

    #[test]
    fn normalize_strips_dots_and_slashes() {
        assert_eq!(normalize_mcp_name("my.server").unwrap(), "my_server");
        assert_eq!(normalize_mcp_name("server/name").unwrap(), "server_name");
        assert_eq!(normalize_mcp_name("a.b/c-d_e.f").unwrap(), "a_b_c-d_e_f");
    }

    #[test]
    fn normalize_preserves_underscores_and_dashes() {
        assert_eq!(normalize_mcp_name("my_server").unwrap(), "my_server");
        assert_eq!(normalize_mcp_name("my-server").unwrap(), "my-server");
        assert_eq!(normalize_mcp_name("my_server-name").unwrap(), "my_server-name");
    }

    #[test]
    fn normalize_rejects_empty() {
        assert!(normalize_mcp_name("").is_err());
        assert!(normalize_mcp_name("...").is_err());
    }

    #[test]
    fn prefixed_name_format() {
        assert_eq!(prefixed_tool_name("docs", "search").unwrap(), "mcp__docs__search");
        assert_eq!(prefixed_tool_name("my-server", "my-tool").unwrap(), "mcp__my-server__my-tool");
    }

    #[test]
    fn prefixed_name_exceeds_64_chars() {
        let long_name = "a".repeat(100);
        let result = prefixed_tool_name(&long_name, "tool");
        assert!(result.is_err());
        if let Err(e) = result {
            assert!(e.to_string().contains("exceeds 64 chars"));
        }
    }
}
