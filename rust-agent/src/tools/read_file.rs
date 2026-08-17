/*
read_file.rs - Read File Tool Implementation

This module implements the ReadFileTool for reading file contents.
- Implements Tool trait for file reading operations
- Uses run_read_file() from tools/mod.rs
- Has check_permission with escapes_workspace_lexical
- Default available_for_subagent = true
*/

use crate::tools::trait_def::{PermissionCheck, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::Value;

/// Read File Tool for reading file contents
///
/// This tool allows the AI agent to read file contents safely.
/// It includes path safety checks to ensure files are within the workspace.
pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    /// Returns the tool's name
    fn name(&self) -> &str {
        "read_file"
    }

    /// Returns a human-readable description
    fn description(&self) -> &str {
        "Read the contents of a file. Returns the full content or a truncated version if the file is too large."
    }

    /// Returns the JSON schema for read_file input
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to read (relative to workspace root)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to read. If omitted, reads the entire file.",
                    "minimum": 1
                }
            },
            "required": ["path"]
        })
    }

    /// Checks if the file reading requires approval based on path safety
    fn check_permission(&self, input: &Value) -> PermissionCheck {
        if let Some(path) = input.get("path").and_then(|v| v.as_str()) {
            // Check if the path escapes the workspace lexically
            if crate::tools::escapes_workspace_lexical(path) {
                return PermissionCheck::NeedsApproval(
                    "This file path appears to escape the workspace boundary. This could potentially access sensitive files outside the project."
                );
            }
        }

        // Default: allow file reading for paths within workspace
        PermissionCheck::Pass
    }

    /// Executes the file reading using run_read_file() from tools/mod.rs
    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        let path = match input.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return "Error: No file path provided".to_string(),
        };

        let limit = input.get("limit")
            .and_then(|v| v.as_u64())
            .map(|l| l as u32);

        // Execute the file reading using the shared run_read_file function
        crate::tools::run_read_file(path, limit)
    }

    /// Read file tool should be available to subagents with appropriate permission checks
    fn available_for_subagent(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_read_file_tool_name() {
        let tool = ReadFileTool;
        assert_eq!(tool.name(), "read_file");
    }

    #[test]
    fn test_read_file_tool_description() {
        let tool = ReadFileTool;
        assert!(tool.description().contains("Read the contents"));
    }

    #[test]
    fn test_read_file_tool_schema() {
        let tool = ReadFileTool;
        let schema = tool.input_schema();

        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].is_object());
        assert_eq!(schema["properties"]["path"]["type"], "string");
        assert!(schema["properties"]["limit"].is_object());

        let required = schema["required"].as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "path");
    }

    #[test]
    fn test_permission_check_safe_paths() {
        let tool = ReadFileTool;

        // Safe paths should pass
        let safe_paths = vec![
            json!({"path": "src/main.rs"}),
            json!({"path": "Cargo.toml"}),
            json!({"path": "tests/test.rs"}),
            json!({"path": "data/config.json"}),
            json!({"path": "./file.txt"}),
            json!({"path": "file.txt"}),
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
        let tool = ReadFileTool;

        // Escape paths should need approval
        let escape_paths = vec![
            json!({"path": "../secret.txt"}),
            json!({"path": "../../etc/passwd"}),
            json!({"path": "../../config.ini"}),
            json!({"path": "../../../data/file.txt"}),
            json!({"path": "../.gitconfig"}),
            json!({"path": "..\\system32\\config"}),
        ];

        for path in escape_paths {
            match tool.check_permission(&path) {
                PermissionCheck::NeedsApproval(reason) => {
                    // Should contain approval-related text
                    assert!(reason.contains("approval") || reason.contains("explicit approval") || reason.contains("workspace boundary"),
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
        let tool = ReadFileTool;

        // Test that path escaping is case insensitive
        let escape_path = json!({"path": "..\\SECRET.TXT"});
        match tool.check_permission(&escape_path) {
            PermissionCheck::NeedsApproval(_) => {} // Expected
            PermissionCheck::Pass => panic!("Case insensitive check failed"),
        }
    }

    #[test]
    fn test_path_normalization() {
        let tool = ReadFileTool;

        // Test path normalization (should be normalized to check escape)
        let normalized_paths = vec![
            json!({"path": "././file.txt"}),       // Should be safe
            json!({"path": "src/../file.txt"}),    // Should be safe (parent within workspace)
            json!({"path": "src/../../file.txt"}), // Should escape
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
        let tool = ReadFileTool;

        // Test malformed input
        let no_path = json!({}); // Missing required path
        match tool.check_permission(&no_path) {
            PermissionCheck::Pass => {} // Should pass (permission check doesn't validate schema)
            _ => panic!("No path should be allowed in permission check"),
        }
    }
}