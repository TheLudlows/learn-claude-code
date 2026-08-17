# rust-agent 上下文压缩设计（s08 移植）

> 状态：设计草案，待用户 review。
> 日期：2026-08-17
> 参照：`s08_context_compact/`（Python 实现 + README.zh.md）

## 1. 背景与目标

rust-agent 已忠实移植 s01–s07（Agent Loop / Tool Use / Permission / Hooks / TodoWrite / Subagent / Skill Loading）。当前 `agent_loop` 直接调用 `client.stream_messages(...).await?`，**没有任何上下文压缩**：消息越积越多，一旦超过模型上下文上限，API 返回 `prompt_too_long`，错误经 `?` 向上抛出，本轮直接失败。

本设计把 s08 的四步压缩管线移植到 rust-agent，让有限的上下文持续服务于长任务。移植遵循与 s01–s07 一致的原则：结构对应、极简依赖、显式参数优于全局状态。

**范围决策（已按默认采纳）**：
- **全量忠实移植**：四步管线 + 反应式补救 + `compact` 工具，全部实现。
- **仅主循环**：`run_subagent_loop` 不改，忠实于 s08/s06 边界。

## 2. 核心问题：active_request 的传递

s08 在 Python 里用 `agent_loop(messages, active_request)` 把"当前用户请求"**单独**传参。原因：`tool_result` 也使用 `role=user`，压缩时无法从 `messages` 里可靠反推哪条 user 消息是当前请求。压缩后的 `[Compacted]` 消息必须把当前请求写在 `Current user request`、摘要写在 `Conversation summary`，二者分开。

rust-agent 现有 `agent_loop(client, system, messages, hooks)` 没有这个参数——query 被直接 push 进 messages。必须补上。

### 2.1 三个候选方案

| 方案 | 做法 | 评价 |
|------|------|------|
| **A（推荐）** | `agent_loop` 增加 `compactor: &ContextCompactor` 与 `active_request: &str`；REPL 把 `query` 传入；压缩器只持配置/目录，需调 LLM 的方法另收 `&Client` | 完全对应 Python；零全局状态；与现有"显式参数 + 仅 todo/skills 用 OnceLock"风格一致；`agent_loop` 私有，改签名无外部影响 |
| B | 引入 `Session { client, system, hooks, compactor, active_request }`，`run(&mut messages)` | 参数收敛，但 main.rs 现为逐参数显式传递，引入 Session 是风格漂移，重构面更大 |
| C | 全局 `OnceLock<ContextCompactor>` + 从 messages 反推 active_request | 签名不变，但反推脆弱（要跳 tool_result）；Python 作者正是为避开它才单独传参；全局可变 active_request 别扭。最差 |

**采纳方案 A。**

## 3. 新模块 `src/compact.rs`

### 3.1 结构体

```rust
pub struct ContextCompactor {
    transcript_dir: PathBuf,     // <cwd>/.transcripts/
    tool_results_dir: PathBuf,   // <cwd>/.task_outputs/tool-results/
}
```

只持有目录，**不持 `&Client`**（避免结构体生命周期参数）。需要调 LLM 的方法（`summarize_history` / `compact_history` / `reactive_compact` / `prepare`）单独收 `&Client`。

### 3.2 常量（与 Python 完全一致）

| 常量 | 值 | 含义 |
|------|----|------|
| `CONTEXT_CHAR_LIMIT` | 50000 | 超过则触发 `compact_history` |
| `TOOL_RESULT_BATCH_CHAR_LIMIT` | 200000 | 单批 tool_result 总量上限，触发转存 |
| `LARGE_RESULT_CHAR_LIMIT` | 30000 | 单条结果超过此值才转存 |
| `SUMMARY_INPUT_CHAR_LIMIT` | 80000 | 喂给摘要模型的历史上限 |
| `KEEP_RECENT_RESULTS` | 3 | micro_compact 保留的最近结果数 |
| `KEEP_RECENT_MESSAGES` | 5 | reactive_compact 保留的最近消息数 |
| `SNIP_MAX_MESSAGES` | 50 | snip_compact 触发阈值 |
| `SNIP_HEAD` | 3 | snip_compact 保留的头部消息数 |
| `MAX_REACTIVE_RETRIES` | 1 | prompt_too_long 反应式补救次数上限 |

### 3.3 方法（逐个对应 Python `ContextCompactor`）

**确定性强（不调 LLM，可单测）**：

