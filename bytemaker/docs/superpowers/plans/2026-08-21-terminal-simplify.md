# 终端层简化（第 2 档：逐行 I/O）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把交互模式终端从"raw 模式 + 滚动区 + 末行固定输入栏"降级为普通逐行 I/O，删除 RawModeGuard / move_to_input_line / `\r\n` 机制，team 唤醒改为下一轮用户回合开头 defer。

**Architecture:** 删减型重构。`emit` 的 `\r\n` → `\n`（`mid_line` 保留）；删 `RawModeGuard`（raw+DECSTBM）与 `move_to_input_line`；`run_interactive` 去掉内层 `select!{line_rx, notify}`，改为纯 `await line_rx` + 回合开头无条件调 `run_team_wake`（其内部 `consume_lead_inbox` 空则 no-op）。非交互模式 `run_noninteractive` 不动（async future 可 drop，立即唤醒不糊屏）。

**Tech Stack:** Rust 2021 / tokio / crossterm 0.29 / reedline 0.50。lib + bin（`[lib] path=src/lib.rs`，`src/main.rs` 为默认 bin）。测试：`cargo test`（lib 单测 + bin 内 `#[cfg(test)]`）；`tempfile` dev-dep；`smoke` feature。

**Branching & commits:** 你当前在 `main`。执行前先开分支：`git checkout -b terminal-simplify`。每个 Task 末尾提交一次（如下）。若不想提交，跳过 commit 步即可。

---

## File Structure

| 文件 | 责任 | 本次改动 |
|------|------|---------|
| `src/render/mod.rs` | Coordinator（输出协调）+ `RawModeGuard` + 后端 | 删 `RawModeGuard` 及 2 测试；`emit` `\r\n`→`\n` |
| `src/render/input.rs` | InputTask（独占 stdin 的 reedline 线程） | 删 `move_to_input_line` + 3 个 crossterm import；改模块 doc |
| `src/main.rs` | REPL 外层（装配 + `run_interactive`/`run_noninteractive`/`run_user_turn`/`run_team_wake`） | 删 `RawModeGuard` import + guard 行；重写 `run_interactive`；改顶部注释 + logo 注释 |
| `src/output.rs` | logo + heading | 不动 |

无新增文件。

---

## Task 1: `emit` 换行符 `\r\n` → `\n`（TDD）

**Files:**
- Modify: `src/render/mod.rs:40-49`（`emit` 方法）
- Test: `src/render/mod.rs:255-265`（`emit_after_partial_finishes_the_partial_line`）

- [ ] **Step 1: 收紧测试断言（要求 `\n`、禁止 `\r`），先让它 fail**

把 `src/render/mod.rs:255-265` 的测试改为：

```rust
    #[test]
    fn emit_after_partial_finishes_the_partial_line() {
        let mut c = Coordinator::new(VirtualTerm::new(24, 80));
        c.emit_partial("hello").unwrap();
        c.emit("world").unwrap();
        let dump = c.into_backend().screendump();
        // 逐行 I/O（cooked 模式）：emit 用 \n，不应出现 \r。
        assert!(dump.contains("hello\nworld\n"), "got: {dump:?}");
        assert!(!dump.contains('\r'), "raw-mode \\r no longer used: {dump:?}");
    }
```

- [ ] **Step 2: 跑测试，确认 FAIL**

Run: `cargo test --lib render::tests::emit_after_partial_finishes_the_partial_line`
Expected: FAIL — 当前 `emit` 写 `\r\n`，`!dump.contains('\r')` 不成立。

- [ ] **Step 3: 改 `emit`，`\r\n` → `\n`**

把 `src/render/mod.rs:40-49` 改为：

```rust
    pub fn emit(&mut self, line: &str) -> io::Result<()> {
        if self.mid_line {
            self.backend.write_str("\n")?;
            self.mid_line = false;
        }
        self.backend.write_str(line)?;
        self.backend.write_str("\n")?;
        self.backend.flush()?;
        Ok(())
    }
```

（仅两处 `"\r\n"` → `"\n"`：第 42、46 行。`mid_line` 字段与其余逻辑不变。）

- [ ] **Step 4: 跑测试，确认 PASS；顺带跑 render 全部测试**

Run: `cargo test --lib render::`
Expected: PASS（含 `emit_after_partial_finishes_the_partial_line`、`emit_two_full_lines_have_newlines`、`render_tool_result_*`）。

- [ ] **Step 5: Commit**

