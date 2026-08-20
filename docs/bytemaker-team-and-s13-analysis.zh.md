# bytemaker team 模块 与 s13 Python 实现 逻辑分析

> 范围：分析 Rust 集成 harness `bytemaker/src/team/`（8 文件 / 2304 行）的团队运行时逻辑，并整理对应教学实现 `s13_agent_teams/code.py`（单文件 / 1794 行）的实现结构。
>
> 两版实现的是同一套"团队运行时 + 协作协议"骨架：Lead 提案→用户确认→spawn 队友→WORK/IDLE 循环→result+idle 双事件经 MessageBus 投递→REPL 唤醒新一轮 Lead→idle 队友优先消费消息、其次原子认领 ready task→类型化协议用 request_id+work_version 保证关机/审批精确生效→可选 worktree 绑定全程 fail-closed。

---

## 一、bytemaker `team` 模块逻辑（Rust）

### 1. 总体架构：一个 `Arc<TeamCtx>` 贯穿全队

team 模块把"一个团队的所有共享状态"塞进一个 `TeamCtx`，用 `Arc` 在 Lead 与各队友线程间共享（`mod.rs:26-35`）：

```rust
pub struct TeamCtx {
    bus: MessageBus,            // 文件收件箱 + Notify
    assignments: AssignmentRegistry, // owner -> {task_id, cwd} + 版本号
    protocols: ProtocolRegistry,      // 计划闸门 + pending 协议请求
    active: Mutex<HashMap<String, TeammateStatus>>, // 活跃队友状态机
    lead_notify: tokio::sync::Notify, // 唤醒 Lead REPL
    task_store: Arc<TaskStore>,  // s10 的任务存储
    workdir: PathBuf,
    lock: TaskStoreLock,         // 进程内 Mutex + 跨进程文件锁
}
```

关键设计：**Lead 与队友不共享同一个 messages 数组**，避免一个队友的工具结果污染另一个队友的推理上下文。通信只能走 `MessageBus`。队友运行时通过 `lead_agent.child_teammate(...)` 构造子 Agent，不直接引用 Lead 的 Agent，避免 `TeamCtx → Agent → TeamCtx` 的 Arc 循环（`runtime.rs:31-33` 注释明确写了这点）。

### 2. 八个文件各司其职

| 文件 | 职责 | 对应的核心抽象 |
|---|---|---|
| `mod.rs` (562) | `TeamCtx` 定义 + 任务认领/完成 + 队友生命周期辅助 + inbox drain + Lead inbox 投递 | `claim_task` / `complete_task` / `drain_inbox` / `consume_lead_inbox` |
| `lock.rs` (72) | 双层任务锁：进程内 `Mutex` + `fs4` 跨进程排他锁 | `TaskStoreLock` |
| `bus.rs` (175) | 文件收件箱 `.mailboxes/<name>.jsonl` + 每 agent 一个 `Notify` | `MessageBus` |
| `assignment.rs` (143) | owner→{task_id,cwd} 注册表 + 每_owner 版本计数器 + cwd 解析 | `AssignmentRegistry` / `assignment_cwd` |
| `protocols.rs` (188) | 计划闸门 `GateStatus` + 类型化协议 `ProtocolState` + 响应匹配 | `ProtocolRegistry` / `match_response` |
| `worktree.rs` (375) | 任务绑定 worktree 的创建/移除/cwd 解析，全部 fail-closed | `create_worktree` / `remove_worktree` / `task_worktree_cwd` |
| `runtime.rs` (247) | 持久队友的 WORK/IDLE 循环 + spawn 逻辑 | `TeammateRuntime` / `spawn_teammate_thread` |
| `tools.rs` (542) | Lead/Teammate 工具的 `Tool` trait 实现 + 协议发起函数 | `SubmitPlanTool` / `SpawnTeammateTool` / `review_plan` 等 |

### 3. 关键流程

#### 3.1 任务认领/完成 —— 原子、带锁、查依赖

`claim_task` (`mod.rs:71-109`) 在 `team.lock` 守卫内完成"读-改-写"：

1. 加 `TaskStoreLock`（进程内 Mutex + fs4 文件锁，`lock.rs:22-29`）
2. 加载任务，校验 `status == Pending` 且 `owner is None`
3. 校验该 owner 没有别的进行中任务（`assignments.get(owner).is_some()` → 拒绝）
4. 校验依赖全部 `Completed`（`incomplete_deps_empty`）
5. 解析 worktree cwd，**绑定损坏直接报错，绝不回退到仓库目录**（`task_worktree_cwd` 返回 `Some(err)` 就失败）
6. 推进 `InProgress` + 写 owner + `save` + 注册 assignment + `advance_version`

