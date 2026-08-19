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

/// worker: 执行命令, 落盘输出, finalize。
///
/// tokio::time::timeout 包裹 child.wait() (借用 &mut child, 非 move): 超时 → kill_tree 杀进程树
/// + Failed/None + 写 "Error: Timeout (Ns)"。wait_with_output 会 move child, 无法在超时分支
/// 复用 child 调 kill_tree, 故改用 wait() + 独立 drain_pipe 读管道。(取消臂在 Task 5 加。)
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
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let text = format!("Error: spawn failed: {}", e);
            let _ = std::fs::write(&output_file, &text);
            finalize(&mgr, &id, TaskStatus::Failed, None);
            return;
        }
    };
    let timeout_secs = mgr.timeout_secs;
    // 先 take 管道: drain future 拥有所有权, 不与 child.wait() 的 &mut child 借用冲突。
    // 用 tokio::join! 让管道与 wait 并发推进, 避免子进程输出超过管道缓冲 (Windows 低至 4KB)
    // 时阻塞在 write()、wait 永不返回的死锁。timeout 的 inner future 借 &mut child;
    // Elapsed 时被 drop, 借用释放, Err 分支可再拿 &mut child 调 kill_tree。
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let (status, exit_code, text) = match tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        async {
            let (exit_status, stdout, stderr) = tokio::join!(
                child.wait(),
                drain_pipe(stdout_pipe),
                drain_pipe(stderr_pipe),
            );
            (exit_status, stdout, stderr)
        },
    )
    .await
    {
        Ok((Ok(exit_status), stdout, stderr)) => {
            let code = exit_status.code();
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
        Ok((Err(e), _, _)) => (
            TaskStatus::Failed,
            None,
            format!("Error: wait failed: {}", e),
        ),
        Err(_elapsed) => {
            kill_tree(&mut child).await;
            let _ = child.wait().await;
            (
                TaskStatus::Failed,
                None,
                format!("Error: Timeout ({}s)", timeout_secs),
            )
        }
    };
    let _ = std::fs::write(&output_file, &text);
    finalize(&mgr, &id, status, exit_code);
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
/// 最后兜底 child.kill() (直接子进程)。
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
}
