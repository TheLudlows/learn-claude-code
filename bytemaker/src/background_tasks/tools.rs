/*
tools.rs - 后台任务工具与 Hook (s11)

TaskOutputTool / TaskStopTool 把 BackgroundManager 暴露给模型;
BackgroundStopHook 在循环退出前主动注入已完成通知 (主动唤醒);
collect_and_inject 在循环顶部被动兜底收集。
BackgroundManager 由 Agent 持有（Arc），经 ToolContext.agent.bg_manager 下传；
BackgroundStopHook 经构造器 DI 拿到 manager，不再用进程级全局。
*/

use crate::background_tasks::manager::BackgroundManager;
use crate::hooks::StopHook;
use crate::tools::trait_def::{PermissionCheck, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

/// 主动唤醒：循环退出前若 ready 非空，返回通知强制继续（对齐 hooks.rs StopHook 语义）。
/// 经构造器 DI 持有 bg_manager（原读全局 get_manager）—— Agent::build_hooks 装配时传入。
pub struct BackgroundStopHook {
    bg_manager: Arc<BackgroundManager>,
}

impl BackgroundStopHook {
    pub fn new(bg_manager: Arc<BackgroundManager>) -> Self {
        Self { bg_manager }
    }
}

impl StopHook for BackgroundStopHook {
    fn on_stop(&self, _messages: &[crate::client::Message]) -> Option<String> {
        let notifications = self.bg_manager.collect();
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

    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String {
        let Some(task_id) = input.get("task_id").and_then(|v| v.as_str()) else {
            return "Error: task_id required".to_string();
        };
        let block = input.get("block").and_then(|v| v.as_bool()).unwrap_or(false);
        let timeout_ms = input
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(30_000);
        ctx.agent.bg_manager.output(task_id, block, timeout_ms).await
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

    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String {
        let Some(task_id) = input.get("task_id").and_then(|v| v.as_str()) else {
            return "Error: task_id required".to_string();
        };
        ctx.agent.bg_manager.stop(task_id)
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
}