```bash
git add src/render/mod.rs
git commit -m "refactor(render): emit uses \n instead of \r\n (drop raw-mode newline)"
```

---

## Task 2: 删 `move_to_input_line`（`render/input.rs`）

**Files:**
- Modify: `src/render/input.rs:22-30`（imports）、`:89-95`（函数）、`:99` 与 `:108`（调用）、模块 doc `:1-20`

- [ ] **Step 1: 删 3 个 crossterm imports**

`src/render/input.rs:25-27` 当前：

```rust
use crossterm::cursor::MoveTo;
use crossterm::execute;
use crossterm::terminal;
```

整段删除（删后 `move_to_input_line` 不再编译，正是下一步要删的）。保留 `:22` `use std::borrow::Cow;`、`:23` `use std::io;`、`:28` `use tokio::sync::{mpsc, oneshot};`、`:30` `use crate::hooks::PermissionQuery;`。

- [ ] **Step 2: 删 `move_to_input_line` 函数**

`src/render/input.rs:89-95` 当前：

```rust
/// 把光标移到末行（滚动区之外），让 reedline 把输入栏锚定在末行。
/// 仅在交互模式（真 TTY）下有意义；InputTask 只在交互模式下被 spawn。
fn move_to_input_line() {
    if let Ok((_, rows)) = terminal::size() {
        let _ = execute!(io::stdout(), MoveTo(0, rows.saturating_sub(1)));
    }
}
```

整段删除（含上方 doc 注释）。

- [ ] **Step 3: 删两处调用**

`src/render/input.rs:99`（`read_line` 内）与 `:108`（`read_permission` 内）各有一行 `move_to_input_line();`，删除这两行。

删后 `read_line` 起始为：

```rust
fn read_line(ed: &mut reedline::Reedline, prompt: &ReplPrompt) -> Option<String> {
    match ed.read_line(prompt) {
        Ok(reedline::Signal::Success(line)) => Some(line),
        _ => None,
    }
}
```

`read_permission` 起始为：

```rust
fn read_permission(ed: &mut reedline::Reedline, prompt: &ReplPrompt) -> bool {
    match ed.read_line(prompt) {
        Ok(reedline::Signal::Success(l)) => l.trim().eq_ignore_ascii_case("y"),
        _ => false,
    }
}
```

- [ ] **Step 4: 改模块 doc 末段**

`src/render/input.rs:14-20` 当前末段：

```rust
//! `apply_submit` / `apply_ctrl_c` 是不碰 stdin 的纯转移函数，便于单测；
//! reedline 自带编辑/历史，不在此重测。
//!
//! 终端卫生：reedline 的 `read_line` 每次进出会自行 `enable_raw_mode` /
//! `disable_raw_mode`，并以**当前光标行**作为提示符锚点（见
//! `Reedline::read_line_helper` 的 `initialize_prompt_position`）。故每次
//! 读取前线程都把光标移到末行（滚动区之外），让输入栏固定在末行，输出
//! 区在其上方滚动。滚动区由 `RawModeGuard` 在交互模式进入时设置。
```

改为：

```rust
//! `apply_submit` / `apply_ctrl_c` 是不碰 stdin 的纯转移函数，便于单测；
//! reedline 自带编辑/历史，不在此重测。
//!
//! 终端卫生：reedline 的 `read_line` 每次进出会自行 `enable_raw_mode` /
//! `disable_raw_mode`（瞬态，仅读期间 raw 开）。逐行 I/O 模型下，输出在
//! 回合内（cooked 模式，`\n` 正常）流式渲染；读期间不产生输出，故无需
//! 滚动区 / 末行锚定，`move_to_input_line` 已移除。
```

- [ ] **Step 5: 编译 + 跑测试**

Run: `cargo build` && `cargo test --lib render::input`
Expected: 编译通过（无 unused import 警告，因 imports 已删）；input 模块测试 PASS（`submit_pushes_to_queue_and_clears`、`ctrl_c_emits_cancel`）。

- [ ] **Step 6: Commit**

```bash
git add src/render/input.rs
git commit -m "refactor(render): drop move_to_input_line (no scroll region / fixed input bar)"
```

---

## Task 3: `main.rs` 去 `RawModeGuard` + 重写 `run_interactive`（defer 唤醒）

**Files:**
- Modify: `src/main.rs:1-16`（顶部注释）、`:28`（use）、`:47-48`（logo 注释）、`:63`（guard 行）、`:113-161`（`run_interactive`）

