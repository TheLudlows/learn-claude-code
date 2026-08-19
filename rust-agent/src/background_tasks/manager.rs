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

    /// 启动后台任务: 注册 + spawn worker + 立即返回 bg_id。
    /// 同步方法 (内部 tokio::spawn 是同步调用)。调用方把 bg_id 拼进占位 tool_result。
    pub fn start(&self, command: &str, tool_use_id: &str) -> Result<String, String> {
        let command = command.trim();
        if command.is_empty() {
            return Err("Error: empty command".to_string());
        }
        let started_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let cancel = Arc::new(Notify::new());

        // 持锁跨 id 生成 + 插入, 消除 TOCTOU 窗口 (见 T2 review note):
        // generate_id_locked 在已持锁的 state 上生成不碰撞 id, 紧接着 insert,
        // 两次 start 并发时不会抽出同一个未插入的 id 再互相覆盖。
        let (id, output_file) = {
            let mut state = self.state.lock().expect("state mutex poisoned");
            let id = generate_id_locked(&state);
            if id.is_empty() {
                return Err("Error: failed to allocate task id".to_string());
            }
            let output_file = self.output_dir.join(format!("{}.log", id));
            let task = BackgroundTask {
                id: id.clone(),
                command: command.to_string(),
                status: TaskStatus::Running,
                tool_use_id: tool_use_id.to_string(),
                started_at,
                output_file: output_file.clone(),
                exit_code: None,
            };
            state.tasks.insert(id.clone(), task);
            state.cancels.insert(id.clone(), cancel.clone());
            (id, output_file)
        };

        let mgr = self.clone();
        let cmd = command.to_string();
        tokio::spawn(run_worker(mgr, id.clone(), cmd, output_file, cancel));
        Ok(id)
    }

    /// 收集已完成任务, 返回 <task_notification> XML 列表。
    /// 收集后从 tasks 移除 (通知一次即丢弃, 防重复注入)。
    pub fn collect(&self) -> Vec<String> {
        let drained: Vec<String> = {
            let mut state = self.state.lock().expect("state mutex poisoned");
            state.ready.drain(..).collect()
        };
        let mut out = Vec::new();
        for id in drained {
            let task_opt = {
                let mut state = self.state.lock().expect("state mutex poisoned");
                state.tasks.remove(&id)
            };
            if let Some(task) = task_opt {
                out.push(format_notification(&task));
            }
        }
        out
    }
}

/// 在已持锁的 state 上生成不碰撞的 bg_id (重试 100 次)。
/// 调用方必须已持有 state 的锁 —— 这样 generate + insert 可在同一锁持有期内完成,
/// 消除旧 `generate_id(&self)` 自行加锁带来的 TOCTOU 窗口。
fn generate_id_locked(state: &State) -> String {
    for _ in 0..BackgroundManager::MAX_ID_RETRIES {
        let id = format!("bg_{:08x}", fastrand::u32(..));
        if !state.tasks.contains_key(&id) {
            return id;
        }
    }
    String::new() // 极低概率; 调用方按错误处理
}

/// worker: 执行命令, 落盘输出, finalize (更新状态 + 入 ready)。
///
/// 此版本: 直接 await child.output(), 仅区分 completed/failed。
/// 超时/取消/panic 守卫在后续任务 (Task 4-6) 加。`_cancel` 本任务未用 (Task 5 接线)。
#[allow(unused_variables)]
async fn run_worker(
    mgr: BackgroundManager,
    id: String,
    command: String,
    output_file: PathBuf,
    _cancel: Arc<Notify>,
) {
    let mut cmd = build_command(&command);
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let (status, exit_code, text) = match cmd.output().await {
        Ok(output) => {
            let stdout = crate::tools::command::decode_console(&output.stdout);
            let stderr = crate::tools::command::decode_console(&output.stderr);
            let body = format!("{}\n{}", stdout, stderr).trim().to_string();
            let body = if body.is_empty() {
                "(no output)".to_string()
            } else {
                truncate_chars(&body, MAX_OUTPUT_BYTES)
            };
            let code = output.status.code();
            let status = if code == Some(0) {
                TaskStatus::Completed
            } else {
                TaskStatus::Failed
            };
            (status, code, body)
        }
        Err(e) => (
            TaskStatus::Failed,
            None,
            format!("Error: spawn failed: {}", e),
        ),
    };
    let _ = std::fs::write(&output_file, &text);
    finalize(&mgr, &id, status, exit_code);
}

