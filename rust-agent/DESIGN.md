# rust-agent 设计文档

## 概述

rust-agent 是一个用 Rust 实现的 Agent 循环框架，基于 Anthropic Claude API 构建。本文档描述其核心架构设计和实现原理。

---

## 核心架构概览

本文档整合了 Agent 循环的五个关键组件：

1. **Agent Loop** — 核心循环结构
2. **Tool Use** — 工具分发机制
3. **Permission** — 权限检查系统
4. **Hooks** — 扩展点机制
5. **TodoWrite** — 规划能力

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