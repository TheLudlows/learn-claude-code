# s11 Background Tasks 设计规格

**日期：** 2026-08-19
**状态：** Approved
**关联：** `s11_background_tasks/README.zh.md`、`s11_background_tasks/code.py`（Python 参考实现）、s10 task system（模块/全局模式参照）

## 概述

在 rust-agent 中实现后台任务能力：慢速 bash 命令在后台执行，当前工具调用立即返回 `bg_id` 占位 `tool_result`，agent 循环继续推进；命令完成后在**后续轮次**以 `<task_notification>` 注入会话。循环不被一次慢命令阻塞，可继续处理同一响应内的其它工具调用与下一轮。

本设计为「高质量中间形态」：保留 s11 的核心思想（显式 `run_in_background` 触发、占位 tool_result、后续轮次通知、1:1 tool_use↔tool_result 不变量），并补齐使其真正可用的增量——并发上限、`TaskOutput`（poll/block）、`TaskStop`（取消 + Cancelled 态）、输出落盘、Windows 感知的进程树清理。范围限定 bash-only、内存态（持久化是 s10 的职责）。

## 背景

读文件、`git status` 很快；`npm install`、全量测试、构建动辄数分钟。同步执行会让整个 agent 循环阻塞在一次 Bash 调用上——harness 既无法在同一响应里处理下一个工具调用，也无法进入下一轮。若后续工作不依赖该命令，等待即浪费。

s11 的 Python 参考实现是单文件教学版，刻意最小化：仅 bash、内存态、collect-on-next-turn、无 TaskOutput/TaskStop/并发上限/持久化。本设计在其基础上做工程化增强。

## 架构

```
rust-agent/src/
├── background_tasks/
│   ├── mod.rs          # 模块导出 + 全局注册表 LazyLock<Arc<BackgroundManager>>
│   ├── task.rs         # BackgroundTask 数据结构 + TaskStatus 枚举（纯数据，零业务依赖）
│   ├── manager.rs      # BackgroundManager：注册/查询/取消/收集 + tokio::spawn worker
│   └── tools.rs        # TaskOutputTool / TaskStopTool + BackgroundStopHook + collect_and_inject
├── tools/
│   ├── command.rs      # 现有 CommandTool 加 run_in_background 参数 + 抽共享 runner
│   └── mod.rs          # 注册 TaskOutputTool / TaskStopTool
└── main.rs             # agent_loop 循环顶部插 collect_and_inject；注册 BackgroundStopHook
```

**职责边界**（每单元单一目的，可独立测试）：
- `task.rs`：纯数据结构，零业务逻辑依赖。`BackgroundTask` + `TaskStatus`。
- `manager.rs`：进程内注册表 + worker 调度 + 收集/取消。内部 `Arc<Mutex<State>>`。
- `tools.rs`：把 manager 暴露成 `Tool` trait + StopHook 实现。胶水层，无核心逻辑。
- `command.rs`：仅决定「前台 or 后台」，后台路径委托 manager，前台路径走现有 `run_bash`。

**全局状态**：`static BG_MANAGER: LazyLock<Arc<BackgroundManager>>`（内存态，无落盘）。worker 用 `tokio::spawn`——本项目首次引入 spawn，提供真实异步执行。

## 核心组件

### 1. 数据结构 — `src/background_tasks/task.rs`

```rust
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Running,    // worker 执行中
    Completed,  // exit_code == 0
    Failed,     // exit_code != 0 或超时或异常
    Cancelled,  // 被 TaskStop 取消
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackgroundTask {
    pub id: String,              // "bg_" + 8 hex，如 bg_a1b2c3d4
    pub command: String,         // bash 命令串
    pub status: TaskStatus,
    pub tool_use_id: String,     // 原始 tool_use id，仅做关联；通知不复用它
    pub started_at: u64,         // 启动时间戳（Unix 秒），用于超时判定与展示
    pub output_file: PathBuf,    // 输出落盘文件路径（TaskOutput 读这里，不占内存）
    pub exit_code: Option<i32>,  // 完成后填；Running 时 None
}
```