`complete_task` (`mod.rs:112-140`) 同样在锁内：校验进行中、校验 owner 一致、**校验计划闸门不阻塞**（`gate.blocks_mutating_tools()` 时拒绝 complete）、置 `Completed`。

并发认领靠"持锁期间读-改-写"保证只有一个赢家，单测 `concurrent_claims_one_winner` (`mod.rs:492-507`) 用三个线程验证。

#### 3.2 队友运行时：WORK / IDLE / Stop 状态机

`TeammateRuntime::run` (`runtime.rs:53-65`) 主循环：

```
Continue → work() → Idle → wait_for_work() → Continue ...
                                  └─ false → Stop
```

- **`work()`** (`runtime.rs:69-105`)：跑一轮模型 `run_loop`。结束后判断：若状态被置为 `Stopping` → `Stop`；若计划闸门 `Pending` → 置 `WaitingApproval` 并 `Idle`（等审批，不报 result）；否则发 `result` + `release_completed_assignment` + 置 `Idle` + 发 `idle_notification`。
- **`wait_for_work()`** (`runtime.rs:109-141`)：先 `bus.wait_for_messages(name, IDLE_SCAN_INTERVAL=2s)` 阻塞等消息；无消息则 `claim_next_task` 自动认领 ready task。这是"消息优先于任务板扫描"的体现。

退出时 `release_teammate_assignment` (`mod.rs:202-218`) 把该队友仍持有的 in-progress 任务退回 `Pending`、清 owner、清 assignment、重置闸门。

#### 3.3 inbox drain + 协议状态推进（队友侧）

`drain_inbox` (`mod.rs:269-308`) 把队友收件箱抽干，按 `msg_type` 分流：

- `shutdown_request` → `apply_shutdown_request` 校验"Lead 发给本队友、pending、未在 stopping"，通过则置 `Stopping` 并回 `shutdown_response`
- `plan_approval_response` → `apply_plan_response` 校验"request_id 是当前 plan、work_version/task_id 一致、pending"，通过则更新闸门为 `Approved`/`Rejected` 并置 `Working`
- `plan_request` → 注入 `[Plan required]` 文本
- 其它 → 普通消息

**work_version 机制** (`mod.rs:344-393`)：认领/释放任务时 `advance_version` 自增；审批响应要求 `state.work_version == 当前版本` 且 `state.task_id == 当前任务`，否则视为"属于更早的 assignment"而忽略。这防止了"队友已换了任务，旧审批才姗姗来迟"的错误生效。

#### 3.4 Lead inbox 投递（REPL 侧）

`consume_lead_inbox` (`mod.rs:400-415`) 读 Lead 收件箱，对 `*_response` 类消息调 `protocols.match_response` 推进协议状态（类型、角色对、未重复解析三重校验，`protocols.rs:79-112`），返回消息列表。`format_team_events` (`mod.rs:418-434`) 把它们渲染成单条 `[Team events]` user 消息注入 Lead 下一轮。

REPL 主循环用 `tokio::select!` 同时等终端输入和 `lead_notify`，消息一到就消费 inbox 并启动新一轮 Lead 调用——Lead 不必轮询 `list_teammates`。

#### 3.5 类型化协议 + 计划闸门

`protocols.rs` 定义两类协议 `Shutdown` / `PlanApproval`，每个 `ProtocolState` 带 `request_id`、`sender`/`target`、`status`、`work_version`、`task_id`。`match_response` 三重校验：响应类型匹配、角色对匹配、状态仍是 pending，防止错配或重复生效。

`GateStatus` (`protocols.rs:6-19`)：`NotRequired`/`Required`/`Pending`/`Approved`/`Rejected`。`blocks_mutating_tools()` 对 `Required|Pending|Rejected` 返回 true——即审批前 bash/write_file/edit_file 被 team 工具层挡住。

#### 3.6 任务绑定 worktree（fail-closed + partial 报告）

`task_worktree_cwd` (`worktree.rs:91-99`)：无绑定 → 仓库目录；有绑定 → `registered_worktree` 校验 git worktree 注册表里存在该路径且分支是 `wt/<name>`，**绑定损坏返回 `(workdir, Some(err))`，调用方据此失败**，绝不悄悄回退。

