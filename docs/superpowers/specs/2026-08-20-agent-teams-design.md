# s13 Agent Teams 设计文档

**日期**: 2026-08-20
**目的**: 在 bytemaker（Rust / tokio）中实现 s13「Agent Teams」——持久队友、共享任务板、文件收件箱、类型化协议与可选 worktree，复用本分支已落地的 `Agent` 对象抽象作为地基。

## 背景

s13 是 17 阶段路线图（`README-zh.md` L351–369）的 Phase 5「让多个 agent 协作」。权威概念见 `s13_agent_teams/README.zh.md`，参考实现见 `s13_agent_teams/code.py`（1794 行 Python）。

本分支 `refactor/agent-object-abstraction` 已落地 s13 的**地基**：`bytemaker/src/agent.rs`（`Agent` 对象抽象，替代旧的 `subagent.rs`，修复 S2/S4/S8/D2/A2），`main.rs` 退化为薄 REPL。但**团队运行时尚未实现**（全仓 grep `MessageBus` / `TeammateRuntime` / `spawn_teammate` 均无命中）。本设计覆盖团队运行时及其与现有 `Agent` / `TaskStore` / 工具注册表 / REPL 的集成。

s11 后台任务与 s12 定时任务**不带入** s13（README 明确）。它们不参与队友通信、任务认领或计划审批。

## 设计决策

### 1. 线程模型

**选择**：队友为 `tokio::spawn` 出的异步 task，复用统一 `Agent::run_loop`。

**原因**：
- 代码库已是 tokio 原生（`Client` 流式、`Agent::run_loop`、cron/bg/skills 全异步）。
- 复用 s13 地基的核心成果——「父子共用同一个 `run_loop`」；队友再开第二条循环会倒退该成果。
- Python 参考用 OS 线程 + 阻塞 Anthropic 客户端 + `select()` 轮询 stdin，在 bytemaker 里引入阻塞子运行时会逼出第二套客户端、复制循环逻辑、Lead（tokio）与队友（阻塞）之间出现尴尬 FFI，故不采用。

### 2. 跨进程/跨线程文件锁

**选择**：新增 `fs4` crate 的独占建议锁，锁文件 `.tasks/.lock`。

**原因**：
- Python 用 `fcntl.flock`（仅 Unix）。bytemaker 在 **win32** 运行，`fcntl` 不编译，必须跨平台。
- `fs4` 提供跨平台独占锁，覆盖「同进程多线程 + 多进程」两类并发。
- 现有 `TaskStore`（s10）**无任何锁**：`create/load/save/list` 直接读写，`claim_task`/`complete_task` 做 load→check→save 无锁。s10 单 agent 下安全；s13 多队友并发认领则**必须**加锁。故 s13 为 TaskStore 变更路径加锁。

**重入**：Python 的 `task_store_lock` 是可重入 `RLock`（`claim_task`→`save_task` 嵌套）。Rust 侧**避免重入**：将 `claim_task` / `complete_task` 设计为在顶层获取一次锁，内部不再调用其它持锁函数（`save_task` 拆成不持锁的内部写）。进程内用 `Mutex` 串行化线程，进程外用 `fs4` 锁串行化进程，二者组合成 `TaskStoreLock` guard。

### 3. 模块结构

**选择**：`team/` 目录，多文件子模块（对标 s10 `task_system/`、s11 `background_tasks/`，而非 s12 的单文件）。

**原因**：s13 体量约为 s12 的 3–4 倍，单文件会超 800 行；按职责切分更易理解与测试。

```
src/team/
  mod.rs          模块导出 + TeamCtx + AgentKind 集成
  bus.rs          MessageBus（文件收件箱 + Notify 唤醒）
  runtime.rs      TeammateRuntime + spawn_teammate_thread
  protocols.rs    ProtocolState + plan 闸门 + match_response
  assignment.rs   assignment 注册表 + assignment_cwd + 版本失效
  worktree.rs     create/remove/bind（实现阶段化，见 §worktree）
  tools.rs        Lead 团队工具 + SubmitPlanTool
  lock.rs         TaskStoreLock（Mutex + fs4）
```

`lib.rs` 增加 `pub mod team;`。`tools/mod.rs::build_registry()` 注册团队工具。

### 4. Agent 上下文分类：`AgentKind` 枚举

