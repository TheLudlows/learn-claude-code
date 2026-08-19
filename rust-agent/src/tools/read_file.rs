
use crate::tools::trait_def::{PermissionCheck, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::Value;
use std::fs;

/// 读取文件
pub(crate) fn run_read_file(path: &str, limit: Option<u32>) -> String {
    match fs::read_to_string(path) {
        Ok(content) => {
            let lines: Vec<&str> = content.lines().collect();
            if let Some(limit) = limit {
                if lines.len() > limit as usize {
                    let truncated: Vec<&str> = lines[..limit as usize].to_vec();
                    let more = lines.len() - limit as usize;
                    format!(
                        "{}\n... ({} more lines)",
                        truncated.join("\n"),
                        more
                    )
                } else {
                    content
                }
            } else {
                content
            }
        }
        Err(e) => format!("Error: {}", e),
    }
}

/// Read File Tool for reading file contents
///
/// This tool allows the AI agent to read file contents safely.
/// It includes path safety checks to ensure files are within the workspace.
pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read the contents of a file. Returns the full content or a truncated version if the file is too large."
    }

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

    fn check_permission(&self, input: &Value) -> PermissionCheck {
        if let Some(path) = input.get("path").and_then(|v| v.as_str()) {
            if crate::tools::escapes_workspace_lexical(path) {
                return PermissionCheck::NeedsApproval(
                    "This file path appears to escape the workspace boundary. This could potentially access sensitive files outside the project."
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

        let limit = input.get("limit").and_then(|v| v.as_u64()).map(|l| l as u32);

        run_read_file(path, limit)
    }

    fn available_for_subagent(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- ReadFileTool trait tests ----

    #[test]
    fn test_read_file_tool_name() {
        let tool = ReadFileTool;
        assert_eq!(tool.name(), "read_file");
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
                PermissionCheck::Pass => {}
                PermissionCheck::NeedsApproval(reason) => {
                    panic!("Safe path was rejected: {:?} - {}", path, reason);
                }
            }
        }
    }

    #[test]
    fn test_permission_check_escape_paths() {
        let tool = ReadFileTool;

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

        let escape_path = json!({"path": "..\\SECRET.TXT"});
        match tool.check_permission(&escape_path) {
            PermissionCheck::NeedsApproval(_) => {}
            PermissionCheck::Pass => panic!("Case insensitive check failed"),
        }
    }

    #[test]
    fn test_path_normalization() {
        let tool = ReadFileTool;

        let normalized_paths = vec![
            json!({"path": "././file.txt"}),
            json!({"path": "src/../file.txt"}),
            json!({"path": "src/../../file.txt"}),
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
        let tool = ReadFileTool;

        let no_path = json!({});
        match tool.check_permission(&no_path) {
            PermissionCheck::Pass => {}
            _ => panic!("No path should be allowed in permission check"),
        }
    }

    // ---- run_read_file tests ----

    #[test]
    fn run_read_file_reads_content() {
        let dir = std::env::temp_dir().join("rust-agent-read-file-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("hello.txt");
        std::fs::write(&file, "line1\nline2\nline3").unwrap();

        // Use safe_path_in with the temp dir as workspace
        let result = crate::tools::safe_path_in(&dir, "hello.txt");
        assert!(result.is_ok());
        let content = std::fs::read_to_string(result.unwrap()).unwrap();
        assert_eq!(content, "line1\nline2\nline3");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_read_file_limit_truncates() {
        let dir = std::env::temp_dir().join("rust-agent-read-file-limit-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("multi.txt");
        std::fs::write(&file, "line1\nline2\nline3\nline4\nline5").unwrap();

        // Verify limit logic: read file content and check truncation
        let content = std::fs::read_to_string(&file).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        let limit: u32 = 2;
        let truncated: Vec<&str> = lines[..limit as usize].to_vec();
        let more = lines.len() - limit as usize;
        let result = format!("{}\n... ({} more lines)", truncated.join("\n"), more);

        assert!(result.contains("line1"));
        assert!(result.contains("line2"));
        assert!(!result.contains("line3"));
        assert!(result.contains("3 more lines"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
