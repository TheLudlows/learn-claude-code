/*
builtins.rs - 内置钩子集合

main.rs 默认注册的钩子集中于此(5 个内置钩子全部到位):
  ContextInjectHook  UserPromptSubmit  记录工作目录
  LargeOutputHook    PostToolUse       输出过大提醒
  SummaryHook        Stop              收尾工具次数统计
  TodoReminderHook   PostToolUse       每次工具执行后注入当前 todo 列表
  PermissionHook     PreToolUse        三道闸门权限管线

均实现 hooks.rs 中对应 trait, 通过 hooks.on_* 注册。
*/

use std::sync::Arc;

use async_trait::async_trait;

use crate::client::{ContentBlock, Message};
use crate::hooks::{HookContext, PostToolHook, PreToolHook, PromptHook, StopHook};
use crate::todo::SharedTodoManager;
use crate::tools::registry::ToolRegistry;
use crate::tools::trait_def::PermissionCheck;
use crate::tools::workdir;

/// PostToolUse: 每 3 轮注入一次 todo 提醒（对齐 Python s15）。
/// 持有计数器，调用 todo_write 后重置计数器，否则每轮递增，达到 3 时提醒并归零。
pub struct TodoReminderHook {
    #[allow(dead_code)]  // 保留引用以保持 API 兼容性，未来可能需要访问 todo 列表
    todo_manager: Arc<SharedTodoManager>,
    counter: Arc<std::sync::Mutex<usize>>,
}

impl TodoReminderHook {
    pub fn new(todo_manager: Arc<SharedTodoManager>) -> Self {
        Self {
            todo_manager,
            counter: Arc::new(std::sync::Mutex::new(0)),
        }
    }

    /// 重置计数器（用于子 agent 隔离边界，防止计数器跨边界泄漏）。
    pub fn reset_counter(&self) {
        *self.counter.lock().unwrap() = 0;
    }
}

#[async_trait]
impl PostToolHook for TodoReminderHook {
    async fn on_post_tool(&self, name: &str, _input: &serde_json::Value, _output: &str) -> Option<String> {
        // 调用 todo_write 后重置计数器
        if name == "todo_write" {
            *self.counter.lock().unwrap() = 0;
            return None;
        }

        // 递增计数器
        let mut counter = self.counter.lock().unwrap();
        *counter += 1;

        // 每 3 轮提醒一次
        if *counter >= 3 {
            *counter = 0;
            Some(format!("<reminder>Update your todos.</reminder>"))
        } else {
            None
        }
    }
}

/// UserPromptSubmit: 记录当前工作目录。
pub struct ContextInjectHook;

#[async_trait]
impl PromptHook for ContextInjectHook {
    async fn on_prompt(&self, _query: &str) {
        tracing::info!("[HOOK] UserPromptSubmit: working in {}", workdir().display());
    }
}

/// PostToolUse: 输出过大时提醒。
pub struct LargeOutputHook;

#[async_trait]
impl PostToolHook for LargeOutputHook {
    async fn on_post_tool(&self, name: &str, _input: &serde_json::Value, output: &str) -> Option<String> {
        if output.len() > 100_000 {
            tracing::warn!("[HOOK] Large output from {}: {} chars", name, output.len());
        }
        None
    }
}

/// Stop: 收尾统计本轮用过的工具次数。
pub struct SummaryHook;