**选择**：把 `Agent::for_subagent: bool` 与 `Tool::available_for_subagent()` 升级为 `AgentKind { Lead, Subagent, Teammate }` + `Tool::available_for(kind)`。

**原因**：
- 现有「二态」`for_subagent` 无法表达第三态「队友」：队友需要 `submit_plan`（队友专属），且**不能**有 `spawn_teammate`/`create_worktree`/`request_plan`/`review_plan`/`request_shutdown`（Lead 专属），也不能有 cron/bg（仅 Lead）。
- 队友若复用 `for_subagent=true` 的工具集，会拿到 cron/bg（过多）且拿不到 `submit_plan`（过少）。故必须有第三态。
- 这是地基代码的「定向改进」——`for_subagent` 不scale 到三态，升级为枚举更干净，符合「顺手改进所在代码」原则。

### 5. owner 与按任务 cwd 注入 `ToolContext`

**选择**：
- `Agent` 新增 `owner: String`（Lead/Subagent = `"agent"`，队友 = 队友名；`"lead"`/`"agent"` 为保留名，禁止作为队友名）。
- `ToolContext` 暴露 `cwd()`：若 `Agent.team` 存在且有活跃 assignment → 返回该 owner 的 assignment cwd；否则返回 `self.workdir`。
- 文件工具（`command`/`read_file`/`write_file`/`edit_file`/`glob`）从 `tools::workdir()` / `safe_path(path)` 改为 `ctx.cwd()` / `safe_path_in(ctx.cwd(), path)`；`command` 的 `current_dir` 用 `ctx.cwd()`。
- `ClaimTaskTool`/`CompleteTaskTool` 从硬编码 `"agent"` 改为读 `ctx.agent.owner`。

**原因**：Python 队友在每次工具调用经 `assignment_cwd(owner)` 动态解析目录，天然支持任务切换。Rust 侧经 `ToolContext::cwd()` 达成同样效果，无需把目录烘焙进 `Agent`。Lead 的 `owner="agent"` 与 s10 既有行为一致（owner 就是 `"agent"`）。

### 6. MessageBus 唤醒机制

**选择**：文件为真相源（`.mailboxes/<name>.jsonl`，追加写、破坏性读），进程内用 `tokio::sync::Notify` 唤醒等待者；`wait_for_messages(timeout)` 用 `tokio::time::timeout(IDLE_SCAN_INTERVAL)`。

**原因**：Python 用 `threading.Condition`。tokio 侧 `Notify` 等价于「有消息即唤醒」。文件格式与 Python 一致，便于观测与重启恢复。队友都在单进程内，跨进程邮箱非必需，但保留文件以匹配参考并支持可观测性。

### 7. Lead 收件箱投递接入 REPL

**选择**：`main.rs` 用 `tokio::select!` 在「stdin 异步读行」与「lead 收件箱 `Notify`」之间竞速；醒来后 `consume_lead_inbox()` → 格式化 `[Team events]` → push user 消息 → `agent.run_loop`。

**原因**：Python 用 `select.select([sys.stdin])` 轮询，在 Windows 上 `select` 对 stdin 不工作。tokio 的 `stdin().read_line` 在内部起阻塞读线程，Windows 可用。

### 8. 错误处理

**选择**：团队工具与存储层用 `Result<T, String>`（与 s10/s11/s12 一致）；`Agent::new` 已返回 `Result`（s13 地基 S8）。MessageBus / 协议状态失败 best-effort 降级，不中断 Lead 主循环；队友致命错误经 `error` 事件回报 Lead 并释放 assignment。

### 9. worktree 实现阶段化

**选择**：设计中**完整覆盖** worktree（create/remove/bind、partial-op 恢复、宿主侧清理、Windows 适配），但**实现计划**中标注为可延后阶段。

**原因**：README 明确「未绑定任务仍使用仓库目录」「worktree 是可选字段」；worktree 在 Windows 上风险最高（路径长度、git junction、跨平台锁）。设计先行可把风险显式化（`fs4` 锁、`dunce` 路径、git 子进程），实现可在团队协作核心稳定后再落地。

## 架构

### 整体流程

