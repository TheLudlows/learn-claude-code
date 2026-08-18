/*
task.rs - Task data structure for s10 Task System

Defines Task struct and TaskStatus enum with serialization support.
*/

use serde::{Deserialize, Serialize};

/// 任务状态
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

/// 任务数据结构
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    /// 任务 ID，格式：task_xxxxxxxx（8位十六进制）
    pub id: String,
    /// 任务简短标题
    pub subject: String,
    /// 任务详细描述
    pub description: String,
    /// 当前状态
    pub status: TaskStatus,
    /// 负责该任务的 agent
    pub owner: Option<String>,
    /// 前置任务 ID 列表
    pub blocked_by: Vec<String>,
}

impl Task {
    /// 检查任务是否可以被认领
    pub fn can_claim(&self, incomplete_deps: &[String]) -> bool {
        self.status == TaskStatus::Pending && incomplete_deps.is_empty()
    }

    /// 检查任务是否可以被完成
    pub fn can_complete(&self, owner: &str) -> bool {
        self.status == TaskStatus::InProgress
            && self.owner.as_deref() == Some(owner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_status_serialization() {
        let status = TaskStatus::Pending;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, "\"pending\"");

        let deserialized: TaskStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, TaskStatus::Pending);
    }

    #[test]
    fn test_task_serialization() {
        let task = Task {
            id: "task_12345678".to_string(),
            subject: "Test task".to_string(),
            description: "A test task".to_string(),
            status: TaskStatus::Pending,
            owner: None,
            blocked_by: vec!["task_87654321".to_string()],
        };

        let json = serde_json::to_string(&task).unwrap();
        assert!(json.contains("\"id\":\"task_12345678\""));
        assert!(json.contains("\"status\":\"pending\""));
        assert!(json.contains("\"blocked_by\""));
    }

    #[test]
    fn test_can_claim() {
        let task = Task {
            id: "task_12345678".to_string(),
            subject: "Test".to_string(),
            description: "".to_string(),
            status: TaskStatus::Pending,
            owner: None,
            blocked_by: vec![],
        };
        assert!(task.can_claim(&[]));

        let task_with_deps = Task {
            id: "task_12345678".to_string(),
            subject: "Test".to_string(),
            description: "".to_string(),
            status: TaskStatus::Pending,
            owner: None,
            blocked_by: vec!["task_other".to_string()],
        };
        assert!(!task_with_deps.can_claim(&["task_other".to_string()]));
    }

    #[test]
    fn test_can_complete() {
        let task = Task {
            id: "task_12345678".to_string(),
            subject: "Test".to_string(),
            description: "".to_string(),
            status: TaskStatus::InProgress,
            owner: Some("agent".to_string()),
            blocked_by: vec![],
        };
        assert!(task.can_complete("agent"));
        assert!(!task.can_complete("other"));
    }
}