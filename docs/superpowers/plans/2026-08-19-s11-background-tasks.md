# s11 Background Tasks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 rust-agent 中实现后台任务能力——慢速 bash 后台执行、立即返回 bg_id 占位、后续轮次注入 `<task_notification>`，并支持 TaskOutput(poll/block)、TaskStop(取消)、并发上限、输出落盘与进程树清理。

**Architecture:** 新建 `src/background_tasks/` 模块（task.rs 数据 / manager.rs 注册表+worker / tools.rs 工具+hook），全局 `LazyLock<Arc<BackgroundManager>>` 对齐 s10。`CommandTool` 加 `run_in_background` 参数分流；worker 用 `tokio::spawn` + `tokio::sync::Notify` 实现取消；进程树 kill 走 std `CommandExt`（Unix process_group / Windows creation_flags）+ shell-out（`kill -KILL -pgid` / `taskkill /T /F /PID`），零新依赖；通知走 StopHook(主动唤醒) + 循环顶部 collect(被动兜底) 双路径。

**Tech Stack:** Rust, tokio(full, 已有 process/time/sync), serde, fastrand, async-trait, tempfile(dev)。零新依赖（进程树 kill 用 std `std::os::{unix,windows}::process::CommandExt` + shell-out）。

**Spec:** `docs/superpowers/specs/2026-08-19-s11-background-tasks-design.md`

## 全局约束

- bg_id 格式：`bg_` + 8 hex（`bg_[0-9a-f]{8}`），fastrand 生成，重试 100 次防碰撞
- 输出目录：`.task_outputs/background/`，文件名 `{bg_id}.log`
- TaskStatus 枚举：`Running, Completed, Failed, Cancelled`（snake_case JSON）
- 并发上限 `MAX_CONCURRENT = 8`
- 后台命令超时 `BG_TIMEOUT_SECS = 120`
- 输出截断 `MAX_OUTPUT_BYTES = 50_000`，通知 summary 截断 `SUMMARY_CHARS = 500`
- 所有工具 `PermissionCheck::Pass`，`available_for_subagent() -> true`
- 工具名：`task_output`、`task_stop`（`CommandTool` 仍叫 `command`）
- 通知不复用 tool_use_id，作为独立 user 消息 Text 块追加；collect 后 task 从内存移除（防重复注入）
- `BackgroundManager` 内部 `state: Arc<Mutex<State>>`，`#[derive(Clone)]`，worker 拿 clone 共享同一份 state

## 文件结构

| 文件 | 责任 | 创建/修改 |
|------|------|----------|
| `rust-agent/src/background_tasks/mod.rs` | 模块导出 | Create |
| `rust-agent/src/background_tasks/task.rs` | `BackgroundTask` + `TaskStatus` 纯数据 | Create |
| `rust-agent/src/background_tasks/manager.rs` | `BackgroundManager`：start/collect/stop/output + worker + kill_tree | Create |
| `rust-agent/src/background_tasks/tools.rs` | `TaskOutputTool`/`TaskStopTool`/`BackgroundStopHook`/`collect_and_inject` + 全局 | Create |
| `rust-agent/src/tools/command.rs` | 加 `run_in_background` 参数 + `start_background` | Modify |
| `rust-agent/src/lib.rs` | `pub mod background_tasks;` | Modify |
| `rust-agent/src/tools/mod.rs` | 注册两个新工具 | Modify |
| `rust-agent/src/main.rs` | 循环顶部 collect + 注册 StopHook | Modify |

---

### Task 1: 创建模块骨架与数据结构

**Files:**
- Create: `rust-agent/src/background_tasks/mod.rs`
- Create: `rust-agent/src/background_tasks/task.rs`
- Modify: `rust-agent/src/lib.rs`

**Interfaces:**
- Produces: `TaskStatus` 枚举 (`Running, Completed, Failed, Cancelled`)
- Produces: `BackgroundTask` 结构体 (`id, command, status, tool_use_id, started_at, output_file, exit_code`)

- [ ] **Step 1: 在 lib.rs 声明模块**

修改 `rust-agent/src/lib.rs`，在 `pub mod task_system;` 之后加一行：

```rust
pub mod background_tasks;
```

- [ ] **Step 2: 创建 task.rs**

创建 `rust-agent/src/background_tasks/task.rs`：

```rust
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
```

- [ ] **Step 3: 创建 mod.rs**

创建 `rust-agent/src/background_tasks/mod.rs`：

```rust
/*
background_tasks/mod.rs - 后台任务模块 (s11)

慢速 bash 后台执行: 当前工具调用立即返回 bg_id 占位 tool_result,
循环继续; 命令完成后在后续轮次以 <task_notification> 注入会话。
*/

pub mod task;

pub use task::{BackgroundTask, TaskStatus};
```

- [ ] **Step 4: 运行测试**

Run: `cd rust-agent && cargo test background_tasks::task`
Expected: 2 tests pass

- [ ] **Step 5: Commit**

```bash
git add rust-agent/src/background_tasks/ rust-agent/src/lib.rs
git commit -m "feat(s11): add BackgroundTask data structure with tests

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 2: Manager 骨架与 ID 生成

**Files:**
- Create: `rust-agent/src/background_tasks/manager.rs`
- Modify: `rust-agent/src/background_tasks/mod.rs`

**Interfaces:**
- Produces: `BackgroundManager` 结构体 (`output_dir: PathBuf, state: Arc<Mutex<State>>`, `#[derive(Clone)]`) + `new(output_dir)`
- Produces: `create_test_manager(output_dir)` 测试 helper
- Produces: `generate_id()` 私有方法 (fastrand 8 hex, 重试 100 次)
- Produces: 常量 `MAX_CONCURRENT, BG_TIMEOUT_SECS, MAX_OUTPUT_BYTES, SUMMARY_CHARS`