```
   用户输入                lead 收件箱
      │                       │
      v                       v
   tokio::select!  ──────>  consume_lead_inbox
      │                       │ (更新协议状态)
      v                       v
   push user msg        push [Team events]
      └──────────┬──────────┘
                 v
          Lead Agent::run_loop
                 │
        spawn_teammate (Lead 工具)
                 │ 认领初始 task + 起线程
                 v
        tokio::spawn(TeammateRuntime::run)
                 │
      ┌──────────┴──────────┐
      v                     v
   WORK (run_loop)        IDLE (wait_for_work)
      │                     │
      │ 工具经 ctx.cwd()     │ 优先读收件箱
      │ + plan 闸门          │ 再扫 ready task
      │                     │ 原子 claim_next_task
      v                     v
   complete_task        claim → WORK
      │
      v
   result + idle_notification → MessageBus → lead 收件箱 → 唤醒 Lead
```

### 核心组件

#### 1. `TeamCtx`（团队共享状态，一个 `Arc`）

Lead 与所有队友共享同一实例：

```rust
pub struct TeamCtx {
    pub bus: MessageBus,
    pub assignments: Mutex<HashMap<String, Assignment>>,   // owner -> assignment
    pub assignment_versions: Mutex<HashMap<String, u32>>,  // 旧审批失效用
    pub pending_requests: Mutex<HashMap<String, ProtocolState>>,
    pub plan_gates: Mutex<HashMap<String, GateStatus>>,
    pub plan_request_ids: Mutex<HashMap<String, String>>,
    pub active_teammates: Mutex<HashMap<String, TeammateStatus>>,
    pub lead_notify: Notify,   // lead 收件箱到达时唤醒 REPL
    pub task_store: Arc<TaskStore>,   // 复用 s10
    pub workdir: PathBuf,
}
```

#### 2. `MessageBus`

```rust
pub struct MessageBus { /* per-agent Notify 或单一 Notify */ }
impl MessageBus {
    pub fn send(&self, from: &str, to: &str, content: &str,
                msg_type: &str, metadata: Option<Value>);
    pub fn read_inbox(&self, agent: &str) -> Vec<MessageRecord>;  // 破坏性读
    pub fn peek(&self, agent: &str) -> bool;
    pub async fn wait_for_messages(&self, agent: &str, timeout: Duration) -> Vec<MessageRecord>;
}
```

每条消息：`{ from, to, content, type, ts, metadata }`，追加写 `.mailboxes/<name>.jsonl`。收件箱文件名校验 `^[A-Za-z0-9_-]{1,64}$`，路径不得越界 `.mailboxes/` 根（复用 `tools::safe_path_in` 风格校验）。

#### 3. `TeammateRuntime`

```rust
pub struct TeammateRuntime {
    name: String,
    agent: Agent,          // child_teammate：共享 infra、kind=Teammate、team=Some(ctx)
    messages: Vec<Message>,
    team: Arc<TeamCtx>,
    // 不持 cron/compactor/memory（与 child_agent 一致）
}
impl TeammateRuntime {
    pub async fn run(&self);          // WORK→IDLE→WORK 直到 shutdown
    async fn work(&self) -> Phase;    // run_loop 一轮到自然停止；发 result/idle
    async fn wait_for_work(&self) -> bool;  // 读收件箱 / claim_next_task
    fn handle_inbox(&self, inbox: Vec<MessageRecord>) -> bool;  // 返回 true=应关机
}
```

- `work()` 调 `agent.run_loop(&mut messages, active_request)`（复用统一循环，跑到模型自然停止）。
- 循环**每轮顶部**：仅 `kind==Teammate` 时，`team.drain_inbox_into(self.owner, messages)` 排空**本队友**收件箱（Lead 的收件箱由 `main.rs` 在 `run_loop` 外经 `consume_lead_inbox` 消费，循环内不重复排空，避免双重消费）。`shutdown_request`→置停止信号；`plan_approval_response`→更新闸门；`plan_request`/直接消息→注入为 user 内容。
- `run_loop` 返回 `Completed` 后：若闸门 `pending` → `waiting_approval`（不发 result）；否则发 `result` + `idle_notification`，`release_completed_assignment`。
- 队友 hook 集与 Lead 不同：无 `TodoReminder`/`Summary`；权限 hook 走**非交互模式**（危险命令/越界路径返回错误，不弹 prompt，因为队友不能读用户输入）。

#### 4. `Assignment` 与 `assignment_cwd`

```rust
pub struct Assignment { pub task_id: String, pub cwd: PathBuf }
pub fn assignment_cwd(team: &TeamCtx, owner: &str) -> Result<PathBuf, String>;
```

