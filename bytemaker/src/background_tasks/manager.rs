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
    timeout_secs: u64,
}

impl BackgroundManager {
    const MAX_ID_RETRIES: usize = 100;

    /// 创建管理器。纯内存构造, 不 fallible (与 s10 TaskStore::new 不同)。
    pub fn new(output_dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&output_dir);
        Self {
            output_dir,
            state: Arc::new(Mutex::new(State::new())),
            timeout_secs: BG_TIMEOUT_SECS,
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
            if state.running_count() >= MAX_CONCURRENT {
                return Err(format!(
                    "Error: too many concurrent background tasks ({}). Wait for some to finish via TaskOutput.",
                    MAX_CONCURRENT
                ));
            }
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

    /// 循环顶部被动兜底：drain ready，把通知作为独立 user 消息 Text 块追加（原 free `collect_and_inject`）。
    pub fn collect_and_inject(&self, messages: &mut Vec<crate::client::Message>) -> Option<usize> {
        let notifications = self.collect();
        if notifications.is_empty() {
            return None;
        }
        let count = notifications.len();
        let blocks: Vec<crate::client::ContentBlock> = notifications
            .into_iter()
            .map(|n| crate::client::ContentBlock::Text { text: n })
            .collect();
        messages.push(crate::client::Message::user_blocks(blocks));
        Some(count)
    }

    /// 取后台任务输出与状态。
    ///
    /// - block=false: 立即返回状态 + output_file 当前内容 (截断 MAX_OUTPUT_BYTES)。
    /// - block=true: 轮询至任务非 Running 或 timeout_ms 到; 超时不取消 task。
    pub async fn output(&self, task_id: &str, block: bool, timeout_ms: u64) -> String {
        if block {
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
            loop {
                let done = {
                    let state = self.state.lock().expect("state mutex poisoned");
                    state
                        .tasks
                        .get(task_id)
                        .map(|t| t.status != TaskStatus::Running)
                        .unwrap_or(true) // 不存在 -> 视为 done (下面返回 not found)
                };
                if done || std::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
        let (status, output_file, exit_code, command) = {
            let state = self.state.lock().expect("state mutex poisoned");
            match state.tasks.get(task_id) {
                Some(t) => (
                    t.status,
                    t.output_file.clone(),
                    t.exit_code,
                    t.command.clone(),
                ),
                None => return format!("Error: task {} not found", task_id),
            }
        };
        let body = std::fs::read_to_string(&output_file)
            .unwrap_or_default()
            .trim()
            .to_string();
        let body = truncate_chars(&body, MAX_OUTPUT_BYTES);
        format!(
            "task_id: {}\nstatus: {}\ncommand: {}\nexit_code: {}\noutput:\n{}",
            task_id,
            status_word(status),
            command,
            exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "none".to_string()),
            body
        )
    }

    /// 取消后台任务: 触发 cancel Notify -> worker 走取消分支 -> kill 进程树 -> Cancelled -> 入 ready。
    /// 已完成/不存在的任务返回提示, no-op。
    pub fn stop(&self, task_id: &str) -> String {
        let cancel_opt = {
            let state = self.state.lock().expect("state mutex poisoned");
            match state.tasks.get(task_id).map(|t| t.status) {
                None => return format!("Error: task {} not found", task_id),
                Some(TaskStatus::Running) => state.cancels.get(task_id).cloned(),
                Some(other) => return format!("Task {} already {}", task_id, status_word(other)),
            }
        };
        match cancel_opt {
            Some(cancel) => {
                cancel.notify_one();
                format!("[Stopped {}]", task_id)
            }
            None => format!("Error: task {} not found", task_id),
        }
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

/// worker: 把执行体 spawn 为内部 task, 外层 await JoinHandle 守卫 panic。
///
/// 正常: 内部 task 执行 select! + 落盘 + finalize。
/// panic: JoinHandle.await 返回 Err -> 兜底 finalize(Failed), task 不卡 Running。
async fn run_worker(
    mgr: BackgroundManager,
    id: String,
    command: String,
    output_file: PathBuf,
    cancel: Arc<Notify>,
) {
    let output_file_for_body = output_file.clone();
    let mgr_panic = mgr.clone();
    let id_panic = id.clone();
    let handle = tokio::spawn(async move {
        let mut cmd = build_command(&command);
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let text = format!("Error: spawn failed: {}", e);
                let _ = std::fs::write(&output_file_for_body, &text);
                finalize(&mgr, &id, TaskStatus::Failed, None);
                return;
            }
        };
        let timeout_secs = mgr.timeout_secs;
        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();
        let (status, exit_code, text) = tokio::select! {
            biased;
            _ = cancel.notified() => {
                kill_tree(&mut child).await;
                let _ = child.wait().await;
                (TaskStatus::Cancelled, None, "Cancelled by TaskStop".to_string())
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)) => {
                kill_tree(&mut child).await;
                let _ = child.wait().await;
                (TaskStatus::Failed, None, format!("Error: Timeout ({}s)", timeout_secs))
            }
            (exit_status, stdout, stderr) = async {
                tokio::join!(
                    child.wait(),
                    drain_pipe(stdout_pipe),
                    drain_pipe(stderr_pipe),
                )
            } => match exit_status {
                Ok(es) => {
                    let code = es.code();
                    let status = if code == Some(0) {
                        TaskStatus::Completed
                    } else {
                        TaskStatus::Failed
                    };
                    let body = format!("{}\n{}", stdout, stderr).trim().to_string();
                    let body = if body.is_empty() {
                        "(no output)".to_string()
                    } else {
                        truncate_chars(&body, MAX_OUTPUT_BYTES)
                    };
                    (status, code, body)
                }
                Err(e) => (
                    TaskStatus::Failed,
                    None,
                    format!("Error: wait failed: {}", e),
                ),
            }
        };
        let _ = std::fs::write(&output_file_for_body, &text);
        finalize(&mgr, &id, status, exit_code);
    });
    // panic 兜底: 内部 task 崩了, 仍把 task 置 Failed + 入 ready, 不卡 Running。
    if let Err(_join_err) = handle.await {
        let _ = std::fs::write(&output_file, "Error: worker panicked");
        finalize(&mgr_panic, &id_panic, TaskStatus::Failed, None);
    }
}

/// 读取子进程管道 (stdout/stderr) 全部内容并解码。wait 完成后调用:
/// 子进程已退出, 管道写端关闭, read_to_end 返回 EOF。
async fn drain_pipe<R: tokio::io::AsyncRead + Unpin>(pipe: Option<R>) -> String {
    use tokio::io::AsyncReadExt;
    match pipe {
        Some(mut p) => {
            let mut buf = Vec::new();
            let _ = p.read_to_end(&mut buf).await;
            crate::tools::command::decode_console(&buf)
        }
        None => String::new(),
    }
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

/// 构造跨平台命令。设置新进程组/CREATE_NEW_PROCESS_GROUP, 供 kill_tree 杀整棵进程树。
fn build_command(command: &str) -> tokio::process::Command {
    let mut c = if cfg!(windows) {
        let mut c = tokio::process::Command::new("cmd.exe");
        c.args(["/C", command]);
        c
    } else {
        let mut c = tokio::process::Command::new("bash");
        c.arg("-c").arg(command);
        c
    };
    c.current_dir(crate::tools::workdir());

    // 新进程组: Unix process_group(0) 让 PGID = child PID; Windows CREATE_NEW_PROCESS_GROUP。
    // kill_tree 据此杀整组/整棵树。tokio::process::Command 自带这两个 inherent 方法。
    #[cfg(unix)]
    {
        c.process_group(0);
    }
    #[cfg(windows)]
    {
        // CREATE_NEW_PROCESS_GROUP = 0x00000200
        c.creation_flags(0x00000200);
    }
    c
}

/// 杀整棵进程树 (零依赖, shell-out)。
///
/// - Unix: `kill -KILL -{pgid}` (负 PID = 进程组; child 经 process_group(0) 自成一组)。
/// - Windows: `taskkill /T /F /PID {pid}` (`/T` 终止整棵子树)。
///   最后兜底 child.kill() (直接子进程)。
async fn kill_tree(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        let mut kill_cmd = if cfg!(windows) {
            let mut c = tokio::process::Command::new("taskkill");
            c.args(["/T", "/F", "/PID", &pid.to_string()]);
            c
        } else {
            let mut c = tokio::process::Command::new("kill");
            c.args(["-KILL", &format!("-{}", pid)]);
            c
        };
        kill_cmd.stdout(std::process::Stdio::null());
        kill_cmd.stderr(std::process::Stdio::null());
        let _ = kill_cmd.output().await;
    }
    let _ = child.kill().await;
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
pub(crate) fn create_test_manager_with_timeout(
    output_dir: &std::path::Path,
    timeout_secs: u64,
) -> BackgroundManager {
    BackgroundManager {
        output_dir: output_dir.to_path_buf(),
        state: Arc::new(Mutex::new(State::new())),
        timeout_secs,
    }
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

    #[tokio::test]
    async fn timeout_yields_failed_with_consistent_text() {
        // 回归 s11 bug: 超时 exit_code=None, 状态必须 Failed 且文本写明 Timeout。
        let dir = tempdir().unwrap();
        let mgr = create_test_manager_with_timeout(dir.path(), 1);
        let cmd = if cfg!(windows) { "ping -n 120 127.0.0.1" } else { "sleep 120" };
        let id = mgr.start(cmd, "toolu_3").unwrap();
        let mut got = Vec::new();
        for _ in 0..400 {
            got = mgr.collect();
            if !got.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert_eq!(got.len(), 1, "should have completed via timeout");
        let n = &got[0];
        assert!(n.contains("failed"), "status must be failed: {}", n);
        assert!(n.contains("none"), "exit_code must be none: {}", n);
        let log = std::fs::read_to_string(dir.path().join(format!("{}.log", id)))
            .unwrap_or_default();
        assert!(log.contains("Timeout"), "log must say Timeout: {}", log);
    }

    #[tokio::test]
    async fn stop_cancels_running_task() {
        let dir = tempdir().unwrap();
        let mgr = create_test_manager_with_timeout(dir.path(), 60);
        let cmd = if cfg!(windows) { "ping -n 60 127.0.0.1" } else { "sleep 60" };
        let id = mgr.start(cmd, "toolu_4").unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let msg = mgr.stop(&id);
        assert!(msg.contains(&format!("[Stopped {}]", id)), "stop msg: {}", msg);

        let mut got = Vec::new();
        for _ in 0..200 {
            got = mgr.collect();
            if !got.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(got.len(), 1);
        assert!(got[0].contains("cancelled"), "status must be cancelled: {}", got[0]);
    }

    #[tokio::test]
    async fn stop_on_finalized_task_returns_already() {
        // stop 一次 → Cancelled (finalize 把状态置 Cancelled, 但 task 仍在 tasks 里直到 collect)。
        // 紧接着再 stop 一次, 命中 "already {status}" 分支 (非 Running)。
        let dir = tempdir().unwrap();
        let mgr = create_test_manager_with_timeout(dir.path(), 60);
        let cmd = if cfg!(windows) { "ping -n 60 127.0.0.1" } else { "sleep 60" };
        let id = mgr.start(cmd, "toolu_5").unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let first = mgr.stop(&id);
        assert!(first.contains("[Stopped"));

        // 等 worker 走完取消臂 + finalize (状态置 Cancelled, 但未 collect 所以仍在 tasks)。
        // 轮询第二次 stop 直到命中 "already" 分支 (finalize 完成, 状态非 Running)。
        // 用轮询而非固定 sleep: 并行测试时 CPU 争用会让 finalize 慢于固定 sleep, 造成 flake。
        // 重复 stop 对 Running 任务只是再 fire notify_one (worker 已离开 select cancel 臂), 无副作用。
        #[allow(unused_assignments)]
        let mut second = String::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            second = mgr.stop(&id);
            if second.contains("already") || std::time::Instant::now() >= deadline {
                break;
            }
        }
        assert!(
            second.contains("already"),
            "second stop on finalized task should say 'already', got: {}",
            second
        );
        // 清理: collect 掉这个 cancelled task, 不影响其它测试
        let _ = mgr.collect();
    }

    #[test]
    fn stop_unknown_task_is_noop_error() {
        let dir = tempdir().unwrap();
        let mgr = create_test_manager(dir.path());
        let msg = mgr.stop("bg_deadbeef");
        assert!(msg.contains("not found"), "unknown stop: {}", msg);
    }
}