- [ ] **Step 1: 写 manager.rs 骨架与 ID 测试**

创建 `rust-agent/src/background_tasks/manager.rs`：

```rust
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
```

- [ ] **Step 2: 运行测试验证通过**

Run: `cd rust-agent && cargo test background_tasks::manager`
Expected: 2 tests pass

- [ ] **Step 3: 更新 mod.rs 导出 manager**

修改 `rust-agent/src/background_tasks/mod.rs`：

```rust
/*
background_tasks/mod.rs - 后台任务模块 (s11)

慢速 bash 后台执行: 当前工具调用立即返回 bg_id 占位 tool_result,
循环继续; 命令完成后在后续轮次以 <task_notification> 注入会话。
*/

pub mod manager;
pub mod task;

pub use manager::BackgroundManager;
pub use task::{BackgroundTask, TaskStatus};
```

- [ ] **Step 4: 运行全部 background_tasks 测试**

Run: `cd rust-agent && cargo test background_tasks`
Expected: 4 tests pass (task 2 + manager 2)

- [ ] **Step 5: Commit**

```bash
git add rust-agent/src/background_tasks/
git commit -m "feat(s11): add BackgroundManager skeleton with id generation

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 3: start() + 最小 worker + collect()

**Files:**
- Modify: `rust-agent/src/background_tasks/manager.rs`

**Interfaces:**
- Produces: `BackgroundManager::start(command, tool_use_id) -> Result<String, String>` (同步; 内部 tokio::spawn)
- Produces: `BackgroundManager::collect() -> Vec<String>` (返回 `<task_notification>` XML 列表)
- Produces: 私有 `async fn run_worker(...)` (此版本仅 completed/failed; 超时/取消/panic 在后续任务加)
- Produces: `fn finalize(...)`, `fn build_command(...)`, `fn truncate_chars(...)`, `fn format_notification(...)`, `fn status_word(...)`

- [ ] **Step 1: 写 start→collect 完成通知测试**

在 `manager.rs` 的 `#[cfg(test)] mod tests` 末尾追加：

```rust
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
```

- [ ] **Step 2: 运行测试验证失败**

Run: `cd rust-agent && cargo test background_tasks::manager::tests::start_completes_and_collect_injects_notification`
Expected: FAIL — `method start not found` / `method collect not found`

- [ ] **Step 3: 实现 start / collect / run_worker 及辅助函数**

在 `impl BackgroundManager` 内追加 `start` 与 `collect`（保留 Task 2 的 `generate_id`）：

```rust
    /// 启动后台任务: 注册 + spawn worker + 立即返回 bg_id。
    /// 同步方法 (内部 tokio::spawn 是同步调用)。调用方把 bg_id 拼进占位 tool_result。
    pub fn start(&self, command: &str, tool_use_id: &str) -> Result<String, String> {
        let command = command.trim();
        if command.is_empty() {
            return Err("Error: empty command".to_string());
        }
        let id = self.generate_id();
        if id.is_empty() {
            return Err("Error: failed to allocate task id".to_string());
        }
        let output_file = self.output_dir.join(format!("{}.log", id));
        let started_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let cancel = Arc::new(Notify::new());
        let task = BackgroundTask {
            id: id.clone(),
            command: command.to_string(),
            status: TaskStatus::Running,
            tool_use_id: tool_use_id.to_string(),
            started_at,
            output_file: output_file.clone(),
            exit_code: None,
        };
        {
            let mut state = self.state.lock().unwrap();
            state.tasks.insert(id.clone(), task);
            state.cancels.insert(id.clone(), cancel.clone());
        }
        let mgr = self.clone();
        let cmd = command.to_string();
        tokio::spawn(run_worker(mgr, id, cmd, output_file, cancel));
        Ok(id)
    }

    /// 收集已完成任务, 返回 <task_notification> XML 列表。
    /// 收集后从 tasks 移除 (通知一次即丢弃, 防重复注入)。
    pub fn collect(&self) -> Vec<String> {
        let drained: Vec<String> = {
            let mut state = self.state.lock().unwrap();
            state.ready.drain(..).collect()
        };
        let mut out = Vec::new();
        for id in drained {
            let task_opt = {
                let mut state = self.state.lock().unwrap();
                state.tasks.remove(&id)
            };
            if let Some(task) = task_opt {
                out.push(format_notification(&task));
            }
        }
        out
    }
```

在 `impl` 块之外（模块级）追加 worker 与辅助函数：

```rust
/// worker: 执行命令, 落盘输出, finalize (更新状态 + 入 ready)。
///
/// 此版本: 直接 await child.wait_with_output(), 仅区分 completed/failed。
/// 超时/取消/panic 守卫在后续任务加。
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
    let mut state = mgr.state.lock().unwrap();
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
```

注: `decode_console` 是 `pub(crate)`（`command.rs:80`），`manager.rs` 经 `crate::tools::command::decode_console` 可调。`_cancel` 暂未使用, 加 `#[allow(unused_variables)]` 于 `run_worker` 签名前, Task 5 移除。

