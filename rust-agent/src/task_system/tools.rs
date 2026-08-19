/*
tools.rs - Tool implementations for s10 Task System

Implements create_task, list_tasks, get_task, claim_task, complete_task tools.
The global TaskStore is held in an Arc behind a OnceLock for thread-safe
shared state, initialized once at startup via init_task_store().
*/

use crate::task_system::store::{TaskStore, TaskStoreError};
use crate::task_system::task::{Task, TaskStatus};
use crate::tools::trait_def::{PermissionCheck, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

/// 全局任务存储（Arc 共享，OnceLock 保证只初始化一次）
static TASK_STORE: std::sync::OnceLock<Arc<TaskStore>> = std::sync::OnceLock::new();

/// 初始化全局任务存储。
///
/// 在工作目录下建立 `.tasks/` 目录。使用 OnceLock，因此多次调用幂等：
/// 仅首次调用真正构造 TaskStore，后续调用直接返回已有实例。
pub fn init_task_store() -> Result<(), TaskStoreError> {
    let workdir = std::env::current_dir()
        .map_err(|_| TaskStoreError::EscapesWorkspace)?;
    let tasks_dir = workdir.join(".tasks");
    let store = TaskStore::new(tasks_dir)?;
    TASK_STORE.get_or_init(|| Arc::new(store));
    Ok(())
}

/// 获取全局任务存储的句柄。
///
/// 调用前必须先调用 `init_task_store`，否则 panic。
fn get_store() -> Arc<TaskStore> {
    TASK_STORE
        .get()
        .expect("TaskStore not initialized. Call init_task_store() first.")
        .clone()
}

/// 把 TaskStoreError 转成工具输出字符串。
fn error_to_output(e: TaskStoreError) -> String {
    format!("Error: {}", e)
}

pub struct CreateTaskTool;

#[async_trait]
impl Tool for CreateTaskTool {
    fn name(&self) -> &str {
        "create_task"
    }

    fn description(&self) -> &str {
        "Create a task with optional dependencies. The task is stored in .tasks/{id}.json"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "subject": {
                    "type": "string",
                    "description": "Brief title for the task"
                },
                "description": {
                    "type": "string",
                    "description": "Detailed description of the task"
                },
                "blockedBy": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "List of task IDs this task depends on"
                }
            },
            "required": ["subject"]
        })
    }

    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        let subject = input
            .get("subject")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let description = input
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let blocked_by = input
            .get("blockedBy")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let store = get_store();
        match store.create(subject.to_string(), description.to_string(), blocked_by) {
            Ok(task) => {
                let deps_str = if task.blocked_by.is_empty() {
                    String::new()
                } else {
                    format!(" (blockedBy: {})", task.blocked_by.join(", "))
                };
                format!("Created {}: {}{}", task.id, task.subject, deps_str)
            }
            Err(e) => error_to_output(e),
        }
    }
}

pub struct ListTasksTool;

#[async_trait]
impl Tool for ListTasksTool {
    fn name(&self) -> &str {
        "list_tasks"
    }

    fn description(&self) -> &str {
        "List all tasks with their status, owner, and dependencies"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {}
        })
    }

    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, _input: &Value) -> String {
        let store = get_store();
        match store.list() {
            Ok(tasks) => {
                if tasks.is_empty() {
                    return "No tasks. Use create_task to add some.".to_string();
                }

                let mut lines = Vec::new();
                for task in tasks {
                    let marker = match task.status {
                        TaskStatus::Pending => "[ ]",
                        TaskStatus::InProgress => "[>]",
                        TaskStatus::Completed => "[x]",
                    };
                    let deps_str = if task.blocked_by.is_empty() {
                        String::new()
                    } else {
                        format!(" (blockedBy: {})", task.blocked_by.join(", "))
                    };
                    let owner_str = task
                        .owner
                        .as_ref()
                        .map_or(String::new(), |o| format!(" [{}]", o));

                    lines.push(format!(
                        "{} {}: {} [{}]{}{}",
                        marker,
                        task.id,
                        task.subject,
                        serde_json::to_string(&task.status).unwrap(),
                        owner_str,
                        deps_str
                    ));
                }
                lines.join("\n")
            }
            Err(e) => error_to_output(e),
        }
    }
}

#[cfg(test)]
mod tool_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_create_tool_name_and_description() {
        let tool = CreateTaskTool;
        assert_eq!(tool.name(), "create_task");
        assert!(tool.description().contains("Create a task"));
    }

    #[test]
    fn test_create_tool_schema() {
        let tool = CreateTaskTool;
        let schema = tool.input_schema();

        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["subject"]["type"], "string");
        assert_eq!(schema["properties"]["description"]["type"], "string");
        assert_eq!(schema["properties"]["blockedBy"]["type"], "array");

        let required = schema["required"].as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "subject");
    }

    #[test]
    fn test_create_tool_passes_permission() {
        let tool = CreateTaskTool;
        let input = json!({"subject": "test"});
        let check = tool.check_permission(&input);
        assert!(matches!(check, PermissionCheck::Pass));
    }

    #[test]
    fn test_list_tool_name_and_schema() {
        let tool = ListTasksTool;
        assert_eq!(tool.name(), "list_tasks");
        assert_eq!(tool.input_schema()["type"], "object");
    }

    #[test]
    fn test_list_tool_passes_permission() {
        let tool = ListTasksTool;
        let check = tool.check_permission(&json!({}));
        assert!(matches!(check, PermissionCheck::Pass));
    }
}
