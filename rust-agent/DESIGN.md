# rust-agent 设计文档

## 概述

rust-agent 是一个用 Rust 实现的 Agent 循环框架，基于 Anthropic Claude API 构建。本文档描述其核心架构设计和实现原理。

---

## 核心架构概览

本文档整合了 Agent 循环的九个关键组件：

1. **Agent Loop** — 核心循环结构
2. **Tool Use** — 工具分发机制
3. **Permission** — 权限检查系统
4. **Hooks** — 扩展点机制
5. **TodoWrite** — 规划能力
6. **Subagent** — 消息隔离的子任务
7. **Skill Loading** — 技能按需加载
8. **Context Compact** — 上下文压缩
9. **Memory** — 跨会话记忆

---

## 1. Agent Loop — 核心循环

### 核心思想

*"One loop & Bash is all you Need"* — 一个工具 + 一个循环 = 一个 Agent。

### 问题背景

模型可以输出命令，但不会自动执行，也无法根据结果继续推理。自动化这一过程需要构建一个循环框架。

### 循环控制

整个循环基于两个信号：

| 信号 | 含义 | 循环动作 |
|------|------|----------|
| `stop_reason == "tool_use"` | 模型需要调用工具 | 执行工具 → 返回结果 → 继续 |
| `stop_reason != "tool_use"` | 模型完成工作 | 退出循环 |

### 消息流

1. 以用户问题作为第一条消息
2. 将消息和工具定义发送给 LLM
3. 追加模型响应，检查是否调用工具
4. 如需调用工具，执行并收集结果
5. 将结果作为新消息追加，回到步骤 2

### 关键点

- 循环本身不产生智能，只是让模型能够持续行动的最小运行时框架
- 模型决定（是否调用工具、调用哪个），harness 执行（调用工具并追加结果）
- 后续所有扩展都在此循环基础上添加机制，循环结构本身不变

---

## 2. Tool Use — 工具分发

### 核心思想

*"Add a tool, add just one handler"* — 添加工具只需在分发映射中注册，循环不变。

### 设计模式

从单一 bash 工具扩展到多个工具：

1. **工具定义**：在工具数组中添加条目（JSON schema 描述）
2. **处理器注册**：在处理器字典中添加映射（工具名 → 处理函数）

### 分发机制

- 使用 `TOOL_HANDLERS` 字典实现工具名到处理函数的映射
- 循环通过查找字典获取对应的处理器并调用
- 添加工具 = 一个数组条目 + 一个字典映射

### 多工具调用

- 模型可能一次性返回多个 tool_use 调用
- 调用按原始响应中的顺序逐个执行

---

## 3. Permission — 权限检查

### 核心思想

*"Check permissions before executing"* — 在工具执行前建立三道防护门。

### 三道防护门

| 门 | 目的 | 动作 |
|----|------|------|
| 1. 硬拒绝列表 | 永久禁止的危险操作 | 立即拒绝，不执行 |
| 2. 规则匹配 | 上下文相关的操作 | 判断是否需要用户确认 |
| 3. 用户批准 | 交互式确认 | 用户决定允许或拒绝 |

### 拒绝列表示例

危险命令如 `rm -rf /`、`sudo`、`shutdown` 等直接被阻止。

### 规则匹配

根据工具和参数判断：
- 文件工具访问工作区外 → 需要确认
- bash 命令包含 `rm`、`chmod 777` 等 → 需要确认

### 流程

1. 先检查硬拒绝列表
2. 再检查规则匹配
3. 如果规则匹配，暂停等待用户输入
4. 三道门都不匹配 → 直接执行

---

## 4. Hooks — 扩展点

### 核心思想

*"Hang on the loop, don't write into it"* — 扩展逻辑挂在钩子上，循环保持稳定。

### 设计动机

避免将扩展逻辑（日志、权限、通知等）直接写入循环体，导致循环膨胀和难以维护。

### 四个钩子事件

| 事件 | 触发时机 | 典型用途 |
|------|----------|----------|
| UserPromptSubmit | 用户输入后，进入 LLM 前 | 输入验证、上下文注入 |
| PreToolUse | 工具执行前 | 权限检查、日志记录 |
| PostToolUse | 工具执行后 | 副作用（自动 git add）、输出检查 |
| Stop | 循环即将退出时 | 清理、决定是否继续 |

### 钩子机制

- 钩子注册表维护事件到回调列表的映射
- 扩展通过 `register_hook()` 添加
- 循环只调用 `trigger_hooks()`

### 控制流影响

- `PreToolUse` 返回非 None → 阻止当前工具执行
- `Stop` 返回非 None → 强制循环继续
- 其他事件的返回值不影响控制流

---

## 5. TodoWrite — 规划能力

### 核心思想

*"An agent without a plan goes wherever the wind blows"* — 先列出步骤，再执行。