- [ ] **Step 1: 顶部 use 去掉 `RawModeGuard`**

`src/main.rs:28` 当前：

```rust
use bytemaker::render::{Coordinator, CrosstermBackend, RawModeGuard};
```

改为：

```rust
use bytemaker::render::{Coordinator, CrosstermBackend};
```

- [ ] **Step 2: 删 guard 构造行**

`src/main.rs:62-63` 当前：

```rust
    let interactive = std::io::stdout().is_terminal();
    let _guard = RawModeGuard::new(interactive);
```

删第 63 行，保留 `:62`（`interactive` 仍在 `:77` 的 `if interactive { spawn InputTask }` 使用）：

```rust
    let interactive = std::io::stdout().is_terminal();
```

- [ ] **Step 3: 改 logo 注释**

`src/main.rs:47-48` 当前：

```rust
    // logo 在进 raw 模式前打印（cooked 模式下 `\n` 正常；进 raw 后所有输出走 Coordinator 的 `\r\n`）。
    output::logo();
```

改为：

```rust
    // logo：cooked 模式 `\n` 正常（reedline 仅在读期间瞬态开 raw，不影响此处）。
    output::logo();
```

- [ ] **Step 4: 重写 `run_interactive`（去 `select!`、加 defer 唤醒）**

`src/main.rs:112-161` 当前整段（`/// 交互模式 REPL...` 注释 + `async fn run_interactive`）替换为：

```rust
/// 交互模式 REPL：reedline InputTask，纯 await line（不再 select! notify）。
/// team 事件 defer 到下一轮用户回合开头排空（`run_team_wake` 内部 consume，
/// 空则 no-op）——避免在 reedline 阻塞读期间流式输出糊屏。
async fn run_interactive(
    agent: &Agent,
    messages: &mut Vec<Message>,
    coordinator: &Arc<Mutex<Coordinator<CrosstermBackend>>>,
    cmd_tx: tokio::sync::mpsc::Sender<InputCmd>,
) -> Result<(), AgentError> {
    loop {
        // 请求 InputTask 读一行查询（reedline 自行渲染 ` >> ` 提示符）。
        let (line_tx, line_rx) = oneshot::channel();
        if cmd_tx.send(InputCmd::ReadLine(line_tx)).await.is_err() {
            break; // InputTask 线程已退出
        }
        let line = match line_rx.await {
            Ok(Some(l)) => l,
            _ => break, // EOF / Ctrl+C：InputTask 已退出
        };
        let query = line.trim().to_string();
        if query.is_empty() {
            continue; // 空行：重新发 ReadLine
        }
        if query.eq_ignore_ascii_case("q") || query == "exit" {
            let _ = cmd_tx.send(InputCmd::Shutdown).await;
            return Ok(());
        }
        // defer 唤醒：每轮用户回合开头排空 Lead 收件箱。`run_team_wake`
        // 内部自己 `consume_lead_inbox`，空收件箱 → is_empty → 返回 false
        //（no-op）；勿在外层预 consume（会与内部 consume 重复排空成空）。
        if run_team_wake(agent, messages, coordinator).await {
            let _ = cmd_tx.send(InputCmd::Shutdown).await;
            return Ok(());
        }
        if run_user_turn(agent, messages, &query, coordinator).await {
            let _ = cmd_tx.send(InputCmd::Shutdown).await;
            return Ok(());
        }
    }
    Ok(())
}
```

注意：删去了原内层 `loop { select!{ line_rx, notify } }` 与 `let notify = agent.lead_notify()...`。`run_user_turn` / `run_team_wake` 函数体不动。`run_noninteractive`（`:164-198`）不动。

- [ ] **Step 5: 改顶部块注释**

`src/main.rs:1-16` 当前整块替换为：

```rust
/*
main.rs - REPL 入口（逐行 I/O 简化模型）

核心循环与共享状态都移入 lib 的 `Agent`（agent.rs）：main 只做 CLI 装配——
读 env、构造 Agent、启动 cron、REPL 调 `agent.run_loop`。

终端交互（简化后，2026-08-21）：不再用 raw 模式 + 滚动区维持"末行固定输入栏"，
改为普通逐行 I/O。
- 交互模式（真 TTY）：`InputTask`（reedline）独占 stdin，经单一命令通道收
  `ReadLine` / `AskPermission`；main 只 `await` 该行（不再 `select!` notify）。
  team 事件**延迟到下一轮用户回合开头**排空（`run_team_wake` 内部 consume，
  空则 no-op）——不再打断正在打字的回合，避免在 reedline 阻塞读期间流式输出糊屏。
- 非交互模式（管道/CI）：tokio 行读取 + `select!{ line, lead_notify }`，team
  事件立即唤醒（async future 可 drop，无 raw 模式糊屏问题）；不 spawn InputTask。
  权限钩子无 ask 通道 → 需批准的命令直接拒绝（不挂起）。
两种模式的用户回合 / team 唤醒逻辑共享 `run_user_turn` / `run_team_wake`。
*/
```