- 无 worktree → 返回 `workdir`。
- 有 worktree → 解析 `.worktrees/<name>` 是否在 git worktree 注册表、分支是否 `wt/<name>`、目录是否存在；**绑定损坏则报错（fail-closed，不静默回退仓库目录）**。
- 进程重启后据持久化任务的 `owner + worktree` 重建 assignment；owner 已转新任务则替换旧 lease。

#### 5. `ProtocolState` 与 plan 闸门

```rust
pub enum GateStatus { NotRequired, Required, Pending, Approved, Rejected }
pub struct ProtocolState {
    pub request_id: String,  // req_XXXXXX
    pub type_: ProtocolType,  // Shutdown | PlanApproval
    pub sender: String, pub target: String,
    pub status: ProtocolStatus,  // Pending | Approved | Rejected
    pub payload: String,
    pub work_version: Option<u32>,
    pub task_id: Option<String>,
    pub created_at: f64,
}
```

- `match_response`：按 `request_id` 找原始请求，校验 `type` 配对（shutdown↔shutdown_response；plan_approval↔plan_approval_response）、sender/target 角色、status 仍为 `Pending`；满足则置 `Approved`/`Rejected`，否则忽略。
- 认领/释放任务调 `advance_assignment_version(owner)`：自增版本，把非 `not_required` 的闸门重置为 `required`，清除该 owner 的 `plan_request_id` → **旧审批失效**，防止「换任务后旧批准仍放行写操作」。
- `submit_plan` 记录当前 `task_id + work_version`；`review_plan` 返回时两者仍一致才生效。

## 数据结构变更

### `Task`（s10 扩展）

```rust
pub struct Task {
    pub id: String,
    pub subject: String,
    pub description: String,
    pub status: TaskStatus,
    pub owner: Option<String>,
    pub blocked_by: Vec<String>,
    pub worktree: Option<String>,   // 新增，#[serde(default)] 向后兼容旧 JSON
}
```

### `Agent`（s13 地基扩展）

新增 per-loop 字段（不改共享 infra 字段）：

```rust
pub struct Agent {
    // ... 既有共享 infra + per-loop ...
    pub owner: String,                 // Lead/Subagent="agent"；队友=队友名
    pub kind: AgentKind,               // 替代 for_subagent: bool
    pub team: Option<Arc<TeamCtx>>,    // Lead/队友=Some；s06 subagent=None
}
pub enum AgentKind { Lead, Subagent, Teammate }
```

- `run_loop` 顶部 `if self.kind == Teammate { team.drain_inbox_into(self.owner, messages)... }`（仅队友排空自己收件箱；Lead 的收件箱由 `main.rs` 在循环外消费；subagent 跳过）。
- `run_loop` 内 `if !self.for_subagent { bg.collect }` 改为 `if self.kind == Lead { bg.collect }`（bg 顶部收集仅 Lead；队友/subagent 跳过）。
- `execute_tool` 在 `trigger_pre_tool` 之后、`dispatch` 之前查 plan 闸门：`bash`/`write_file`/`edit_file` 且 `kind==Teammate` 且闸门非 `not_required`/`approved` → 返回 `ToolResult::Denied { reason: "Blocked: plan status is X" }`。
- 新增 `child_teammate(name, role, system, team)`：如 `child_agent` 但 `kind=Teammate`、`owner=name`、`team=Some(team)`、队友 hook 集。
- `ToolContext::cwd()` / `ToolContext::owner()` 访问器。

### `Tool` trait（s13 地基扩展）

```rust
fn available_for(&self, kind: AgentKind) -> bool { true }  // 替代 available_for_subagent()
```

`ToolRegistry`：`definitions_for(kind)` + `dispatch(..., kind)` 派发层二道闸（声明层已过滤，派发层再挡防幻觉）。

## 模块结构

| 路径 | 职责 |
|---|---|
| `src/team/mod.rs` | 模块导出；`TeamCtx`；`AgentKind`；与 `Agent`/`ToolContext` 集成 |
| `src/team/bus.rs` | `MessageBus`：文件收件箱 + `Notify` 唤醒 |
| `src/team/runtime.rs` | `TeammateRuntime` + `spawn_teammate_thread` |
| `src/team/protocols.rs` | `ProtocolState`/`GateStatus`/`match_response`/plan 闸门 |
| `src/team/assignment.rs` | `Assignment` 注册表、`assignment_cwd`、版本失效 |
| `src/team/worktree.rs` | `create_worktree`/`remove_worktree`/`task_worktree_cwd`（实现阶段化） |
| `src/team/tools.rs` | Lead 团队工具 + `SubmitPlanTool` |
| `src/team/lock.rs` | `TaskStoreLock`（`Mutex` + `fs4`） |