**设计决策：**
- `TaskStatus` 用 enum（类型安全），`#[serde(rename_all = "snake_case")]` 与 s10/Python 兼容。
- ID 用 `bg_` + 8 hex（`fastrand::u32(..)`，重试 100 次防碰撞），对齐 s10 的 hex 风格，而非 s11 的 4 位计数器（不暴露计数顺序）。
- 新增 `started_at`、`output_file`、`exit_code` 三个 s11 没有的字段——分别服务于超时判定、输出落盘、成败细节展示。

### 2. BackgroundManager — `src/background_tasks/manager.rs`

**内部状态**（一个 `Mutex` 守护，锁粒度小、持锁期短，只动 HashMap/队列，不做 IO）：

```rust
struct State {
    tasks: HashMap<String, BackgroundTask>,      // bg_id -> task
    ready: VecDeque<String>,                     // 已完成待收集的 bg_id（FIFO）
    cancels: HashMap<String, Arc<Notify>>,       // bg_id -> 取消信号
}

pub struct BackgroundManager {
    output_dir: PathBuf,            // .task_outputs/background/
    state: Mutex<State>,
    max_concurrent: usize,          // 8
    command_timeout: Duration,      // 120s
}
```

**关键方法：**
- `new(output_dir)`：纯内存构造，`new` 不 fallible（与 s10 的 `TaskStore::new` 不同，后者 fallible 因校验工作区）。
- `start(command, tool_use_id) -> Result<String, BgError>`：生成 bg_id、预建 output_file、注册 task、并发闸门检查、`tokio::spawn` worker、返回 bg_id。
- `output(task_id, block, timeout_ms) -> String`：读 output_file 当前内容（截断 50000 字符）；`block=true` 时 `tokio::time::timeout` 等待该 task 的完成 Notify。
- `stop(task_id) -> String`：触发 cancel Notify → worker 走取消分支 → kill 子进程 → 置 Cancelled → 入 ready。
- `collect() -> Vec<String>`：持锁 drain `ready`，每条格式化为 `<task_notification>` XML，从 `tasks` 移除已收集 task（通知一次即丢弃，防重复注入）。
- `drain_running_for_cleanup()`：进程退出时扫所有 Running task，统一 kill 子进程树（生命周期卫生，非沙箱）。

**全局：**
```rust
static BG_MANAGER: std::sync::LazyLock<Arc<BackgroundManager>> =
    std::sync::LazyLock::new(|| {
        Arc::new(BackgroundManager::new(workdir().join(".task_outputs").join("background")))
    });
```
懒初始化于首次工具调用（对齐 s10 的 `get_store` 模式）。

### 3. 工具与 Hook — `src/background_tasks/tools.rs`

| 组件 | 类型 | 作用 |
|------|------|------|
| `TaskOutputTool` | `Tool` | poll/block 取后台任务输出与状态 |
| `TaskStopTool` | `Tool` | 取消后台任务并 kill 进程树 |
| `BackgroundStopHook` | `StopHook` | 主动唤醒：ready 非空时返回通知强制继续循环 |
| `collect_and_inject(&mut [Message])` | 自由函数 | 被动兜底：循环顶部 drain ready 并追加通知 |

`CommandTool`（`tools/command.rs`）加 `run_in_background` 参数后，`execute` 内分流：`true` → 委托 `manager.start()` 立即返回占位；`false` → 走现有 `run_bash` 前台路径。

## 状态机

```
        start()                 worker 完成
running ──────────→ running ──────────────→ completed | failed
   │                   │
   │     TaskStop      │  TaskStop（完成后取消 = no-op）
   └──────────────────→ cancelled
```