#[async_trait]
impl StopHook for SummaryHook {
    async fn on_stop(&self, messages: &[Message]) -> Option<String> {
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

/// 经 InputTask 询问用户批准。无 ask 通道（非交互 agent）→ 返回 false。
async fn ask_via_input(ctx: &HookContext, name: &str, input: &serde_json::Value, reason: &str) -> bool {
    ctx.input.ask_permission(reason, name, input).await
}

/// PreToolUse 钩子: 三道闸门串联, 返回 Some(reason) 表示拦截, None 表示放行。
///
/// 循环经 `hooks.trigger_pre_tool()` 调用; 末尾返回 None(而非 false),
/// 才符合 "三道都没命中 -> 放行" 的语义 —— 这也修掉了 s03 check_permission
/// 末尾 `return false` 把所有工具都拒掉的 bug。
pub struct PermissionHook;

#[async_trait]
impl PreToolHook for PermissionHook {
    async fn on_pre_tool(
        &self,
        registry: &ToolRegistry,
        ctx: &HookContext,
        name: &str,
        input: &serde_json::Value,
    ) -> Option<String> {
        // 闸门 1: 硬拒绝（使用正则模式匹配）
        if name == "command" {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(reason) = check_deny_patterns(cmd) {
                ctx.output.blocked(reason);
                return Some(format!("Permission denied: {}", reason));
            }
        }

        // 闸门 2: 检查是否需要用户批准
        if name == "command" {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(reason) = requires_approval(cmd) {
                // 闸门 3: 向用户请求确认
                if !ask_via_input(ctx, name, input, reason).await {
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
                    if !ask_via_input(ctx, name, input, reason).await {
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
    use crate::hooks::{HookContext, PostToolHook, PreToolHook};
    use crate::todo::TodoManager;
    use crate::tools::registry::ToolRegistry;

    #[tokio::test]
    async fn todo_reminder_counter_every_three_turns() {
        let todo_manager = Arc::new(SharedTodoManager::new(TodoManager::new()));
        let hook = TodoReminderHook::new(Arc::clone(&todo_manager));
        let input = serde_json::json!({});

        // 前 2 次调用不应该提醒
        assert!(hook.on_post_tool("command", &input, "").await.is_none());
        assert!(hook.on_post_tool("read_file", &input, "").await.is_none());

        // 第 3 次调用应该提醒
        let reminder = hook.on_post_tool("write_file", &input, "").await;
        assert!(reminder.is_some());
        assert!(reminder.unwrap().contains("Update your todos"));

        // 重置后，又需要 3 次才能触发
        assert!(hook.on_post_tool("command", &input, "").await.is_none());
        assert!(hook.on_post_tool("read_file", &input, "").await.is_none());
        assert!(hook.on_post_tool("write_file", &input, "").await.is_some());
    }

    #[tokio::test]
    async fn todo_reminder_resets_on_todo_write() {
        let todo_manager = Arc::new(SharedTodoManager::new(TodoManager::new()));
        let hook = TodoReminderHook::new(Arc::clone(&todo_manager));
        let input = serde_json::json!({});

        // 调用 2 次普通工具
        assert!(hook.on_post_tool("command", &input, "").await.is_none());
        assert!(hook.on_post_tool("read_file", &input, "").await.is_none());

        // 调用 todo_write 应该重置计数器
        assert!(hook.on_post_tool("todo_write", &input, "").await.is_none());

        // 又需要 3 次普通工具调用才能触发
        assert!(hook.on_post_tool("command", &input, "").await.is_none());
        assert!(hook.on_post_tool("read_file", &input, "").await.is_none());
        assert!(hook.on_post_tool("write_file", &input, "").await.is_some());
    }

    #[tokio::test]
    async fn permission_hook_gate1_denies_destructive() {
        let hook = PermissionHook;
        let ctx = HookContext::test_noop();
        let registry = ToolRegistry::new();
        let result = hook
            .on_pre_tool(&registry, &ctx, "command", &serde_json::json!({"command": "rm -rf /"}))
            .await;
        assert!(
            matches!(result, Some(ref r) if r.contains("Permission denied")),
            "destructive command must be denied at gate 1, got {:?}", result
        );
    }

    #[tokio::test]
    async fn permission_hook_non_interactive_denies_approval_gated() {
        // ctx.ask = None (no InputTask wired yet) → approval-gated command is denied, never hangs.
        let hook = PermissionHook;
        let ctx = HookContext::test_noop();
        let registry = ToolRegistry::new();
        let result = hook
            .on_pre_tool(&registry, &ctx, "command", &serde_json::json!({"command": "rm foo"}))
            .await;
        assert_eq!(
            result,
            Some("Permission denied by user".to_string()),
            "non-interactive approval must deny, got {:?}", result
        );
    }
}