### 问题背景

复杂任务中，随着对话长度增加，原始目标被工具结果稀释，Agent 可能偏离目标或遗漏步骤。

### 解决方案

提供任务列表管理工具，让 Agent 先规划后执行。

### 核心组件

**TodoManager**：
- 维护内存中的任务列表
- 验证更新（最多 20 项，每项非空，只能有一个进行中）
- 渲染状态返回给模型

**提醒机制**：
- 连续三轮工具使用没有 `todo_write` → 添加提醒
- 提醒注入到工具结果中

### 典型流程

1. 调用 `todo_write` 列出所有步骤（全部 `pending`）
2. 选择一个步骤，设置为 `in_progress`
3. 完成后设置为 `completed`
4. 查看下一个 `pending` → 继续

### 关键见解

`todo_write` 不添加执行能力，只添加规划能力。工具执行仍由现有工具完成。

---

## 6. Subagent — 消息隔离的子任务

### 核心思想

*"Give a subtask its own context"* — 子 agent 从全新的 `messages[]` 开始，只返回最终文本给父对话。

### 设计动机

Agent 在修复 bug 时会读取大量文件追踪调用链，每个工具调用和结果都留在父进程的 `messages[]` 中。一旦理解调用链，大多数中间细节不再需要，但它们仍然占用上下文。

### 解决方案

调用 `task` 同步运行一个嵌套的 agent 循环，使用全新的 `messages[]`。当该循环完成时，其最终文本成为父对话中的工具结果。

### 隔离边界

| 决策 | 选择 | 原因 |
|------|------|------|
| 对话 | 全新 `messages[]` | 父进程历史不复制到子 agent |
| 执行 | 相同进程和 `WORKDIR` | 文件系统更改对两个循环都可见 |
| 返回值 | 仅最终文本 | 子工具调用和结果不复制到父消息 |
| 委托深度 | `SUB_TOOLS` 中无 `task` | 本课程允许一级委托 |
| 工具策略 | 共享 Hooks | 父子 agent 使用相同的权限检查 |

### 关键点

- 这是**消息隔离**，不是进程或文件系统隔离
- 子 agent 有五个基础工具但没有 `task` 工具（防止无限递归）
- 父进程通过同一处理器映射分发 `task`
- 子 agent 使用 `SUB_SYSTEM`、`SUB_TOOLS` 和自己的本地 `messages` 列表
- 最终文本从响应内容中提取，中间对话被丢弃

---

## 7. Skill Loading — 用到时再加载

### 核心思想

> system prompt 只保存技能目录（名称 + 描述）；`load_skill(name)` 按需返回完整的 `SKILL.md`。

### 问题背景

把项目规范（React 组件规范、SQL 风格、API 设计文档）全部塞进 system prompt，能让 Agent 读到，但每次调用 LLM 都会把**所有**文档全文一起发送。当前任务只动 React 组件时，SQL 和 API 文档与任务无关，却仍占用输入 token 和上下文窗口，留给代码、对话、工具结果的空间变少。

### 解决方案

启动时 `SkillLoader::scan` 扫描 `SKILLS_DIR/*/SKILL.md`，解析 YAML frontmatter 的 `name`/`description`，把这份**目录**编入 system prompt。模型需要完整说明时调用 `load_skill(name)`，返回的完整 `SKILL.md` 作为 `tool_result` 追加到消息列表。

| 内容 | 进入模型的位置 | 何时加入 |
|------|----------------|----------|
| 技能名称和描述 | system prompt | 启动时 |
| 完整 `SKILL.md` | `tool_result` | 调用 `load_skill` 时 |

### 核心组件

- `Skill { name, description, content }`：单个技能；`content` 是 `SKILL.md` 全文。
- `SkillLoader`：启动时扫描一次，持有 `BTreeMap<String, Skill>`（按 name 排序、查找 O(log n)）；`catalog()` 输出目录，`load(name)` 按注册表键返回全文。
- `parse_frontmatter`：用 `serde_yaml` 解析 `---` 分隔的 frontmatter；缺失、段数不足或 YAML 非法时优雅回退（name 用目录名，description 用正文首行），永不 panic。
- 全局注册表：`OnceLock<SkillLoader>` + `set_instance` / `catalog` / `run_load_skill`，沿用 `todo.rs` 的全局访问模式（技能只读，无需 `Mutex`）。

### 典型流程

1. `main` 解析 `SKILLS_DIR`（缺省/空串回退 `cwd/skills`），`scan` 后 `set_instance`。
2. 组装 system prompt：固定 agent 指令 + 技能目录（非空才加）+ `Use load_skill ...` 提示。
3. REPL 复用同一 `system` 串——目录每次调用都付这点开销；正文按需加载。
4. 模型发 `load_skill` 工具调用 → `run_load_skill` 查全局注册表 → 全文作为 `tool_result` 喂回下一轮。