**worker 完成判定**（`manager.rs`，`tokio::spawn` 内）：
- 读 manager 的 cancel 信号（task 级 `Arc<Notify>`）。
- 执行命令，stdout+stderr 合并 pipe → 流式写入 `output_file`。
- `tokio::time::timeout(120s, child.wait())`。
- 分支：
  - 超时 → kill child，`status=Failed`、`exit_code=None`、output_file 追加 `Error: Timeout (120s)`。
  - 取消 → kill child，`status=Cancelled`、`exit_code=None`。
  - 正常 → `status = exit_code==0 ? Completed : Failed`、`exit_code=Some(code)`。
- 持锁更新 task 字段，把 bg_id 推入 `ready` 队列。

**对 s11 一个 bug 的对照修复：** s11 里超时返回 `exit_code=None`，格式化逻辑把 `None` 当成功（无 "Error:" 前缀），但状态判 `failed`——文本与状态矛盾。本设计显式：超时 → `status=Failed`、`exit_code=None`、输出文件写明 `Error: Timeout (Ns)`，状态与文本一致（有专项回归测试）。

## 执行入口与 worker 调度

**`CommandTool` schema 变更**（唯一改动 schema 的现有工具；新增 TaskOutput、TaskStop 两个工具，模型只多见这两个）：

```rust
fn input_schema(&self) -> Value {
    json!({
        "type": "object",
        "properties": {
            "command": { "type": "string" },
            "run_in_background": {
                "type": "boolean",
                "description": "若 true，命令在后台执行，立即返回 bg_id；完成后在后续轮次以 <task_notification> 注入。仅用于独立的慢命令（install/build/test）。"
            }
        },
        "required": ["command"]
    })
}
```

**`execute` 分流**（与 s11 的 `should_run_background` 等价，内联在 tool 里）：
```rust
let bg = input["run_in_background"].as_bool().unwrap_or(false);
if bg { start_background(ctx, command).await }   // 立即返回占位 tool_result
else  { run_bash(command).await }                // 现有前台路径，零改动
```

**共享 runner 抽取：** 把现有 `run_bash` 的核心抽成 `build_command(cmd) -> tokio::process::Command` 与 `format_output(out, code) -> String`，前台后台共用，避免逻辑分叉。前台保持 30s 超时；后台用 manager 的 120s。

**`start_background` 流程：**
1. 生成 `bg_id`（fastrand 8 hex，重试 100 次防碰撞）。
2. 在 `.task_outputs/background/{bg_id}.log` 预创建输出文件。
3. 注册 task 到 manager（status=Running）。
4. **并发闸门**：若当前 `Running` 任务数 ≥ `MAX_CONCURRENT`(8)，拒绝并返回错误 tool_result，让模型重试或等。
5. `tokio::spawn` worker，捕获 `output_file` 路径 + `bg_id` + `Arc<Notify>` 的 owned 拷贝。
6. 立即返回占位：`"[Background task {bg_id} started] The result will be collected on a later turn. Use TaskOutput to poll, TaskStop to cancel."`

**取消机制：** 每个 task 关联一个 `tokio::sync::Notify`（用裸 Notify，避免引入 `tokio_util`）。worker 在 `timeout` 竞选中 `select!` 监听 cancel 信号；`TaskStop` 触发该 Notify → worker 走取消分支 → kill 子进程。

**进程清理（Windows 感知）：**

| 平台 | 启动方式 | kill 方式 |
|------|---------|----------|
| Unix | `Command::process_group(0)`（建新进程组） | kill 整个进程组（SIGTERM→SIGKILL） |
| Windows | `CREATE_NEW_PROCESS_GROUP` | `taskkill /T /F /PID`（`/T` 杀整棵进程树，零依赖） |

Windows 用 `taskkill /T` 而非 Job Object——首发零新依赖，`/T` 杀进程树跨平台够用。进程退出时（`main` 结束）扫 manager 内所有 Running task 统一清理。**明确这不是沙箱**（与 s11 一致），仅生命周期卫生。

## 通知注入与工具 API

**双路径共用同一份 `collect()`：**

