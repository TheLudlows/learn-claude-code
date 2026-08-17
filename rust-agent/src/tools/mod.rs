/*
tools/mod.rs - Tool system module

This module contains the tool system infrastructure:
- trait_def: Core abstractions (Tool trait, ToolContext, PermissionCheck)
- registry: Tool registry for tool management and dispatch
- (Future modules will contain individual tool implementations)
*/

// Core modules
pub mod trait_def;
pub mod registry;

// Tool module implementations
pub mod command;
pub mod read_file;
pub mod write_file;
pub mod edit_file;
pub mod glob;
pub mod load_skill;
pub mod todo_write;
pub mod task;

// Re-exports for convenient access
pub use self::registry::ToolRegistry;
pub use self::trait_def::{PermissionCheck, Tool, ToolContext};

// Shared utility functions from tools_legacy.rs

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
pub fn safe_path_in(workdir: &Path, path_str: &str) -> Result<PathBuf, String> {
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
pub fn canonical_form_of(path: &Path) -> Option<PathBuf> {
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
pub fn safe_path(path_str: &str) -> Result<PathBuf, String> {
    safe_path_in(&workdir(), path_str)
}

/// 把命令输出字节解码成字符串：先按 UTF-8（cargo 等现代程序直接用 UTF-8），
/// 失败再按 OEM 代码页解码（cmd.exe 内建命令、git 等在中文 locale 下用 GBK），
/// 都不行才退化为 lossy。避免非 ASCII 被替成 U+FFFD（乱码）。
pub fn decode_console(bytes: &[u8]) -> String {
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
pub fn decode_with_oem_codepage(bytes: &[u8]) -> Option<String> {
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

pub fn run_bash(command: &str) -> String {
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
pub fn run_read_file(path: &str, limit: Option<u32>) -> String {
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
pub fn run_write_file(path: &str, content: &str) -> String {
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
pub fn run_edit_file(path: &str, old_text: &str, new_text: &str) -> String {
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
pub fn to_unix_path(p: &str) -> String {
    p.replace('\\', "/")
}

/// 单个路径段是否匹配单个模式段（段内不含 `/`）。
/// `*` 匹配任意长度（含空）的字符；`?` 匹配单个字符；其余按字面量。
pub fn seg_match(pat: &[u8], text: &[u8]) -> bool {
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
pub fn glob_match(pattern: &str, path: &str) -> bool {
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
pub fn glob_in(pattern: &str, base: &Path) -> Vec<String> {
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
pub fn run_glob(pattern: &str) -> String {
    let results = glob_in(pattern, &workdir());
    if results.is_empty() {
        "Error: no matches".to_string()
    } else {
        results.join("\n")
    }
}

/// 词法检查路径是否可能越界（不访问文件系统）。
///
/// 这是 `safe_path_in` 的轻量版本，用于在路径尚不存在时快速检查。
/// 对路径做词法归一化后检查是否仍以工作目录开头。
pub fn escapes_workspace_lexical(path_str: &str) -> bool {
    let workdir = workdir();
    let mut norm = PathBuf::new();
    for c in workdir.join(path_str).components() {
        match c {
            Component::ParentDir => {
                norm.pop();
            }
            Component::CurDir => {}
            other => norm.push(other.as_os_str()),
        }
    }
    !norm.starts_with(&workdir)
}

/// 路径归一化（消解 `..`/`.`，不访问文件系统）。
///
/// 对路径做词法归一化，返回一个不含 `.` 和 `..` 的路径。
pub fn normalize(path_str: &str) -> PathBuf {
    let mut norm = PathBuf::new();
    for c in PathBuf::from(path_str).components() {
        match c {
            Component::ParentDir => {
                norm.pop();
            }
            Component::CurDir => {}
            other => norm.push(other.as_os_str()),
        }
    }
    norm
}

/// Build and return a tool registry with all tools registered.
///
/// This function populates the registry with individual tool implementations.
/// Additional tools will be added in Tasks 6-12.
pub fn build_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();

    // Task 5: Command tool for shell command execution
    registry.register(Box::new(crate::tools::command::CommandTool));

    // Task 6: Read file tool for reading file contents
    registry.register(Box::new(crate::tools::read_file::ReadFileTool));

    // Task 7: Write file tool for writing file contents
    registry.register(Box::new(crate::tools::write_file::WriteFileTool));

    // Task 8: Edit file tool for editing file contents
    registry.register(Box::new(crate::tools::edit_file::EditFileTool));

    // Task 9: Glob tool for file pattern matching
    registry.register(Box::new(crate::tools::glob::GlobTool));

    // Task 10: Load skill tool for loading skill definitions
    registry.register(Box::new(crate::tools::load_skill::LoadSkillTool));

    // Task 11: Todo write tool for updating todo tasks
    registry.register(Box::new(crate::tools::todo_write::TodoWriteTool));

    // Task 12: Task tool for running subagents
    registry.register(Box::new(crate::tools::task::TaskTool));

    registry
}

// Tests moved from tools_legacy.rs

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