# bytemaker 技术规格文档

> **版本**: 0.1.0  
> **语言**: Rust (2021 edition)  
> **运行时**: Tokio async  
> **对应教学系列**: s04–s13 集成实现

---

## 1. 概述

bytemaker 是一个基于 Anthropic Claude API 的**集成式 AI Agent Harness**，将 s04–s13 各章节的独立机制统一在一个 Rust 生产运行时中。它提供：

- **基础工具**：bash、文件读写、编辑、glob
- **Hooks 系统**：四节点生命周期回调（s04）
- **Skill 加载**：按需注入技能文档（s07）
- **上下文压缩**：四步压缩管线 + 反应式压缩（s08）
- **跨会话记忆**：提取/召回/整理/索引（s09）
- **任务队列**：创建/认领/完成/依赖图（s10）
- **后台任务**：独立进程 + poll/stop（s11）
- **定时任务**：cron 表达式调度（s12）
- **Agent Teams**：Lead + Teammate 多 agent 协作（s13）
- **Subagent**：上下文隔离的子任务委派（s06）

**本质**：把 Python 教学代码（s01–s17）中的机制用 Rust 重新实现，生产可用，共享同一架构。

---

## 2. 架构总览

```
用户输入 (stdin / team inbox)
  ↓
┌─────────────────────────────────────────────────────────────┐
│ main.rs (REPL)                                              │
│  ├─ 读 env、构造 AgentConfig                                 │
│  ├─ tokio::select! { stdin | team_notify }                  │
│  └─ agent.run_loop(&mut messages, query)                    │
└─────────────────────────────────────────────────────────────┘
  ↓
┌─────────────────────────────────────────────────────────────┐
│ Agent (agent.rs) — 核心循环                                  │
│                                                             │
│  1. 记忆召回 (memory.load_memories)                          │
│  2. 上下文压缩 (compactor.prepare)                           │
│  3. 组装 system prompt (base + skills + memory)              │
│  4. API 调用 (client.stream_messages)                        │
│  5. 判断 stop_reason                                         │
│     ├─ end_turn → Stop hooks → 提取记忆 → 返回              │
│     └─ tool_use → execute_tool_use_blocks → 继续循环         │
│                                                             │
│  共享基础设施 (Arc):                                         │
│    client, registry, skills, task_store, bg_manager,        │
│    todo_manager, coordinator                                │
│                                                             │
│  隔离状态:                                                   │
│    compactor, memory, hooks, kind, max_turns                │
└─────────────────────────────────────────────────────────────┘
  ↓                          ↑
  ↓ tool_use                 ↑ tool_result
  ↓                          ↑
┌─────────────────────────────────────────────────────────────┐
│ Tool System (tools/)                                        │
│  ├─ Registry: name → Box<dyn Tool>                         │
│  ├─ dispatch: available_for(kind) + check_permission        │
│  └─ execute: ToolContext + input → String                  │
│                                                             │
│  工具列表 (23 个):                                           │
│    基础: command, read_file, write_file, edit_file, glob    │
│    s06:  task (Lead-only)                                   │
│    s07:  load_skill                                         │
│    s10:  create_task, list_tasks, get_task, claim_task,     │
│          complete_task                                       │
│    s11:  task_output, task_stop                             │
│    s12:  schedule_cron, list_crons, cancel_cron             │
│    s13:  spawn_teammate, list_teammates, send_message,      │
│          request_shutdown, request_plan, review_plan,       │
│          submit_plan, create_worktree                        │
│    todo: todo_write                                         │
└─────────────────────────────────────────────────────────────┘
```

---

## 3. 核心组件

### 3.1 Agent (agent.rs)

**职责**：持有所有基础设施，驱动主循环。