- [ ] **Step 6: 编译 + 跑全量测试**

Run: `cargo build` && `cargo test`
Expected: 编译通过（`RawModeGuard` 仍在 `render/mod.rs` 但 bin 不再引用；pub 项不触发 dead_code，无警告）。所有非 ignored 测试 PASS。

- [ ] **Step 7: Commit**

```bash
git add src/main.rs
git commit -m "refactor(main): drop RawModeGuard, defer team wake to next turn (line-by-line I/O)"
```

---

## Task 4: 删 `RawModeGuard`（`render/mod.rs`）

**Files:**
- Modify: `src/render/mod.rs:215-249`（doc + struct + impl + Drop）、`:277-282` 与 `:303-317`（2 测试）

- [ ] **Step 1: 删 `RawModeGuard` 定义（含 doc 注释）**

`src/render/mod.rs:215-249` 整段删除——从：

```rust
/// raw 模式 RAII 守卫：构造开启、Drop 恢复，保证 panic/早退也复位终端。
///
/// 交互模式下同时设置滚动区 `行 1..rows-1`（末行留给 reedline 输入栏），
/// 把"输出区滚动"与"输入栏固定末行"解耦。`reedline::read_line` 自身会在
/// 每次读取进出时 toggle raw 模式，但滚动区（DECSTBM）是终端级设置，
/// 跨 raw/cooked 切换持续生效，故只需在此设置一次。
pub struct RawModeGuard { enabled: bool }
impl RawModeGuard {
    /// `interactive=true` 才真正进 raw 模式 + 设滚动区（非 TTY 传 false）。
    pub fn new(interactive: bool) -> Self {
        if interactive {
            let _ = ct::enable_raw_mode();
            // DECSTBM：ESC[<top>;<bottom>r，1-indexed。末行(rows)留给输入栏。
            if let Ok((_, rows)) = ct::size() {
                let bottom = rows.saturating_sub(1);
                let mut out = io::stdout().lock();
                let _ = write!(out, "\x1b[1;{bottom}r");
                let _ = write!(out, "\x1b[H"); // 光标归位（DECSTBM 规定光标移到原点）
                let _ = out.flush();
            }
        }
        Self { enabled: interactive }
    }
}
impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.enabled {
            let mut out = io::stdout().lock();
            let _ = write!(out, "\x1b[r"); // 重置滚动区为全屏
            let _ = write!(out, "\x1b[?25h"); // 确保光标可见
            let _ = out.flush();
            let _ = ct::disable_raw_mode();
        }
    }
}
```

到其结束 `}` 全删。注意：`use crossterm::terminal as ct;`（`:9`）保留——`CrosstermBackend::size` 仍用 `ct::size()`（`:210`）。`io::Write`（`:8`）保留——`CrosstermBackend::write_str` 仍用 `io::Write::write_all`（`:203`）。

- [ ] **Step 2: 删 2 个引用 `RawModeGuard` 的测试**

`src/render/mod.rs:277-282`：

```rust
    #[test]
    fn raw_mode_guard_is_drop_safe_when_not_a_tty() {
        // 非 TTY（CI）下不应 panic，构造/析构皆 Ok。
        let g = RawModeGuard::new(false);
        drop(g); // 不 panic 即过
    }
```

与 `:303-317`：

```rust
    #[test]
    fn non_tty_coordinator_writes_plain_no_scroll_region() {
        // 非 TTY（CI）路径：RawModeGuard::new(false) 不进 raw、不设滚动区；
        // Coordinator 直写后端（VirtualTerm 累字节），输出原样不含滚动区转义码。
        let g = RawModeGuard::new(false);
        let mut c = Coordinator::new(VirtualTerm::new(24, 80));
        c.emit("hi").unwrap();
        let dump = c.into_backend().screendump();
        assert!(dump.contains("hi"), "plain write should contain 'hi': {dump:?}");
        assert!(
            !dump.contains("\x1b[1;"),
            "non-TTY path must not emit a scroll-region sequence: {dump:?}"
        );
        drop(g); // 守卫析构在非 TTY 下应为 no-op，不 panic
    }
```