`create_worktree` (`worktree.rs:103-196`) 校验链很长：name 格式 → 任务 pending 且 unowned 且未绑定 → 该 worktree 未绑别的任务 → 路径不存在 → 是 git 仓库根 → 分支名合法 → 分支不存在 → `git worktree add`。git 失败但已留下 checkout/分支时，报告 `Partial operation` 并保持任务未绑定，保留现场供人工恢复。移除 `remove_worktree` 只能由宿主调用，校验"绑定的任务全 completed + 无 lease + 干净（或显式 discard）"，移除后**保留 `wt/<name>` 分支**。

### 4. 并发模型小结

- 队友是 `tokio::spawn` 的 async task（`runtime.rs:195`）
- team 状态用 `std::sync::Mutex`（**非可重入**，故 `tools.rs:18-20` 有那段"不能在持 pending 锁时调 new_request_id"的注释防死锁）
- 跨进程靠 `fs4` 文件锁
- 唤醒靠 `tokio::sync::Notify`

---

## 二、s13 Python 实现逻辑整理

`s13_agent_teams/code.py` 是**单文件**教学版（1794 行），与 Rust 版概念一一对应，但用 `threading` + `select` 而非 tokio。按代码分区整理如下。

### 1. 文件分区与功能映射

| 行区间 | 分区 | 对应 Rust |
|---|---|---|
| 23-66 | 导入 + 全局状态（WORKDIR、task_lock、teammate_assignments、assignment_versions） | `TeamCtx` 字段 |
| 70-109 | `task_store_lock()` 可重入上下文管理器（RLock + flock） | `lock.rs` TaskStoreLock |
| 92-109 | `advance_assignment_version` | `assignment.rs::advance_version` |
| 112-294 | Task 数据类 + CRUD + `claim_task`/`complete_task` | `mod.rs` claim/complete + s10 task_store |
| 297-574 | worktree 创建/移除/cwd 解析 | `worktree.rs` |
| 579-603 | 系统提示词（含 teams 段：先提案再确认） | `mod.rs::teammate_system_prompt` |
| 606-714 | 基础工具 + agent_cwd 包装 | `tools/` 各 Tool |
| 770-923 | `MessageBus` 类 + 协议状态 + `consume_lead_inbox`/`format_team_events` | `bus.rs` + `protocols.rs` + `mod.rs` 投递 |
| 926-1048 | 计划提交/响应应用、关机应用、teammate send_message | `tools.rs::submit_plan` + `mod.rs::apply_*` |
| 1051-1079 | idle 任务发现 `scan_unclaimed_tasks`/`claim_next_task` | `mod.rs` scan/claim_next |
| 1085-1353 | `TeammateRuntime` 类 + `spawn_teammate_thread` | `runtime.rs` |
| 1356-1435 | Lead 团队工具 run_* 函数 | `tools.rs` Lead tools |
| 1440-1582 | 工具定义 BASE/TASK/TEAMMATE/TEAM_TOOLS + TOOL_HANDLERS | `tools::registry` |
| 1585-1685 | Hooks + 权限检查 + `execute_tool` | s04 hooks + s03 permission |
| 1688-1794 | `agent_loop` + CLI 事件循环 `wait_for_cli_event` | Lead REPL 主循环 |

### 2. 核心机制（与 Rust 对照）

#### 2.1 双层锁 —— RLock + flock，可重入

`task_store_lock()` (`code.py:70-89`) 用 `threading.local()` 的 `depth` 计数实现可重入：最外层拿 flock，内层只计数。Python 用 **RLock（可重入）**，而 Rust 用非可重入 `Mutex`——这是两边最大的实现差异。正因可重入，Python 版 `claim_task` 内可自由调用 `advance_assignment_version`（它再拿 `task_lock` 和 `team_lock`）而不死锁；Rust 版则要靠注释和锁序小心规避。

#### 2.2 MessageBus —— Condition 替代 Notify

`MessageBus` (`code.py:783-840`)：文件收件箱 + `threading.Condition`。`wait_for_messages` 用 `Condition.wait(timeout)` 在 `peek` 为空时阻塞，`send` 后 `notify_all` 唤醒。Rust 版是 `tokio::sync::Notify` + 文件读取循环。两边都是**破坏性读**（读完 `unlink` 收件箱文件），保证 Lead 单消费者。

#### 2.3 队友运行时 WORK/IDLE 循环

`TeammateRuntime` (`code.py:1085-1308`) 与 Rust `TeammateRuntime` 几乎同构：