```rust
pub struct Agent {
    // 共享基础设施 (Arc clone 给子 agent)
    pub(crate) client: Arc<Client>,
    pub(crate) registry: Arc<ToolRegistry>,
    pub(crate) skills: Arc<SkillLoader>,
    pub(crate) task_store: Arc<TaskStore>,
    pub(crate) bg_manager: Arc<BackgroundManager>,
    pub(crate) todo_manager: Arc<TodoManager>,
    pub(crate) coordinator: Arc<Mutex<Coordinator<CrosstermBackend>>>,
    
    // 隔离状态
    pub compactor: ContextCompactor,
    pub memory: MemoryStore,
    pub hooks: Hooks,
    pub kind: AgentKind,
    pub max_turns: usize,
    pub base_system: String,
    pub workdir: PathBuf,
    
    // Team (s13)
    pub team: Option<Arc<TeamCtx>>,
    pub team_input_sender: Option<mpsc::Sender<String>>,
    
    // Cron (s12)
    pub cron_manager: Option<Arc<CronManager>>,
}
```

**关键方法**：
- `new(cfg)` — 构造主 agent，初始化所有子系统
- `run_loop(&mut messages, query)` — 核心循环，直到 `end_turn` 或 `max_turns`
- `child_agent(max_turns, sub_system)` — 产出子 agent（s06）
- `teammate_agent(max_turns, sub_system, name)` — 产出 teammate（s13）
- `run_subagent(prompt, max_turns)` — 子 agent 入口（s06）
- `execute_tool_use_blocks(content)` — 执行一轮工具调用
- `build_hooks(bg_manager, todo_manager)` — 装配默认 hook 集

**AgentKind 三态**：

```rust
pub enum AgentKind {
    Lead,      // 主 agent，拥有所有工具
    Subagent,  // 子 agent，无 task/spawn_teammate
    Teammate,  // 队友，无 task/spawn_teammate/schedule_cron
}
```

---

### 3.2 Client (client.rs)

**职责**：封装 Anthropic API 交互，流式解析 SSE。

```rust
pub struct Client {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
}

pub enum CallResult {
    Success(MessagesResponse),
    PromptTooLong(AgentError),  // 上下文超限，调用方可压缩后重试
    Failure(AgentError),
    Cancelled,
}

pub enum Delta {
    Text(String),
    ToolUseStart { id: String, name: String, input: Value },
}
```

**流式解析**：
- `stream_messages()` — 流式调用 `/v1/messages`，累加 text 和 tool_use
- `DeltaSink` — 增量回调，实时渲染文本和工具调用
- `CallResult::PromptTooLong` — 区分上下文超限，触发反应式压缩

---

### 3.3 Tool System (tools/)

**职责**：工具注册、分发、权限检查、执行。

#### ToolRegistry (registry.rs)

```rust
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn register(&mut self, tool: Box<dyn Tool>);
    pub fn definitions_for(&self, kind: AgentKind) -> Vec<ToolDefinition>;
    pub async fn dispatch(&self, ctx: &ToolContext, name: &str, input: &Value) -> Result<String, AgentError>;
}
```

**关键行为**：
- `definitions_for(kind)` — 按 AgentKind 过滤工具（`available_for()`）
- `dispatch()` — 查找工具 → 检查权限 → 执行 → 返回结果
- 子 agent 调用 `task` 会被 `ToolRejected`（Lead-only）

#### Tool trait (trait_def.rs)

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> Value;
    fn check_permission(&self, input: &Value) -> PermissionCheck;
    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String;
    fn available_for(&self, kind: AgentKind) -> bool { true }  // 默认全可见
}