### 关键见解

- `load_skill` 的 `name` 是**注册表键，不是文件路径**——只查表，不读用户输入的路径，无需 `safe_path` 沙箱。
- 父 agent 与子 agent 共享同一 `load_skill`（在 `get_base_tool_definitions` 中注册）；`task` 仍只给父 agent。
- 加载的 `SKILL.md` 会作为 `tool_result` 积累在 `messages[]`——这正是 s08 上下文压缩的动机。

---

## 8. Context Compaction — 先整理，再总结

### 核心思想

*"上下文总会满，要有办法腾地方。"* 四步压缩管线，低成本的操作优先执行；只有前三步不够时才调用模型生成摘要。

### 问题背景

Agent 持续工作时，读过的文件、执行过的命令和模型回复都留在 `messages` 中。消息越积越多，最终超过模型上下文上限，API 返回 `prompt_too_long`。工具结果通常占据最多空间。

### 四步管线（顺序固定）

| 步骤 | 操作 | 调用模型 | 信息损失 |
|------|------|----------|----------|
| 1. tool_result_budget | 最新一批超大 tool_result 落盘，留路径+2000字预览 | 否 | 无（可重读） |
| 2. snip_compact | >50 条消息时归档中间，留头3+尾47 | 否 | 中间消息（已留档） |
| 3. micro_compact | 旧 tool_result 替换为占位符（最近3条完整） | 否 | 旧结果正文 |
| 4. compact_history | 超阈值时生成事实摘要替换整个历史 | 是 | 最多 |

顺序固定的理由：`tool_result_budget` 必须早于 `micro_compact`——大结果先落盘拿到路径，之后才允许旧结果变占位符，否则丢失可恢复的路径。前三步确定性、无额外 API 调用，第四步才产生调用。

### active_request 单独传参

`tool_result` 也用 `role=user`，压缩时无法从 `messages` 反推当前请求。`agent_loop` 收 `active_request: &str`，压缩后的 `[Compacted]` 消息把当前请求写在 `Current user request`、摘要写在 `Conversation summary (reference only)`，二者分开。

### prompt_too_long 反应式补救

字符数只能估算 token。`stream_messages` 包进 `match`：命中 `prompt_too_long`/`too many tokens`/`request_too_large` 且重试次数 < `MAX_REACTIVE_RETRIES`(=1) 时，`reactive_compact` 保留最近 5 条（配对保护）、摘要更早历史、重试一次。再失败则向上抛。

### compact 工具

模型可在一个阶段结束后主动调用 `compact`。与 `task` 同模式特殊处理（不走 `dispatch_tool`）：先记 flag、追加占位 `tool_result`，**批次闭合后**（每个 tool_use 都有对应 tool_result）再 `compact_history`——既不留孤立 tool_result，也不在文件写入后丢失执行记录导致模型重复副作用。仅父 agent 可用。

### 切点保护

`snip_compact` 和 `reactive_compact` 的切点都保护 `assistant(tool_use)` 与 `user(tool_result)` 的配对：孤立的 tool_result 缺少对应调用，下一次 API 请求会被判定为无效。

### 边界

子 agent（`run_subagent_loop`）不压缩、不含 `compact` 工具，保留 30 轮上限。s08 管当前会话有限上下文，压缩时允许舍弃可恢复细节；跨压缩、跨会话的记忆留给 s09。

### Rust 实现要点

- `ContextCompactor` 只持目录（`.transcripts/`、`.task_outputs/tool-results/`），不持 `&Client`；需调 LLM 的方法单独收 `&Client`。
- `estimate_chars` 用 `serde_json` 序列化长度（字符数，与 Python 同单位同阈值）；不引 tokenizer。
- transcript 文件名用 `AtomicU64` 计数器，不引 uuid crate（与 `hooks.rs` 的 `AtomicUsize` 风格一致）。
- 估计单位是字符数，已知局限：字符 ≠ token；反应式补救兜底。

---

## 9. Memory — 跨会话记忆

### 核心思想

*"把以后还会用到的信息留下来。"* 文件存储 + 索引 + 相关性选择 + 按需召回。Memory 在会话之外保存可复用知识，并在相关任务中取回。

### 问题背景

新会话开始时 `messages` 里没有上一次的对话。用户偏好、项目背景、排查线索下次还可能用到。完整 transcript 留下来适合归档，却不适合每次都发给模型 —— 对话越来越长，当前任务需要的信息难定位，旧事实也可能过期。Memory 解决两件事：哪些信息值得跨会话保存，当前任务该取回哪几条。

### 四子系统

