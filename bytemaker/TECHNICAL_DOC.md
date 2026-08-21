# bytemaker 技术文档

> *"用 Rust 实现一个完整的 Claude Code 式 Agent"* — 从最小循环到多 Agent 协作系统。

---

## 目录

- [整体架构](#整体架构)
- [s01: Agent Loop — 核心循环](#s01-agent-loop--核心循环)
- [s02: Tool Use — 工具分发](#s02-tool-use--工具分发)
- [s03: Permission — 权限控制](#s03-permission--权限控制)
- [s04: Hooks — 钩子系统](#s04-hooks--钩子系统)
- [s05: TodoWrite — 任务规划](#s05-todowrite--任务规划)
- [s06: Subagent — 子 Agent](#s06-subagent--子-agent)
- [s07: Skill Loading — 技能加载](#s07-skill-loading--技能加载)
- [s08: Context Compact — 上下文压缩](#s08-context-compact--上下文压缩)
- [s09: Memory — 跨会话记忆](#s09-memory--跨会话记忆)
- [s10: Task System — 持久化任务图](#s10-task-system--持久化任务图)
- [s11: Background Tasks — 后台执行](#s11-background-tasks--后台执行)
- [s12: Cron Scheduler — 定时调度](#s12-cron-scheduler--定时调度)
- [s13: Agent Teams — 多 Agent 协作](#s13-agent-teams--多-agent-协作)
- [关键技术点](#关键技术点)
- [代码结构](#代码结构)

---

## 整体架构

bytemaker 是一个用 Rust 实现的 Claude Code 式 Agent 系统，覆盖了从最简循环到多 Agent 协作的完整功能栈。

### 三条设计原则

**循环不变**：新机制挂在 hooks 或工具上，不改 `while true` 主体。这是整个架构的基石——17 个阶段叠加上去，循环本身始终不变。

**依赖注入，消灭全局**：所有共享状态（client、registry、skills、task_store、bg_manager 等）由 `Agent` 结构体持有，通过 `ToolContext` 下传给工具。父子 Agent 之间用 `Arc::clone` 共享 infra，用独立目录隔离状态。不再依赖 `OnceLock`/`LazyLock` 全局单例，同一进程可以并行构造多个互不污染的 Agent。

**best-effort**：memory/compact 失败降级（日志警告后继续），不中断主循环。Agent 永远优先保证可用性。

### 模块全景

```
bytemaker/src/
├── main.rs              # REPL 入口（CLI 装配）
├── agent.rs             # Agent 对象抽象（核心循环）
├── client.rs            # API 请求 + 流式解析
├── output.rs            # 终端渲染与着色
├── error.rs             # 统一错误类型
├── render/mod.rs        # 控制台 I/O 分离（Coordinator）
├── tools/               # 工具系统（s02）
├── hooks.rs             # 钩子系统（s04）
├── builtins.rs          # 内置钩子（权限/提醒/总结）
├── todo.rs              # TodoManager（s05）
├── skills.rs            # SkillLoader（s07）
├── compact.rs           # ContextCompactor（s08）
├── memory.rs            # MemoryStore（s09）
├── task_system/         # 持久化任务图（s10）
├── background_tasks/    # 后台线程（s11）
├── cron_scheduler.rs    # 定时调度（s12）
└── team/                # Agent 团队（s13）
```

### 一次完整请求的数据流

```
用户输入
  ↓
trigger_prompt（UserPromptSubmit 钩子）
  ↓
messages.push(user_text)
  ↓
run_loop ─────────────────────────────────────────┐
  │                                                │
  ├─ 收集后台任务结果（Lead only）                  │
  ├─ 交付定时任务（cron queue）                     │
  ├─ 排空 Teammate 收件箱（Teammate only）          │
  ├─ compactor.prepare（四步压缩管线）               │
  ├─ build_system（base + 记忆召回）                │
  ├─ client.stream_messages（SSE 流式）             │
  ├─ messages.push(assistant_content)               │
  ├─ stop_reason == "tool_use" ?                    │
  │    ├─ Yes: execute_tool_use_blocks              │
  │    │    ├─ trigger_pre_tool（权限拦截）          │
  │    │    ├─ plan_gate（Teammate 闸门）            │
  │    │    ├─ registry.dispatch                    │
  │    │    ├─ trigger_post_tool（提醒注入）         │
  │    │    └─ 回到循环顶部 ────────────────────→ ──┘
  │    │
  │    └─ No:  trigger_stop（Stop 钩子）
  │            extract_memories（记忆提取）
  │            consolidate_memories（记忆整理）
  │            return LoopOutcome::Completed
  └──────────────────────────────────────────────────
```

---

## s01: Agent Loop — 核心循环

### 问题

你向大模型提问："帮我读取目录下的文件，并执行 XXX.py"。模型能输出 bash 命令，但输出完就停了——它不会自己跑，也不会看到结果后继续推理。每一个来回，你都在做中间层。把这个过程自动化，就是 Agent Loop 要做的事。

### 解决方案

一个 `while True` 循环，模型调用工具就继续，不调用就停。整个过程只有两个信号：

| 信号 | 含义 | 循环动作 |
|------|------|---------|
| `stop_reason == "tool_use"` | 模型举手说"我要用工具" | 执行 → 结果喂回去 → 继续 |
| `stop_reason != "tool_use"` | 模型说"我做完了" | 退出循环 |

### bytemaker 的实现

bytemaker 的核心循环在 `agent.rs` 的 `run_loop` 方法中。它比最小循环多了几件事，但主体逻辑不变：

**每次循环迭代做的事（按顺序）：**

1. **收集后台任务结果**（仅 Lead）：把已完成的后台命令结果作为 `<task_notification>` 注入消息
2. **交付定时任务**：从 cron 队列取出到期任务，作为 `[Scheduled]` 消息注入
3. **排空 Teammate 收件箱**（仅 Teammate）：把其他 Agent 发来的消息注入
4. **上下文压缩**：运行 `compactor.prepare` 四步管线（详见 s08）
5. **组装 system prompt**：base_system + 记忆目录 + 召回的记忆正文
6. **调用 LLM**：`client.stream_messages` 流式请求，通过 `DeltaSink` 实时渲染
7. **追加助手响应**到消息历史
8. **确认定时任务**：模型调用成功 → ack 定时任务（防止重复交付）
9. **判断 stop_reason**：
   - `"tool_use"` → 执行工具 → 回到循环顶部
   - 其他 → 触发 Stop 钩子 → 记忆提取/整理 → 返回

### 三种循环出口

| 出口 | 触发条件 | 含义 |
|------|---------|------|
| `Completed` | 模型不再调用工具 | 正常结束 |
| `MaxTurnsReached` | 达到 `max_turns` 上限 | 防止无限循环（子 Agent 用） |
| `Cancelled` | 用户取消（Ctrl+C） | `CancellationToken` 触发 |

### 关键点：父子共享循环

Lead、Subagent、Teammate 三种角色共用同一个 `run_loop`。区别通过 `AgentKind` 和配置参数控制：

| 属性 | Lead | Subagent | Teammate |
|------|------|----------|----------|
| max_turns | `usize::MAX` | 有限值（默认 30） | `usize::MAX` |
| cron_manager | 有 | 无 | 无 |
| memory | 读写 | 只读 | 只读 |
| team | 有 | 无 | 有 |
| hooks | 完整 5 个 | 完整 5 个（刷新） | 精简 2 个 |

---

## s02: Tool Use — 工具分发

### 问题

s01 只有一个 bash 工具。读文件要 `cat`，写文件要 `echo "..." > file.py`，改文件要 `sed`。模型想的是"读这个文件"，却要拼出 shell 命令。多了一层翻译，浪费 token，还容易出错。

### 解决方案

给 Agent 加专用工具。加工具只需要两步：

1. **定义工具**：在 `build_registry()` 中注册一个实现了 `Tool` trait 的结构体
2. **注册处理函数**：在结构体的 `execute` 方法中实现逻辑

循环不变——工具分发由 `ToolRegistry` 查表完成。

### 工具 Trait

每个工具必须实现 6 个方法：

| 方法 | 作用 | 返回值 |
|------|------|--------|
| `name()` | 工具名（唯一标识） | `&str` |
| `description()` | 描述（告诉模型"我能做什么"） | `&str` |
| `input_schema()` | 输入参数的 JSON Schema | `Value` |
| `check_permission()` | 权限检查（默认放行） | `PermissionCheck` |
| `execute()` | 执行逻辑 | `String` |
| `available_for()` | 对哪些角色可见（默认全部） | `bool` |

### ToolRegistry 查表分发

`ToolRegistry` 内部用 `BTreeMap<String, Box<dyn Tool>>` 存储工具。分发逻辑：

```
registry.dispatch(name, ctx, input, kind)
  ↓
tools.get(name) ?
  ├─ None → ToolResult::NotFound（列出可用工具）
  └─ Some(tool)
       ↓
     tool.available_for(kind) ?
       ├─ false → ToolResult::Rejected（子 Agent 调受限工具）
       └─ true → tool.execute(ctx, input) → ToolResult::Output
```

**双层过滤**：`definitions_for(kind)` 在声明层过滤（发给模型的工具列表就不含受限工具）；`dispatch` 在派发层再挡一道（防止模型幻觉出不存在的工具调用）。

### bytemaker 的 24 个工具

| 类别 | 工具 | 来源 |
|------|------|------|
| 基础 | command, read_file, write_file, edit_file, glob | s02 |
| 技能 | load_skill | s07 |
| 规划 | todo_write | s05 |
| 委托 | task | s06 |
| 任务图 | create_task, list_tasks, get_task, claim_task, complete_task | s10 |
| 后台 | task_output, task_stop | s11 |
| 定时 | schedule_cron, list_crons, cancel_cron | s12 |
| 团队 | spawn_teammate, list_teammates, send_message, request_shutdown, request_plan, review_plan, submit_plan, create_worktree | s13 |

### 路径安全

文件工具需要防止路径穿越攻击。bytemaker 的 `safe_path_in` 采用**两步验证**：

1. **词法归一化**：用 `path-clean` 消解 `..`/`.`（不访问文件系统）
2. **越界比较**：用 `dunce::canonicalize` 剥除 Windows `\\?\` 前缀后做 `starts_with` 检查

特殊处理：**允许尚不存在的路径**（`write_file` 新建文件时目标还不存在）。对不存在的路径，沿祖先上溯到第一个已存在的目录，canonicalize 后拼回尾部再做比较。

### 知识点：BTreeMap vs HashMap

bytemaker 选择 `BTreeMap` 而非 `HashMap`，因为工具数量少（<30），而**确定性排序**（API 请求的工具定义顺序稳定）比 O(1) 查找更重要。测试和调试时行为可预测。

---

## s03: Permission — 权限控制

### 问题

文件工具受 `safe_path` 保护，但 bash 不受限制，`rm -rf /` 还能跑。需要在工具执行之前加一道门。

### 解决方案：三道闸门

bytemaker 的权限系统由 `PermissionHook`（一个 `PreToolUse` 钩子）实现，串联三道闸门：

```
工具调用 → 闸门 1 → 闸门 2 → 闸门 3 → 放行
              ↓         ↓         ↓
           硬拒绝     需批准     工具自带
           (拦截)    (问用户)   (问用户)
```

**闸门 1：硬拒绝列表**

永远禁止的命令模式（正则匹配，防止编码绕过）：

| 模式 | 拦截原因 |
|------|---------|
| `rm -rf /` 及其变体 | 递归删除根目录 |
| `sudo` | 权限提升 |
| `shutdown`/`reboot`/`halt` | 系统关机 |
| `mkfs`/`dd if=` | 磁盘格式化/直接写入 |
| `chmod 777` | 危险权限设置 |
| `chown -R root:` | 递归改变所有者 |

**闸门 2：需批准列表**

危险但可能合理的命令，暂停等用户确认（y/N）：

| 模式 | 拦截原因 |
|------|---------|
| `rm`/`dd`/`mkfs` | 删除/写入块设备 |
| `sudo`/`su`/`doas` | 提权命令 |
| `curl \| bash` | 管道执行下载内容 |
| `eval` | 动态执行 |

**闸门 3：工具自带权限检查**

每个工具的 `check_permission` 方法可以返回 `NeedsApproval(reason)`，由闸门 3 向用户确认。例如 `read_file` 检测到路径越界时返回需要批准。

### 为什么用正则而不是字符串包含

- **词边界** `\b`：`\brm\b` 不会误匹配 `firmware`
- **大小写不敏感** `(?i)`：防止 `RM -RF` 绕过
- **灵活空白** `\s+`：匹配 `rm  -rf  /`（多个空格）

### s13 新增：计划闸门

Teammate 在计划被 Lead 批准前，不能执行变更操作（`command`、`write_file`、`edit_file`）。这道闸门在 PreToolUse 之后、dispatch 之前检查。

---

## s04: Hooks — 钩子系统

### 问题

循环把扩展逻辑写进体内，每加一个功能都要改循环。违反了"循环不变"原则。

### 解决方案

循环在四个固定节点触发回调，扩展逻辑全在回调里：

| 事件 | 触发时机 | 返回值语义 |
|------|---------|-----------|
| `UserPromptSubmit` | 用户输入后、进入 LLM 前 | 无控制流（纯通知） |
| `PreToolUse` | 工具执行前 | `Some(reason)` → 阻止本次工具执行 |
| `PostToolUse` | 工具执行后 | `Some(msg)` → 注入为独立 user 消息（不覆盖 tool_result） |
| `Stop` | 循环即将退出时 | `Some(msg)` → 注入并继续循环（强制继续） |

### 设计决策

**为什么用 trait object（`Box<dyn Trait>`）而非裸函数指针？**

- **携带状态**：钩子可以有 owned 状态（如 `TodoReminderHook` 持有 `Arc<SharedTodoManager>`）
- **跨 async 边界**：`Send + Sync` 超 trait 保证可在 async 上下文安全使用
- **一次堆分配**：注册时的开销可忽略不计

**短路语义**：`PreToolUse` 第一个返回 `Some` 的回调立即短路，后续回调不再执行。这保证了：如果权限钩子拦截了，后面的钩子不会意外放行。

**提醒不覆盖结果**：`PostToolUse` 返回的提醒作为**独立的 user 消息**追加，而不是塞进 `tool_result`。这是因为 Anthropic API 要求 `tool_result` 的内容必须是工具的真实输出。

### 5 个内置钩子

| 钩子 | 事件 | 作用 |
|------|------|------|
| `ContextInjectHook` | UserPromptSubmit | 记录工作目录到日志 |
| `PermissionHook` | PreToolUse | 三道闸门权限检查 |
| `LargeOutputHook` | PostToolUse | 输出 >100KB 时警告 |
| `TodoReminderHook` | PostToolUse | 注入当前 todo 列表 |
| `SummaryHook` | Stop | 统计工具调用次数 |

`TodoReminderHook` 持有 `Arc<SharedTodoManager>`，每次工具执行后把当前 todo 列表渲染成 `<reminder>` 标签注入。这让模型始终"看到"任务进度。

---

## s05: TodoWrite — 任务规划

### 问题

模型执行多步任务时容易"忘记"整体计划，做完一步不知道下一步该干什么。

### 解决方案

提供一个 `todo_write` 工具，让模型在开始前制定计划，执行中更新状态。配合 `TodoReminderHook`，每次工具执行后自动注入当前列表。

### 数据结构

每个 todo 项有三个属性：`content`（描述）、`status`（状态）。状态三态：

```
Pending [ ]  →  InProgress [>]  →  Completed [x]
```

### 验证规则

| 规则 | 原因 |
|------|------|
| 最多 20 项 | 防止模型创建过多项浪费 token |
| 只允许 1 个 `in_progress` | 强制模型聚焦当前任务 |
| `content` 不能为空 | 防止创建无意义的占位项 |
| `status` 必须是三态之一 | 类型安全 |

### 线程安全

`SharedTodoManager` 用 `Mutex<TodoManager>` 包装。旧实现用 `RefCell` 并 `unsafe impl Sync`，这是**不健全**的——`RefCell` 的运行时借用检查不是线程安全的，多线程下会触发 UB。`Mutex<T: Send>` 自动满足 `Sync`，无需 `unsafe`。

---

## s06: Subagent — 子 Agent

### 问题

某些任务需要隔离执行（如代码审查），不能污染主对话的上下文。模型需要一个"委派"能力。

### 解决方案

提供 `task` 工具（仅 Lead 可用），启动一个子 Agent 独立执行任务。子 Agent 共享 infra 但隔离状态，完成后返回摘要。

### 共享与隔离

| 资源 | 共享（Arc-clone） | 隔离（独立实例） |
|------|-------------------|-----------------|
| client（API 客户端） | ✓ | |
| registry（工具注册表） | ✓ | |
| skills（技能加载器） | ✓ | |
| task_store（任务存储） | ✓ | |
| bg_manager（后台管理） | ✓ | |
| todo_manager（todo 管理） | ✓ | |
| coordinator（渲染器） | ✓ | |
| compactor（压缩器） | | ✓ 独立子目录 `.subagents/id/` |
| memory（记忆） | | ✓ 只读模式 |
| hooks（钩子） | | ✓ 刷新（计数器归零） |
| cron_manager | | ✓ 无（子 Agent 不投递定时任务） |
| team | | ✓ 无（子 Agent 不参与团队） |

### 关键约束

- **max_turns 有限**：默认 30 轮，最高 50 轮，防止无限循环
- **task 工具不可用**：`available_for(Subagent) = false`，防止递归委托（子 Agent 再开子 Agent）
- **memory 只读**：可召回 Lead 的知识库，但不写盘
- **统一权限**：子 Agent 的工具调用也经过 `execute_tool`（含 `trigger_pre_tool`），不旁路权限检查。这是修复旧 bug 的关键——旧的 `subagent.rs` 直接调 `registry.dispatch` 绕过了权限

### 执行流程

```
Lead 调 task 工具
  ↓
child_agent(max_turns, SUB_SYSTEM)
  ↓  Arc-clone infra, 刷新 per-loop 状态
child.run_loop(messages, prompt)
  ↓  同一个循环，独立消息列表
提取最后一条 assistant text → 返回给 Lead
```

---

## s07: Skill Loading — 技能加载

### 问题

Agent 需要知道如何处理特定任务（如代码审查、PDF 处理），但把所有技能说明都塞进 system prompt 会浪费大量 token。

### 解决方案：两阶段加载

| 阶段 | 内容 | 何时加入 |
|------|------|---------|
| 启动扫描 | 名称 + 描述（目录） | 每次请求的 system prompt |
| 按需加载 | 完整 SKILL.md 正文 | 模型调用 `load_skill(name)` 时 |

这样每则请求只付目录的 token 开销（几百字符），完整说明只在需要时加载。

### 扫描流程

```
skills/
├── code-review/SKILL.md  →  解析 frontmatter → { name, description }
├── pdf/SKILL.md          →  解析 frontmatter → { name, description }
├── agent-builder/SKILL.md
└── mcp-builder/SKILL.md

         ↓ 启动时扫描

SkillLoader.catalog()  →  "Skills available:\n- code-review: Do code reviews.\n- pdf: ..."

         ↓ 编入 system prompt

LLM 需要时 → load_skill("code-review") → 返回完整 SKILL.md 作为 tool_result
```

### frontmatter 解析的容错设计

`parse_frontmatter` 有多重回退，**永远不 panic**：

| 情况 | 回退策略 |
|------|---------|
| 无 `---` 开头 | 全文作正文 |
| `splitn(3, "---")` 段数不足 | 全文 |
| YAML 解析失败 | 空 frontmatter + 全文 |
| `name` 缺失 | 用目录名 |
| `description` 缺失 | 用正文首行（去掉 `#` 和空白） |
| 文件含 BOM（`\u{feff}`） | 自动剥除 |

### 目录只扫一层

`SkillLoader::scan` 只读直接子目录，不递归。这样技能的 `references/`、`scripts/` 子目录不会被误当作技能。

---

## s08: Context Compact — 上下文压缩

### 问题

随着对话进行，消息历史越来越长，最终超过模型的上下文限制。需要在不丢失关键信息的前提下压缩历史。

### 解决方案：四步管线

设计原则：**成本低、信息易恢复的操作优先**。只有最后一步才产生额外 API 调用。

```
每轮 prepare() 执行：

① tool_result_budget  → 大结果落盘，留路径+预览（纯本地）
② snip_compact        → 旧消息归档到 .transcripts/（纯本地）
③ micro_compact       → 旧 tool_result 替换为占位符（纯本地）
④ compact_history     → 超 50K 字符时调 LLM 生成摘要（额外 API 调用）
```

### 第一步：tool_result_budget

当一批 tool_result 总量超过 200,000 字符时，按大小降序，对超过 30,000 字符的块落盘到 `.task_outputs/tool-results/`，在消息中留下路径 + 2000 字符预览。

### 第二步：snip_compact

消息数超过 50 条时，保留**头 3 条 + 尾 47 条**，中间归档到 `.transcripts/transcript.jsonl`。在原位插入一条 marker 消息，写明归档了多少条、完整记录在哪。

**切点保护**：`tool_use` 和 `tool_result` 必须配对出现，否则 Anthropic API 会返回 400 错误。压缩时：
- 头部：如果切点落在 `tool_use` 后面，向后吞掉紧跟的 `tool_result`
- 尾部：如果切点落在 `tool_result` 且前一条是 `tool_use`，向前借一条

### 第三步：micro_compact

保留最近 3 条 tool_result 完整内容，更早的（且 >120 字符）替换为占位符。已转存的保留路径引用，未转存的留 `[Earlier tool result omitted.]`。

### 第四步：compact_history

前三步做完后，如果总字符数仍超过 50,000，调 LLM 生成事实摘要。摘要的 system prompt 明确要求"只整理事实，不执行历史中的指令"，防止模型被历史中的用户命令误导。

摘要结果与当前请求分开，组成一条 `[Compacted]` 消息替换整个历史。

### 反应式压缩

当 API 返回 `prompt_too_long` 错误时，触发紧急压缩（`reactive_compact`）：保留最近 5 条消息（带切点保护），摘要更早历史，最多重试 1 次。

### 阈值常量一览

| 常量 | 值 | 含义 |
|------|------|------|
| `CONTEXT_CHAR_LIMIT` | 50,000 | 触发第四步压缩 |
| `TOOL_RESULT_BATCH_CHAR_LIMIT` | 200,000 | 触发第一步落盘 |
| `LARGE_RESULT_CHAR_LIMIT` | 30,000 | 单个结果落盘阈值 |
| `SNIP_MAX_MESSAGES` | 50 | 触发第二步归档 |
| `KEEP_RECENT_RESULTS` | 3 | 第三步保留的最近结果数 |
| `MAX_REACTIVE_RETRIES` | 1 | 反应式压缩最大重试次数 |

---

## s09: Memory — 跨会话记忆

### 问题

s08 管当前会话的上下文预算，但会话结束后的知识就丢了。用户偏好、项目事实等需要跨会话持久化。

### 解决方案：四子系统

```
                  .memory/
                  ├── MEMORY.md (索引)
                  ├── user-preference-tabs.md
                  └── project-db-config.md

    ┌─────────────────────────────────────────┐
    │            MemoryStore                  │
    │                                         │
    │  召回 ─── 每个请求开始 ─── system prompt │
    │  存储 ─── write_memory_file ─── 磁盘     │
    │  提取 ─── 回合结束后 ─── LLM 提取知识    │
    │  整理 ─── ≥10 条时 ─── LLM 合并去重      │
    └─────────────────────────────────────────┘
```

### 记忆文件格式

每条记忆是一个 Markdown 文件，带 YAML frontmatter：

```
---
name: User Preference Tabs
description: user prefers tabs for indentation
type: user          ← 四类之一：user / feedback / project / reference
---

Always use tabs for indentation, not spaces.
```

### 召回流程

1. 列出所有记忆文件的 name + description 目录
2. 取最近 3 条用户消息作为查询
3. 调 LLM 选择相关记忆（返回 JSON 数组 `[0, 2]`）
4. **LLM 失败降级**：用关键词匹配（tokenize + 命中数排序）
5. 加载选中记忆的正文，按总量 20,000 字符截断
6. 注入 system prompt 的 `Relevant memory records` 段

### 提取流程

回合结束后（`stop_reason != "tool_use"` 且 Stop 钩子放行后）：

1. 取最近 12 条消息的对话文本
2. 调 LLM 提取持久知识，要求返回 `[{ name, type, scope, description, body }]`
3. 对每个候选做三重过滤：
   - **scope 检查**：只存 `scope == "persistent"`，`current_task` 跳过
   - **临时标记检查**：正文含 "this session"、"本次会话"、"暂时" 等标记 → 跳过
   - **查重**：slug / description / body 与现有记忆重复 → 跳过
4. 通过过滤的写入 `.memory/`，重建索引

### 整理流程

记忆条数 ≥ 10 时触发：

1. 把所有记忆文件内容拼成 catalog
2. 调 LLM 合并去重（"Merge duplicates, apply newer corrections"）
3. **快照 + 失败恢复**：替换前先保存所有原文件内容，替换失败则从快照还原
4. 最多保留 30 条

### best-effort 设计

记忆系统的每个环节都可能失败，但永远不中断主循环：

| 失败点 | 降级策略 |
|--------|---------|
| LLM 召回调用失败 | 降级关键词选择 |
| LLM 提取调用失败 | 跳过提取，返回 0 |
| 写盘失败 | `tracing::warn!` 后继续 |
| 整理失败 | 从快照恢复，返回 0 |

### 只读模式

Subagent 和 Teammate 的 `MemoryStore` 使用 `read_only` 模式：可以召回（load_memories）Lead 的知识库，但 extract/consolidate 直接返回 0，不写盘。

---

## s10: Task System — 持久化任务图

### 问题

复杂任务需要分解、追踪依赖、持久化状态。

### 解决方案

每个任务是一个独立的 JSON 文件（`.tasks/task_XXXXXXXX.json`），支持并发读写。

### Task 生命周期

```
Pending ──claim──→ InProgress ──complete──→ Completed
                     ↑
                  owner 绑定
```

### 依赖管理

任务可以有 `blocked_by` 字段（任务 ID 列表）。`claim_task` 会检查所有依赖是否已 `Completed`，未完成则拒绝领取。

### 5 个任务工具

| 工具 | 作用 | 可用角色 |
|------|------|---------|
| `create_task` | 创建任务（subject + description + blocked_by） | Lead |
| `list_tasks` | 列出所有任务及状态 | Lead, Teammate |
| `get_task` | 获取单个任务详情 | Lead, Teammate |
| `claim_task` | 领取任务（绑定 owner，状态 → InProgress） | Teammate |
| `complete_task` | 完成任务（状态 → Completed） | Teammate |

### 原子性保证

`claim_task` 在跨进程锁（`TaskStoreLock`）保护下执行，保证并发领取时只有一个 winner。

---

## s11: Background Tasks — 后台执行

### 问题

某些命令执行时间很长（如 `npm install`、`cargo build`），阻塞主循环。

### 解决方案

`command` 工具支持 `run_in_background: true` 选项：

```
command("cargo build --release", run_in_background=true)
  ↓
立即返回 "[Background task bg_a1b2c3d4 started] ..."
  ↓  (循环继续)
  ... 几轮后 ...
  ↓
collect_and_inject() → 注入 <task_notification> 到消息
```

### 执行模型

- 后台任务在独立的 tokio worker 中运行
- 命令超时 120 秒
- 最多 8 个并发任务
- 进程树杀死（`portable-pty`）
- panic 恢复（worker panic 不崩主进程）

### 交付机制

主循环每轮顶部调用 `bg_manager.collect_and_inject(messages)`，把已完成的任务结果作为 `<task_notification>` XML 注入到消息列表中。模型看到后可以用 `task_output` 工具主动轮询，或用 `task_stop` 取消。

---

## s12: Cron Scheduler — 定时调度

### 问题

某些任务需要在特定时间执行（如"每天早上 9 点检查部署状态"）。

### 解决方案

提供 `schedule_cron` 工具，使用 5 字段 Vixie cron 表达式在指定时间将 prompt 注入到 agent 循环。

### 执行模型

```
schedule_cron("*/5 * * * *", "check deploy status")
  ↓
CronManager.state.jobs.insert(job)
  ↓
后台 tick_loop（每 60 秒检查一次）
  ↓ 匹配 cron 表达式
delivery_queue.push_back(job)
  ↓
主循环顶部 cron.consume_queue()
  ↓
messages.push("[Scheduled] check deploy status")
```

### 持久化

`durable: true` 的任务会写入 `.scheduled_tasks.json`，进程重启后自动加载。

### 三个工具

| 工具 | 作用 | 可用角色 |
|------|------|---------|
| `schedule_cron` | 创建定时任务 | Lead |
| `list_crons` | 列出所有任务 | Lead |
| `cancel_cron` | 取消任务 | Lead |

这三个工具对 Teammate 不可见（Teammate 不需要定时能力）。

---

## s13: Agent Teams — 多 Agent 协作

### 问题

复杂任务需要多个 Agent 协作完成（如一个写代码、一个做测试、一个做审查）。

### 解决方案：Lead/Teammate 模式

```
                    ┌─────────────┐
                    │    Lead     │  协调者：分配任务、审查计划、接收结果
                    └──────┬──────┘
                           │
              ┌────────────┼────────────┐
              │            │            │
        ┌─────┴─────┐ ┌───┴─────┐ ┌───┴─────┐
        │ Teammate A │ │Teammate B│ │Teammate C│  执行者：领取任务、提交计划
        └───────────┘ └─────────┘ └─────────┘
```

### TeamCtx：共享团队状态

`TeamCtx` 持有所有团队共享状态，由 Lead 创建，Teammate 通过 `Arc` 共享：

| 组件 | 作用 |
|------|------|
| `MessageBus` | 文件 backed 邮箱，异步通信 |
| `AssignmentRegistry` | owner → (task_id, cwd) 绑定 |
| `ProtocolRegistry` | 协议状态（计划审批、关闭请求） |
| `active` | 活跃 Teammate 状态表 |
| `lead_notify` | `tokio::sync::Notify`，唤醒 Lead REPL |
| `task_store` | 共享的任务存储 |
| `lock` | 跨进程排他锁（`.tasks/.lock`） |

### 通信机制

Agent 之间通过 `MessageBus` 异步通信。每个 Agent 有一个收件箱文件。消息类型：

| msg_type | 方向 | 含义 |
|----------|------|------|
| `result` | Teammate → Lead | 任务完成结果 |
| `shutdown_request` | Lead → Teammate | 请求关闭 |
| `shutdown_response` | Teammate → Lead | 确认关闭 |
| `plan_request` | Lead → Teammate | 要求提交计划 |
| `plan_approval_response` | Teammate → Lead | 提交计划等待审批 |

Lead 的 REPL 用 `tokio::select!` 同时监听 stdin 和 `lead_notify`，Teammate 有事件时自动唤醒 Lead 开始新一轮。

### 计划闸门（Plan Gate）

Teammate 在执行变更操作前必须先提交计划并获得 Lead 批准：

```
Teammate 调 submit_plan → Lead 收到 plan_approval_response
                              ↓
                         Lead 调 review_plan(approve=true/false)
                              ↓
                    gate 状态变为 Approved / Rejected
                              ↓
              Teammate 的 command/write_file/edit_file 被放行 / 拦截
```

Gate 状态有 5 种：`NotRequired`（默认放行）、`Required`（需要计划）、`Pending`（已提交待审）、`Approved`（已批准）、`Rejected`（已拒绝）。

### Teammate 运行时

`TeammateRuntime` 驱动一个 Teammate 的完整生命周期：

```
spawn_teammate("alice", "backend developer")
  ↓
child_teammate("alice", system_prompt, team)
  ↓
tokio::spawn → TeammateRuntime.run()
  ↓
循环：
  ├─ 排空收件箱（处理 shutdown/plan 消息）
  ├─ 无任务时自动 claim_next_task
  ├─ run_loop（执行任务）
  ├─ 提取最终文本 → 发送到 Lead inbox
  ├─ complete_task → 释放 assignment
  └─ 扫描新任务 / 进入 idle
```

### 任务分配约束

| 约束 | 原因 |
|------|------|
| 一个 Teammate 同一时间只能领一个任务 | 聚焦 |
| 一个任务只能被一个 owner 领取 | 跨进程锁保证 |
| 依赖未完成的任务不可领取 | 保证执行顺序 |
| Teammate 关闭时释放未完成任务（回退到 Pending） | 防止任务泄漏 |

---

## 关键技术点

### 1. 流式输出

Anthropic API 返回 SSE（Server-Sent Events）流。bytemaker 用 `eventsource-stream` 解析协议层，自己做 JSON 解析和内容累加。

**DeltaSink 回调**：每次收到增量（text delta 或 tool_use start），立即通过 `Coordinator` 渲染到终端。用户看到逐字输出，体验更好。

**CallResult 三态**：`Success` / `PromptTooLong` / `Failure` / `Cancelled`。`PromptTooLong` 单独分类，O(1) 判别，不依赖字符串扫描。触发反应式压缩后重试。

### 2. AgentError 统一错误

所有错误收归到一个 `thiserror` 枚举，覆盖 API 错误、网络错误、流错误、工具错误、路径错误、文件系统错误、验证错误等。`From` trait 实现自动转换（`io::Error` → `FileSystem`，`reqwest::Error` → `Network` 等）。

`is_prompt_too_long()` 方法扫描错误消息中的关键词（`prompt_too_long`、`too many tokens`、`request_too_large`），大小写不敏感。

### 3. 控制台解码

中文 Windows 的 `cmd.exe` 默认输出 GBK 编码。bytemaker 的 `decode_console` 先尝试 UTF-8，失败再尝试 GBK（`encoding_rs::GBK`），避免中文输出变成 U+FFFD 替换符。

### 4. 字符数 vs Token 数

bytemaker 统一用**字符数**（不是 tokenizer token 数）估算上下文大小。原因：
- 与 Python 参考实现的 `len(str)` 对齐
- 不引入 tokenizer 依赖
- 阈值足够保守（50K 字符 ≈ 12-15K tokens），留有安全余量

### 5. Coordinator 控制台 I/O 分离

`Coordinator<B: Backend>` 把"输出写进滚动区"与"输入栏固定末行"解耦：
- `emit(line)` 写一行完整输出
- `emit_partial(s)` 写半行（流式 token 拼接）
- `emit` 前如果当前是半行，先补换行

测试用 `VirtualTerm`（实现 `Backend` trait），可以 dump 屏缓冲做断言。

---

## 代码结构

```
bytemaker/
├── src/
│   ├── main.rs              # REPL 入口
│   ├── agent.rs             # Agent 对象（核心循环）
│   ├── client.rs            # API 客户端 + SSE 流式解析
│   ├── output.rs            # 终端渲染与着色
│   ├── error.rs             # 统一错误类型
│   ├── render/mod.rs        # Coordinator 控制台 I/O 分离
│   ├── hooks.rs             # 钩子系统（4 个扩展点）
│   ├── builtins.rs          # 5 个内置钩子
│   ├── todo.rs              # TodoManager
│   ├── skills.rs            # SkillLoader
│   ├── compact.rs           # ContextCompactor（四步管线）
│   ├── memory.rs            # MemoryStore（四子系统）
│   ├── cron_scheduler.rs    # CronManager
│   ├── tools/               # 工具系统
│   │   ├── mod.rs           # build_registry（24 个工具注册）
│   │   ├── trait_def.rs     # Tool trait + ToolResult + ToolContext
│   │   ├── registry.rs      # ToolRegistry（BTreeMap 分发）
│   │   ├── command.rs       # Shell 命令（跨平台 + GBK 解码）
│   │   ├── read_file.rs     # 读文件
│   │   ├── write_file.rs    # 写文件
│   │   ├── edit_file.rs     # 编辑文件
│   │   ├── glob_tool.rs     # 文件匹配
│   │   ├── load_skill.rs    # 加载技能
│   │   ├── todo_write.rs    # 任务规划
│   │   └── task.rs          # 子 Agent（Lead-only）
│   ├── task_system/         # 持久化任务图
│   │   ├── task.rs          # Task 结构 + TaskStatus
│   │   ├── store.rs         # TaskStore（JSON 文件 + 校验）
│   │   └── tools.rs         # 5 个任务工具
│   ├── background_tasks/    # 后台执行
│   │   ├── manager.rs       # BackgroundManager（tokio worker）
│   │   ├── task.rs          # BackgroundTask 状态
│   │   └── tools.rs         # task_output / task_stop
│   └── team/                # Agent 团队
│       ├── mod.rs           # TeamCtx + claim/complete/drain
│       ├── bus.rs           # MessageBus（文件邮箱）
│       ├── assignment.rs    # AssignmentRegistry
│       ├── protocols.rs     # ProtocolRegistry + GateStatus
│       ├── runtime.rs       # TeammateRuntime
│       ├── tools.rs         # 8 个团队工具
│       ├── lock.rs          # TaskStoreLock（跨进程锁）
│       └── worktree.rs      # Git worktree 支持
├── skills/                  # 4 个示例技能
├── Cargo.toml
└── README.md
```

### 运行时生成目录（gitignore）

| 目录 | 用途 |
|------|------|
| `.memory/` | 记忆文件 + MEMORY.md 索引 |
| `.transcripts/` | 压缩前的完整对话归档 |
| `.task_outputs/` | 大结果落盘 + 后台任务输出 |
| `.tasks/` | 任务 JSON 文件 + .lock |
| `.subagents/` | 子 Agent 隔离目录 |
| `.teammates/` | Teammate 隔离目录 |
| `.scheduled_tasks.json` | 持久化定时任务 |

### 依赖概览

| 类别 | 主要 crate |
|------|-----------|
| HTTP/API | reqwest, eventsource-stream, futures-util |
| 序列化 | serde, serde_json, serde_yaml |
| 异步 | tokio (full), async-trait, tokio-util |
| 错误 | thiserror |
| 日志 | tracing, tracing-subscriber |
| 时间/调度 | chrono, croner |
| 文件系统 | glob, path-clean, dunce, fs4 |
| 终端 UI | crossterm, reedline, colored |
| 工具 | fastrand, regex, encoding_rs |

---

## 总结

bytemaker 从零实现了一个完整的 Claude Code 式 Agent 系统，13 个阶段逐步叠加：

| 阶段 | 一句话 | 核心机制 |
|------|--------|---------|
| s01 | 一个循环就够了 | `while true` + `stop_reason` |
| s02 | 多加一个工具，只加一行 | ToolRegistry 查表分发 |
| s03 | 三道闸门保安全 | 硬拒绝 + 需批准 + 工具自带 |
| s04 | 四个扩展点 | Hook 回调，循环不改 |
| s05 | 规划清单 | TodoManager + Reminder 注入 |
| s06 | 消息隔离的委托 | child_agent + 共享/隔离 |
| s07 | 按需加载技能 | 目录进 system，正文走 tool |
| s08 | 四步压缩管线 | 本地操作优先，LLM 兜底 |
| s09 | 跨会话记忆 | 召回/提取/整理 + best-effort |
| s10 | 持久化任务图 | JSON 文件 + 依赖管理 |
| s11 | 后台执行慢命令 | tokio worker + 通知注入 |
| s12 | 定时调度 | cron 表达式 + 持久化 |
| s13 | 多 Agent 协作 | Lead/Teammate + 计划闸门 |

三条原则贯穿始终：
- **循环不变**：新机制挂在 hooks 或工具上
- **依赖注入**：所有状态由 Agent 持有，Arc 共享
- **best-effort**：失败降级，不中断主循环
