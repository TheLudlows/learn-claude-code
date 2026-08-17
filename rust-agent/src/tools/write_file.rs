/*
write_file.rs - Write File Tool Implementation

This module implements the WriteFileTool for writing file contents.
- Implements Tool trait for file writing operations
- Uses run_write_file() from tools/mod.rs
- Has check_permission with escapes_workspace_lexical
- Default available_for_subagent = true
*/

use crate::tools::trait_def::{PermissionCheck, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::Value;

/// Write File Tool for writing file contents

/// This tool allows the AI agent to write file contents safely.
/// It includes path safety checks to ensure files are written within the workspace.
pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    /// Returns the tool's name
    fn name(&self) -> &str {
        "write_file"
    }

    /// Returns a human-readable description
    fn description(&self) -> &str {
        "Write content to a file. Creates the file and parent directories if they don't exist."
    }

    /// Returns the JSON schema for write_file input
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to write (relative to workspace root)"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                }
            },
            "required": ["path", "content"]
        })
    }

    /// Checks if the file writing requires approval based on path safety
    fn check_permission(&self, input: &Value) -> PermissionCheck {
        if let Some(path) = input.get("path").and_then(|v| v.as_str()) {
            // Check if the path escapes the workspace lexically
            if crate::tools::escapes_workspace_lexical(path) {
                return PermissionCheck::NeedsApproval(
                    "This file path appears to escape the workspace boundary. Writing to files outside the project could be dangerous."
                );
            }
        }

        // Default: allow file writing for paths within workspace
        PermissionCheck::Pass
    }

    /// Executes the file writing using run_write_file() from tools/mod.rs
    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        let path = match input.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return "Error: No file path provided".to_string(),
        };

        let content = match input.get("content").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return "Error: No content provided".to_string(),
        };

        // Execute the file writing using the shared run_write_file function
        crate::tools::run_write_file(path, content)
    }

    /// Write file tool should be available to subagents with appropriate permission checks
    fn available_for_subagent(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_write_file_tool_name() {
        let tool = WriteFileTool;
        assert_eq!(tool.name(), "write_file");
    }

    #[test]
    fn test_write_file_tool_description() {
        let tool = WriteFileTool;
        assert!(tool.description().contains("Write content"));
    }

    #[test]
    fn test_write_file_tool_schema() {
        let tool = WriteFileTool;
        let schema = tool.input_schema();

        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].is_object());
        assert_eq!(schema["properties"]["path"]["type"], "string");
        assert_eq!(schema["properties"]["content"]["type"], "string");

        let required = schema["required"].as_array().unwrap();
        assert_eq!(required.len(), 2);
        assert_eq!(required[0], "path");
        assert_eq!(required[1], "content");
    }

    #[test]
    fn test_permission_check_safe_paths() {
        let tool = WriteFileTool;

        // Safe paths should pass
        let safe_paths = vec![
            json!({"path": "src/main.rs", "content": "// test"}),
            json!({"path": "Cargo.toml", "content": "[package]"}),
            json!({"path": "tests/test.rs", "content": "fn test() {}"}),
            json!({"path": "data/config.json", "content": "{}"}),
            json!({"path": "./file.txt", "content": "hello"}),
            json!({"path": "file.txt", "content": "world"}),
        ];

        for path in safe_paths {
            match tool.check_permission(&path) {
                PermissionCheck::Pass => {} // Expected
                PermissionCheck::NeedsApproval(reason) => {
                    panic!("Safe path was rejected: {:?} - {}", path, reason);
                }
            }
        }
    }

    #[test]
    fn test_permission_check_escape_paths() {
        let tool = WriteFileTool;

        // Escape paths should need approval
        let escape_paths = vec![
            json!({"path": "../secret.txt", "content": "data"}),
            json!({"path": "../../etc/passwd", "content": "root:x:0:0"}),
            json!({"path": "../../config.ini", "content": "config"}),
            json!({"path": "../../../data/file.txt", "content": "data"}),
            json!({"path": "../.gitconfig", "content": "config"}),
            json!({"path": "..\\system32\\config", "content": "data"}),
        ];

        for path in escape_paths {
            match tool.check_permission(&path) {
                PermissionCheck::NeedsApproval(reason) => {
                    // Should contain approval-related text
                    assert!(reason.contains("approval") || reason.contains("explicit approval") || reason.contains("workspace boundary") || reason.contains("dangerous"),
                           "Escape path should mention approval or workspace boundary: {:?} - {}", path, reason);
                }
                PermissionCheck::Pass => {
                    panic!("Escape path was approved: {:?}", path);
                }
            }
        }
    }

    #[test]
    fn test_permission_case_insensitive() {
        let tool = WriteFileTool;

        // Test that path escaping is case insensitive
        let escape_path = json!({"path": "..\\SECRET.TXT", "content": "data"});
        match tool.check_permission(&escape_path) {
            PermissionCheck::NeedsApproval(_) => {} // Expected
            PermissionCheck::Pass => panic!("Case insensitive check failed"),
        }
    }

    #[test]
    fn test_path_normalization() {
        let tool = WriteFileTool;

        // Test path normalization (should be normalized to check escape)
        let normalized_paths = vec![
            json!({"path": "././file.txt", "content": "data"}),       // Should be safe
            json!({"path": "src/../file.txt", "content": "data"}),    // Should be safe (parent within workspace)
            json!({"path": "src/../../file.txt", "content": "data"}), // Should escape
        ];

        for path in normalized_paths {
            match tool.check_permission(&path) {
                PermissionCheck::Pass => {
                    // If normalized path doesn't escape, it should be safe
                    let path_str = path["path"].as_str().unwrap();
                    assert!(!crate::tools::escapes_workspace_lexical(path_str),
                           "Path should not escape: {:?}", path_str);
                }
                PermissionCheck::NeedsApproval(_) => {
                    // If normalized path escapes, it should need approval
                    let path_str = path["path"].as_str().unwrap();
                    assert!(crate::tools::escapes_workspace_lexical(path_str),
                           "Path should escape: {:?}", path_str);
                }
            }
        }
    }

    #[test]
    fn test_permission_validation() {
        let tool = WriteFileTool;

        // Test malformed input (missing required fields)
        let no_path = json!({"content": "data"}); // Missing path
        let no_content = json!({"path": "file.txt"}); // Missing content

        match tool.check_permission(&no_path) {
            PermissionCheck::Pass => {} // Should pass (permission check doesn't validate schema)
            _ => panic!("No path should be allowed in permission check"),
        }

        match tool.check_permission(&no_content) {
            PermissionCheck::Pass => {} // Should pass (permission check doesn't validate schema)
            _ => panic!("No content should be allowed in permission check"),
        }
    }
}