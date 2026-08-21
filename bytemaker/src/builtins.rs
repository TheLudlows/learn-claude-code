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
use tokio::sync::oneshot;

use crate::client::{ContentBlock, Message};
use crate::hooks::{HookContext, PermissionQuery, PostToolHook, PreToolHook, PromptHook, StopHook};
use crate::todo::SharedTodoManager;
use crate::tools::registry::ToolRegistry;
use crate::tools::trait_def::PermissionCheck;
use crate::tools::workdir;

/// PostToolUse: 每次工具执行后注入当前 todo 列表。
/// 持有 Arc<SharedTodoManager>，经 render() 只读获取当前状态。
pub struct TodoReminderHook {
    todo_manager: Arc<SharedTodoManager>,
}

impl TodoReminderHook {
    pub fn new(todo_manager: Arc<SharedTodoManager>) -> Self {
        Self { todo_manager }
    }
}

#[async_trait]
impl PostToolHook for TodoReminderHook {
    async fn on_post_tool(&self, _name: &str, _input: &serde_json::Value, _output: &str) -> Option<String> {
        let todos = self.todo_manager.render();
        Some(format!("<reminder>\nCurrent todos:\n{}\n</reminder>", todos))
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

/// 经 InputTask 询问用户批准。无 ask 通道（非交互 agent）→ 渲染 blocked 并拒绝。
async fn ask_via_input(ctx: &HookContext, name: &str, input: &serde_json::Value, reason: &str) -> bool {
    let Some(ask) = ctx.ask.as_ref() else {
        ctx.coordinator.lock().unwrap().error(&format!("cannot approve {name}: no interactive input channel"));
        return false;
    };
    let (tx, rx) = oneshot::channel();
    ctx.coordinator.lock().unwrap().permission(reason, name, input);
    let _ = ask.send(PermissionQuery {
        reason: reason.into(),
        name: name.into(),
        input: input.clone(),
        reply: tx,
    }).await;
    rx.await.unwrap_or(false)
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
                ctx.coordinator.lock().unwrap().blocked(reason);
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
    use crate::hooks::{HookContext, PreToolHook};
    use crate::tools::registry::ToolRegistry;

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


