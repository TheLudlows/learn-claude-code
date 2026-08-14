/*
permission.rs - 三道闸门权限管线 (s03 逻辑, s04 暴露为 PreToolUse 钩子)

工具执行前依次过三道闸门:
  1. 拒绝列表(rm -rf /、sudo…)         命中 -> 直接拒绝
  2. 规则匹配(写工作区外 / 破坏性命令)   命中 -> 交给闸门 3
  3. 用户审批(暂停等 y/N)              用户决定
三道都没命中 -> 放行。

s04: 本模块的 check_permission() 已改成 permission_hook() —— 返回
Option<String>(Some=拦截理由, None=放行), 注册为 PreToolUse 钩子,
由 hooks.trigger_pre_tool() 触发; 闸门内部逻辑一字不改。

注: 字符串匹配仅用于演示闸门位置, 非完整安全边界(见 s03 README)。
文件类工具另有 tools::safe_path 做工作区沙箱(defense in depth)。
*/

use crate::tools::workdir;
use std::io::{self, Write};
use std::path::{Component, PathBuf};

/// 闸门 1: 硬拒绝列表 —— 永远禁止
const DENY_LIST: &[&str] = &[
    "rm -rf /", "sudo", "shutdown", "reboot", "mkfs", "dd if=", "> /dev/sda",
];

fn check_deny_list(command: &str) -> Option<&'static str> {
    DENY_LIST.iter().copied().find(|p| command.contains(p))
}

/// 词法判断相对路径是否会逃出工作区(不访问文件系统, 支持不存在的路径)
fn escapes_workspace(path: &str) -> bool {
    !normalize(&workdir(), path).starts_with(workdir())
}

/// 把 `base/path` 词法归一化: 消解 `..`/`.`, 绝对路径则替换 base。
/// 不访问文件系统, 因而对尚不存在的路径(新建文件)也成立。
fn normalize(base: &std::path::Path, path: &str) -> PathBuf {
    let mut norm = PathBuf::new();
    for c in base.join(path).components() {
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

/// 闸门 2: 规则匹配 —— 命中返回需向用户说明的理由, 交给闸门 3
fn check_rules(name: &str, input: &serde_json::Value) -> Option<&'static str> {
    match name {
        "read_file" | "write_file" | "edit_file" => {
            let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if escapes_workspace(path) {
                return Some("Access outside workspace");
            }
        }
        "bash" => {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if ["rm ", "> /etc/", "chmod 777"].iter().any(|kw| cmd.contains(kw)) {
                return Some("Potentially destructive command");
            }
        }
        _ => {}
    }
    None
}

/// 闸门 3: 暂停等用户确认
fn ask_user(name: &str, input: &serde_json::Value, reason: &str) -> bool {
    println!("\n\x1b[33m[permission] {}\x1b[0m", reason);
    println!("   Tool: {}({})", name, input);
    print!("   Allow? [y/N] ");
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin().read_line(&mut line).ok();
    matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}

/// PreToolUse 钩子: 三道闸门串联, 返回 Some(reason) 表示拦截, None 表示放行。
///
/// 循环经 `hooks.trigger_pre_tool()` 调用本函数; 末尾返回 None(而非 false),
/// 才符合 "三道都没命中 -> 放行" 的语义 —— 这也修掉了 s03 check_permission
/// 末尾 `return false` 把所有工具都拒掉的 bug。
pub fn permission_hook(name: &str, input: &serde_json::Value) -> Option<String> {
    // 闸门 1: 硬拒绝
    if name == "bash" {
        let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
        if let Some(p) = check_deny_list(cmd) {
            println!("\n\x1b[31m[blocked] '{}' is on the deny list\x1b[0m", p);
            return Some(format!("Permission denied: '{}' on deny list", p));
        }
    }
    // 闸门 2 + 3: 规则命中 -> 问用户
    if let Some(reason) = check_rules(name, input) {
        if !ask_user(name, input, reason) {
            return Some("Permission denied by user".to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn normalize_strips_dotdot() {
        let base = Path::new("/home/u/proj");
        assert_eq!(normalize(base, "src/a.rs"), Path::new("/home/u/proj/src/a.rs"));
        assert_eq!(normalize(base, "src/../a.rs"), Path::new("/home/u/proj/a.rs"));
    }

    #[test]
    fn escapes_relative() {
        // 用真实 cwd(=crate 根), 相对路径在所有平台语义一致
        assert!(!escapes_workspace("src/main.rs")); // 工作区内
        assert!(!escapes_workspace(""));            // 工作目录本身
        assert!(escapes_workspace("../secret"));   // 逃到父目录
        assert!(escapes_workspace("../"));          // 父目录
    }

    #[test]
    fn deny_list_matches() {
        assert_eq!(check_deny_list("sudo apt update"), Some("sudo"));
        assert!(check_deny_list("rm -rf /").is_some());
        assert!(check_deny_list("ls -la").is_none());
    }

    #[test]
    fn rules_fire_on_destructive() {
        let rm = serde_json::json!({"command": "rm test.txt"});
        assert_eq!(check_rules("bash", &rm), Some("Potentially destructive command"));
        assert_eq!(check_rules("bash", &serde_json::json!({"command":"ls"})), None);
    }

    #[test]
    fn permission_hook_allows_safe() {
        // 安全命令: 不进 deny list, 不命中规则 -> 放行(且不读 stdin)
        assert_eq!(
            permission_hook("bash", &serde_json::json!({"command": "ls"})),
            None
        );
    }

    #[test]
    fn permission_hook_blocks_deny_list() {
        // 命中闸门 1, 直接拦截(且不读 stdin)
        assert_eq!(
            permission_hook("bash", &serde_json::json!({"command": "sudo apt update"})),
            Some("Permission denied: 'sudo' on deny list".to_string())
        );
    }
}