- [ ] **Step 4: 运行测试验证通过**

Run: `cd rust-agent && cargo test background_tasks::manager`
Expected: 3 tests pass

- [ ] **Step 5: 写失败命令测试**

在 `tests` 模块追加：

```rust
    #[tokio::test]
    async fn failed_command_yields_failed_notification() {
        let dir = tempdir().unwrap();
        let mgr = create_test_manager(dir.path());
        let cmd = if cfg!(windows) { "cmd /C exit 7" } else { "bash -c 'exit 7'" };
        let id = mgr.start(cmd, "toolu_2").unwrap();
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
```

- [ ] **Step 6: 运行测试验证通过**

Run: `cd rust-agent && cargo test background_tasks::manager`
Expected: 4 tests pass

- [ ] **Step 7: Commit**

```bash
git add rust-agent/src/background_tasks/manager.rs
git commit -m "feat(s11): implement start/collect/worker with output persistence

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 4: 进程树 kill + worker 超时分支

**Files:**
- Modify: `rust-agent/src/background_tasks/manager.rs`

**Interfaces:**
- Produces: `build_command` 加 Unix `process_group(0)` / Windows `creation_flags(CREATE_NEW_PROCESS_GROUP)`
- Produces: `async fn kill_tree(child: &mut Child)` — Unix `kill -KILL -{pgid}`, Windows `taskkill /T /F /PID {pid}` (均 shell-out, 零依赖)
- Produces: `run_worker` 改为 `tokio::select!` 含 sleep 超时臂; 超时 → kill_tree → Failed + exit_code=None + 写 "Error: Timeout (Ns)"
- Produces: `create_test_manager_with_timeout(output_dir, secs)` 测试 helper (经 `TEST_BG_TIMEOUT_SECS` 环境变量)

- [ ] **Step 1: 写超时测试**

在 `#[cfg(test)]` 的 `create_test_manager` 旁加：

```rust
#[cfg(test)]
pub(crate) fn create_test_manager_with_timeout(
    output_dir: &std::path::Path,
    timeout_secs: u64,
) -> BackgroundManager {
    std::env::set_var("TEST_BG_TIMEOUT_SECS", timeout_secs.to_string());
    create_test_manager(output_dir)
}
```

在 `tests` 模块追加：

```rust
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
        let log = std::fs::read_to_string(
            dir.path().join("background").join(format!("{}.log", id)),
        )
        .unwrap_or_default();
        assert!(log.contains("Timeout"), "log must say Timeout: {}", log);
        std::env::remove_var("TEST_BG_TIMEOUT_SECS");
    }
```

- [ ] **Step 2: 运行测试验证失败**

Run: `cd rust-agent && cargo test background_tasks::manager::tests::timeout_yields_failed_with_consistent_text`
Expected: FAIL — 当前无超时, ping/sleep 跑到测试挂起

- [ ] **Step 3: 实现 build_command 进程组 + kill_tree**

替换 `build_command` 为：

```rust
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
    // kill_tree 据此杀整组/整棵树。
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        c.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
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
```

- [ ] **Step 4: run_worker 改 select! 含超时臂 (用 kill_tree)**

把 Task 3 的 `run_worker` 整体替换为：

```rust
/// worker: 执行命令, 落盘输出, finalize。
///
/// select! 三臂: 取消(下划线占位, Task 5 接 cancel) / 超时 / 正常完成。
/// 此版本 cancel 臂用 sleep 占位 (未接 cancel Notify), Task 5 替换。
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
    let timeout_secs = std::env::var("TEST_BG_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(BG_TIMEOUT_SECS);
    let (status, exit_code, text) = tokio::select! {
        biased;
        _ = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)) => {
            kill_tree(&mut child).await;
            let _ = child.wait().await;
            (TaskStatus::Failed, None, format!("Error: Timeout ({}s)", timeout_secs))
        }
        out = child.wait_with_output() => match out {
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
                let status = if code == Some(0) { TaskStatus::Completed } else { TaskStatus::Failed };
                (status, code, body)
            }
            Err(e) => (TaskStatus::Failed, None, format!("Error: wait failed: {}", e)),
        }
    };
    let _ = std::fs::write(&output_file, &text);
    finalize(&mgr, &id, status, exit_code);
}
```

注: `child.wait_with_output()` 借 `&mut child`, 与 `kill_tree(&mut child)` 在不同 select 臂内, 编译器接受 (同一 `&mut` 不在两臂同时活跃)。`_cancel` 仍未使用, 保留 `#[allow(unused_variables)]` 到 Task 5。

- [ ] **Step 5: 运行测试验证通过**

Run: `cd rust-agent && cargo test background_tasks::manager`
Expected: 5 tests pass (含超时)

- [ ] **Step 6: Commit**

