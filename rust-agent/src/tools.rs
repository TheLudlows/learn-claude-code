/*
tools.rs - Tool definitions and handlers

This module contains all tool-related code:
- Tool definitions (what we tell the model we can do)
- Tool execution functions (the actual implementations)
- Tool dispatch (mapping tool names to handlers)
- Path safety (keeping file operations inside workspace)
*/

use serde::Serialize;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 工作目录
pub fn workdir() -> PathBuf {
    env::current_dir().unwrap_or_else(|_| ".".into())
}

/// 路径安全校验 - 确保路径在工作目录内
fn safe_path(path_str: &str) -> Result<PathBuf, String> {
    let workdir = workdir();
    let path = workdir.join(path_str);
    let abs_path = path.canonicalize()
        .map_err(|e| format!("Error resolving path: {}", e))?;

    if !abs_path.starts_with(&workdir) {
        return Err(format!("Path escapes workspace: {}", path_str));
    }

    Ok(abs_path)
}

/// 执行 bash 命令
fn run_bash(command: &str) -> String {
    let dangerous = ["rm -rf /", "sudo", "shutdown", "reboot", "> /dev/"];

    for d in dangerous {
        if command.contains(d) {
            return "Error: Dangerous command blocked".to_string();
        }
    }

    match Command::new("bash")
        .arg("-c")
        .arg(command)
        .current_dir(workdir())
        .output()
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let result = format!("{}\n{}", stdout, stderr).trim().to_string();
            if result.is_empty() {
                "(no output)".to_string()
            } else if result.len() > 50000 {
                result[..50000].to_string()
            } else {
                result
            }
        }
        Err(e) => format!("Error: {}", e),
    }
}

/// 读取文件
fn run_read_file(path: &str, limit: Option<u32>) -> String {
    match safe_path(path) {
        Ok(abs_path) => {
            match fs::read_to_string(&abs_path) {
                Ok(content) => {
                    let lines: Vec<&str> = content.lines().collect();
                    if let Some(limit) = limit {
                        if lines.len() > limit as usize {
                            let truncated: Vec<&str> = lines[..limit as usize].to_vec();
                            let more = lines.len() - limit as usize;
                            format!("{}\n... ({} more lines)",
                                truncated.join("\n"), more)
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
        Err(e) => e,
    }
}

/// 写入文件
fn run_write_file(path: &str, content: &str) -> String {
    match safe_path(path) {
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

/// 编辑文件（替换文本）
fn run_edit_file(path: &str, old_text: &str, new_text: &str) -> String {
    match safe_path(path) {
        Ok(abs_path) => {
            match fs::read_to_string(&abs_path) {
                Ok(content) => {
                    if !content.contains(old_text) {
                        return format!("Error: text not found in {}", path);
                    }
                    let new_content = content.replacen(old_text, new_text, 1);
                    match fs::write(&abs_path, &new_content) {
                        Ok(_) => format!("Edited {}", path),
                        Err(e) => format!("Error: {}", e),
                    }
                }
                Err(e) => format!("Error: {}", e),
            }
        }
        Err(e) => e,
    }
}

/// 查找匹配的文件
fn run_glob(pattern: &str) -> String {
    let workdir = workdir();
    let mut results = Vec::new();

    // 简单的 glob 实现
    let pattern_suffix = pattern.replace("**", "");

    fn walk_dir(dir: &Path, pattern: &str, results: &mut Vec<String>, workdir: &Path) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name.contains(pattern) || pattern == "*" {
                        if let Ok(rel) = path.strip_prefix(workdir) {
                            results.push(rel.to_string_lossy().to_string());
                        }
                    }
                }
                if path.is_dir() {
                    walk_dir(&path, pattern, results, workdir);
                }
            }
        }
    }

    walk_dir(&workdir, &pattern_suffix, &mut results, &workdir);

    if results.is_empty() {
        "(no matches)".to_string()
    } else {
        results.join("\n")
    }
}

/// 工具分发 - 根据工具名调用对应的处理函数
///
/// 这是 s02 的核心：加一个工具只需要在这里加一个 match 分支。
/// 循环逻辑保持不变。
pub fn dispatch_tool(tool_name: &str, input: &serde_json::Value) -> String {
    match tool_name {
        "bash" => {
            if let Some(cmd) = input.get("command").and_then(|c| c.as_str()) {
                run_bash(cmd)
            } else {
                "Error: missing command".to_string()
            }
        }
        "read_file" => {
            let path = input.get("path").and_then(|p| p.as_str()).unwrap_or("");
            let limit = input.get("limit").and_then(|l| l.as_u64()).map(|l| l as u32);
            run_read_file(path, limit)
        }
        "write_file" => {
            let path = input.get("path").and_then(|p| p.as_str()).unwrap_or("");
            let content = input.get("content").and_then(|c| c.as_str()).unwrap_or("");
            run_write_file(path, content)
        }
        "edit_file" => {
            let path = input.get("path").and_then(|p| p.as_str()).unwrap_or("");
            let old_text = input.get("old_text").and_then(|o| o.as_str()).unwrap_or("");
            let new_text = input.get("new_text").and_then(|n| n.as_str()).unwrap_or("");
            run_edit_file(path, old_text, new_text)
        }
        "glob" => {
            let pattern = input.get("pattern").and_then(|p| p.as_str()).unwrap_or("");
            run_glob(pattern)
        }
        _ => format!("Unknown tool: {}", tool_name),
    }
}

/// 工具定义
///
/// 这些定义告诉模型有什么工具可用、每个工具的输入参数是什么。
/// 加一个新工具只需要在这里加一条 ToolDefinition。
#[derive(Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// 获取工具定义列表
pub fn get_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "bash".to_string(),
            description: "Run a shell command.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" }
                },
                "required": ["command"]
            }),
        },
        ToolDefinition {
            name: "read_file".to_string(),
            description: "Read file contents.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "limit": { "type": "integer" }
                },
                "required": ["path"]
            }),
        },
        ToolDefinition {
            name: "write_file".to_string(),
            description: "Write content to a file.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
        },
        ToolDefinition {
            name: "edit_file".to_string(),
            description: "Replace exact text in a file once.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_text": { "type": "string" },
                    "new_text": { "type": "string" }
                },
                "required": ["path", "old_text", "new_text"]
            }),
        },
        ToolDefinition {
            name: "glob".to_string(),
            description: "Find files matching a glob pattern.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" }
                },
                "required": ["pattern"]
            }),
        },
    ]
}