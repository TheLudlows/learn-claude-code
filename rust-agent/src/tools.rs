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
    let workdir_canonical = workdir.canonicalize()
        .map_err(|e| format!("Error: {}", e))?;
    let path = workdir.join(path_str);
    let abs_path = path.canonicalize()
        .map_err(|e| format!("Error: {}", e))?;

    if !abs_path.starts_with(&workdir_canonical) {
        return Err(format!("Error: path escapes workspace"));
    }

    Ok(abs_path)
}

/// 执行命令（跨平台）
///
/// - Windows: 使用 cmd.exe
/// - Unix: 使用 bash
/// 危险命令的拦截已移至 permission::permission_hook 闸门(s03/s04),
/// 在到达这里之前就已被拒; safe_path 仍是文件工具的工作区沙箱。
fn run_bash(command: &str) -> String {
    let result = if cfg!(windows) {
        Command::new("cmd.exe")
            .args(["/C", command])
            .current_dir(workdir())
            .output()
    } else {
        Command::new("bash")
            .arg("-c")
            .arg(command)
            .current_dir(workdir())
            .output()
    };

    match result {
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

/// 把路径分隔符统一成 `/`（Windows 上 `strip_prefix` 给的是 `\`），方便做 glob 匹配。
fn to_unix_path(p: &str) -> String {
    p.replace('\\', "/")
}

/// 单个路径段是否匹配单个模式段（段内不含 `/`）。
/// `*` 匹配任意长度（含空）的字符；`?` 匹配单个字符；其余按字面量。
fn seg_match(pat: &[u8], text: &[u8]) -> bool {
    let (mut pi, mut ti) = (0, 0);
    let mut star_pi: Option<usize> = None;
    let mut star_ti = 0;
    while ti < text.len() {
        if pi < pat.len() && (pat[pi] == b'?' || pat[pi] == text[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pat.len() && pat[pi] == b'*' {
            star_pi = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }
    pi == pat.len()
}

/// 判断 `path`（`/` 分隔的相对路径）是否匹配 glob `pattern`。
/// `*` 匹配一段内的任意字符；`**` 作为独立段时匹配零或多个整段路径。
fn glob_match(pattern: &str, path: &str) -> bool {
    fn rec(pat: &[&str], txt: &[&str]) -> bool {
        if pat.is_empty() {
            return txt.is_empty();
        }
        if pat[0] == "**" {
            // ** 匹配零或多个整段：先试零段，失败再吃掉一段 txt
            if rec(&pat[1..], txt) {
                return true;
            }
            if !txt.is_empty() {
                return rec(pat, &txt[1..]);
            }
            false
        } else {
            // 普通段必须恰好匹配一个 txt 段
            if txt.is_empty() || !seg_match(pat[0].as_bytes(), txt[0].as_bytes()) {
                return false;
            }
            rec(&pat[1..], &txt[1..])
        }
    }
    let pat: Vec<&str> = pattern.split('/').collect();
    let txt: Vec<&str> = path.split('/').collect();
    rec(&pat, &txt)
}

/// 在 `base` 下递归收集所有匹配 glob `pattern` 的相对路径（`/` 分隔）。
fn glob_in(pattern: &str, base: &Path) -> Vec<String> {
    let mut results: Vec<String> = Vec::new();

    fn walk(dir: &Path, base: &Path, pattern: &str, results: &mut Vec<String>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Ok(rel) = path.strip_prefix(base) {
                    let rel = to_unix_path(&rel.to_string_lossy());
                    if glob_match(pattern, &rel) {
                        results.push(rel);
                    }
                }
                if path.is_dir() {
                    walk(&path, base, pattern, results);
                }
            }
        }
    }

    walk(base, base, pattern, &mut results);
    results
}

/// 查找匹配的文件（按 glob 规则匹配相对路径，递归整个工作区）
fn run_glob(pattern: &str) -> String {
    let results = glob_in(pattern, &workdir());
    if results.is_empty() {
        "Error: no matches".to_string()
    } else {
        results.join("\n")
    }
}

/// 添加错误前缀
fn with_error_prefix(prefix: &str, message: &str) -> String {
    if message.starts_with("Error:") {
        format!("[ERROR:{}] {}", prefix, &message[7..].trim_start())
    } else if message.starts_with("[ERROR:") {
        message.to_string() // Already has error prefix
    } else {
        message.to_string()
    }
}

/// 工具分发 - 根据工具名调用对应的处理函数
///
/// 这是 s02 的核心：加一个工具只需要在这里加一个 match 分支。
/// 循环逻辑保持不变。
pub fn dispatch_tool(tool_name: &str, input: &serde_json::Value) -> String {
    let result = match tool_name {
        "command" => {
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
        "todo_write" => {
            if let Some(todos) = input.get("todos") {
                crate::todo::run_todo_write(todos)
            } else {
                "Error: missing todos".to_string()
            }
        }
        _ => return format!("[ERROR:unknown] Unknown tool: {}", tool_name),
    };

    // Add error prefix for known tools
    if result.starts_with("Error:") {
        with_error_prefix(tool_name, &result)
    } else {
        result
    }
}

/// 工具定义
///
/// 这些定义告诉模型有什么工具可用、每个工具的输入参数是什么。
/// 加一个新工具只需要在这里加一条 ToolDefinition。
#[derive(Serialize, Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// 获取工具定义列表
pub fn get_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "command".to_string(),
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
        ToolDefinition {
            name: "todo_write".to_string(),
            description: "Create and manage a task list for your current coding session.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "maxItems": 20,
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": {"type": "string", "minLength": 1},
                                "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                            },
                            "required": ["content", "status"]
                        }
                    }
                },
                "required": ["todos"]
            }),
        },
    ]
}