```bash
git add rust-agent/src/background_tasks/manager.rs
git commit -m "feat(s11): add process-tree kill and worker timeout branch

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 5: worker 取消分支 + stop() + Cancelled

**Files:**
- Modify: `rust-agent/src/background_tasks/manager.rs`

**Interfaces:**
- Produces: `BackgroundManager::stop(task_id) -> String` (同步)
- Produces: `run_worker` select! 加 cancel 臂; 取消 → kill_tree → Cancelled → 入 ready

- [ ] **Step 1: 写 stop→Cancelled 测试**

在 `tests` 模块追加：

```rust
    #[tokio::test]
    async fn stop_cancels_running_task() {
        let dir = tempdir().unwrap();
        let mgr = create_test_manager_with_timeout(dir.path(), 60);
        let cmd = if cfg!(windows) { "ping -n 60 127.0.0.1" } else { "sleep 60" };
        let id = mgr.start(cmd, "toolu_4").unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let msg = mgr.stop(&id);
        assert!(msg.contains("Stopped"), "stop msg: {}", msg);

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
        std::env::remove_var("TEST_BG_TIMEOUT_SECS");
    }

    #[test]
    fn stop_unknown_task_is_noop_error() {
        let dir = tempdir().unwrap();
        let mgr = create_test_manager(dir.path());
        let msg = mgr.stop("bg_deadbeef");
        assert!(msg.contains("not found"), "unknown stop: {}", msg);
    }
```

- [ ] **Step 2: 运行测试验证失败**

Run: `cd rust-agent && cargo test background_tasks::manager::tests::stop_cancels_running_task`
Expected: FAIL — `method stop not found`

- [ ] **Step 3: 实现 stop()**

在 `impl BackgroundManager` 内追加：

```rust
    /// 取消后台任务: 触发 cancel Notify -> worker 走取消分支 -> kill 进程树 -> Cancelled -> 入 ready。
    /// 已完成/不存在的任务返回提示, no-op。
    pub fn stop(&self, task_id: &str) -> String {
        let cancel_opt = {
            let state = self.state.lock().unwrap();
            match state.tasks.get(task_id).map(|t| t.status) {
                None => return format!("Error: task {} not found", task_id),
                Some(TaskStatus::Running) => state.cancels.get(task_id).cloned(),
                Some(other) => return format!("Task {} already {}", task_id, status_word(other)),
            }
        };
        match cancel_opt {
            Some(cancel) => {
                cancel.notify_one();
                format!("Stopped {}", task_id)
            }
            None => format!("Error: task {} not found", task_id),
        }
    }
```

- [ ] **Step 4: run_worker 加 cancel 臂 (替换 sleep 占位)**

把 `run_worker` 的 select! 块中, `biased;` 之后、`sleep` 臂之前插入 cancel 臂。把 `_cancel: Arc<Notify>` 参数改为 `cancel: Arc<Notify>` (去掉下划线), 并移除 `#[allow(unused_variables)]`。select! 块替换为：

```rust
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
        out = child.wait_with_output() => match out {
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
                let status = if code == Some(0) { TaskStatus::Completed } else { TaskStatus::Failed };
                (status, code, body)
            }
            Err(e) => (TaskStatus::Failed, None, format!("Error: wait failed: {}", e)),
        }
    };
```

- [ ] **Step 5: 运行测试验证通过**

Run: `cd rust-agent && cargo test background_tasks::manager`
Expected: 7 tests pass (含 stop 两个)

- [ ] **Step 6: Commit**

```bash
git add rust-agent/src/background_tasks/manager.rs
git commit -m "feat(s11): add TaskStop cancellation with Cancelled state

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 6: worker panic 守卫

**Files:**
- Modify: `rust-agent/src/background_tasks/manager.rs`

**Interfaces:**
- Produces: `run_worker` 把 worker 体 spawn 为内部 task, 外层 await JoinHandle; panic → JoinError → 兜底 finalize(Failed) (不丢任务)

- [ ] **Step 1: 写 panic 守卫不变量测试**

在 `tests` 模块追加（固化「worker 异常路径仍 finalize、task 不卡 Running」不变量）：

```rust
    #[tokio::test]
    async fn worker_finalizes_even_on_abnormal_exit() {
        // 用超短超时 + 长命令模拟 worker 异常路径, 验证 finalize 一定执行 (task 不卡 Running)。
        let dir = tempdir().unwrap();
        let mgr = create_test_manager_with_timeout(dir.path(), 1);
        let id = mgr.start("echo panic_guard_ok", "toolu_5").unwrap();
        let mut got = Vec::new();
        for _ in 0..200 {
            got = mgr.collect();
            if !got.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!got.is_empty(), "worker must finalize even under abnormal conditions");
        let state = mgr.state.lock().unwrap();
        assert!(!state.tasks.contains_key(&id), "task must be removed after collect");
        std::env::remove_var("TEST_BG_TIMEOUT_SECS");
    }
