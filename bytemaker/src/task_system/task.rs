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
    /// Optional task-bound worktree name (s13). Old JSON without it deserializes to None.
    #[serde(default)]
    pub worktree: Option<String>,
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

impl TaskStatus {
    /// 裸单词形式（`in_progress`），便于拼进面向用户的消息。
    pub fn as_word(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
}