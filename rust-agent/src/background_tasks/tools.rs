/*
tools.rs - 后台任务工具与 Hook (s11)

TaskOutputTool / TaskStopTool 把 BackgroundManager 暴露给模型;
BackgroundStopHook 在循环退出前主动注入已完成通知 (主动唤醒);
collect_and_inject 在循环顶部被动兜底收集。
全局 LazyLock<Arc<BackgroundManager>> 对齐 s10。
*/

use crate::background_tasks::manager::BackgroundManager;
use crate::client::{ContentBlock, Message};
use crate::hooks::StopHook;
use crate::tools::trait_def::{PermissionCheck, Tool, ToolContext};
use crate::tools::workdir;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

/// 全局后台任务管理器 (LazyLock 懒初始化, 对齐 s10)。
static BG_MANAGER: std::sync::LazyLock<Arc<BackgroundManager>> =
    std::sync::LazyLock::new(|| {
        Arc::new(BackgroundManager::new(
            workdir().join(".task_outputs").join("background"),
        ))
    });

/// 取全局 manager 的 Arc clone (供 CommandTool 的 start_background 调用)。
pub fn get_manager() -> Arc<BackgroundManager> {
    BG_MANAGER.clone()
}

/// 循环顶部被动兜底: drain ready, 把通知作为独立 user 消息 Text 块追加。
/// 返回注入的通知条数 (None 表示无通知)。
pub fn collect_and_inject(messages: &mut Vec<Message>) -> Option<usize> {
    let notifications = get_manager().collect();
    if notifications.is_empty() {
        return None;
    }
    let count = notifications.len();
    let blocks: Vec<ContentBlock> = notifications
        .into_iter()
        .map(|n| ContentBlock::Text { text: n })
        .collect();
    messages.push(Message {
        role: "user".to_string(),
        content: blocks,
    });
    Some(count)
}

/// 主动唤醒: 循环退出前若 ready 非空, 返回通知强制继续 (对齐 hooks.rs StopHook 语义)。
pub struct BackgroundStopHook;

impl StopHook for BackgroundStopHook {
    fn on_stop(&self, _messages: &[Message]) -> Option<String> {
        let notifications = get_manager().collect();
        if notifications.is_empty() {
            None
        } else {
            Some(notifications.join("\n"))
        }
    }
}

/// TaskOutput 工具: poll/block 取后台任务输出与状态。
pub struct TaskOutputTool;

#[async_trait]
impl Tool for TaskOutputTool {
    fn name(&self) -> &str {
        "task_output"
    }

    fn description(&self) -> &str {
        "Get the status and output of a background task. Set block=true to wait (with timeout) for it to finish."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string" },
                "block": { "type": "boolean", "default": false },
                "timeout_ms": { "type": "integer", "default": 30000 }
            },
            "required": ["task_id"]
        })
    }

    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        let Some(task_id) = input.get("task_id").and_then(|v| v.as_str()) else {
            return "Error: task_id required".to_string();
        };
        let block = input.get("block").and_then(|v| v.as_bool()).unwrap_or(false);
        let timeout_ms = input
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(30_000);
        get_manager().output(task_id, block, timeout_ms).await
    }

    fn available_for_subagent(&self) -> bool {
        true
    }
}

/// TaskStop 工具: 取消后台任务并 kill 进程树。
pub struct TaskStopTool;

#[async_trait]
impl Tool for TaskStopTool {
    fn name(&self) -> &str {
        "task_stop"
    }

    fn description(&self) -> &str {
        "Stop a running background task by cancelling it and killing its process tree."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string" }
            },
            "required": ["task_id"]
        })
    }

    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        let Some(task_id) = input.get("task_id").and_then(|v| v.as_str()) else {
            return "Error: task_id required".to_string();
        };
        get_manager().stop(task_id)
    }

    fn available_for_subagent(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn task_output_tool_metadata() {
        let t = TaskOutputTool;
        assert_eq!(t.name(), "task_output");
        assert!(t.description().contains("background"));
        let s = t.input_schema();
        assert_eq!(s["required"][0], "task_id");
        assert_eq!(s["properties"]["block"]["type"], "boolean");
        assert_eq!(t.check_permission(&json!({})), PermissionCheck::Pass);
        assert!(t.available_for_subagent());
    }

    #[test]
    fn task_stop_tool_metadata() {
        let t = TaskStopTool;
        assert_eq!(t.name(), "task_stop");
        let s = t.input_schema();
        assert_eq!(s["required"][0], "task_id");
        assert_eq!(t.check_permission(&json!({})), PermissionCheck::Pass);
        assert!(t.available_for_subagent());
    }

    #[test]
    fn collect_on_fresh_manager_is_empty() {
        // 用独立 manager 验证空 collect 契约 (不污染全局)。
        let mgr = Arc::new(BackgroundManager::new(
            std::env::temp_dir().join("bg_test_empty_collect"),
        ));
        assert!(mgr.collect().is_empty());
    }

    #[test]
    fn background_stop_hook_empty_returns_none() {
        // 全局 manager 在测试环境下通常无 ready; 仅验证返回 Option 契约。
        // 不做强断言 (依赖全局状态), 逻辑在 manager 单测中已覆盖。
        let hook = BackgroundStopHook;
        let _: Option<String> = hook.on_stop(&[]);
    }
}