pub struct ToolContext<'a> {
    pub agent: &'a Agent,
    pub cancel: CancellationToken,
}
```

**PermissionCheck 三态**：
```rust
pub enum PermissionCheck {
    Pass,                    // 自动放行
    NeedsApproval(String),   // 需用户确认
    Deny(String),            // 直接拒绝
}
```

#### 工具可见性矩阵

| Tool | Lead | Subagent | Teammate |
|------|------|----------|----------|
| command, read_file, write_file, edit_file, glob | ✅ | ✅ | ✅ |
| todo_write | ✅ | ✅ | ✅ |
| load_skill | ✅ | ✅ | ✅ |
| create_task, list_tasks, get_task | ✅ | ✅ | ✅ |
| claim_task, complete_task | ✅ | ✅ | ✅ |
| task_output, task_stop | ✅ | ✅ | ❌ |
| task (subagent) | ✅ | ❌ | ❌ |
| spawn_teammate, list_teammates | ✅ | ❌ | ❌ |
| send_message, request_shutdown | ✅ | ❌ | ❌ |
| request_plan, review_plan | ✅ | ❌ | ❌ |
| create_worktree | ✅ | ❌ | ❌ |
| submit_plan | ❌ | ❌ | ✅ |
| schedule_cron, list_crons, cancel_cron | ✅ | ❌ | ❌ |

---

### 3.4 Hooks System (hooks.rs)

**职责**：四节点生命周期回调，扩展点。

```rust
pub struct Hooks {
    user_prompt: Vec<Box<dyn PromptHook>>,
    pre_tool: Vec<Box<dyn PreToolHook>>,
    post_tool: Vec<Box<dyn PostToolHook>>,
    stop: Vec<Box<dyn StopHook>>,
}
```

**四节点语义**：

| 事件 | 触发时机 | 返回值语义 |
|------|---------|-----------|
| `UserPromptSubmit` | 用户输入后、进入 LLM 前 | 无控制流 |
| `PreToolUse` | 工具执行前 | `Some(reason)` → 阻止工具，reason 作为 tool_result |
| `PostToolUse` | 工具执行后 | `Some(msg)` → 作为独立 user 消息注入 |
| `Stop` | 循环即将退出时 | `Some(msg)` → 注入 msg 并继续循环 |

**默认 Hook 集**（`Agent::build_hooks`）：

1. **PermissionHook** — PreToolUse：deny list、destructive 确认、路径越界检查、MCP 策略
2. **LogHook** — PreToolUse：记录工具调用
3. **LargeOutputHook** — PostToolUse：大输出警告
4. **BackgroundStopHook** — Stop：后台任务完成通知
5. **TodoReminderHook** — PostToolUse：TODO 提醒

**权限查询**：
```rust
pub struct PermissionQuery {
    pub reason: String,
    pub name: String,
    pub input: Value,
    pub reply: oneshot::Sender<bool>,
}
```
Hook 发往 InputTask（main.rs 的 stdin 循环），用户 y/N 后经 oneshot 回答。

---

### 3.5 Context Compaction (compact.rs)

**职责**：控制上下文增长，四步管线 + 反应式压缩。

```rust
pub struct ContextCompactor {
    transcript_dir: PathBuf,
    tool_results_dir: PathBuf,
}
```

**四步管线**（`prepare()` 每次 API 调用前运行）：

```
1. tool_result_budget  → 大结果落盘（>30k chars），留路径+预览
2. snip_compact        → 消息数 >50 时，归档中间到 transcript.jsonl
3. micro_compact       → 旧 tool_result 替换为占位符（保留最近 3 条）
4. compact_history     → 字符数 >50k 时，LLM 生成事实摘要
```

**阈值常量**：

| 常量 | 值 | 单位 |
|------|-----|------|
| `CONTEXT_CHAR_LIMIT` | 50,000 | 字符 |
| `TOOL_RESULT_BATCH_CHAR_LIMIT` | 200,000 | 字符 |
| `LARGE_RESULT_CHAR_LIMIT` | 30,000 | 字符 |
| `SUMMARY_INPUT_CHAR_LIMIT` | 80,000 | 字符 |
| `KEEP_RECENT_RESULTS` | 3 | 条 |
| `KEEP_RECENT_MESSAGES` | 5 | 条 |
| `SNIP_MAX_MESSAGES` | 50 | 条 |

**反应式压缩**（`reactive_compact`）：
- API 返回 `prompt_too_long` 时触发
- 保留最近 5 条消息（配对保护），摘要更早历史
- 最多重试 1 次（`MAX_REACTIVE_RETRIES`）

**切点保护**：
- tool_use 和 tool_result 必须配对
- snip 时若切在 tool_use 后，向后吞掉 tool_result
- 若切在 tool_result 前，向前借 tool_use

---

### 3.6 Memory (memory.rs)

**职责**：跨会话记忆，四子系统（存储/召回/提取/整理）。

```rust
pub struct MemoryStore {
    memory_dir: PathBuf,
    read_only: bool,  // 子 agent / teammate 用只读模式
}
```

**四子系统**：

| 子系统 | 方法 | 触发时机 | 行为 |
|--------|------|---------|------|
| 召回 | `load_memories()` | 每个请求开始 | 选 ≤5 条相关记忆，加载正文到 system prompt |
| 提取 | `extract_memories()` | 回合结束（Stop hook） | 从对话提取持久知识，写盘 |
| 整理 | `consolidate_memories()` | 提取后（≥10 条时） | LLM 合并去重，快照+恢复 |
| 索引 | `rebuild_memory_index()` | 每次写盘后 | 重建 MEMORY.md |

**记忆类型**：
```rust
const MEMORY_TYPES: &[&str] = &["user", "feedback", "project", "reference"];
```

**召回策略**：
- 首选：LLM 选择（给模型 catalog，返回 indices）
- 降级：关键词匹配（分词 + 命中数排序）

**存储过滤**：
- scope 必须是 `persistent`（不是 `current_task`）
- 无临时标记（"this session"、"本次会话" 等）
- 不与现有记录重复（slug / description / body）

**只读模式**：
- `new_read_only()` — 可召回但不写盘
- Subagent / Teammate 用此共享 Lead 的知识库

---

### 3.7 Skills (skills.rs)

**职责**：按需加载技能文档，避免 system prompt 堆文档。

```rust
pub struct SkillLoader {
    skills: BTreeMap<String, Skill>,  // name → Skill
}

