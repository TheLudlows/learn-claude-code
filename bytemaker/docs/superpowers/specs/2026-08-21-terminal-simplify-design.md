# 终端层简化设计（第 2 档：降级为逐行 I/O）

- **日期**: 2026-08-21
- **范围**: `src/main.rs`、`src/render/mod.rs`、`src/render/input.rs`（`src/output.rs` 不动）
- **状态**: 待审阅

---

## 1. 背景与问题

bytemaker 的交互模式终端用了"末行固定输入栏 + 上方滚动输出"的布局。这套布局逼出一批终端底层机制：

- `RawModeGuard`：进 raw 模式 + 设 DECSTBM 滚动区转义码（`render/mod.rs:221-249`），进程级、跨 raw/cooked 持续生效。
- `Coordinator::mid_line` + `emit` 用 `\r\n`（`render/mod.rs:40-49`）：raw 模式下 `\n` 不回车，必须 `\r\n`。
- `move_to_input_line`（`render/input.rs:89-95`）：每次读取前把光标移到末行，让 reedline 把输入栏锚定末行。
- 启动 logo 必须在进 raw 模式前打印（`main.rs:47-48` 注释）。

这些是 bytemaker 全仓库里最绕、对"Agent 概念"最不本质的代码（约 120 行），也是上次排查中最难读的部分。

**滚动区唯一不可替代的职责**：team 事件在 reedline 阻塞读期间打断时，让流式输出不糊掉输入栏。依据项目自身注释：
- `render/input.rs:16` — reedline `read_line` 进出时自行 enable/disable raw 模式，**读期间 raw 开着**（进程级）。
- `render/mod.rs:218-220` — DECSTBM 跨 raw/cooked 持续生效，所以只在启动设一次。
- `main.rs:148-156` — team notify 打断读时直接流式输出，靠滚动区托住上方、输入栏不动。

删掉滚动区后，这个"读期间流式输出"会糊屏，必须先解决 team 唤醒时机。

## 2. 目标与非目标

**目标**：交互模式从"末行固定输入栏 + 滚动区"降级为普通逐行 I/O，删除上述终端底层机制；保留 reedline 行编辑/历史。向现有的非交互模式（`run_noninteractive`，本就干净）看齐。

**非目标（本次不做）**：
- 不动 `Arc<Mutex<Coordinator>>`（"输出 actor" 属第 1 档）。
- 不换 reedline（async crossterm 事件属第 3 档）。
- 不改工具系统、agent 循环、hooks、渲染内容格式（`emit`/`emit_partial`/`render_tool_result` 的语义与着色保留）。

## 3. 关键子决策：team 唤醒 defer

team 事件不再打断"正在打字的回合"，改为**下一轮用户回合开头排空 Lead 收件箱**。

三条候选出路与取舍：

| 方案 | 做法 | 评价 |
|------|------|------|
| abort-read | notify 时中断 reedline 阻塞读、关 raw、流式、再重读 | reedline 不暴露 cancel，需伪造按键/杀线程，更复杂，违背简化初衷 |
| **defer（选定）** | 下一轮用户回合开头排空收件箱 | 最干净，`select!` 也能删；代价见下 |
| 留滚动区只为它 | 只删 raw、留 DECSTBM | 等于没简化这块，否决 |

**行为变化（唯一伤用法的点）**：队友在你打字时完工，结果要等你按回车才显示。与第 2 档"简化优先于 UX 保真"的前提一致，已与用户确认接受。

## 4. 设计

### 4.1 逐文件改动

| 文件 | 改动 |
|------|------|
| `render/mod.rs` | **删** `RawModeGuard`（结构体 + impl + Drop，`:221-249`）及其 2 个测试（`raw_mode_guard_is_drop_safe_when_not_a_tty` `:277`、`non_tty_coordinator_writes_plain_no_scroll_region` `:303`）。`emit` 的 `\r\n` → `\n`（`:42`、`:46`）；`mid_line` 保留，逻辑不变。Coordinator / `emit_partial` / `render_tool_result` / `VirtualTerm` / `CrosstermBackend` 全留 |
| `render/input.rs` | **删** `move_to_input_line`（`:89-95`）及其 2 处调用（`read_line` `:99`、`read_permission` `:108`）。`InputTask` / `InputCmd` / `spawn` / `ReplPrompt` 全留（reedline 阻塞 → 仍需 OS 线程）。重写模块顶部注释 |
| `main.rs` | **删** `RawModeGuard::new(interactive)`（`:63`）及 `use` 导入。`run_interactive` 去掉内层 `select!{line_rx, notify}`，改为"await `line_rx` → 回合开头排空 team 收件箱 → run"（见 4.2）。顶部 `/* 控制台 I/O 分离 */` 注释块重写 |
| `output.rs` | 不动。logo/heading 仍 `println!`，cooked 模式下 `\n` 正常；logo 不再有"进 raw 前打印"的时序约束（约束随 `RawModeGuard` 消失） |

