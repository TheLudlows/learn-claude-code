/*
manager.rs - BackgroundManager: 后台任务注册表 + worker 调度 (s11)

进程内注册表, 内存态 (持久化是 s10 职责)。state 用 Arc<Mutex<State>>:
worker (tokio::spawn) 拿 BackgroundManager 的 clone 共享同一份 state。
锁粒度小、持锁期短 (只动 HashMap/队列, 不做 IO)。
*/

use crate::background_tasks::task::{BackgroundTask, TaskStatus};
use fastrand;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

/// 并发后台任务上限。
pub const MAX_CONCURRENT: usize = 8;
/// 后台命令超时 (秒)。
pub const BG_TIMEOUT_SECS: u64 = 120;
/// 输出落盘/读取截断字节数。
pub const MAX_OUTPUT_BYTES: usize = 50_000;
/// 通知 summary 截断字符数。
pub const SUMMARY_CHARS: usize = 500;

struct State {
    /// bg_id -> task
    tasks: HashMap<String, BackgroundTask>,
    /// 已完成待收集的 bg_id (FIFO)
    ready: VecDeque<String>,
    /// bg_id -> 取消信号
    cancels: HashMap<String, Arc<Notify>>,
}

impl State {
    fn new() -> Self {
        Self {
            tasks: HashMap::new(),
            ready: VecDeque::new(),
            cancels: HashMap::new(),
        }
    }

    /// 当前 Running 任务数 (并发闸门用)。
    fn running_count(&self) -> usize {
        self.tasks
            .values()
            .filter(|t| t.status == TaskStatus::Running)
            .count()
    }
}

/// 后台任务管理器。Clone 廉价 (Arc 共享 state), worker 拿 clone。
#[derive(Clone)]
pub struct BackgroundManager {
    output_dir: PathBuf,
    state: Arc<Mutex<State>>,
}

impl BackgroundManager {
    const MAX_ID_RETRIES: usize = 100;

    /// 创建管理器。纯内存构造, 不 fallible (与 s10 TaskStore::new 不同)。
    pub fn new(output_dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&output_dir);
        Self {
            output_dir,
            state: Arc::new(Mutex::new(State::new())),
        }
    }

    /// 生成 bg_id: bg_ + 8 hex, 重试防碰撞 (对齐 s10)。
    fn generate_id(&self) -> String {
        for _ in 0..Self::MAX_ID_RETRIES {
            let id = format!("bg_{:08x}", fastrand::u32(..));
            let state = self.state.lock().unwrap();
            if !state.tasks.contains_key(&id) {
                return id;
            }
        }
        String::new() // 极低概率; 调用方按错误处理
    }
}

#[cfg(test)]
pub(crate) fn create_test_manager(output_dir: &std::path::Path) -> BackgroundManager {
    BackgroundManager::new(output_dir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn new_creates_output_dir() {
        let dir = tempdir().unwrap();
        let out = dir.path().join("background");
        let _mgr = create_test_manager(&out);
        assert!(out.exists(), "output dir should be created");
    }

    #[test]
    fn generate_id_format_and_unique() {
        let dir = tempdir().unwrap();
        let mgr = create_test_manager(dir.path());
        let mut ids = Vec::new();
        for _ in 0..50 {
            let id = mgr.generate_id();
            assert!(id.starts_with("bg_"));
            assert_eq!(id.len(), 11); // "bg_" + 8 hex
            assert!(id[3..].chars().all(|c| c.is_ascii_hexdigit()));
            assert!(!ids.contains(&id), "id collision: {}", id);
            ids.push(id.clone());
            // 占住该 id, 验证下次不碰撞
            let mut state = mgr.state.lock().unwrap();
            state.tasks.insert(
                id,
                BackgroundTask {
                    id: String::new(),
                    command: "echo".to_string(),
                    status: TaskStatus::Running,
                    tool_use_id: "t".to_string(),
                    started_at: 0,
                    output_file: PathBuf::from("/tmp/x"),
                    exit_code: None,
                },
            );
        }
    }
}