pub struct Skill {
    pub name: String,
    pub description: String,
    pub content: String,  // 完整 SKILL.md
}
```

**两阶段加载**：

| 阶段 | 内容 | 进入模型的位置 | 何时加入 |
|------|------|---------------|---------|
| 启动扫描 | name + description | system prompt catalog | 启动时 |
| `load_skill(name)` | 完整 SKILL.md | tool_result | 模型调用时 |

**扫描规则**：
- 只扫 `skills_dir/*/SKILL.md`（一层直接子目录）
- 不递归（`references/`、`scripts/` 不被误收）
- YAML frontmatter 容错（缺失/格式错误回退到目录名 + 首行）

---

### 3.8 Task System (task_system/)

**职责**：任务队列，支持依赖图和 owner 认领。

```rust
pub struct TaskStore {
    tasks_dir: PathBuf,
}

pub struct Task {
    pub id: String,
    pub subject: String,
    pub description: String,
    pub status: TaskStatus,  // Pending | InProgress | Completed
    pub owner: Option<String>,
    pub blocked_by: Vec<String>,
    pub worktree: Option<PathBuf>,
}
```

**工具**：
- `create_task` — 创建任务，支持 `blockedBy` 依赖
- `list_tasks` — 列出所有任务及状态
- `get_task` — 按 ID 获取详情
- `claim_task` — 认领 Pending 任务（设 owner + InProgress）
- `complete_task` — 完成已认领任务，报告新解锁任务

**持久化**：`.tasks/{id}.json`

---

### 3.9 Background Tasks (background_tasks/)

**职责**：长时间后台进程管理。

```rust
pub struct BackgroundManager {
    output_dir: PathBuf,
    tasks: Mutex<HashMap<String, BackgroundTask>>,
}
```

**工具**：
- `task_output` — poll/block 取后台任务输出
- `task_stop` — 取消任务并 kill 进程树

**主动唤醒**（`BackgroundStopHook`）：
- Stop hook 检查是否有已完成的后台任务
- 有则返回通知，强制循环继续

---

### 3.10 Cron Scheduler (cron_scheduler.rs)

**职责**：定时任务调度。

```rust
pub struct CronManager {
    workdir: PathBuf,
    scheduler: Option<JoinHandle<()>>,
}
```

**工具**：
- `schedule_cron` — 创建定时任务（cron 表达式 + prompt）
- `list_crons` — 列出所有定时任务
- `cancel_cron` — 取消定时任务

**持久化**：`.scheduled_tasks.json`

**限制**：
- Lead-only（Subagent / Teammate 不可用）
- 子 agent 的 `cron_manager` 置 `None`

---

### 3.11 Agent Teams (team/)

**职责**：Lead + Teammate 多 agent 协作。

```rust
pub struct TeamCtx {
    pub workdir: PathBuf,
    pub task_store: Arc<TaskStore>,
    pub inbox: Mutex<Vec<TeamEvent>>,
    pub teammates: Mutex<HashMap<String, TeammateState>>,
}
```

**Teammate 生命周期**：
1. Lead 调用 `spawn_teammate` 创建 teammate
2. Teammate 独立运行，通过 `send_message` 向 Lead 报告
3. Lead 的 main.rs 监听 `lead_notify`，收到事件后唤醒循环
4. Lead 可调用 `request_shutdown` 终止 teammate

**工具**：
- `spawn_teammate` — 创建 teammate（Lead-only）
- `list_teammates` — 列出所有 teammate（Lead-only）
- `send_message` — 向 Lead 或 teammate 发消息
- `request_shutdown` — 请求 teammate 关闭（Lead-only）
- `request_plan` / `review_plan` / `submit_plan` — 计划协作（Lead/Teammate 分工）
- `create_worktree` — 为 teammate 创建 git worktree（Lead-only）

**Worktree 隔离**：
- 每个 teammate 可分配独立 git worktree
- Teammate 的 `cwd` 指向 worktree，避免文件冲突

---

### 3.12 Subagent (agent.rs:run_subagent)

**职责**：上下文隔离的子任务委派。

**启动流程**：
```
TaskTool.execute()
  → ctx.agent.run_subagent(prompt, max_turns)
    → child = child_agent(max_turns, SUB_SYSTEM)
    → child.run_loop(&mut messages, prompt)
    → extract_final_text(messages)
    → 返回文本作为 tool_result
```

**隔离边界**：

| 方面 | 共享 (Arc clone) | 隔离 (新实例) | 置空 |
|------|-----------------|--------------|------|
| client | ✅ | | |
| registry | ✅ | | |
| skills | ✅ | | |
| task_store | ✅ | | |
| bg_manager | ✅ | | |
| coordinator | ✅ | | |
| compactor | | ✅ (`.subagents/<id>/`) | |
| memory | | ✅ (read_only) | |
| hooks | | ✅ (全新) | |
| cron_manager | | | ✅ None |
| team | | | ✅ None |

**递归防护**：
- `TaskTool.available_for()` 只对 `AgentKind::Lead` 返回 true
- Subagent 调用 `task` 会被 `ToolRejected`

---

## 4. 数据流

### 4.1 单次请求完整流程

```
用户输入 "帮我重构 auth 模块"
  ↓
main.rs: messages.push(Message::user_text(query))
  ↓
Agent.run_loop(&mut messages, query)
  ↓
1. memory.load_memories(client, messages)
   → 召回相关记忆（如 "用户偏好 tabs"）
   ↓
2. compactor.prepare(client, messages, query)
   → tool_result_budget → snip_compact → micro_compact
   → 若 >50k chars → compact_history (LLM 摘要)
   ↓
3. 组装 system prompt
   → base_system + skills.catalog() + memory.build_system()
   ↓
4. client.stream_messages(system, messages, tools, max_tokens, delta_sink)
   → 流式 SSE 解析，实时渲染文本
   ↓
5. 判断 stop_reason
   ├─ "end_turn" → Stop hooks → memory.extract_memories → 返回
   └─ "tool_use" → execute_tool_use_blocks
      ↓
      for each tool_use block:
        → hooks.trigger_pre_tool (权限检查)
          ├─ Some(reason) → 工具被阻止，reason 作为 tool_result
          └─ None → registry.dispatch(ctx, name, input)
             → tool.execute(ctx, input) → String
        → hooks.trigger_post_tool (大输出警告 / TODO 提醒)
        → 追加 tool_result 到 messages
      ↓
      回到步骤 1 (继续循环)
```

### 4.2 Subagent 调用流程

```
父 agent 收到 tool_use: {name: "task", input: {prompt: "..."}}
  ↓
TaskTool.execute(ctx, input)
  ↓
ctx.agent.run_subagent(prompt, max_turns)
  ↓
child = child_agent(max_turns, SUB_SYSTEM)
  → Arc clone 共享基础设施
  → 新 compactor (.subagents/<id>/)
  → 新 memory (read_only)
  → 新 hooks
  → kind = Subagent, max_turns = 30
  ↓
child.run_loop(&mut messages, prompt)
  → 独立循环，最多 30 轮
  → 工具调用走 registry.dispatch (权限检查照常)
  ↓
extract_final_text(messages)
  → 从最后一条 assistant message 提取 Text block
  ↓
返回 "Task completed:\n\n<final text>"
  ↓
作为 tool_result 追加到父 messages
  → 子 messages 丢弃
```

---

## 5. 错误处理

### 5.1 AgentError (error.rs)

```rust
pub enum AgentError {
    Api { status: u16, body: String },      // HTTP 错误
    Network(String),                         // 网络/传输错误
    Stream(String),                          // SSE 流错误
    Timeout { seconds: u64 },                // 超时
    InvalidResponse(String),                 // 响应格式错误
    ToolNotFound { name, available },        // 工具不存在
    ToolRejected { name, reason },           // 工具被拒绝（如 subagent 调 task）
    ToolDenied { name, reason },             // 权限拒绝
    ToolExecution { name, reason },          // 工具执行失败
    PathTraversal { path },                  // 路径越界
    FileSystem(String),                      // 文件系统错误
    Validation(String),                      // 输入校验失败
    Other(String),                           // 其他错误
}
```

**关键方法**：
- `is_prompt_too_long()` — 检测上下文超限（`prompt_too_long` / `too many tokens` / `request_too_large`）
- `From<io::Error>` / `From<reqwest::Error>` / `From<serde_json::Error>` — 自动转换

### 5.2 错误恢复策略

| 错误类型 | 恢复策略 |
|---------|---------|
| `prompt_too_long` | 反应式压缩（`reactive_compact`），最多重试 1 次 |
| 工具执行失败 | 返回错误字符串作为 tool_result，模型可在下一轮修正 |
| 网络错误 | 终止循环，返回错误 |
| Memory LLM 失败 | 降级关键词匹配 / 吞错返回 0，不中断主循环 |
| Consolidate 失败 | 快照恢复，返回 0 |

---

## 6. 配置与环境

### 6.1 环境变量

| 变量 | 必需 | 默认值 | 说明 |
|------|------|--------|------|
| `ANTHROPIC_API_KEY` 或 `ANTHROPIC_AUTH_TOKEN` | ✅ | | API 密钥 |
| `ANTHROPIC_BASE_URL` | ❌ | `https://api.anthropic.com` | API 基础 URL |
| `MODEL_ID` | ✅ | | 模型 ID（如 `claude-3-5-sonnet-20241022`） |
| `SKILLS_DIR` | ❌ | `{cwd}/skills` | 技能目录 |
| `RUST_LOG` | ❌ | `info` | 日志级别（`warn` / `debug`） |

### 6.2 AgentConfig

```rust
pub struct AgentConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub workdir: PathBuf,
    pub skills_dir: PathBuf,
    pub coordinator: Arc<Mutex<Coordinator<CrosstermBackend>>>,
    pub team_input_sender: Option<mpsc::Sender<String>>,
}
```

---

## 7. 文件系统布局

### 7.1 工作目录结构

```
{workdir}/
├── .tasks/                    # s10: 任务队列
│   └── {id}.json
├── .task_outputs/             # s11: 后台任务输出
│   ├── background/
│   └── tool-results/
├── .transcripts/              # s08: 压缩归档
│   └── transcript.jsonl
├── .memory/                   # s09: 跨会话记忆
│   ├── MEMORY.md              # 索引
│   └── {slug}.md              # 记忆记录
├── .scheduled_tasks.json      # s12: 定时任务
├── .subagents/                # s06: 子 agent 隔离目录
│   └── subagent_{id}/
│       ├── .transcripts/
│       └── .task_outputs/
└── .teammates/                # s13: teammate 隔离目录
    └── {name}/
        ├── .transcripts/
        └── .task_outputs/
```

### 7.2 源码结构

```
bytemaker/src/
├── main.rs                    # REPL 入口
├── lib.rs                     # 模块声明
├── agent.rs                   # Agent 核心循环
├── client.rs                  # Anthropic API 客户端
├── error.rs                   # 统一错误类型
├── hooks.rs                   # Hook 系统
├── compact.rs                 # 上下文压缩
├── memory.rs                  # 跨会话记忆
├── skills.rs                  # 技能加载
├── output.rs                  # 终端输出渲染
├── builtins.rs                # 内置 Hook 实现
├── todo.rs                    # TODO 管理
├── cron_scheduler.rs          # 定时任务
├── tools/
│   ├── mod.rs                 # 工具注册表构建
│   ├── trait_def.rs           # Tool trait + AgentKind
│   ├── registry.rs            # ToolRegistry 实现
│   ├── command.rs             # bash 工具
│   ├── read_file.rs           # 读文件
│   ├── write_file.rs          # 写文件
│   ├── edit_file.rs           # 编辑文件
│   ├── glob_tool.rs           # glob 工具
│   ├── load_skill.rs          # 技能加载工具
│   ├── todo_write.rs          # TODO 写入工具
│   └── task.rs                # Subagent 委派工具
├── task_system/
│   ├── mod.rs                 # 模块导出
│   ├── task.rs                # Task 数据结构
│   ├── store.rs               # TaskStore 持久化
│   └── tools.rs               # 任务工具实现
├── background_tasks/
│   ├── mod.rs                 # 模块导出
│   ├── task.rs                # BackgroundTask 结构
│   ├── manager.rs             # BackgroundManager
│   └── tools.rs               # 后台任务工具 + StopHook
├── team/
│   ├── mod.rs                 # 模块导出
│   ├── bus.rs                 # 消息总线
│   ├── runtime.rs             # Teammate 运行时
│   ├── assignment.rs          # 任务分配
│   ├── lock.rs                # 文件锁
│   ├── protocols.rs           # 协议定义
│   ├── worktree.rs            # Git worktree 管理
│   └── tools.rs               # Team 工具实现
└── render/
    └── mod.rs                 # 终端渲染（crossterm）
```

---

## 8. 依赖

### 8.1 核心依赖

| crate | 版本 | 用途 |
|-------|------|------|
| `tokio` | 1.0 | async 运行时 |
| `reqwest` | 0.12 | HTTP 客户端 + SSE stream |
| `serde` / `serde_json` / `serde_yaml` | 1.0 | 序列化 |
| `async-trait` | 0.1 | async trait 支持 |
| `thiserror` | 2 | 错误类型派生 |
| `futures-util` | 0.3 | Stream 扩展 |
| `eventsource-stream` | 0.2 | SSE 协议解析 |

### 8.2 工具依赖

| crate | 版本 | 用途 |
|-------|------|------|
| `glob` | 0.3 | 文件模式匹配 |
| `path-clean` | 1 | 路径归一化 |
| `dunce` | 1 | 路径 canonicalize（Windows 兼容） |
| `regex` | 1 | 正则表达式 |
| `croner` | 3 | cron 表达式解析 |

### 8.3 终端依赖

| crate | 版本 | 用途 |
|-------|------|------|
| `crossterm` | 0.29 | 终端控制 |
| `reedline` | 0.50 | 命令行编辑 |
| `colored` | 3 | 终端着色 |

### 8.4 其他

| crate | 版本 | 用途 |
|-------|------|------|
| `chrono` | 0.4 | 时间处理 |
| `fastrand` | 2.1 | 随机数（subagent ID） |
| `fs4` | 0.9 | 文件锁 |
| `tracing` | 0.1 | 结构化日志 |
| `dotenv` | 0.15 | .env 加载 |

---

## 9. 设计决策

### 9.1 为什么用 Rust 而非 Python？

**决策**：生产实现用 Rust。

**原因**：
- 性能：异步运行时 + 零成本抽象，适合长时间运行的 agent
- 类型安全：编译期检查，减少运行时错误
- 内存安全：无 GC 停顿，适合实时交互
- 生态：reqwest + tokio 提供成熟的 async HTTP

### 9.2 为什么 Agent 持有所有基础设施？

**决策**：`Agent` struct 持有 `Arc<Client>`、`Arc<ToolRegistry>` 等。

**原因**：
- 子 agent 通过 `Arc clone` 共享基础设施，无需全局变量
- `ToolContext<'a>` 借用 Agent，工具可访问所有子系统
- 避免 `Rc<RefCell<>>` 或 `static mut` 的复杂性

### 9.3 为什么用 trait object 而非 enum？

**决策**：`Box<dyn Tool>` 而非 `enum ToolKind`。

**原因**：
- 工具可独立实现，无需修改中央 enum
- 新工具只需 `impl Tool` + `registry.register()`
- 代价：一次堆分配 + 动态分发（可接受）

### 9.4 为什么 Hook 用 trait 而非闭包？

**决策**：`Box<dyn PreToolHook>` 而非 `Box<dyn Fn(...)>`。

**原因**：
- Hook 可携带状态（如 `TodoReminderHook` 的计数器）
- `Send + Sync` 超trait 保证跨 async 边界安全
- 测试可注入 mock hook（如 `AlwaysBlock`、`NeverBlock`）

### 9.5 为什么 Memory 用文件系统而非数据库？

**决策**：`.memory/*.md` + `MEMORY.md` 索引。

**原因**：
- 人类可读，可直接用编辑器修改
- 可 git 跟踪，版本控制
- 与 Claude Code 的 memory 系统兼容
- 无需额外依赖（SQLite / Redis）

---

## 10. 限制与未来工作

### 10.1 当前限制

1. **无 MCP 支持**：s14 的 MCP 工具发现未集成
2. **无 streaming tool**：工具调用同步返回，无法流式输出
3. **无 tool 并发**：同一轮工具调用串行执行
4. **无 tool 超时**：工具执行无时间限制（除 bash 的 120s）
5. **无 tool 重试**：工具失败后不自动重试
6. **单用户**：无多用户隔离

### 10.2 未来工作

- 集成 s14 MCP 工具发现
- 集成 s15 Integrated Harness（合并所有子系统）
- 支持 s16 Workflow Runtime（多阶段工作流）
- 支持 s17 Evaluation（自动评估）
- 工具并发执行（独立工具并行）
- 工具超时和重试机制
- Web UI / API 接口

---

## 11. 测试

### 11.1 测试策略

- **单元测试**：每个模块的纯函数（如 `memory_slug`、`parse_frontmatter`）
- **集成测试**：`cargo test` 跑所有非 ignored 测试
- **烟雾测试**：`cargo test -- --ignored` 跑需 API key 的测试
- **端到端测试**：`portable-pty` 模拟终端交互

### 11.2 测试命名

```rust
#[test]
fn slug_normalizes_punctuation() { ... }

#[tokio::test]
async fn pre_tool_first_some_short_circuits() { ... }

#[tokio::test]
#[ignore]  // 需 API key
async fn select_relevant_memories_smoke() { ... }
```

### 11.3 测试工具

- `tempfile::TempDir` — 临时目录，测试结束自动清理
- `MemoryStore::new(dir)` — 独立 store，不污染全局
- `HookContext::test_noop()` — 测试用 hook context

---

**文档版本**: v1  
**生成时间**: 2026-08-21  
**基于**: bytemaker/src/**/*.rs + Cargo.toml  
**代码行数**: ~15,000 行（含测试）