## 工具

### Lead 团队工具（`AgentKind::Lead` 可见）

| 工具 | 说明 |
|---|---|
| `spawn_teammate` | 认领初始 task（若给 `task_id`）→ 起 `tokio::task`；`require_plan=true` 则先开门再起线程；失败不启动队友 |
| `list_teammates` | 列出 `active_teammates` 名+状态 |
| `send_message` | Lead → 队友（或 `lead` 协调者） |
| `request_shutdown` | 建 `ProtocolState{type=Shutdown}` pending，发 `shutdown_request` |
| `request_plan` | 置队友闸门 `required`，发 `plan_request` |
| `review_plan` | 校验 `request_id` + `work_version`+`task_id` 一致 → 置 `Approved`/`Rejected`，发 `plan_approval_response` |
| `create_worktree` | Lead 专属；校验后 `git worktree add -b wt/<name> HEAD`，写任务绑定（实现阶段化） |

### 队友工具（`AgentKind::Teammate` 可见）

基础（`bash`/`read_file`/`write_file`/`edit_file`/`glob`，经 `ctx.cwd()`）+ `send_message` + `submit_plan` + `list_tasks` + `claim_task` + `complete_task`。

队友**不可**见：`spawn_teammate`、`create_worktree`、`request_plan`、`review_plan`、`request_shutdown`、`get_task`、`create_task`、cron/bg 工具。

### `submit_plan`

队友专属：校验无 pending plan → 建 `ProtocolState{type=PlanApproval, work_version, task_id}`、置闸门 `pending`、`active=waiting_approval`，发 `plan_approval_request`。

## 协议

### shutdown 握手

```
Lead: request_shutdown(teammate) → pending ProtocolState{Shutdown, sender=lead, target=teammate}
  → bus.send(lead→teammate, shutdown_request, request_id)
队友: handle_inbox 收到 shutdown_request → apply_shutdown_request（校验角色/pending）
  → bus.send(teammate→lead, "Shutdown acknowledged.", shutdown_response, request_id, approve=true)
  → run_loop 退出 → release_teammate_assignment → active 移除
Lead: consume_lead_inbox → match_response（type 配对、角色、pending）→ status=approved
```

### plan 审批

```
spawn_teammate(require_plan=true) → 闸门 required，开门后起线程
队友: submit_plan → 置 pending，记 work_version+task_id → plan_approval_request
Lead: review_plan(request_id, approve, feedback) → 校验 work_version+task_id 仍一致
  → 置 Approved/Rejected → plan_approval_response
队友: handle_inbox → apply_plan_response → 闸门=Approved → 可跑 bash/write/edit
```

闸门为 `Required`/`Pending`/`Rejected` 时：队友可读文件、提交/修改计划，但**不能**跑 `bash`/`write_file`/`edit_file`（`execute_tool` 拦截）。

## 任务认领与 assignment

### 原子认领（`team::claim_task`）

```
TeamStoreLock 持锁 {
  load_task(task_id)
  if status != Pending → 拒绝
  if owner != None → 拒绝
  if owner 已有 in_progress → 拒绝（一次一任务）
  if !can_start → 拒绝（依赖未完成）
  (cwd, err) = task_worktree_cwd(task); if err → 拒绝（fail-closed，不回退仓库）
  task.owner = name; task.status = InProgress; save_task(task)
  assignments[name] = { task_id, cwd }
  advance_assignment_version(name)
}
```

### `complete_task`

`TeamStoreLock` 持锁下校验 `InProgress` + owner + 闸门非 `required/pending/rejected` → 置 `Completed`，**不立即清 assignment**（当前轮后续工具仍用该目录）；队友回 IDLE 时 `release_completed_assignment` 才释放。

### IDLE 发现与认领

`wait_for_work`：`wait_for_messages(IDLE_SCAN_INTERVAL)` → 有消息则处理（shutdown/plan 响应/直接消息）；无消息则 `scan_unclaimed_tasks` → 对每个候选 `claim_task`，首个成功者把 `[Auto-claimed task X]` 注入 messages → 回 WORK。无消息无 ready task 则保持 IDLE。

