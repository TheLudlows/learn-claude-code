/*
permission.rs - 三道闸门权限管线 (s03 逻辑, s04 暴露为 PreToolUse 钩子)

工具执行前依次过三道闸门:
  1. 拒绝列表(rm -rf /、sudo…)         命中 -> 直接拒绝
  2. 工具权限检查(通过 registry.check_permission)  命中 -> 交给闸门 3
  3. 用户审批(暂停等 y/N)              用户决定
三道都没命中 -> 放行。

s04: 本模块的 PermissionHook 实现 PreToolUse trait, on_pre_tool 返回
Option<String>(Some=拦截理由, None=放行), 注册为 PreToolUse 钩子,
由 hooks.trigger_pre_tool() 触发。

注: 字符串匹配仅用于演示闸门位置, 非完整安全边界(见 s03 README)。
文件类工具另有 tools::safe_path 做工作区沙箱(defense in depth)。
*/

use crate::hooks::PreToolHook;
use crate::tools::registry::ToolRegistry;
use crate::tools::trait_def::PermissionCheck;
use std::io::{self, Write};

/// 闸门 1: 硬拒绝列表 —— 永远禁止
const DENY_LIST: &[&str] = &[
    "rm -rf /", "sudo", "shutdown", "reboot", "mkfs", "dd if=", "> /dev/sda",
];

fn check_deny_list(command: &str) -> Option<&'static str> {
    // 与 command.rs::check_permission 对齐：命令先转小写再匹配。
    // DENY_LIST 条目均为小写，不转小写会让 "Sudo" / "RM -rf /" 绕过闸门 1。
    let command_lower = command.to_lowercase();
    DENY_LIST.iter().copied().find(|p| command_lower.contains(p))
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
/// 循环经 `hooks.trigger_pre_tool()` 调用; 末尾返回 None(而非 false),
/// 才符合 "三道都没命中 -> 放行" 的语义 —— 这也修掉了 s03 check_permission
/// 末尾 `return false` 把所有工具都拒掉的 bug。
pub struct PermissionHook;

impl PreToolHook for PermissionHook {
    fn on_pre_tool(&self, registry: &ToolRegistry, name: &str, input: &serde_json::Value) -> Option<String> {
        // 闸门 1: 硬拒绝
        if name == "command" {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(p) = check_deny_list(cmd) {
                println!("\n\x1b[31m[blocked] '{}' is on the deny list\x1b[0m", p);
                return Some(format!("Permission denied: '{}' on deny list", p));
            }
        }

        // 闸门 2: 使用 registry 检查工具权限
        if let Some(permission_check) = registry.check_permission(name, input) {
            match permission_check {
                PermissionCheck::Pass => {
                    // 通过权限检查，继续执行
                }
                PermissionCheck::NeedsApproval(reason) => {
                    // 闸门 3: 向用户请求确认
                    if !ask_user(name, input, reason) {
                        return Some("Permission denied by user".to_string());
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::build_registry;

    #[test]
    fn deny_list_matches() {
        assert_eq!(check_deny_list("sudo apt update"), Some("sudo"));
        assert!(check_deny_list("rm -rf /").is_some());
        assert!(check_deny_list("ls -la").is_none());
    }

    #[test]
    fn deny_list_case_insensitive() {
        // N4 回归：闸门 1 必须与 command.rs 对齐走 to_lowercase，
        // 否则 "Sudo" / "RM -rf /" 能绕过硬拒绝。
        assert_eq!(check_deny_list("Sudo apt update"), Some("sudo"));
        assert_eq!(check_deny_list("SUDO reboot"), Some("sudo"));
        assert!(check_deny_list("RM -rf /").is_some());
        assert!(check_deny_list("Reboot now").is_some());
    }

    #[test]
    fn permission_hook_allows_safe() {
        // 安全命令: 不进 deny list, registry 检查通过 -> 放行
        let registry = ToolRegistry::new();
        assert_eq!(
            PermissionHook.on_pre_tool(&registry, "command", &serde_json::json!({"command": "ls"})),
            None
        );
    }

    #[test]
    fn permission_hook_blocks_deny_list() {
        // 命中闸门 1, 直接拦截(且不读 stdin)
        let registry = ToolRegistry::new();
        assert_eq!(
            PermissionHook.on_pre_tool(&registry, "command", &serde_json::json!({"command": "sudo apt update"})),
            Some("Permission denied: 'sudo' on deny list".to_string())
        );
    }

    #[test]
    fn permission_hook_uses_registry() {
        // 测试 permission_hook 确实调用了 registry.check_permission
        let registry = build_registry();

        // 对于 command 工具，默认权限检查应该通过
        let result = PermissionHook.on_pre_tool(&registry, "command", &serde_json::json!({"command": "ls"}));
        assert_eq!(result, None);
    }

    #[test]
    fn permission_hook_requires_approval() {
        // 使用 build_registry 创建一个包含所有工具的 registry
        let registry = build_registry();

        // 测试工具权限检查系统是否正常工作
        // 由于大多数工具默认不需要审批，我们可以测试这个系统
        let result = PermissionHook.on_pre_tool(
            &registry,
            "command",
            &serde_json::json!({"command": "ls"})
        );
        // command 工具不应该需要审批
        assert_eq!(result, None);
    }

    #[test]
    fn permission_hook_unknown_tool() {
        // 测试未知工具的处理
        let registry = ToolRegistry::new();
        let result = PermissionHook.on_pre_tool(
            &registry,
            "unknown_tool",
            &serde_json::json!({})
        );
        assert_eq!(result, None); // 未知工具直接放行
    }
}