### 4.2 `run_interactive` 新流程

```
loop {
    let (line_tx, line_rx) = oneshot::channel();
    if cmd_tx.send(InputCmd::ReadLine(line_tx)).await.is_err() { break; }   // InputTask 线程退出
    let line = match line_rx.await { Ok(Some(l)) => l, _ => break };          // 仅等用户输入，不再 select! notify
    let query = line.trim().to_string();
    if query.is_empty() { continue; }
    if query.eq_ignore_ascii_case("q") || query == "exit" {
        let _ = cmd_tx.send(InputCmd::Shutdown).await; return Ok(());
    }
    // defer：回合开头排空 Lead 收件箱。run_team_wake 内部自己 consume_lead_inbox，
    // 空收件箱 → is_empty → 返回 false（no-op）；故此处无条件调用，勿在外层预 consume（会与
    // run_team_wake 的内部 consume 重复，把收件箱排空成空导致 team turn 永不执行）。
    if run_team_wake(agent, messages, coordinator).await {
        let _ = cmd_tx.send(InputCmd::Shutdown).await; return Ok(());
    }
    if run_user_turn(agent, messages, &query, coordinator).await {
        let _ = cmd_tx.send(InputCmd::Shutdown).await; return Ok(());
    }
}
```

- 无条件在回合开头排空收件箱（空收件箱 `is_empty` 即 no-op），无需追踪 notify 状态、无需 `select!`。
- team 唤醒仍走独立一轮 `run_loop`（复用 `run_team_wake`，最小行为差），其后才跑用户回合。
- `run_user_turn` / `run_team_wake` 函数体不变。

### 4.3 非交互模式

**不动**。`run_noninteractive`（`main.rs:164-198`）的 `reader.next_line()` 是可 drop 的 async future，`select!` + 立即唤醒本就不糊屏。两条路径在唤醒时机上轻微不对称（交互 defer / 非交互立即），各自内部自洽；非交互若也 defer，管道 EOF 时会饿死 team 事件，故保留其立即唤醒。

## 5. 数据流

**交互模式（简化后）**：

```
loop {
  line = await read_line()                     // 仅等用户输入
  drain Lead 收件箱 → 有事件则先 run 一轮 team turn   // defer 唤醒
  run_user_turn(line)                          // 流式输出在 cooked 模式，\n 正常
}
```

流式渲染链路不变：`client.stream_messages` → `DeltaSink` 回调（`agent.rs:401-420`）→ `coordinator.emit_partial`（逐字）/ `emit`（整行，如 `⚙ 工具名`）/ `render_tool_result`（`↳ 结果`）。只是 `emit` 的换行符从 `\r\n` 变 `\n`。

## 6. 错误处理

不变。`RawModeGuard` 的 Drop（panic/早退时复位终端）职责随结构体消失；reedline 自身在每次 `read_line` 出入时切 raw、会自愈；非 TTY 路径本就没进 raw。无需额外兜底。

`emit` 返回 `io::Result<()>`，调用点已有 `let _ =` 忽略，换行符变更不影响错误传播。

## 7. 测试

- **删**：`raw_mode_guard_is_drop_safe_when_not_a_tty`、`non_tty_coordinator_writes_plain_no_scroll_region`（随 `RawModeGuard` 删）。
- **改**：`emit_after_partial_finishes_the_partial_line`（`render/mod.rs:255`）等断言里的 `\r\n` → `\n`。
- **新增**：交互模式 deferred-wake 行为测——注入 pending team 事件 + 模拟用户提交一行，断言该轮开头先排空收件箱并跑 team turn。可用 `VirtualTerm` + 直接调用 `run_team_wake` 路径做。
- **复核**：PTY 端到端 REPL I/O 烟雾测试（commit `db42713`，ignored）——去滚动区后末行不再固定，断言需相应调整或标记为已知差异。

## 8. 迁移与回滚

- 纯删减 + 局部重写，无数据格式/磁盘布局变化，无破坏性迁移。
- 回滚：`git revert` 单次提交即可（待用户确认是否提交）。
- 影响面：交互模式终端外观（末行不再固定，输出逐行往下铺）；非交互模式、agent 循环、工具系统均不受影响。

## 9. 范围之外

- 第 1 档（output actor 替代 `Arc<Mutex<Coordinator>>`、合并 `output.rs`）：可作为后续独立改动。
- 第 3 档（reedline → async crossterm 事件，删 OS 线程）：需手搓行编辑器，保真损失更大，留作后续。