```
agent_loop 每轮迭代顶部（LLM 调用前）：
   collect_and_inject(messages)   ← 被动兜底：drain ready，追加 <task_notification>

agent_loop 退出前（stop_reason != tool_use）：
   BackgroundStopHook::on_stop()  ← 主动唤醒：若 ready 非空，返回 Some(通知)，强制 continue
```

**`collect()` 产出的 `<task_notification>`（复用 s11 的 XML 形态，稳定可解析）：**
```xml
<task_notification>
  <task_id>bg_a1b2c3d4</task_id>
  <status>completed</status>
  <command>npm install</command>
  <exit_code>0</exit_code>
  <summary>{output_file 前 500 字符}</summary>
</task_notification>
```

通知作为**一条独立 user 消息**的多个 Text 块追加（沿用 `assemble_post_tool_messages` 的「独立 user 消息」语义，**绝不复用 tool_use_id**）。收集后从 `tasks` 移除该 task（通知一次即丢弃，防重复注入）。

**主动唤醒 vs 被动兜底的协作：**
- 正常：worker 完成 → 入 ready → agent 停止时 StopHook 查到 ready 非空 → 返回通知 → 循环继续 → 顶部 collect 兜底再 drain（此时已空，no-op）。
- 竞争安全：worker 在 StopHook 检查之后、下一轮 collect 之前完成 → StopHook 当轮没看到 → 下一轮顶部 collect 兜底捕获。**通知永不丢失，永不重复**（collect 后即移除）。
- 停止 runaway 风险：StopHook 仅在 `ready 非空` 时返回 Some；空则 None 让循环真退出。不会空转。

**承重机制确认：** `hooks.rs:94-102` 的 `trigger_stop` + `main.rs:151-159` 的循环退出逻辑——当 `stop_reason != "tool_use"` 时触发 StopHook，返回 `Some(msg)` → 注入一条 user 消息 → `continue` 重新进入循环。主动唤醒完全复用此路径，不破坏 1:1 tool_use↔tool_result 不变量。

**循环顶部收集落点**（`main.rs::agent_loop`，line 110 `loop {` 之后、line 112 `compactor.prepare` 之前）：
```rust
loop {
    // s11: 循环顶部收集已完成后台任务通知（被动兜底）
    let _ = rust_agent::background_tasks::collect_and_inject(messages);
    compactor.prepare(client, messages, active_request).await?;
    ...
}
```

### 工具 API

**1. `TaskOutputTool`（`task_output`）——poll/block 取输出：**
```json
{
  "name": "task_output",
  "input_schema": {
    "type": "object",
    "properties": {
      "task_id": { "type": "string" },
      "block": { "type": "boolean", "default": false, "description": "true 则阻塞等待完成（带超时）后再返回" },
      "timeout_ms": { "type": "integer", "default": 30000, "description": "block 模式的最长等待" }
    },
    "required": ["task_id"]
  }
}
```
- `block=false`：立即返回 task 状态 + output_file 当前已落盘内容（截断 50000 字符）。
- `block=true`：`tokio::time::timeout(timeout_ms, 等待完成 Notify)`。完成 → 完整输出 + 状态；超时 → 当前状态（Running）+ 已有输出，**不取消 task**。
- task 不存在/已收集移除 → 返回错误字符串（非 panic）。

**2. `TaskStopTool`（`task_stop`）——取消：**
```json
{
  "name": "task_stop",
  "input_schema": {
    "type": "object",
    "properties": { "task_id": { "type": "string" } },
    "required": ["task_id"]
  }
}
```
触发 cancel Notify → worker 走取消分支 → kill 子进程树 → 置 `Cancelled` → 入 ready（让下一轮 collect 注入「已取消」通知）。返回 `[Stopped {task_id}]`。task 不存在或已完成 → 返回相应提示，no-op。

