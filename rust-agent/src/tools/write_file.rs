/*
write_file.rs - Write File Tool Implementation

This module implements:
- WriteFileTool: Tool trait implementation for writing file contents
- run_write_file(): File writing with parent directory creation
*/

use crate::tools::trait_def::{PermissionCheck, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::Value;
use std::fs;

/// 写入文件
pub(crate) fn run_write_file(path: &str, content: &str) -> String {
    match crate::tools::safe_path(path) {
        Ok(abs_path) => {
            if let Some(parent) = abs_path.parent() {
                fs::create_dir_all(parent).ok();
            }
            match fs::write(&abs_path, content) {
                Ok(_) => format!("Wrote {} bytes to {}", content.len(), path),
                Err(e) => format!("Error: {}", e),
            }
        }
        Err(e) => e,
    }
}

/// Write File Tool for writing file contents
///
/// This tool allows the AI agent to write file contents safely.
/// It includes path safety checks to ensure files are written within the workspace.
pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Write content to a file. Creates the file and parent directories if they don't exist."
    }

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

    fn check_permission(&self, input: &Value) -> PermissionCheck {
        if let Some(path) = input.get("path").and_then(|v| v.as_str()) {
            if crate::tools::escapes_workspace_lexical(path) {
                return PermissionCheck::NeedsApproval(
                    "This file path appears to escape the workspace boundary. Writing to files outside the project could be dangerous."
                );
            }
        }

        PermissionCheck::Pass
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        let path = match input.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return "Error: No file path provided".to_string(),
        };

        let content = match input.get("content").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return "Error: No content provided".to_string(),
        };

        run_write_file(path, content)
    }

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
                PermissionCheck::Pass => {}
                PermissionCheck::NeedsApproval(reason) => {
                    panic!("Safe path was rejected: {:?} - {}", path, reason);
                }
            }
        }
    }

    #[test]
    fn test_permission_check_escape_paths() {
        let tool = WriteFileTool;

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

        let escape_path = json!({"path": "..\\SECRET.TXT", "content": "data"});
        match tool.check_permission(&escape_path) {
            PermissionCheck::NeedsApproval(_) => {}
            PermissionCheck::Pass => panic!("Case insensitive check failed"),
        }
    }

    #[test]
    fn test_path_normalization() {
        let tool = WriteFileTool;

        let normalized_paths = vec![
            json!({"path": "././file.txt", "content": "data"}),
            json!({"path": "src/../file.txt", "content": "data"}),
            json!({"path": "src/../../file.txt", "content": "data"}),
        ];

        for path in normalized_paths {
            match tool.check_permission(&path) {
                PermissionCheck::Pass => {
                    let path_str = path["path"].as_str().unwrap();
                    assert!(!crate::tools::escapes_workspace_lexical(path_str),
                           "Path should not escape: {:?}", path_str);
                }
                PermissionCheck::NeedsApproval(_) => {
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

        let no_path = json!({"content": "data"});
        let no_content = json!({"path": "file.txt"});

        match tool.check_permission(&no_path) {
            PermissionCheck::Pass => {}
            _ => panic!("No path should be allowed in permission check"),
        }

        match tool.check_permission(&no_content) {
            PermissionCheck::Pass => {}
            _ => panic!("No content should be allowed in permission check"),
        }
    }
}