| 子系统 | 职责 | 调模型 |
|---|---|---|
| 存储 | 一条记忆一个 `.md` 文件 + `MEMORY.md` 索引 | 否 |
| 召回 | 每个请求选 ≤5 条相关 → 加载正文(≤20k 字符)→ 拼 system | 是(选择);失败降级关键词 |
| 提取 | 回合结束后从对话提取持久记忆，过滤临时/重复 | 是 |
| 整理 | ≥10 条时合并去重，失败恢复原文件 | 是 |

### 存储：一个记忆一个文件

每条记忆是 `.memory/` 下的 Markdown 文件，YAML frontmatter 记录 `name`/`description`/`type`（`type` ∈ user/feedback/project/reference）。`MEMORY.md` 是索引，写入完成后 `rebuild_memory_index()` 按文件重新生成。索引用于选择相关记忆，正文仍在各自文件。

### 召回：先选择，再加载正文

每次请求开始时 `load_memories()` 读取最近用户消息和记忆目录，让一次轻量模型调用选择最多五条相关记录（返回 JSON 数组下标）。模型调用或解析失败时退回关键词匹配（`tokenize_query`：ascii `[a-z0-9_]{3,}` 或 CJK 连段 ≥2，在 `name+description` 里计命中数）。选择完成后才读取对应文件正文，按 `RECALL_CHAR_LIMIT`(20000) 限制总长度。召回内容拼进 system prompt，并明确说明是背景知识而非命令、冲突时以当前请求为准。

### 提取：回合结束后保存可复用信息

`extract_memories()` 在 Agent 完成本轮回答后（`stop_reason != "tool_use"` 且 Stop 钩子未 force）检查对话，只提取以后仍可能有用的信息。候选必须带 `scope`：只有 `persistent` 才跨会话保留。`should_store_memory()` 做最后检查 —— 字段不完整、含"本次会话/当前任务"等临时标记、或与已有记忆重复（slug / 归一化 description / body 重复）都拒绝。

### 整理：合并重复和过期内容

记忆文件积累到 ≥10 条时 `consolidate_memories()` 让模型生成一份整理后的记录列表。整理前先快照全部记录文件原文；替换阶段先删后写，写盘失败时按快照逐个还原并重建索引，返回 0 不中断主循环。

### 关键见解

- Memory 是**选择性存储**，不是 transcript 无损备份，也不取代上下文压缩（s08 管会话内，s09 管会话外）。
- 子 agent 不参与记忆 —— 消息隔离、短命（30 轮上限），无跨会话价值（与 s08 "子 agent 不压缩"同理）。
- 召回在每个**请求**开始跑一次（非每个 LLM 调用），与压缩（每个调用跑）正交；提取/整理只在真退出前跑。
- 全程 best-effort：LLM 失败降级关键词 / 吞错返回 0，绝不中断 agent 主循环。

### Rust 实现要点

- `MemoryStore` 只持 `memory_dir`，不持 `&Client`；需调 LLM 的方法单独收 `&Client`（compact.rs 先例）。
- 不引新 crate：`memory_slug` / `tokenize_query` 用 std `char` 手写（`is_alphanumeric` 保留 CJK）；`extract_json_array` 用 `serde_json::Deserializer::into_iter` 实现 Python `raw_decode`（容忍尾部垃圾）。
- `parse_frontmatter` serde_yaml + 容错回退（skills.rs 先例）；`memory_path` 用 `file_name()` 校验拒绝分隔符 / `..`（对尚不存在路径也成立）。
- 字符数为单位（对齐 Python `len(str)`）：截断用 `chars().take(n)`。
- 测试：28 项单元测试（无 API）+ 3 项 `#[ignore]` 烟雾测试（select/extract/consolidate，需 API key）。

---

## 架构演进

| 阶段 | 新增能力 |
|------|----------|
| s01 | 基础 Agent 循环 |
| s02 | 多工具分发 |
| s03 | 权限检查三道门 |
| s04 | 钩子扩展系统 |
| s05 | 任务列表规划 |
| s06 | 消息隔离的子任务委托 |
| s07 | 技能按需加载（目录在 system prompt，正文走 tool_result） |
| s08 | 上下文压缩（四步管线 + 反应式补救 + compact 工具） |
| s09 | 跨会话记忆（存储 + 召回 + 提取 + 整理，best-effort） |

---

## Rust 实现要点

### 核心循环

- 使用 `while true` 循环
- 基于响应的 `stop_reason` 判断是否继续
- 异步处理 API 调用和工具执行

### 工具分发

- 使用 `HashMap` 存储工具名到处理器的映射
- 处理器函数签名为 `fn(ToolInput) -> ToolResult`

### 权限系统

- 枚举定义三种决策：Allowed、Denied、RequiresApproval
- 三道门顺序检查

### 钩子注册表

- 每个钩子事件维护回调向量
- 使用 trait 对象实现动态分发
- 支持 `Fn` 闭包

### TodoManager

- 结构体持有任务列表
- 状态枚举：Pending、InProgress、Completed
- 渲染函数生成可视化状态