**subagent 可见性：** `TaskOutputTool`/`TaskStopTool` 的 `available_for_subagent() -> true`。后台任务全局共享，子 agent 与主 agent 共用同一 manager（单进程，无 session 隔离，与 s10/todo 一致）。

## 错误处理

所有工具 `execute` 返回 `String`（沿用项目约定，不 panic）：

| 场景 | 行为 |
|------|------|
| `run_in_background` 但 `command` 为空 | 返回 `"Error: empty command"`，不 spawn |
| 并发达 `MAX_CONCURRENT`(8) | 返回 `"Error: too many concurrent background tasks (8). Wait for some to finish via TaskOutput."`，不 spawn |
| `bg_id` 碰撞 100 次 | 返回 `"Error: failed to allocate task id"`（极低概率） |
| worker spawn 子进程失败 | worker 捕获 → status=Failed，output_file 写 `Error: spawn failed: {e}`，入 ready |
| `TaskOutput` task 不存在/已移除 | 返回 `"Error: task {id} not found"` |
| `TaskOutput` block 超时 | 返回当前状态 + 已有输出，task 继续 |
| `TaskStop` task 不存在 | 返回 `"Error: task {id} not found"`，no-op |
| `TaskStop` 已完成 task | 返回 `"Task {id} already {status}"`，no-op |
| output_file 落盘失败 | `start_background` 返回错误 tool_result，不注册 task |
| manager 全局构造失败 | `LazyLock` 闭包内 panic（沿用 s10 设计张力）；实际 `new` 不 fallible，不会失败 |
| worker panic | `catch_unwind` 兜底，task 置 Failed + 入 ready（不丢任务、不丢通知） |

## 测试策略

对齐项目惯例（inline `#[cfg(test)]` + `tempfile` + `#[tokio::test]`，无 `tests/` 目录）：

| 文件 | 测试模块 | 关键用例 |
|------|---------|---------|
| `task.rs` | `mod tests` | TaskStatus 序列化（snake_case）；BackgroundTask 序列化往返；Cancelled 序列化 |
| `manager.rs` | `mod tests` | `#[tokio::test]`：start→collect 通知注入；start→TaskStop→Cancelled→collect；并发闸门达 8 拒绝；block=true 完成返回；block=true 超时不取消；worker panic 兜底置 Failed；超时 status=Failed+exit_code=None+文本一致（s11 bug 对照修复回归）；输出落盘 + 截断 50000；通知 summary 截断 500；collect 后 task 移除（无重复注入） |
| `tools.rs` | `mod tool_tests` | TaskOutputTool/TaskStopTool 的 `name()`/`input_schema()`/`check_permission()==Pass`/`available_for_subagent()==true`；通过 `create_test_manager(tempdir)` 直接调 free 函数（不走 LazyLock，对齐 s10 的 `create_test_store`） |
| `command.rs` | `mod tests` | `run_in_background=true` 分流返回占位串且含 bg_id；`=false` 走原前台路径不回归 |

**测试 helper：** `manager.rs` 内 `#[cfg(test)] pub(crate) fn create_test_manager(output_dir: &Path) -> BackgroundManager`，直接组装私有字段绕过 `LazyLock`（对齐 s10 的 `create_test_store`）。测试用快命令（`echo`/`sleep 0.01`）+ 小超时，不用真实 API key。

**无 `#[ignore]` smoke 测试**——本特性不调 LLM，纯本地进程调度，全部可确定性测试。

## 集成点

1. **`src/lib.rs`**：加 `pub mod background_tasks;`
2. **`src/tools/command.rs`**：schema 加 `run_in_background`；`execute` 分流；抽 `build_command`/`format_output` 共享
3. **`src/tools/mod.rs`**：`build_registry()` 注册 `TaskOutputTool`、`TaskStopTool`
4. **`src/main.rs`**：
   - `agent_loop` line 110 `loop {` 后插 `background_tasks::collect_and_inject(messages)`（被动兜底）
   - `main` line 256 `hooks.on_stop(SummaryHook)` 旁加 `hooks.on_stop(BackgroundStopHook)`（主动唤醒）