/// 落库: 更新 task 字段并入 ready 队列; 移除 cancel 信号。
fn finalize(mgr: &BackgroundManager, id: &str, status: TaskStatus, exit_code: Option<i32>) {
    let mut state = mgr.state.lock().expect("state mutex poisoned");
    if let Some(task) = state.tasks.get_mut(id) {
        task.status = status;
        task.exit_code = exit_code;
        state.ready.push_back(id.to_string());
    }
    state.cancels.remove(id);
}

/// 构造跨平台命令 (复用 command.rs 的 cmd.exe/bash 分流)。
/// 进程组/creation_flags 在 Task 4 加 (供 kill_tree)。
fn build_command(command: &str) -> tokio::process::Command {
    if cfg!(windows) {
        let mut c = tokio::process::Command::new("cmd.exe");
        c.args(["/C", command]);
        c.current_dir(crate::tools::workdir());
        c
    } else {
        let mut c = tokio::process::Command::new("bash");
        c.arg("-c").arg(command);
        c.current_dir(crate::tools::workdir());
        c
    }
}

/// 按字节上限截断, 落在 UTF-8 字符边界上 (与 command.rs 同逻辑)。
fn truncate_chars(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// 把已完成任务格式化成 <task_notification> XML。
fn format_notification(task: &BackgroundTask) -> String {
    let summary = match std::fs::read_to_string(&task.output_file) {
        Ok(s) => truncate_chars(s.trim(), SUMMARY_CHARS),
        Err(_) => "(output unavailable)".to_string(),
    };
    let exit = task
        .exit_code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "none".to_string());
    format!(
        "<task_notification>\n  <task_id>{}</task_id>\n  <status>{}</status>\n  <command>{}</command>\n  <exit_code>{}</exit_code>\n  <summary>{}</summary>\n</task_notification>",
        task.id,
        status_word(task.status),
        task.command,
        exit,
        summary
    )
}

/// 状态 -> 裸单词 (snake_case), 剥除 JSON 引号。
fn status_word(status: TaskStatus) -> String {
    serde_json::to_string(&status)
        .unwrap_or_default()
        .trim_matches('"')
        .to_string()
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
        // 直接在裸 State 上验证 generate_id_locked: 不再经由 manager 的内部锁,
        // 与 start() 的调用方式一致 (start 持锁后调用 generate_id_locked(&state))。
        let mut state = State::new();
        let mut ids = Vec::new();
        for _ in 0..50 {
            let id = generate_id_locked(&state);
            assert!(id.starts_with("bg_"));
            assert_eq!(id.len(), 11); // "bg_" + 8 hex
            assert!(id[3..].chars().all(|c| c.is_ascii_hexdigit()));
            assert!(!ids.contains(&id), "id collision: {}", id);
            ids.push(id.clone());
            // 占住该 id, 验证下次不碰撞
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

    #[tokio::test]
    async fn start_completes_and_collect_injects_notification() {
        let dir = tempdir().unwrap();
        let mgr = create_test_manager(dir.path());
        let id = mgr.start("echo hello_bg", "toolu_1").expect("start should succeed");
        assert!(id.starts_with("bg_"));

        // 等 worker 完成 (快命令, 轮询直到 collect 有内容)
        let mut got = Vec::new();
        for _ in 0..200 {
            got = mgr.collect();
            if !got.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(got.len(), 1, "expected one notification, got: {:?}", got);
        let n = &got[0];
        assert!(n.contains("<task_notification>"));
        assert!(n.contains(&id));
        assert!(n.contains("completed"));
        assert!(n.contains("hello_bg"));

        // collect 后 task 已从内存移除 (防重复注入)
        let again = mgr.collect();
        assert!(again.is_empty(), "collect should drain, not repeat");
    }

    #[tokio::test]
    async fn failed_command_yields_failed_notification() {
        let dir = tempdir().unwrap();
        let mgr = create_test_manager(dir.path());
        let cmd = if cfg!(windows) { "cmd /C exit 7" } else { "bash -c 'exit 7'" };
        let _id = mgr.start(cmd, "toolu_2").unwrap();
        let mut got = Vec::new();
        for _ in 0..200 {
            got = mgr.collect();
            if !got.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(got.len(), 1);
        assert!(got[0].contains("failed"), "got: {}", got[0]);
        assert!(got[0].contains("7"), "exit code in notification: {}", got[0]);
    }
}