#[cfg(test)]
mod test_tool {
    use crate::tools::run_glob;

    #[test]
    fn test_glob() {
        let r = run_glob("**/*.txt");
        print!("{}", r)
    }
}

#[cfg(test)]
mod glob_match_tests {
    use super::*;

    #[test]
    fn literal_exact() {
        assert!(glob_match("a.rs", "a.rs"));
        assert!(!glob_match("a.rs", "b.rs"));
        assert!(!glob_match("src", "src_backup")); // 段必须整体匹配，不做子串
        assert!(!glob_match("src", "src/a.rs"));   // 段数不等
    }

    #[test]
    fn star_within_segment() {
        assert!(glob_match("*.rs", "a.rs"));
        assert!(glob_match("*.rs", "tools.rs"));
        assert!(!glob_match("*.rs", "src"));        // 缺 .rs
        assert!(!glob_match("*.rs", "src/a.rs"));  // * 不跨段
        assert!(glob_match("a*.rs", "abc.rs"));
        assert!(glob_match("a*z", "abcxyz"));
        assert!(!glob_match("a*.rs", "abc.txt"));
    }

    #[test]
    fn question_mark() {
        assert!(glob_match("?.rs", "a.rs"));
        assert!(glob_match("?.rs", "b.rs"));
        assert!(!glob_match("?.rs", "ab.rs"));     // ? 只配一个字符
        assert!(!glob_match("?.rs", ".rs"));        // ? 至少要有一个
    }

    #[test]
    fn double_star_recursive() {
        assert!(glob_match("**", "a.rs"));
        assert!(glob_match("**", "src/a.rs"));
        assert!(glob_match("**", "src/sub/a.rs"));
        assert!(glob_match("**/*.rs", "a.rs"));      // ** 匹配零段
        assert!(glob_match("**/*.rs", "src/a.rs"));
        assert!(glob_match("**/*.rs", "src/sub/a.rs"));
        assert!(glob_match("src/**/*.rs", "src/a.rs"));
        assert!(glob_match("src/**/*.rs", "src/sub/a.rs"));
        assert!(!glob_match("src/**/*.rs", "a.rs"));      // 不在 src 下
        assert!(!glob_match("src/**/*.rs", "test/a.rs")); // 前缀不符
    }

    #[test]
    fn glob_in_walks_tree() {
        let dir = std::env::temp_dir().join("rust-agent-glob-test-walk");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("a.rs"), b"").unwrap();
        std::fs::write(dir.join("b.txt"), b"").unwrap();
        std::fs::write(dir.join("src").join("c.rs"), b"").unwrap();

        let mut got = glob_in("**/*.rs", &dir);
        got.sort();
        let mut want = vec!["a.rs".to_string(), "src/c.rs".to_string()];
        want.sort();
        assert_eq!(got, want);

        let mut top = glob_in("*.rs", &dir);
        top.sort();
        assert_eq!(top, vec!["a.rs".to_string()]);

        assert_eq!(glob_in("zzz", &dir), Vec::<String>::new());

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod dispatch_tool_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_error_prefix_on_read_file_error() {
        let result = dispatch_tool("read_file", &json!({"path": "nonexistent.txt"}));
        assert!(result.starts_with("[ERROR:read_file]"));
    }

    #[test]
    fn test_error_prefix_on_write_file_error() {
        let result = dispatch_tool("write_file", &json!({"path": "/", "content": "test"}));
        assert!(result.starts_with("[ERROR:write_file]"));
    }

    #[test]
    fn test_error_prefix_on_command_error() {
        let result = dispatch_tool("command", &json!({}));
        assert!(result.starts_with("[ERROR:command]"));
    }

    #[test]
    fn test_error_prefix_on_glob_no_matches() {
        let result = dispatch_tool("glob", &json!({"pattern": "**/*.zzzzzzzzz"}));
        assert!(result.starts_with("[ERROR:glob]"));
    }

    #[test]
    fn test_no_error_prefix_on_success() {
        // Create a temp file for testing
        use std::fs;
        let temp_file = std::env::temp_dir().join("test_read.txt");
        fs::write(&temp_file, "hello world").unwrap();

        // Change to temp dir for safe_path
        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(std::env::temp_dir()).unwrap();

        let result = dispatch_tool("read_file", &json!({"path": "test_read.txt"}));

        std::env::set_current_dir(original_dir).unwrap();
        fs::remove_file(temp_file).ok();

        assert!(!result.starts_with("[ERROR:"));
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_error_prefix_on_unknown_tool() {
        let result = dispatch_tool("foo_bar", &json!({}));
        assert_eq!(result, "[ERROR:unknown] Unknown tool: foo_bar");
    }
}