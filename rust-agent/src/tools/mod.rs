/*
tools/mod.rs - Tool system module hub

Responsibilities:
- Module declarations and re-exports
- Shared utilities: workdir(), path safety (safe_path, safe_path_in)
- Tool registry construction (build_registry)

Tool implementations live in their own submodules:
- command:   Shell command execution (run_bash with timeout)
- read_file: File reading
- write_file: File writing
- edit_file: File editing (text replacement)
- glob:      File pattern matching (via glob crate)
- load_skill, todo_write, task: Higher-level tools
*/

// Core modules
pub mod trait_def;
pub mod registry;

// Tool module implementations
pub mod command;
pub mod read_file;
pub mod write_file;
pub mod edit_file;
pub mod glob_tool;
pub mod load_skill;
pub mod todo_write;
pub mod task;

// Re-exports for convenient access
pub use self::registry::ToolRegistry;
pub use self::trait_def::{PermissionCheck, Tool, ToolContext};

use std::env;
use std::path::{Component, Path, PathBuf};

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
pub fn safe_path(path_str: &str) -> Result<PathBuf, String> {
    safe_path_in(&workdir(), path_str)
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
pub fn build_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();

    registry.register(Box::new(crate::tools::command::CommandTool));
    registry.register(Box::new(crate::tools::read_file::ReadFileTool));
    registry.register(Box::new(crate::tools::write_file::WriteFileTool));
    registry.register(Box::new(crate::tools::edit_file::EditFileTool));
    registry.register(Box::new(crate::tools::glob_tool::GlobTool));
    registry.register(Box::new(crate::tools::load_skill::LoadSkillTool));
    registry.register(Box::new(crate::tools::todo_write::TodoWriteTool));
    registry.register(Box::new(crate::tools::task::TaskTool));

    registry
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