## worktree（实现阶段化）

`Task.worktree: Option<String>` 可选。无绑定 → 仓库目录。

- `create_worktree(name, task_id)`：Lead 专属。校验：名 `^(?!.*\.\.)[A-Za-z0-9][A-Za-z0-9._-]{0,63}$`、任务 pending+无人认领+未绑定、`git rev-parse` 顶层=workdir、`check-ref-format`、分支 `wt/<name>` 不存在、worktree 路径未注册 → `git worktree add -b wt/<name> <path> HEAD` → 写任务绑定。`git` 失败但已留 checkout/分支/注册 → 报 partial operation，任务保持未绑定，保留产物供人工恢复（不删 git 数据）。
- `remove_worktree(name, discard_changes)`：**宿主/用户侧**，非模型工具。拒绝 pending/in-progress 绑定、拒绝当前轮在用的 lease；`git status --porcelain --ignored` 有改动且未显式 `discard_changes` 则拒绝；`git worktree remove [--force] <path>`；保留 `wt/<name>` 分支；清空任务绑定。
- Windows 适配：路径用 `dunce`（剥 `\\?\` 前缀、展开短名）与 `path-clean` 归一化，与 `tools::safe_path_in` 一致；git 子进程经 `std::process::Command`（无 shell 注入）。
- Worktree 只分开 git 工作目录与分支，**非安全沙箱**：shell 命令仍能访问父进程有权访问的路径。

## Lead 收件箱投递集成

`main.rs` REPL 改为：

```rust
loop {
    tokio::select! {
        line = stdin().read_line(...) => { /* 用户输入 → trigger_prompt → push → run_loop */ }
        _ = agent.lead_notify().notified() => {
            let inbox = consume_lead_inbox(&team);  // 更新协议状态
            if inbox.is_empty() { continue; }
            messages.push(Message{user, [Team events]});
            run_loop(...);
        }
    }
}
```

`consume_lead_inbox`：读+删 lead 收件箱；对 `*_response` 消息调 `match_response` 更新协议状态；返回消息列表。`format_team_events` 拼成 `[Team events]\n[type request_id=X] from: content`。Lead 启动队友后**结束本轮**，不轮询；队友事件到达即自动唤醒下一轮。

## 与现有代码的集成点

| 文件 | 变更 |
|---|---|
| `src/agent.rs` | 加 `owner`/`kind`/`team` 字段；`cwd()`；`child_teammate()`；`run_loop` 顶部按 kind 排空本队友收件箱（仅 Teammate）；`execute_tool` plan 闸门；队友 hook 集；`for_subagent`→`kind`（bg 顶部收集 `kind==Lead`） |
| `src/tools/trait_def.rs` | `ToolContext::cwd()`/`owner()`；`available_for(kind)` |
| `src/tools/registry.rs` | `definitions_for(kind)`/`dispatch(...,kind)` |
| `src/tools/{command,read_file,write_file,edit_file,glob_tool}.rs` | 改用 `ctx.cwd()`/`safe_path_in(ctx.cwd(),..)` |
| `src/task_system/task.rs` | `Task` 加 `worktree` |
| `src/task_system/store.rs` | 暴露加锁变更入口（`with_lock`）；s10 既有无锁读保留 |
| `src/task_system/tools.rs` | `ClaimTaskTool`/`CompleteTaskTool` 读 `ctx.agent.owner`；有 team 时走 `team::claim_task` 路径 |
| `src/tools/mod.rs` | `build_registry()` 注册 8 个团队工具 |
| `src/lib.rs` | `pub mod team;` |
| `src/main.rs` | `tokio::select!` stdin vs lead_notify |
| `Cargo.toml` | 加 `fs4` 依赖 |

## 错误处理

- 存储层 / 团队工具：`Result<T, String>`（与 s10/s11/s12 一致）。
- `Agent::new` 构造 `TeamCtx`/`TaskStoreLock` 失败传播（地基 S8 一致）。
- MessageBus 文件写失败、协议状态不一致：best-effort，打印诊断不中断 Lead。
- 队友 `run` 顶层 `catch`：致命错误经 `bus.send(..., "error")` 回报 Lead，`finally` 释放 assignment（in_progress 任务回 pending），移除 active 记录。

## 边界情况

- **重启恢复**：assignment 据持久化任务 `owner + worktree` 重建；`pending_requests` 为内存态，重启丢失（与 Python 一致）。
- **并发认领**：多队友同时发现同一候选，`TaskStoreLock` 保证只有一个推进到 `InProgress`。
- **worktree 绑定损坏**：认领 fail-closed 报错，不静默回退仓库目录。
- **旧审批复用**：认领/释放任务 `advance_assignment_version` 使旧 `work_version` 的审批失效。
- **队友未认领任务**：文件/shell 工具返回「先认领任务」错误，不回退仓库目录。
- **保留名**：`lead`/`agent` 不可作队友名；队友名大小写不敏感去重。
- **runaway**：队友无 `max_turns`（持久，与 Python 一致）；记为已知风险，可后续加软上限。
- **Lead 工具可见性**：`AgentKind` 声明层 + 派发层双闸防队友幻觉调用 Lead 工具。
- **complete_task 失败**：保留 assignment 目录，便于修正后重试。
- **partial worktree**：`git worktree add` 失败但留产物 → 报告 partial，任务未绑定，git 数据保留。

## 测试策略

### 单元测试（无需 API key）

- `MessageBus`：send→read_inbox 破坏性读、peek、`wait_for_messages` 超时返回空、收件箱名/路径越界拒绝。
- `ProtocolState`/`match_response`：type 不配对忽略、角色不配对忽略、已 `Approved` 重复响应忽略、正确配对置状态。
- plan 闸门：`Required→Pending→Approved/Rejected`；认领/释放后 `advance_assignment_version` 使旧 `work_version` 审批失效；`submit_plan` 重复提交拒绝。
- `assignment_cwd`：无 worktree→仓库目录；worktree 绑定损坏→`Err`（不回退）；重启后据 owner+worktree 重建。
- `task_worktree_cwd` / worktree 名校验（`..`、超长、非法首字符拒绝）。
- `TaskStoreLock`：两并发 `claim_task` 同一任务只有一个成功；不同任务互不阻塞。
- `AgentKind` 工具可见性：`definitions_for(Teammate)` 不含 Lead 工具；`dispatch(..., Teammate)` 对 Lead 工具返回 `Rejected`。
- `Agent::child_teammate`：`kind=Teammate`、`owner=name`、`team=Some`、infra Arc 指针相等。

### 集成测试（`#[ignore]`，需 API key）