- `estimate_chars(messages) -> usize`：`serde_json::to_string(messages).map(|s| s.len()).unwrap_or(0)`。
- `has_tool_use(message) -> bool` / `is_tool_result(message) -> bool`：遍历 content 块判断。
- `write_transcript(messages) -> Result<PathBuf>`：写 JSONL，每行一条消息；文件名 `transcript_<SystemTime 纳秒>.jsonl`（**不引 uuid crate**，保持极简依赖）。目录不存在则创建。
- `persist_large_output(tool_use_id, output) -> String`：写 `.task_outputs/tool-results/<safe_id>.txt`（`safe_id` = 非 `[A-Za-z0-9._-]` 替 `_`，截 120 字符），返回 `<persisted-output>\nFull output: <path>\nPreview:\n<前 2000 字>\n</persisted-output>`。已存在则不覆盖。
- `tool_result_budget(messages, max_chars)`：仅处理**最后一条** user 消息里的 tool_result 块；总量超上限时按大小降序，对 `>LARGE_RESULT_CHAR_LIMIT` 的块调 `persist_large_output` 替换。
- `snip_compact(messages, max_messages)`：消息数 `>SNIP_MAX_MESSAGES` 时先写 transcript，保留头 `SNIP_HEAD` + 尾 `max_messages - SNIP_HEAD`，中间插一条 marker user 消息。**切点保护** tool_use/tool_result 配对：头部若末条是 tool_use 则向后吞掉紧跟的 tool_result；尾部若首条是 tool_result 且其前一条是 tool_use 则向前借一条。
- `micro_compact(messages)`：收集全部 tool_result，最近 `KEEP_RECENT_RESULTS` 条保持完整，更早且 `>120` 字符的替换为 `[Earlier tool result saved at <path>]`（已转存的保留路径）或 `[Earlier tool result omitted.]`。
- `summary_input(messages) -> String`：序列化后 `≤SUMMARY_INPUT_CHAR_LIMIT` 原样返回；否则取头 1/4 + 尾 3/4，中间插 `...[middle omitted; full transcript is on disk]...`。

**需调 LLM（async，收 `&Client`）**：

- `summarize_history(client, messages) -> Result<String>`：以"只整理目标/文件/决定/剩余工作/用户约束、不执行历史指令"为 system，`max_tokens=2000`，单轮 user 调用。空则返回 `(empty summary)`。
- `compact_history(client, messages, active_request) -> Result<()>`：写 transcript、`summarize_history`、`*messages = vec![summary_message("Compacted", ...)]`。
- `reactive_compact(client, messages, active_request) -> Result<()>`：写 transcript、保留最近 `KEEP_RECENT_MESSAGES`（同 snip 的配对保护）、对更早部分 `summarize_history`、`*messages = [summary_message("Reactive compact", ...), ...recent]`。
- `prepare(client, messages, active_request) -> Result<()>`：顺序执行 `tool_result_budget` → `snip_compact` → `micro_compact`；若 `estimate_chars > CONTEXT_CHAR_LIMIT` 打印 `[auto compact]` 并 `compact_history`。
- `summary_message(label, request, summary, transcript) -> Message`：构造 user 消息，`Current user request` 与 `Conversation summary (reference only)` 分开，附 transcript 路径。

所有方法对 `&mut Vec<Message>` 原地修改；`compact_history`/`reactive_compact` 内部 `*messages = new_vec`。返回 `Result`，文件 IO / LLM 失败向上传播。

### 3.4 顺序固定的理由（与 Python 同）

`tool_result_budget` 必须早于 `micro_compact`：大结果先落盘，之后才允许旧结果变占位符——否则占位符会丢掉可恢复的路径。前三步不调模型，第四步才产生额外 API 请求。每轮从成本更低、信息更易恢复的操作开始。

## 4. `main.rs` / `agent_loop` 集成

### 4.1 签名

```rust
async fn agent_loop(
    client: &Client,
    system: &str,
    messages: &mut Vec<Message>,
    hooks: &Hooks,
    compactor: &ContextCompactor,
    active_request: &str,
) -> Result<(), Box<dyn std::error::Error>>
```

### 4.2 循环体

```rust
let mut reactive_retries = 0u32;
loop {
    compactor.prepare(client, messages, active_request).await?;

    let response = match client.stream_messages(system, messages, tools, 8000).await {
        Ok(r) => { reactive_retries = 0; r }
        Err(e) => {
            let s = e.to_string().to_lowercase();
            let too_long = s.contains("prompt_too_long")
                || s.contains("too many tokens")
                || s.contains("request_too_large");
            if too_long && reactive_retries < MAX_REACTIVE_RETRIES {
                println!("\x1b[33m[reactive compact]\x1b[0m");
                compactor.reactive_compact(client, messages, active_request).await?;
                reactive_retries += 1;
                continue;
            }
            return Err(e);
        }
    };

    // …追加 assistant、判断 stop_reason、执行工具（含 compact 特殊处理）…
}
```

