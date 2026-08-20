/*
builtins.rs - 内置钩子集合

main.rs 默认注册的钩子集中于此(5 个内置钩子全部到位):
  ContextInjectHook  UserPromptSubmit  记录工作目录
  LargeOutputHook    PostToolUse       输出过大提醒
  SummaryHook        Stop              收尾工具次数统计
  TodoReminderHook   PostToolUse       3 轮未 todo_write 时提醒
  PermissionHook     PreToolUse        三道闸门权限管线

均实现 hooks.rs 中对应 trait, 通过 hooks.on_* 注册。
*/

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::client::{ContentBlock, Message};
use crate::hooks::{PostToolHook, PreToolHook, PromptHook, StopHook};
use crate::output;
use crate::tools::registry::ToolRegistry;
use crate::tools::trait_def::PermissionCheck;
use crate::tools::workdir;

/// PostToolUse: 在 3 轮未使用 todo_write 时注入提醒。
/// 计数器为 owned 字段, 不再依赖 static 全局 —— 不同 Hooks 实例互不干扰。
pub struct TodoReminderHook {
    rounds_since_todo: AtomicUsize,
}

impl TodoReminderHook {
    pub fn new() -> Self {
        Self {
            rounds_since_todo: AtomicUsize::new(0),
        }
    }
}

impl Default for TodoReminderHook {
    fn default() -> Self {
        Self::new()
    }
}

impl PostToolHook for TodoReminderHook {
    fn on_post_tool(&self, name: &str, _input: &serde_json::Value, _output: &str) -> Option<String> {
        if name == "todo_write" {
            self.rounds_since_todo.store(0, Ordering::SeqCst);
            None
        } else {
            let count = self.rounds_since_todo.fetch_add(1, Ordering::SeqCst) + 1;
            if count >= 3 {
                self.rounds_since_todo.store(0, Ordering::SeqCst);
                Some("<reminder>Update your todos.</reminder>".to_string())
            } else {
                None
            }
        }
    }
}

/// UserPromptSubmit: 记录当前工作目录。
pub struct ContextInjectHook;

impl PromptHook for ContextInjectHook {
    fn on_prompt(&self, _query: &str) {
        tracing::info!("[HOOK] UserPromptSubmit: working in {}", workdir().display());
    }
}

/// PostToolUse: 输出过大时提醒。
pub struct LargeOutputHook;

impl PostToolHook for LargeOutputHook {
    fn on_post_tool(&self, name: &str, _input: &serde_json::Value, output: &str) -> Option<String> {
        if output.len() > 100_000 {
            tracing::warn!("[HOOK] Large output from {}: {} chars", name, output.len());
        }
        None
    }
}

/// Stop: 收尾统计本轮用过的工具次数。
pub struct SummaryHook;

impl StopHook for SummaryHook {
    fn on_stop(&self, messages: &[Message]) -> Option<String> {
        let tool_count = messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter(|b| matches!(b, ContentBlock::ToolResult { .. }))
            .count();
        tracing::info!("[HOOK] Stop: session used {} tool calls", tool_count);
        None
    }
}

// ---- 权限钩子 (原 permission.rs) ----

/// 闸门 1: 硬拒绝列表 —— 永远禁止
/// 使用正则表达式而不是简单的字符串包含，防止编码绕过
const DENY_PATTERNS: &[&str] = &[
    r"(?i)\brm\s+-rf\s+/?",           // rm -rf / 及其变体
    r"(?i)\bsudo\b",                    // sudo 命令
    r"(?i)\b(shutdown|reboot|halt|poweroff)\b",  // 系统关机相关
    r"(?i)\b(mkfs|dd\s+if=)\b",        // 磁盘格式化和直接写入
    r"(?i)>?\s*/dev/sd[ab]\d?",         // 直接写入块设备
    r"(?i)\b(chmod)\s+777",             // 危险权限设置
    r"(?i)\b(chown)\s+-R\s+root:",      // 递归改变所有者
];

/// 需要额外批准的命令模式
const APPROVAL_PATTERNS: &[&str] = &[
    r"(?i)\b(rm|dd|mkfs)\b",           // 删除、写入块设备命令
    r"(?i)\b(sudo|su|doas)\b",          // 提权命令
    r"(?i)\b(curl|wget)\s+.*\|\s*(sh|bash)",  // 通过管道执行下载内容
    r"(?i)\beval\b",                    // eval 命令
];

/// 检查命令是否匹配硬拒绝列表
/// 使用正则表达式进行模式匹配，防止编码绕过
fn check_deny_patterns(command: &str) -> Option<&'static str> {
    for pattern in DENY_PATTERNS {
        let regex = match regex::Regex::new(pattern) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if regex.is_match(command) {
            // 返回简化的描述而不是完整的正则模式
            let simple_reason = if pattern.contains("rm") {
                "rm -rf / (destructive command)"
            } else if pattern.contains("sudo") {
                "sudo (privilege escalation)"
            } else if pattern.contains("shutdown") {
                "system shutdown command"
            } else if pattern.contains("dd") {
                "dd (direct disk write)"
            } else if pattern.contains("chmod") {
                "chmod 777 (insecure permissions)"
            } else {
                "dangerous command"
            };
            return Some(simple_reason);
        }
    }
    None
}