- `spawn_teammate(task_id)` → 队友完成 → Lead 收到 `result` + `idle_notification` → 唤醒新一轮。
- `request_shutdown` 握手：队友收到 `shutdown_request` → 回 `shutdown_response` → 退出。
- plan 闸门：`require_plan=true` 时 `bash`/`write_file` 被拦截，提交计划获批后放行。
- 两队友并行认领不同任务；IDLE 队友自动认领新 ready task。
- `create_worktree` 后队友 cwd 为 `.worktrees/<name>`；移除保留 `wt/<name>` 分支。

## 与 Python 参考的差异

| 维度 | Python `code.py` | bytemaker |
|---|---|---|
| 并发 | OS 线程 + 阻塞客户端 + `threading.Lock`/`Condition` | tokio task + `Mutex`/`Notify` |
| 文件锁 | `fcntl.flock`（仅 Unix） | `fs4`（跨平台，win32 可用） |
| REPL 事件 | `select.select([sys.stdin])` | `tokio::select!` stdin vs `Notify` |
| Agent 抽象 | 全局单例 | `Agent` 对象（s13 地基，已落地） |
| 循环复用 | Lead/队友各写循环 | 统一 `run_loop` + `team` ctx 注入 |
| 工具可见性 | `TEAMMATE_TOOLS`/`TEAM_TOOLS` 静态数组 | `AgentKind` + `available_for(kind)` 派发层过滤 |
| 权限 | `check_permission(prompt_user=False)` | 队友 hook 非交互模式 |
| worktree | 全实现 | 设计完整，实现阶段化 |

## 不在范围内

- s11 后台任务、s12 定时任务不带入（README 明确）。
- s14 MCP 工具发现、s15 集成 harness、s16 workflow、s17 goal loop 不在 s13 范围。
- worktree 的**实现**可延后到团队协作核心稳定后的子阶段（设计已覆盖）。
