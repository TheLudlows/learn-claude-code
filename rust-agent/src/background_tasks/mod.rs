/*
background_tasks/mod.rs - 后台任务模块 (s11)

慢速 bash 后台执行: 当前工具调用立即返回 bg_id 占位 tool_result,
循环继续; 命令完成后在后续轮次以 <task_notification> 注入会话。
*/

pub mod task;

pub use task::{BackgroundTask, TaskStatus};