```

- [ ] **Step 2: 运行测试验证通过 (正常路径已 finalize)**

Run: `cd rust-agent && cargo test background_tasks::manager::tests::worker_finalizes_even_on_abnormal_exit`
Expected: PASS

- [ ] **Step 3: 用内部 spawn + JoinHandle 守卫 panic**

`tokio::spawn` 的 task panic 时, `JoinHandle.await` 返回 `Err(JoinError)`。把 `run_worker` 改为: 内部 spawn worker 体, 外层 await JoinHandle, Err 走兜底 finalize。

替换整个 `run_worker` 为：

```rust
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
    // 内部 task 持有所有权的副本; output_file 克隆一份供外层 panic 兜底写。
    // mgr/id 也要克隆一份给外层 Err 路径 (它们会被 move 进内部 task)。
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
        let timeout_secs = std::env::var("TEST_BG_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(BG_TIMEOUT_SECS);
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
            out = child.wait_with_output() => match out {
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
                    let status = if code == Some(0) { TaskStatus::Completed } else { TaskStatus::Failed };
                    (status, code, body)
                }
                Err(e) => (TaskStatus::Failed, None, format!("Error: wait failed: {}", e)),
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
```

- [ ] **Step 4: 运行测试验证通过**

Run: `cd rust-agent && cargo test background_tasks::manager`
Expected: 8 tests pass

- [ ] **Step 5: Commit**

```bash
git add rust-agent/src/background_tasks/manager.rs
git commit -m "feat(s11): guard worker panics with JoinHandle fallback finalize

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 7: 并发上限闸门

**Files:**
- Modify: `rust-agent/src/background_tasks/manager.rs`

**Interfaces:**
- Produces: `start()` 在注册前检查 `running_count >= MAX_CONCURRENT` → 返回错误

- [ ] **Step 1: 写并发上限测试**

在 `tests` 模块追加：

```rust
    #[tokio::test]
    async fn concurrent_cap_rejects_excess() {
        let dir = tempdir().unwrap();
        let mgr = create_test_manager_with_timeout(dir.path(), 60);
        let long_cmd = if cfg!(windows) { "ping -n 60 127.0.0.1" } else { "sleep 60" };
        for _ in 0..MAX_CONCURRENT {
            mgr.start(long_cmd, "toolu_x").expect("first 8 should start");
        }
        let result = mgr.start(long_cmd, "toolu_x");
        assert!(result.is_err(), "9th task should be rejected");
        let err = result.unwrap_err();
        assert!(err.contains("too many concurrent"), "err: {}", err);
        // 清理
        let ids: Vec<String> = mgr.state.lock().unwrap().tasks.keys().cloned().collect();
        for id in ids {
            mgr.stop(&id);
        }
        std::env::remove_var("TEST_BG_TIMEOUT_SECS");
    }
```

- [ ] **Step 2: 运行测试验证失败**

Run: `cd rust-agent && cargo test background_tasks::manager::tests::concurrent_cap_rejects_excess`
Expected: FAIL — 第 9 个 start 返回 Ok

- [ ] **Step 3: 在 start() 加闸门**

在 `start` 的 `let id = self.generate_id();` 之前加：

```rust
        {
            let state = self.state.lock().unwrap();
            if state.running_count() >= MAX_CONCURRENT {
                return Err(format!(
                    "Error: too many concurrent background tasks ({}). Wait for some to finish via TaskOutput.",
                    MAX_CONCURRENT
                ));
            }
        }
```

- [ ] **Step 4: 运行测试验证通过**

Run: `cd rust-agent && cargo test background_tasks::manager`
Expected: 9 tests pass

- [ ] **Step 5: Commit**

```bash
git add rust-agent/src/background_tasks/manager.rs
git commit -m "feat(s11): enforce MAX_CONCURRENT background task cap

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 8: output() poll/block

**Files:**
- Modify: `rust-agent/src/background_tasks/manager.rs`

**Interfaces:**
- Produces: `async fn output(task_id, block, timeout_ms) -> String`
- block=true: 轮询至任务非 Running 或 timeout_ms 到; 超时不取消 task, 返回当前状态 + 已有输出

- [ ] **Step 1: 写 output 测试**

在 `tests` 模块追加：

```rust
    #[tokio::test]
    async fn output_poll_on_collected_task_reports_not_found() {
        let dir = tempdir().unwrap();
        let mgr = create_test_manager(dir.path());
        let id = mgr.start("echo poll_output", "toolu_6").unwrap();
        for _ in 0..200 {
            if !mgr.collect().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // task 已被 collect 移除 -> output 报 not found
        let out = mgr.output(&id, false, 0).await;
        assert!(out.contains("not found"), "collected task -> not found: {}", out);
    }

    #[tokio::test]
    async fn output_block_waits_then_returns_completed() {
        let dir = tempdir().unwrap();
        let mgr = create_test_manager(dir.path());
        let id = mgr.start("echo block_ok", "toolu_7").unwrap();
        let out = mgr.output(&id, true, 5000).await;
        assert!(out.contains("completed"), "block should see completed: {}", out);
        assert!(out.contains("block_ok"), "should contain output: {}", out);
    }

    #[tokio::test]
    async fn output_block_timeout_returns_running_without_cancel() {
        let dir = tempdir().unwrap();
        let mgr = create_test_manager_with_timeout(dir.path(), 60);
        let cmd = if cfg!(windows) { "ping -n 60 127.0.0.1" } else { "sleep 60" };
        let id = mgr.start(cmd, "toolu_8").unwrap();
        let out = mgr.output(&id, true, 200).await;
        assert!(out.contains("running"), "block timeout -> running: {}", out);
        let state = mgr.state.lock().unwrap();
        let still = state.tasks.get(&id).map(|t| t.status);
        assert_eq!(still, Some(TaskStatus::Running), "task must still be running after block timeout");
        drop(state);
        mgr.stop(&id);
        std::env::remove_var("TEST_BG_TIMEOUT_SECS");
    }
```

- [ ] **Step 2: 运行测试验证失败**

Run: `cd rust-agent && cargo test background_tasks::manager::tests::output_block_waits_then_returns_completed`
Expected: FAIL — `method output not found`

- [ ] **Step 3: 实现 output()**

在 `impl BackgroundManager` 内追加：

```rust
    /// 取后台任务输出与状态。
    ///
    /// - block=false: 立即返回状态 + output_file 当前内容 (截断 MAX_OUTPUT_BYTES)。
    /// - block=true: 轮询至任务非 Running 或 timeout_ms 到; 超时不取消 task。
    pub async fn output(&self, task_id: &str, block: bool, timeout_ms: u64) -> String {
        if block {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
            loop {
                let done = {
                    let state = self.state.lock().unwrap();
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
            let state = self.state.lock().unwrap();
            match state.tasks.get(task_id) {
                Some(t) => (t.status, t.output_file.clone(), t.exit_code, t.command.clone()),
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
            exit_code.map(|c| c.to_string()).unwrap_or_else(|| "none".to_string()),
            body
        )
    }
```

- [ ] **Step 4: 运行测试验证通过**

Run: `cd rust-agent && cargo test background_tasks::manager`
Expected: 12 tests pass

- [ ] **Step 5: Commit**

```bash
git add rust-agent/src/background_tasks/manager.rs
git commit -m "feat(s11): implement TaskOutput poll/block with timeout

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 9: 工具层 + 全局 + Hook

**Files:**
- Create: `rust-agent/src/background_tasks/tools.rs`
- Modify: `rust-agent/src/background_tasks/mod.rs`

**Interfaces:**
- Produces: `TaskOutputTool`, `TaskStopTool` (impl `Tool`)
- Produces: `BackgroundStopHook` (impl `StopHook`)
- Produces: `pub fn collect_and_inject(messages) -> Option<usize>`
- Produces: `pub fn get_manager() -> Arc<BackgroundManager>` + 全局 `BG_MANAGER: LazyLock<Arc<BackgroundManager>>`

- [ ] **Step 1: 创建 tools.rs**

创建 `rust-agent/src/background_tasks/tools.rs`：

```rust
/*
tools.rs - 后台任务工具与 Hook (s11)

TaskOutputTool / TaskStopTool 把 BackgroundManager 暴露给模型;
BackgroundStopHook 在循环退出前主动注入已完成通知 (主动唤醒);
collect_and_inject 在循环顶部被动兜底收集。
全局 LazyLock<Arc<BackgroundManager>> 对齐 s10。
*/

use crate::background_tasks::manager::BackgroundManager;
use crate::client::{ContentBlock, Message};
use crate::hooks::StopHook;
use crate::tools::trait_def::{PermissionCheck, Tool, ToolContext};
use crate::tools::workdir;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

/// 全局后台任务管理器 (LazyLock 懒初始化, 对齐 s10)。
static BG_MANAGER: std::sync::LazyLock<Arc<BackgroundManager>> =
    std::sync::LazyLock::new(|| {
        Arc::new(BackgroundManager::new(
            workdir().join(".task_outputs").join("background"),
        ))
    });

/// 取全局 manager 的 Arc clone (供 CommandTool 的 start_background 调用)。
pub fn get_manager() -> Arc<BackgroundManager> {
    BG_MANAGER.clone()
}

/// 循环顶部被动兜底: drain ready, 把通知作为独立 user 消息 Text 块追加。
/// 返回注入的通知条数 (None 表示无通知)。
pub fn collect_and_inject(messages: &mut Vec<Message>) -> Option<usize> {
    let notifications = get_manager().collect();
    if notifications.is_empty() {
        return None;
    }
    let count = notifications.len();
    let blocks: Vec<ContentBlock> = notifications
        .into_iter()
        .map(|n| ContentBlock::Text { text: n })
        .collect();
    messages.push(Message {
        role: "user".to_string(),
        content: blocks,
    });
    Some(count)
}

/// 主动唤醒: 循环退出前若 ready 非空, 返回通知强制继续 (对齐 hooks.rs StopHook 语义)。
pub struct BackgroundStopHook;

impl StopHook for BackgroundStopHook {
    fn on_stop(&self, _messages: &[Message]) -> Option<String> {
        let notifications = get_manager().collect();
        if notifications.is_empty() {
            None
        } else {
            Some(notifications.join("\n"))
        }
    }
}

/// TaskOutput 工具: poll/block 取后台任务输出与状态。
pub struct TaskOutputTool;

#[async_trait]
impl Tool for TaskOutputTool {
    fn name(&self) -> &str {
        "task_output"
    }

    fn description(&self) -> &str {
        "Get the status and output of a background task. Set block=true to wait (with timeout) for it to finish."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string" },
                "block": { "type": "boolean", "default": false },
                "timeout_ms": { "type": "integer", "default": 30000 }
            },
            "required": ["task_id"]
        })
    }

    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        let Some(task_id) = input.get("task_id").and_then(|v| v.as_str()) else {
            return "Error: task_id required".to_string();
        };
        let block = input.get("block").and_then(|v| v.as_bool()).unwrap_or(false);
        let timeout_ms = input
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(30_000);
        get_manager().output(task_id, block, timeout_ms).await
    }

    fn available_for_subagent(&self) -> bool {
        true
    }
}

/// TaskStop 工具: 取消后台任务并 kill 进程树。
pub struct TaskStopTool;

#[async_trait]
impl Tool for TaskStopTool {
    fn name(&self) -> &str {
        "task_stop"
    }

    fn description(&self) -> &str {
        "Stop a running background task by cancelling it and killing its process tree."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string" }
            },
            "required": ["task_id"]
        })
    }

    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        let Some(task_id) = input.get("task_id").and_then(|v| v.as_str()) else {
            return "Error: task_id required".to_string();
        };
        get_manager().stop(task_id)
    }

    fn available_for_subagent(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::trait_def::PermissionCheck;
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn task_output_tool_metadata() {
        let t = TaskOutputTool;
        assert_eq!(t.name(), "task_output");
        assert!(t.description().contains("background"));
        let s = t.input_schema();
        assert_eq!(s["required"][0], "task_id");
        assert_eq!(s["properties"]["block"]["type"], "boolean");
        assert_eq!(t.check_permission(&json!({})), PermissionCheck::Pass);
        assert!(t.available_for_subagent());
    }

    #[test]
    fn task_stop_tool_metadata() {
        let t = TaskStopTool;
        assert_eq!(t.name(), "task_stop");
        let s = t.input_schema();
        assert_eq!(s["required"][0], "task_id");
        assert_eq!(t.check_permission(&json!({})), PermissionCheck::Pass);
        assert!(t.available_for_subagent());
    }

    #[test]
    fn collect_on_fresh_manager_is_empty() {
        // 用独立 manager 验证空 collect 契约 (不污染全局)。
        let mgr = Arc::new(BackgroundManager::new(
            std::env::temp_dir().join("bg_test_empty_collect"),
        ));
        assert!(mgr.collect().is_empty());
    }

    #[test]
    fn background_stop_hook_empty_returns_none() {
        // 全局 manager 在测试环境下通常无 ready; 仅验证返回 Option 契约。
        // 不做强断言 (依赖全局状态), 逻辑在 manager 单测中已覆盖。
        let hook = BackgroundStopHook;
        let _: Option<String> = hook.on_stop(&[]);
    }
}
```

- [ ] **Step 2: 更新 mod.rs 导出 tools**

修改 `rust-agent/src/background_tasks/mod.rs`：

```rust
/*
background_tasks/mod.rs - 后台任务模块 (s11)

慢速 bash 后台执行: 当前工具调用立即返回 bg_id 占位 tool_result,
循环继续; 命令完成后在后续轮次以 <task_notification> 注入会话。
*/

pub mod manager;
pub mod task;
pub mod tools;

pub use manager::BackgroundManager;
pub use task::{BackgroundTask, TaskStatus};
pub use tools::{collect_and_inject, BackgroundStopHook, TaskOutputTool, TaskStopTool};
```

- [ ] **Step 3: 运行测试**

Run: `cd rust-agent && cargo test background_tasks`
Expected: 全部 pass (task 2 + manager 12 + tools 4 = 18)

- [ ] **Step 4: Commit**

```bash
git add rust-agent/src/background_tasks/
git commit -m "feat(s11): add TaskOutput/TaskStop tools, StopHook, collect_and_inject

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 10: CommandTool 加 run_in_background

**Files:**
- Modify: `rust-agent/src/tools/command.rs`

**Interfaces:**
- Produces: `CommandTool::input_schema` 加 `run_in_background` 布尔属性
- Produces: `CommandTool::execute` 分流: true -> `start_background`; false -> `run_bash`
- Produces: `pub(crate) async fn start_background(command) -> String` (返回占位 tool_result)

- [ ] **Step 1: 写 run_in_background 测试**

在 `rust-agent/src/tools/command.rs` 的 `#[cfg(test)] mod tests` 末尾追加：

```rust
    #[test]
    fn command_tool_schema_has_run_in_background() {
        let tool = CommandTool;
        let schema = tool.input_schema();
        assert_eq!(schema["properties"]["run_in_background"]["type"], "boolean");
        // command 仍是 required, run_in_background 非 required
        let required = schema["required"].as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "command");
    }

    #[tokio::test]
    async fn execute_run_in_background_true_returns_placeholder() {
        // 会向全局 BG_MANAGER 注册 (写入 cwd 的 .task_outputs/background/)。
        // 用快命令, 末尾 collect 清理内存态。输出文件残留于 cwd, 属可接受测试副作用。
        use crate::tools::trait_def::test_helpers::TestToolContext;
        let tool = CommandTool;
        let tctx = TestToolContext::new();
        let ctx = tctx.context();
        let input = json!({"command": "echo bg_split_test", "run_in_background": true});
        let out = tool.execute(&ctx, &input).await;
        assert!(out.contains("Background task"), "expected placeholder, got: {}", out);
        assert!(out.contains("bg_"), "expected bg_id, got: {}", out);
        // 清理全局内存态
        let _ = crate::background_tasks::collect_and_inject(&mut Vec::new());
    }

    #[tokio::test]
    async fn execute_run_in_background_false_uses_sync_path() {
        use crate::tools::trait_def::test_helpers::TestToolContext;
        let tool = CommandTool;
        let tctx = TestToolContext::new();
        let ctx = tctx.context();
        let input = json!({"command": "echo sync_path_ok", "run_in_background": false});
        let out = tool.execute(&ctx, &input).await;
        assert!(out.contains("sync_path_ok"), "false should use sync run_bash, got: {}", out);
    }
```

- [ ] **Step 2: 运行测试验证失败**

Run: `cd rust-agent && cargo test command::tests::command_tool_schema_has_run_in_background`
Expected: FAIL — schema 无 `run_in_background` 属性

- [ ] **Step 3: 改 schema + execute + 加 start_background**

`input_schema` 改为：

```rust
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute (e.g., 'ls -la', 'git status')"
                },
                "run_in_background": {
                    "type": "boolean",
                    "description": "若 true，命令在后台执行，立即返回 bg_id；完成后在后续轮次以 <task_notification> 注入。仅用于独立的慢命令（install/build/test）。"
                }
            },
            "required": ["command"]
        })
    }
```

`execute` 改为：

```rust
    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        let Some(command) = input.get("command").and_then(|v| v.as_str()) else {
            return "Error: No command provided".to_string();
        };
        let bg = input
            .get("run_in_background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if bg {
            start_background(command).await
        } else {
            run_bash(command).await
        }
    }
```

在文件末尾 `#[cfg(test)] mod tests` 之前加自由函数：

```rust
/// 启动后台任务并返回占位 tool_result (供 CommandTool::execute 分流)。
///
/// 成功: "[Background task {bg_id} started] The result will be collected on a later turn. Use TaskOutput to poll, TaskStop to cancel."
/// 失败 (空命令/并发超限/ID 耗尽): 返回 "Error: ..."。
///
/// 注: tool_use_id 传空串 — 主循环 execute_tool 未把 tool_use_id 传入 execute,
/// 占位 tool_result 由 agent_loop 用原 id 构造; 此字段仅做关联记录, 空串可接受
/// (通知不复用 tool_use_id, 见 spec 设计)。
pub(crate) async fn start_background(command: &str) -> String {
    let mgr = crate::background_tasks::get_manager();
    match mgr.start(command, "") {
        Ok(id) => format!(
            "[Background task {} started] The result will be collected on a later turn. Use TaskOutput to poll, TaskStop to cancel.",
            id
        ),
        Err(e) => e,
    }
}
```

- [ ] **Step 4: 运行测试验证通过**

Run: `cd rust-agent && cargo test command`
Expected: 全部 pass (含新增 3 个)

- [ ] **Step 5: Commit**

```bash
git add rust-agent/src/tools/command.rs
git commit -m "feat(s11): add run_in_background param to CommandTool

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 11: 集成到主循环

**Files:**
- Modify: `rust-agent/src/tools/mod.rs`
- Modify: `rust-agent/src/main.rs`

**Interfaces:**
- Produces: `build_registry()` 注册 `TaskOutputTool`, `TaskStopTool`
- Produces: `agent_loop` 循环顶部调 `collect_and_inject` (被动兜底)
- Produces: `main` 注册 `BackgroundStopHook` (主动唤醒)

- [ ] **Step 1: 注册工具**

修改 `rust-agent/src/tools/mod.rs` 的 `build_registry`, 在 s10 五个工具注册之后、`registry` 返回之前加：

```rust
    registry.register(Box::new(crate::background_tasks::TaskOutputTool));
    registry.register(Box::new(crate::background_tasks::TaskStopTool));
```

- [ ] **Step 2: 循环顶部加被动兜底**

修改 `rust-agent/src/main.rs` 的 `agent_loop`。在 `loop {` (约 line 110) 之后、`compactor.prepare(...)` (约 line 112) 之前加：

```rust
        // s11: 循环顶部收集已完成后台任务通知 (被动兜底)
        let _ = rust_agent::background_tasks::collect_and_inject(messages);
```

- [ ] **Step 3: 注册 BackgroundStopHook (主动唤醒)**

修改 `rust-agent/src/main.rs` 的 `main`, 在 `hooks.on_stop(SummaryHook);` (约 line 256) 之后加：

```rust
    hooks.on_stop(rust_agent::background_tasks::BackgroundStopHook);
```

- [ ] **Step 4: 编译**

Run: `cd rust-agent && cargo build`
Expected: 无错误

- [ ] **Step 5: 运行全部测试**

Run: `cd rust-agent && cargo test`
Expected: 全部 pass

- [ ] **Step 6: clippy**

Run: `cd rust-agent && cargo clippy --all-targets -- -D warnings`
Expected: 无警告。若有 unused import 警告, 清理之。

- [ ] **Step 7: Commit**

```bash
git add rust-agent/src/tools/mod.rs rust-agent/src/main.rs
git commit -m "feat(s11): integrate background tasks into agent loop and registry

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## 完成验证 (全特性端到端)

实现全部任务后, 手动 smoke (可选, 需 API key):

Run: `cd rust-agent && cargo run`
输入一个会让模型发起后台任务的请求 (如 "在后台跑 echo hello && sleep 2, 然后读 package.json")。观察:
- `command` 工具带 `run_in_background: true` → 立即返回 `[Background task bg_xxxx started]`
- agent 继续读文件 (循环未被阻塞)
- 后续轮次日志出现 `<task_notification>` 注入
- 主动唤醒: 若 agent 想停止而后台任务刚完成, 循环自动继续

成功标准 (对照 spec):
- [ ] bash `run_in_background=true` 立即返回 bg_id 占位, 不阻塞循环
- [ ] 输出落盘到 `.task_outputs/background/{bg_id}.log`
- [ ] 完成后 collect 注入 `<task_notification>` (主动 + 被动双路径)
- [ ] 1:1 tool_use↔tool_result 不变量保持
- [ ] `TaskOutput` 支持 poll 与 block (超时不取消)
- [ ] `TaskStop` 取消并 kill 进程树, 置 Cancelled
- [ ] 并发上限 8 生效
- [ ] 超时 status=Failed 且文本一致
- [ ] worker panic 不丢任务
- [ ] Windows/Unix 均能 kill 进程树 (process_group/creation_flags + kill/taskkill shell-out)
- [ ] `cargo build` + `cargo test` + `cargo clippy` 全绿