/// 检查命令是否需要用户批准
fn requires_approval(command: &str) -> Option<&'static str> {
    for pattern in APPROVAL_PATTERNS {
        let regex = match regex::Regex::new(pattern) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if regex.is_match(command) {
            return Some("This command may modify system state and requires approval");
        }
    }
    None
}

/// 检查命令是否包含编码绕过尝试
fn detect_encoding_bypass(command: &str) -> Option<&'static str> {
    // 检查是否有十六进制编码的命令
    if command.contains(r"\x") || command.contains(r"\u") {
        return Some("command contains escape sequences (encoding bypass attempt)");
    }

    // 检查是否有明显的 base64 编码
    if command.len() > 100 {
        let alphanumeric_count = command.chars()
            .filter(|c| c.is_ascii_alphanumeric() || c.is_whitespace())
            .count();
        if alphanumeric_count > command.len() * 9 / 10 {
            return Some("command appears to be encoded (base64-like pattern)");
        }
    }

    // 检查是否有重复的引号或转义字符（可能是混淆）
    let backslash_count = command.chars().filter(|&c| c == '\\').count();
    let quote_count = command.chars().filter(|&c| c == '"' || c == '\'').count();
    if backslash_count > 5 || quote_count > 10 {
        return Some("command contains suspicious escaping or quoting");
    }

    None
}

/// 闸门 3: 暂停等用户确认
fn ask_user(name: &str, input: &serde_json::Value, reason: &str) -> bool {
    output::permission(reason, name, input);
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok();
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
        // 闸门 0: 检测编码绕过尝试
        if name == "command" {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(reason) = detect_encoding_bypass(cmd) {
                output::blocked(reason);
                return Some(format!("Permission denied: {}", reason));
            }
        }

        // 闸门 1: 硬拒绝（使用正则模式匹配）
        if name == "command" {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(reason) = check_deny_patterns(cmd) {
                output::blocked(reason);
                return Some(format!("Permission denied: {}", reason));
            }
        }

        // 闸门 1.5: 命令结构验证
        if name == "command" {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(reason) = validate_command_structure(cmd) {
                output::blocked(reason);
                return Some(format!("Permission denied: {}", reason));
            }
        }

        // 闸门 2: 检查是否需要用户批准
        if name == "command" {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(reason) = requires_approval(cmd) {
                // 闸门 3: 向用户请求确认
                if !ask_user(name, input, reason) {
                    return Some("Permission denied by user".to_string());
                }
            }
        }

        // 闸门 4: 使用 registry 检查工具权限
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

/// PreToolUse hook for teammates: non-interactive version of PermissionHook.
///
/// 同样的 deny/approval 闸门，但**不读 stdin**：任何需要用户批准的命令/路径
/// 在 teammate 上下文里直接拒绝（teammate 是后台非交互的）。返回 Some(reason)
/// 表示拦截，None 表示放行。
pub struct TeammatePermissionHook;

impl PreToolHook for TeammatePermissionHook {
    fn on_pre_tool(&self, registry: &ToolRegistry, name: &str, input: &serde_json::Value) -> Option<String> {
        if name == "command" {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(reason) = detect_encoding_bypass(cmd) {
                return Some(format!("Permission denied: {}", reason));
            }
            if let Some(reason) = check_deny_patterns(cmd) {
                return Some(format!("Permission denied: {}", reason));
            }
            if let Some(reason) = validate_command_structure(cmd) {
                return Some(format!("Permission denied: {}", reason));
            }
            if requires_approval(cmd).is_some() {
                return Some("Permission denied: teammate context cannot prompt for approval".to_string());
            }
        }
        // File tools' check_permission returns NeedsApproval for paths escaping
        // the workspace — teammates can't prompt, so deny those too.
        if let Some(permission_check) = registry.check_permission(name, input) {
            match permission_check {
                PermissionCheck::Pass => {}
                PermissionCheck::NeedsApproval(reason) => {
                    return Some(format!("Permission denied (teammate, non-interactive): {}", reason));
                }
            }
        }
        None
    }
}

/// 验证命令结构，防止命令注入和混淆攻击
fn validate_command_structure(command: &str) -> Option<&'static str> {
    // 检查是否有命令分隔符（子命令）
    if command.contains(';') || command.contains("&&") || command.contains("||") {
        return Some("command contains command separators (injection attempt)");
    }

    // 检查是否有管道到 shell
    if command.contains('|') || command.contains(">") || command.contains("<") {
        // 简单的输出重定向通常允许，但需要验证
        let has_shell_redirect = regex::Regex::new(r"\|\s*(bash|sh|zsh|fish)").is_ok()
            && regex::Regex::new(r"\|\s*(bash|sh|zsh|fish)").unwrap().is_match(command);

        if has_shell_redirect {
            return Some("command pipes to shell (injection attempt)");
        }
    }

    // 检查是否有命令替换（$() 或 ``）
    if regex::Regex::new(r"\$\(|`[^`]*`").unwrap().is_match(command) {
        return Some("command contains command substitution (injection attempt)");
    }

    // 检查是否有 $(...) 模式
    if regex::Regex::new(r"\$\{").unwrap().is_match(command) {
        return Some("command contains variable expansion (injection attempt)");
    }

    None
}

