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
use std::path::{Component, Path, PathBuf};
use std::process::Command;

/// 工作目录
pub fn workdir() -> PathBuf {
    env::current_dir().unwrap_or_else(|_| ".".into())
}

/// 路径安全校验 - 确保路径在工作目录内
///
/// 先对 `canonical_workdir/path` 做**词法归一化**（消解 `..`/`.`，不访问文件系统）
/// 得到绝对路径，再用**canonical 对 canonical**的方式判越界——
/// 因此**对尚不存在的路径（如 `write_file` 新建文件/目录）也成立**：
/// 原实现对目标路径 `canonicalize()`，路径不存在时直接 `Err`，导致永远写不进去。
///
/// 越界比较必须用 canonical 形式：Windows 下 `canonicalize()` 会给路径加 `\\?\`
/// verbatim 前缀并展开 8.3 短名，而词法归一化结果（尤其当 `path_str` 是绝对路径时）
/// 可能既无前缀又含短名——直接 `starts_with` 会把工作区内路径误判成越界（C3 回归）。
fn safe_path_in(workdir: &Path, path_str: &str) -> Result<PathBuf, String> {
    // canonicalize 工作目录本身：工作目录一定存在，不会失败。
    // 以 canonical workdir 作 base 做词法归一化，避免 cwd 自身含符号链接
    // 导致「词法路径 vs canonical 工作目录」误判越界。
    let workdir_canonical = workdir
        .canonicalize()
        .map_err(|e| format!("Error: {}", e))?;

    // 词法归一化：base.join(path) 后按 components 消解 `..`/`.`，不碰文件系统。
    // 注意：path_str 为绝对路径时 join 会替换 base——这是预期行为（绝对路径本就不相对 base）。
    let mut norm = PathBuf::new();
    for c in workdir_canonical.join(path_str).components() {
        match c {
            Component::ParentDir => {
                norm.pop();
            }
            Component::CurDir => {}
            other => norm.push(other.as_os_str()),
        }
    }

    // 越界检查：用 canonical 形式比较（见函数注释）。
    // 连祖先都无法 canonicalize 时按失败闭合处理（安全侧）。
    let within = match canonical_form_of(&norm) {
        Some(c) => c.starts_with(&workdir_canonical),
        None => false,
    };
    if !within {
        return Err(format!("Error: path escapes workspace {:?}, {:?}", workdir_canonical, norm));
    }

    // 返回值：已存在路径返回 canonical（解析符号链接/junction）；尚不存在的路径用词法结果放行。
    if norm.exists() {
        norm.canonicalize().map_err(|e| format!("Error: {}", e))
    } else {
        Ok(norm)
    }
}

/// 把路径归一成 canonical 形式，专供越界比较使用。
///
/// - 路径本身存在：直接 `canonicalize()`。
/// - 路径不存在：沿祖先上溯到第一个已存在的目录，`canonicalize()` 它，再拼回尚不存在的尾部。
///   这样不存在的路径（`write_file` 新建文件/目录）也能得到与 canonical base 可比的形态。
/// - 连根目录都无法 `canonicalize()`：返回 `None`（调用方按失败闭合处理）。
fn canonical_form_of(path: &Path) -> Option<PathBuf> {
    // 快路径：路径本身存在。
    if let Ok(c) = path.canonicalize() {
        return Some(c);
    }
    // 路径不存在：上溯找第一个已存在的祖先，canonicalize 后拼回尾部。
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path.to_path_buf();
    loop {
        match cur.canonicalize() {
            Ok(canon) => {
                let mut full = canon;
                for seg in tail.into_iter().rev() {
                    full.push(seg);
                }
                return Some(full);
            }
            Err(_) => {
                // cur 不存在：记下这一段名字，继续上溯。
                let name = cur.file_name()?.to_owned();
                tail.push(name);
                cur = cur.parent()?.to_path_buf();
            }
        }
    }
}

/// 路径安全校验 - 确保路径在当前工作目录内。
fn safe_path(path_str: &str) -> Result<PathBuf, String> {
    safe_path_in(&workdir(), path_str)
}