5. **`Cargo.toml`**：**零新依赖**——`tokio`(full，已有 process/time/sync)、`fastrand`(已有)、`serde`/`serde_json`(已有)。Windows 用 `taskkill /T`，Unix 用进程组 kill，均零依赖。
6. **输出目录**：`.task_outputs/background/`（紧邻现有 `.task_outputs/tool-results/`，`main.rs:221` 已建 `.task_outputs/` 父目录，manager 内 `create_dir_all` 建子目录）

## 非目标（YAGNI，明确划界）

- 后台 **agent/subagent** 任务（仅 bash）——范围已定
- 跨会话**持久化**（s10 职责，本特性内存态）
- **远程会话**
- stdout/stderr **分离流**（合并简化，对齐 s11）
- 部分输出**流式推送**（TaskOutput 主动 poll 即可）
- Windows **Job Object**（首发 `taskkill /T`，后续可升级）
- 1000 task 生命周期上限（进程内 HashMap，随会话结束自然清理）

## 与 Python s11 的兼容性

| Python s11 特性 | Rust 实现 |
|----------------|----------|
| `BackgroundManager` 单例 + module 别名 | `LazyLock<Arc<BackgroundManager>>` 全局 |
| `threading.Thread(daemon=True)` worker | `tokio::spawn` |
| `threading.Lock` 守护 tasks/results/_ready | `Mutex<State>` 守 tasks/ready/cancels |
| `bg_{:04}` 计数器 ID | `bg_` + 8 hex（fastrand，对齐 s10） |
| `running→completed\|failed` 三态 | + `cancelled`（TaskStop） |
| 内存 `results` dict | 落盘 `output_file`（不占内存，崩溃不丢输出） |
| `collect()` drain `_ready` | `collect()` drain `ready`，格式同 s11 XML |
| `<task_notification>` 追加到最后 user 消息 | 同（独立 user 消息 Text 块，不复用 tool_use_id） |
| `os.killpg` Unix 清理 | + Windows `taskkill /T` 进程树清理 |
| 无 TaskOutput/TaskStop/并发上限 | 新增（高质量增量） |
| 无 `started_at`/`exit_code`/`output_file` | 新增字段 |
| 超时 exit_code=None 状态/文本矛盾 | 显式修复（状态=Failed，文本写明 Timeout） |

## 成功标准

- [ ] bash `run_in_background=true` 立即返回 `bg_id` 占位 tool_result，不阻塞循环
- [ ] 后台命令输出落盘到 `.task_outputs/background/{bg_id}.log`
- [ ] 完成后下一轮 collect 注入 `<task_notification>`（主动唤醒 + 被动兜底双路径）
- [ ] 1:1 tool_use↔tool_result 不变量保持（通知不复用 tool_use_id）
- [ ] `TaskOutput` 支持 poll 与 block（带超时，超时不取消）
- [ ] `TaskStop` 取消并 kill 进程树，置 Cancelled，注入取消通知
- [ ] 并发上限 8 生效，超限返回错误
- [ ] 超时 status=Failed 且文本一致（修复 s11 状态/文本矛盾）
- [ ] worker panic 不丢任务（兜底置 Failed + 入 ready）
- [ ] Windows 与 Unix 均能 kill 子进程树
- [ ] 全部 inline 测试绿，`cargo build` + `cargo test` + `cargo clippy` 通过

## 迁移说明

- 不影响现有 `run_bash` 前台路径（仅分流，零回归）。
- 不影响 s10 task system（独立模块、独立全局）。
- `CommandTool` schema 加可选布尔参数，对已注册的其它工具无影响。
- `docs/superpowers/specs/` 与 `plans/` 目录在工作区已被删除（s10/output-trace 文档 staged 删除），本 spec 写入时重建 `specs/` 目录；不恢复已删除的 s10/output-trace 文档（用户有意状态）。