两段整段删除。

- [ ] **Step 3: 编译 + 跑 render 测试**

Run: `cargo build` && `cargo test --lib render::`
Expected: 编译通过（无残留引用）；render 测试 PASS（剩余：`emit_after_partial...`、`emit_two_full_lines...`、`render_tool_result_*`）。

- [ ] **Step 4: 全量测试兜底**

Run: `cargo test`
Expected: 所有非 ignored 测试 PASS。

- [ ] **Step 5: Commit**

```bash
git add src/render/mod.rs
git commit -m "refactor(render): remove RawModeGuard (raw mode + DECSTBM scroll region)"
```

---

## Task 5: 验证（编译 + 全量测试 + 手动 REPL 烟雾）

**Files:** 无改动。

- [ ] **Step 1: 全量编译（含 bin）**

Run: `cargo build`
Expected: 成功，无 warning。

- [ ] **Step 2: 全量测试**

Run: `cargo test`
Expected: 所有非 ignored 测试 PASS。重点看 `render::tests::emit_after_partial_finishes_the_partial_line`（确认 `\n` 路径）。

- [ ] **Step 3: 残留扫描——确认无 `\r\n` / DECSTBM 残留**

Run: `git grep -n '\\r\\n\|\\x1b\[1;\|RawModeGuard\|move_to_input_line\|enable_raw_mode\|disable_raw_mode' -- 'src/*.rs'`
Expected: 仅可能命中 `src/render/input.rs` 模块 doc 里"enable_raw_mode / disable_raw_mode"这两个词（描述 reedline 行为的文字，非代码调用）与 `src/main.rs` 顶部注释里"raw 模式"文字。**不应**再有任何代码调用或 `RawModeGuard` 标识符。

- [ ] **Step 4: 手动 REPL 烟雾（需 API key；无 key 则跳过，依赖前 3 步）**

前置：`ANTHROPIC_AUTH_TOKEN` / `MODEL_ID` 已设。Run:

```bash
cargo run
```

Expected（逐行 I/O，末行不再固定）：
1. 看到 ByteMaker logo + banner（base_url/model/key/Loaded N skill(s)）。
2. 末行出现 ` >> ` 提示符，可正常打字、左右编辑（reedline）。
3. 输入一句话回车 → 模型流式逐字输出在下方，工具调用显示 `⚙ name` + JSON，结果显示 `↳ name 结果 (...)`。输出逐行往下铺，**不再有固定输入栏悬浮底部**。
4. 再输入 `q` 回车 → 干净退出，终端无残留 raw/滚动区状态。

若行为异常（输出错位、退出后终端乱），回看 Task 3 `run_interactive` 与 Task 1 `emit`。

- [ ] **Step 5: 收尾提交（如有手动调整）**

若 Step 3/4 发现遗漏并修补，提交：

```bash
git add -A
git commit -m "fix(render): residual cleanup from terminal simplify"
```

否则无需提交。

---

## Self-Review 结果

**Spec 覆盖**：spec §4.1 表 → Task 1（emit `\r\n`→`\n`）+ Task 4（删 RawModeGuard）+ Task 2（删 move_to_input_line）+ Task 3（main.rs 重写）；§4.2 `run_interactive` 新流程 → Task 3 Step 4；§4.3 非交互不动 → Task 3 Step 4 末注；§3 defer → Task 3 Step 4；§6 错误处理不变 → 无需任务（reedline 自愈，已注）；§7 测试 → Task 1（改 emit 测试）/ Task 4（删 2 测试）/ deferred-wake 的可测面是 `consume_lead_inbox` 空路径，已在 `src/team/mod.rs:510 consume_lead_inbox_matches_response` 覆盖（lib 单测，无需新增）；spec 提的"PTY 烟雾测试复核"——经查 `tests/repl_io_pty.rs` 已在 commit a025138（清理一次性产物）删除，无文件可调，改为 Task 5 手动 REPL 烟雾。**无 gap。**

**Placeholder 扫描**：无 TBD/TODO；每个代码步含完整代码。

**类型/命名一致**：`run_team_wake` / `run_user_turn` / `consume_lead_inbox` / `InputCmd::ReadLine|Shutdown` / `Coordinator::emit|emit_partial` 均与现有代码一致（未改名）。`emit` 签名不变。