- `work()` (`code.py:1202-1250`)：先 `handle_inbox`（shutdown 则直接 stop）；调模型；`tool_use` → 跑 `_run_teammate_tool` 并 continue；否则发 `result`、release、置 idle、发 `idle_notification`。
- `wait_for_work()` (`code.py:1252-1276`)：`BUS.wait_for_messages(name, IDLE_SCAN_INTERVAL=2.0)` → 无消息则 `claim_next_task`。
- `run()` (`code.py:1278-1308`)：`finally` 里 `release_teammate_assignment` + 清 `active_teammates/plan_gates/plan_request_ids/teammate_threads`。

队友跑在 `threading.Thread(daemon=True)` (`code.py:1344`)，而非 tokio task。

#### 2.4 计划闸门 + 工具分发层强制

`_run_teammate_tool` (`code.py:969-988`) 是闸门执行点：`bash/write_file/edit_file` 在 `gate not in {"not_required","approved"}` 时直接返回 `Blocked: plan status is {gate}`。对应 Rust `GateStatus::blocks_mutating_tools` + team 工具层检查。

`_teammate_submit_plan` (`code.py:942-966`) 提交计划：记录当前 `work_version` + `task_id`，置 gate=pending，发 `plan_approval_request`。`apply_plan_response` (`code.py:991-1019`) 校验 request_id/work_version/task_id/状态一致后才改 gate。`run_review_plan` (`code.py:1408-1431`) 是 Lead 侧审批，同样校验"属于当前 assignment"。

#### 2.5 Lead REPL 事件循环 —— select 轮询

`wait_for_cli_event` (`code.py:1743-1758`) 是 Python 版的"`tokio::select`"：每 0.25s 用 `select.select([sys.stdin], ...)` 检查终端输入，同时 `BUS.peek("lead")` 检查团队事件。团队事件到达 → 消费 inbox → 注入 `[Team events]` → 新一轮 `agent_loop` (`code.py:1776-1786`)。这正是 README 第 4 节"收件箱事件由运行时投递"的实现。

#### 2.6 worktree 创建/移除 —— 与 Rust 完全同构的校验链

`create_worktree` (`code.py:445-521`) 校验链与 Rust 版逐项对应：name → task pending/unowned/未绑定 → 未绑别的任务 → 路径不存在 → git 仓库根 → 分支名 → 分支不存在 → 注册表无冲突 → `git worktree add`。失败时同样报告 `Partial operation` 并保留现场。`remove_worktree` (`code.py:524-574`) 校验"绑定任务全 completed + 无 lease + 干净或显式 discard"，移除后保留分支。

#### 2.7 work_version 与 fail-closed cwd

`assignment_cwd` (`code.py:390-410`) 同样 fail-closed：无 assignment → 仓库目录；assignment 的任务非 in_progress/completed 或 owner 不符 → `raise ValueError`；cwd 与记录不一致 → 报错。绝不悄悄回退。`advance_assignment_version` (`code.py:92-109`) 自增版本并把非 not_required 的 gate 降回 required（即认领新任务让旧审批失效）。

### 3. 两版差异速览

| 维度 | Rust (bytemaker) | Python (s13) |
|---|---|---|
| 并发单元 | `tokio::spawn` async task | `threading.Thread(daemon)` |
| 唤醒 Lead | `tokio::sync::Notify` | `BUS.peek` 轮询 + `select` |
| 收件箱唤醒 | `Notify::notified()` | `Condition.wait/notify_all` |
| 任务锁 | 非可重入 `Mutex` + fs4 | **可重入 RLock** + flock |
| 锁序规避 | 显式注释（`tools.rs:18-20`）防死锁 | RLock 可重入，无需规避 |
| 协议匹配 | `match_response` 三重校验 | `match_response` + `apply_*` 双重 |
| 工具可见性 | `available_for(AgentKind)` trait 方法 | 拆分 `TEAMMATE_TOOLS`/`TEAM_TOOLS` 列表 |
| 文件组织 | 8 文件分模块 | 单文件分区 |

### 4. 一句话总结两版共同骨架

> **Lead 提案→用户确认→spawn 队友（可选先认领任务/开闸门）→ 队友 WORK/IDLE 循环 → result+idle 双事件经 MessageBus 投递 → REPL 唤醒新一轮 Lead → idle 队友优先消费消息、其次原子认领 ready task → 类型化协议用 request_id+work_version 保证关机/审批精确生效 → 可选 worktree 绑定全程 fail-closed。**

两边实现的是同一套"团队运行时 + 协作协议"，Rust 版是工程化集成 harness，Python 版是教学单文件，逻辑完全对齐。
