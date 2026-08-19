/*
task.rs - 后台任务数据结构 (s11)

纯数据结构, 零业务依赖。BackgroundTask + TaskStatus。
后台命令在 worker 线程执行, 状态机: running -> completed | failed | cancelled。
*/

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 后台任务状态
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// worker 执行中
    Running,
    /// exit_code == 0
    Completed,
    /// exit_code != 0 或超时或异常
    Failed,
    /// 被 TaskStop 取消
    Cancelled,
}

/// 后台任务记录
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackgroundTask {
    /// 任务 ID, 格式 bg_[0-9a-f]{8}
    pub id: String,
    /// bash 命令串
    pub command: String,
    /// 当前状态
    pub status: TaskStatus,
    /// 原始 tool_use id, 仅做关联; 通知不复用它
    pub tool_use_id: String,
    /// 启动时间戳 (Unix 秒)
    pub started_at: u64,
    /// 输出落盘文件路径 (TaskOutput 读这里, 不占内存)
    pub output_file: PathBuf,
    /// 完成后填; Running 时 None
    pub exit_code: Option<i32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_status_serializes_snake_case() {
        for (status, expected) in [
            (TaskStatus::Running, "\"running\""),
            (TaskStatus::Completed, "\"completed\""),
            (TaskStatus::Failed, "\"failed\""),
            (TaskStatus::Cancelled, "\"cancelled\""),
        ] {
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, expected);
            let back: TaskStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, status);
        }
    }

    #[test]
    fn background_task_serializes_roundtrip() {
        let task = BackgroundTask {
            id: "bg_a1b2c3d4".to_string(),
            command: "npm install".to_string(),
            status: TaskStatus::Completed,
            tool_use_id: "toolu_01".to_string(),
            started_at: 1700000000,
            output_file: PathBuf::from(".task_outputs/background/bg_a1b2c3d4.log"),
            exit_code: Some(0),
        };
        let json = serde_json::to_string(&task).unwrap();
        assert!(json.contains("\"status\":\"completed\""));
        assert!(json.contains("\"exit_code\":0"));
        let back: BackgroundTask = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "bg_a1b2c3d4");
        assert_eq!(back.status, TaskStatus::Completed);
        assert_eq!(back.exit_code, Some(0));
    }
}