/// 把命令输出字节解码成字符串：先按 UTF-8（cargo 等现代程序直接用 UTF-8），
/// 失败再按 OEM 代码页解码（cmd.exe 内建命令、git 等在中文 locale 下用 GBK），
/// 都不行才退化为 lossy。避免非 ASCII 被替成 U+FFFD（乱码）。
fn decode_console(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    #[cfg(windows)]
    if let Some(s) = decode_with_oem_codepage(bytes) {
        return s;
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// 按 OEM 代码页（中文 locale 为 936/GBK）把字节解成 UTF-16 再转 String。
/// 失败返回 None，让调用方退化为 lossy。
#[cfg(windows)]
fn decode_with_oem_codepage(bytes: &[u8]) -> Option<String> {
    use std::os::raw::{c_int, c_uchar};
    extern "system" {
        fn GetOEMCP() -> u32;
        fn MultiByteToWideChar(
            CodePage: u32,
            dwFlags: u32,
            lpMultiByteStr: *const c_uchar,
            cbMultiByte: c_int,
            lpWideCharStr: *mut u16,
            cchWideChar: c_int,
        ) -> c_int;
    }
    if bytes.is_empty() {
        return Some(String::new());
    }
    let cp = unsafe { GetOEMCP() };
    let n = bytes.len() as c_int;
    let size = unsafe {
        MultiByteToWideChar(cp, 0, bytes.as_ptr(), n, std::ptr::null_mut(), 0)
    };
    if size <= 0 {
        return None;
    }
    let mut buf: Vec<u16> = vec![0u16; size as usize];
    let written = unsafe {
        MultiByteToWideChar(cp, 0, bytes.as_ptr(), n, buf.as_mut_ptr(), size)
    };
    if written <= 0 {
        return None;
    }
    buf.truncate(written as usize);
    Some(String::from_utf16_lossy(&buf))
}

/// 执行命令（跨平台）
///
/// - Windows: 使用 cmd.exe
/// - Unix: 使用 bash
///
/// 危险命令的拦截已移至 permission::permission_hook 闸门(s03/s04),
/// 在到达这里之前就已被拒; safe_path 仍是文件工具的工作区沙箱。
const MAX_OUTPUT_BYTES: usize = 50_000;

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
            let stdout = decode_console(&output.stdout);
            let stderr = decode_console(&output.stderr);
            let result = format!("{}\n{}", stdout, stderr).trim().to_string();
            if result.is_empty() {
                "(no output)".to_string()
            } else if result.len() > MAX_OUTPUT_BYTES {
                // 按字节上限截断，但必须落在 UTF-8 字符边界上，否则
                // `result[..end]` 会在多字节序列（CJK 输出极常见）中间 panic。
                let mut end = MAX_OUTPUT_BYTES;
                while !result.is_char_boundary(end) {
                    end -= 1;
                }
                result[..end].to_string()
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
    } else {
        // 已有 [ERROR: 前缀或无前缀：原样返回（两者行为一致，合并分支）。
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
        "load_skill" => crate::skills::run_load_skill(input),
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
#[derive(Serialize, Clone, Debug)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// 基础工具定义（不含 task，用于子 agent）
fn get_base_tool_definitions() -> Vec<ToolDefinition> {
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
        // s07: 按名称加载完整 SKILL.md 正文。name 是注册表键，不是文件路径。
        ToolDefinition {
            name: "load_skill".to_string(),
            description: "Load the full SKILL.md content by skill name.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" }
                },
                "required": ["name"]
            }),
        },
        ToolDefinition {
            name: "todo_write".to_string(),
            description: "Create or replace the todo list for multi-step tasks. \
                          Each call replaces the entire list; at most one item may be \
                          in_progress. Use this to plan before starting work and \
                          update statuses as you progress."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "maxItems": 20,
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": {
                                    "type": "string",
                                    "description": "The task description."
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"],
                                    "description": "Defaults to pending if omitted."
                                }
                            },
                            "required": ["content"]
                        }
                    }
                },
                "required": ["todos"]
            }),
        },
    ]
}

/// s06: task 工具定义
fn get_task_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "task".to_string(),
        description: "Run a subagent with fresh conversation context and return its final text.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": { "type": "string" }
            },
            "required": ["prompt"]
        }),
    }
}

/// 获取子 agent 的工具列表（不含 task 工具，防止递归）
pub fn get_subagent_tool_definitions() -> Vec<ToolDefinition> {
    get_base_tool_definitions()
}

/// 获取完整工具列表（含 task 工具，用于父 agent）
pub fn get_tool_definitions() -> Vec<ToolDefinition> {
    let mut tools = get_base_tool_definitions();
    tools.push(get_task_tool_definition());
    tools
}