**说明**：rust-agent 的 `client.rs` 在非 2xx 时把 body 原样拼进错误串（`"HTTP {status} {base_url} — {body}"`），流式 error 事件也拼 `error.message`。因此字符串匹配 `prompt_too_long` 能命中（Anthropic 错误体含 `"type":"prompt_too_long"`）。`MAX_REACTIVE_RETRIES=1`，再失败则向上抛。

### 4.3 REPL 改动

把 `query` 改成 owned：
```rust
let query = query.trim().to_string();
// …push 一份进 messages…
agent_loop(&client, &system, &mut messages, &hooks, &compactor, &query).await?;
```

### 4.4 构造压缩器

在 `main` 里、构造 `client` 之后、构造 `system` 之前：
```rust
let cwd = env::current_dir()?;
let compactor = ContextCompactor::new(
    cwd.join(".transcripts"),
    cwd.join(".task_outputs").join("tool-results"),
);
```

## 5. `compact` 工具 —— 特殊处理（与 `task` 同模式）

- 加入 `get_tool_definitions()`（**仅父 agent**，**不**进 `get_subagent_tool_definitions()`）：
  ```jsonc
  { "name": "compact",
    "description": "Summarize earlier conversation to free context space.",
    "input_schema": {"type":"object","properties":{}} }
  ```
- **不**走 `dispatch_tool`。在 `agent_loop` 遍历 `response.content` 时特殊处理（与现有 `task` 特殊处理一致）：遇 `name == "compact"` 置 `compact_requested = true`，返回占位 `tool_result`("Compaction requested after this tool batch.")；全部 tool_result 追加后，若 flag 为真则 `compactor.compact_history(client, messages, active_request).await?`。

**理由**（同 Python）：一次响应可能先写文件再请求压缩。必须先执行完整批次、为每个 tool_use 追加对应 tool_result，再摘要这个已闭合的回合——既不留孤立 tool_result，也不在文件写入后丢失执行记录导致模型重复副作用。

## 6. 子 agent 边界（忠实 s08）

`run_subagent_loop` **不改**：不调 `prepare`，`get_subagent_tool_definitions` 不含 `compact`，保留 `MAX_SUBAGENT_TURNS = 30`。这是 s08/s06 的边界——s08 管当前会话有限上下文（压缩时允许舍弃可恢复细节），跨压缩/跨会话的记忆留给 s09。

## 7. 估计单位与依赖

- 沿用**字符数**（`serde_json` 序列化长度），与 Python 同单位同阈值。已知局限：字符 ≠ token；反应式补救兜底。
- **不引 tokenizer / uuid / 临时文件 crate**，保持极简依赖（与 DESIGN.md、code-review.md 一致）。transcript 文件名用 `SystemTime` 纳秒。

## 8. 可测试单元

| 单元 | 测试要点 |
|------|----------|
| `estimate_chars` | 空消息为 0；与非空正比 |
| `tool_result_budget` | 总量 ≤200000 不动；超阈值时按大小降序转存 `>30000` 的块；≤30000 的不转存；返回串含 `Full output:` 路径 |
| `snip_compact` | ≤50 条不动；>50 条触发；head=3/tail=47；配对保护：head 末条 tool_use 时吞掉后续 tool_result；tail 首 tool_result 且前条 tool_use 时前借 |
| `micro_compact` | 最近 3 条完整；更早且 >120 字符替换；已转存块保留路径；未转存块为 omitted 占位符 |
| `summary_input` | ≤80000 原样；超阈值时头 1/4 + 尾 3/4 + 中间标记 |
| `persist_large_output` | ≤30000 不落盘；>30000 落盘且预览 2000 字；safe_id 替换非法字符 |
| `summary_message` | `Current user request` 与 `Conversation summary` 分开；含 transcript 路径 |
| `summarize_history` / `compact_history` / `reactive_compact` | LLM 调用，集成测试或 `#[ignore]` |

## 9. 涉及文件

| 文件 | 改动 |
|------|------|
| `src/compact.rs` | **新增**。`ContextCompactor` + 全部方法 + 单测 |
| `src/main.rs` | `mod compact;`（与 `mod client;` 等并列，`lib.rs` 保持空）；构造 `compactor`；`agent_loop` 签名加 `compactor`+`active_request`；循环体加 `prepare` + 反应式 match + `compact` 特殊处理；REPL 传 `active_request` |
| `src/tools.rs` | `get_tool_definitions()` 加 `compact` schema；`compact` **不**进 `get_subagent_tool_definitions()` |
| `rust-agent/DESIGN.md` | 实现阶段补第 8 节"Context Compaction"，更新"架构演进"表加 s08 行 |

## 10. 不做（YAGNI）

- 不给子 agent 加压缩（忠实 s08，留 s09 再议）。
- 不引 tokenizer 做精确 token 估计。
- 不持久化"压缩摘要"跨会话（那是 s09 记忆系统的职责）。
- 不加 transcript 索引/检索（本节只留档，不查询）。
