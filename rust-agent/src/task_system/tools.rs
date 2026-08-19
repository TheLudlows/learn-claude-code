/*
tools.rs - Tool implementations for s10 Task System

Implements create_task, list_tasks, get_task, claim_task, complete_task tools.
The global TaskStore is held in an Arc behind a OnceLock for thread-safe
shared state, initialized once at startup via init_task_store().
*/

use crate::task_system::store::{TaskStore, TaskStoreError};
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