#[cfg(test)]
mod test_tool {
    use crate::tools_legacy::run_glob;

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
    fn test_read_file_with_absolute_path_in_workspace() {
        // C3 回归：调用方传「工作区内文件的绝对路径」时，绝对路径会让 join 替换
        // canonical base（丢掉 `\\?\` 前缀、保留 8.3 短名），词法 starts_with 误判越界，
        // 进而 `[ERROR:read_file] path escapes workspace`。修复后应正常读到内容。
        use std::fs;
        let dir = std::env::temp_dir().join("rust-agent-read-abs");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("abs_read.txt");
        fs::write(&file, "absolute ok").unwrap();

        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();

        let abs = file.to_string_lossy().to_string();
        let result = dispatch_tool("read_file", &json!({"path": abs}));

        std::env::set_current_dir(original_dir).unwrap();
        let _ = fs::remove_dir_all(&dir);

        assert!(!result.starts_with("[ERROR:"), "should not error: {:?}", result);
        assert_eq!(result, "absolute ok");
    }

    #[test]
    fn test_error_prefix_on_unknown_tool() {
        let result = dispatch_tool("foo_bar", &json!({}));
        assert_eq!(result, "[ERROR:unknown] Unknown tool: foo_bar");
    }
}

#[cfg(test)]
mod run_bash_tests {
    use super::run_bash;

    /// 回归：cmd.exe 在中文 locale 默认按 GBK(936) 输出，`from_utf8_lossy` 会把
    /// 非 ASCII（如 `ver` 输出里的 "版本"）替换成 U+FFFD（乱码）。强制 UTF-8 后
    /// 应为合法 UTF-8 中文，不含替换符。
    #[test]
    #[cfg(windows)]
    fn decodes_non_ascii_without_replacement_chars() {
        let out = run_bash("ver");
        assert!(
            !out.contains('\u{FFFD}'),
            "命令输出不应含 U+FFFD 替换符（应为合法 UTF-8）: {out:?}"
        );
    }
}

#[cfg(test)]
mod safe_path_tests {
    use super::safe_path_in;
    use std::fs;

    #[test]
    fn allows_nonexistent_path_for_new_file() {
        // C2 回归：write_file 新建文件/目录时目标路径尚不存在，
        // safe_path 不应因 canonicalize 失败而拒绝。
        let dir = std::env::temp_dir().join("rust-agent-safe-path-nonexistent");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let dir_canon = dir.canonicalize().unwrap();

        let got = safe_path_in(&dir, "subdir/newfile.txt");
        assert!(got.is_ok(), "non-existent path should be allowed: {:?}", got);
        let abs = got.unwrap();
        assert!(abs.starts_with(&dir_canon), "{:?} should be under base", abs);
        assert_eq!(abs.file_name(), Some(std::ffi::OsStr::new("newfile.txt")));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_escape_via_dotdot() {
        let dir = std::env::temp_dir().join("rust-agent-safe-path-escape");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let got = safe_path_in(&dir, "../secret.txt");
        assert!(got.is_err(), "path escaping workspace must be rejected");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn existing_path_canonicalized_and_allowed() {
        let dir = std::env::temp_dir().join("rust-agent-safe-path-existing");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("real.txt");
        fs::write(&file, b"hi").unwrap();

        let got = safe_path_in(&dir, "real.txt");
        assert!(got.is_ok(), "existing in-workspace path should be allowed: {:?}", got);
        assert_eq!(
            got.unwrap().canonicalize().unwrap(),
            file.canonicalize().unwrap()
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn absolute_path_in_workspace_is_allowed() {
        // C3 回归：调用方常传绝对路径。绝对路径会让 join 替换 base、丢掉 `\\?\`
        // verbatim 前缀；且 temp_dir 可能用 8.3 短名。两者都使词法 norm 与 canonical
        // base 不可直接 starts_with 比较。canonical 对 canonical 比较后应放行。
        let dir = std::env::temp_dir().join("rust-agent-safe-path-abs");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("inner.txt");
        fs::write(&file, b"x").unwrap();

        let abs = file.to_string_lossy().to_string();
        let got = safe_path_in(&dir, &abs);
        assert!(got.is_ok(), "absolute in-workspace path should be allowed: {:?}", got);
        // 已存在路径应返回 canonical 形式（带 verbatim 前缀），与直接 canonicalize 一致。
        assert_eq!(got.unwrap(), file.canonicalize().unwrap());

        let _ = fs::remove_dir_all(&dir);
    }
}