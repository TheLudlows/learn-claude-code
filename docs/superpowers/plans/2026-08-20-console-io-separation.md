# bytemaker 控制台输入/输出分离 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 bytemaker REPL 的输入与输出在控制台分离——固定底部输入行 + 上方原生滚动输出区 + 实时流式 + 并发排队输入。

**Architecture:** crossterm 原始模式 + 滚动区把"输出区(1..N-1)"与"输入栏(第 N 行)"解耦；reedline 负责输入行编辑/历史；一个 `Coordinator<Backend>` 泛型抽象（`CrosstermBackend` 真终端 / `VirtualTerm` 测试）做游标协调与 `mid_line` 簿记；`client.rs::stream_messages` 加 `DeltaSink` 回调逐 token 推送 + `CancellationToken` 取消；`run_loop` 保持顺序循环不动。

**Tech Stack:** Rust 2021 / tokio / crossterm / reedline / tokio-util(CancellationToken) / async-trait / portable-pty(dev)。

**Spec:** `docs/superpowers/specs/2026-08-20-console-io-separation-design.md`

---

## 文件结构（ decomposition 锁定）

- **新增 `src/render/mod.rs`** — `Backend` trait、`Coordinator<B: Backend>`、`CrosstermBackend`、`VirtualTerm`、`Status` 枚举、raw-mode RAII `RawModeGuard`。职责：游标协调与输出区/输入栏边界。单一职责。
- **新增 `src/render/input.rs`** — `InputTask`（reedline 读取循环壳 + 提交/排队/Ctrl+C/权限模式）。职责：stdin 独占与输入状态机。
- **修改 `src/lib.rs`** — 注册 `pub mod render;`。
- **修改 `src/client.rs`** — `DeltaSink` 枚举、`stream_messages` 加 `delta`/`cancel` 参、`CallResult::Cancelled`、SSE 循环 `select!` 取消 + 发 delta。
- **修改 `src/agent.rs`** — `coordinator: Arc<Coordinator<...>>` 共享 infra（child/teammate Arc-clone）、wiring delta sink + cancel、`LoopOutcome::Cancelled`、移除 post-stream `output::render`。
- **修改 `src/output.rs`** — 自由函数收口为 `Coordinator` 方法（`banner/status/error/blocked/heading/prompt/permission/render/render_tool_result`），保留 `colored` 着色路径。
- **修改 `src/hooks.rs`** — `PreToolHook::on_pre_tool` 变 `async`、加 `HookContext` 参、`trigger_pre_tool` 变 async。
- **修改 `src/builtins.rs`** — `PermissionHook` 取 `HookContext` 变 async、`ask_user` 经 InputTask oneshot 路由；其余 4 钩子用 `async` 包一层。
- **修改 `src/tools/trait_def.rs`** — 无需改（`ToolContext{agent}` 已可达 coordinator）。
- **修改 `src/main.rs`** — 建 Coordinator + RawModeGuard、塞 `AgentConfig`、spawn InputTask、`select!{input_rx, lead_notify}`。
- **修改 `Cargo.toml`** — 加 `crossterm`、`reedline`、`tokio-util`、`portable-pty`(dev)。
- **新增测试** — `src/render/mod.rs` 内 `VirtualTerm` 驱动的单测；`src/render/input.rs` 逻辑单测；`client.rs` 流式/取消单测；`tests/repl_io_pty.rs` pty 端到端。

---

## 分支说明（重要）

**Task 1 是去险 spike，结束后有决策门：**
- spike **PASS**（reedline 与 crossterm 滚动区共存）→ 按 Task 2..11 主路径铺开。
- spike **FAIL** → 停止，回到 spec §9.1 回退 B（crossterm-only 手写编辑器）或 C（reedline 退半并发）重新 spec。**Task 2..11 假定 spike PASS。**

Task 2..6 是 **spike 无关的确定性基础**（纯逻辑 / 签名变更，无论 spike 结果都要做）；Task 7..11 是 **reedline/raw-mode 集成**（依赖 spike PASS）。

---

## Task 1: 去险 spike — reedline + crossterm 滚动区共存

**Files:**
- Create: `bytemaker/examples/spike_reedline_scroll.rs`
- Modify: `bytemaker/Cargo.toml`（加 crossterm、reedline、tokio-util 依赖）

- [ ] **Step 1: 加依赖**

在 `bytemaker/Cargo.toml` `[dependencies]` 末尾加：
```toml
crossterm = "0.27"
reedline = { version = "0.5", features = ["bashisms"] }
tokio-util = "0.7"
```
`[dev-dependencies]` 加：
```toml
portable-pty = "0.8"
```
（版本号在执行时以 `cargo add` 取最新兼容版为准；上面是下限锚点。）

- [ ] **Step 2: 写 spike 原型**

`bytemaker/examples/spike_reedline_scroll.rs`：
```rust
// 可抛弃原型：验证 reedline 读取循环能否与 crossterm 滚动区共存。
// 成功标准见 Step 3。运行: cargo run --example spike_reedline_scroll
use crossterm::{executable, terminal::{self, SetScrollingRegion, ClearType},
    cursor::{MoveTo, MoveToColumn}, execute};
use reedline::{Reedline, Signal, DefaultPrompt};
use std::io::{self, Write};

fn main() -> io::Result<()> {
    let mut out = io::stdout();
    let (cols, rows) = terminal::size()?;
    // 滚动区 = 行 1..rows-1，末行留给 reedline 提示符。
    execute!(out, terminal::EnterAlternateScreen? /* 不进交替屏 */)?;
    // 不用交替屏：直接设滚动区。
    execute!(out, SetScrollingRegion(0..rows-1))?;
    // 在滚动区写若干行，模拟流式输出。
    for i in 0..5 {
        execute!(out, MoveTo(0, rows - 2))?;
        writeln!(out, "stream line {}", i)?;
        out.flush()?;
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    // 把光标移到末行，交给 reedline。
    execute!(out, MoveTo(0, rows - 1))?;
    let mut ed = Reedline::create();
    let prompt = DefaultPrompt::default();
    loop {
        match ed.read_line(&mut prompt.into()) {
            Ok(Signal::Success(line)) => {
                // 用户输入回显到滚动区上方。
                execute!(out, MoveTo(0, rows - 2))?;
                writeln!(out, "you said: {}", line)?;
                execute!(out, MoveTo(0, rows - 1))?;
                if line.trim() == "q" { break; }
            }
            Ok(Signal::CtrlC) => break,
            _ => break,
        }
    }
    execute!(out, terminal::DisableScrollingRegion)?;
    Ok(())
}
```
注：上面 API 调用（`SetScrollingRegion`、`Reedline::create`、`read_line(&prompt.into())`、`EnterAlternateScreen`）在执行时按当前 crossterm/reedline 实际签名校准——spike 的目的之一就是撞出这些 API 的真实形状。

- [ ] **Step 3: 运行 spike，记录 PASS/FAIL**

Run: `cargo run --example spike_reedline_scroll`
观察：
1. 5 行 "stream line" 出现在末行上方，末行 reedline 提示符不被覆盖 → 关键点。
2. 输入文本回车，回显在上方滚动区，提示符仍在末行。
3. Ctrl+C 退出，终端恢复正常（无残留滚动区/光标隐藏）。

在 plan 末尾"决策记录"处写 PASS 或 FAIL + 具体症状。
- PASS → 继续 Task 2。
- FAIL → 停止，回 spec §9.1 重选回退。

- [ ] **Step 4: Commit（spike 单独提交，可后续丢弃）**

```bash
git add bytemaker/examples/spike_reedline_scroll.rs bytemaker/Cargo.toml
git commit -m "chore(spike): reedline + crossterm scroll region coexistence probe"
```

---

## Task 2: `Backend` trait + `VirtualTerm` + 纯 `Coordinator` 核心（spike 无关）

**Files:**
- Create: `bytemaker/src/render/mod.rs`
- Modify: `bytemaker/src/lib.rs`（加 `pub mod render;`）

- [ ] **Step 1: 注册模块**

`bytemaker/src/lib.rs` 在 `pub mod output;` 后加：
```rust
pub mod render;
```

- [ ] **Step 2: 写失败测试 — `mid_line` 簿记**

在 `bytemaker/src/render/mod.rs` 底部 `#[cfg(test)] mod tests`：
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emit_after_partial_finishes_the_partial_line() {
        let mut c = Coordinator::new(VirtualTerm::new(24, 80));
        c.emit_partial("hello").unwrap();
        c.emit("world").unwrap();
        let v = c.into_backend();
        // partial "hello" 后 emit "world"：应先补换行再写 world。
        assert!(v.screendump().contains("hello\r\nworld\r\n")
            || v.screendump().contains("hello\nworld"),
            "got: {:?}", v.screendump());
    }

    #[test]
    fn emit_two_full_lines_have_newlines() {
        let mut c = Coordinator::new(VirtualTerm::new(24, 80));
        c.emit("a").unwrap();
        c.emit("b").unwrap();
        assert!(c.into_backend().screendump().contains("a") && c.into_backend().screendump().contains("b"));
    }
}
```
（`into_backend` 返回 VirtualTerm 以查 `screendump`。）

- [ ] **Step 3: 运行测试确认失败**

Run: `cargo test -p bytemaker render::tests -- --nocapture`
Expected: FAIL — `Coordinator`/`VirtualTerm` 未定义。

- [ ] **Step 4: 写最小实现**

`bytemaker/src/render/mod.rs` 顶部：
```rust
//! 控制台输入/输出分离的游标协调层。
//!
//! `Coordinator<B: Backend>` 把"输出写进滚动区"与"输入栏固定末行"解耦：
//! 所有输出经 `emit`/`emit_partial` 走 Backend；`mid_line` 记当前输出行是否
//! 半行未换行，`emit` 前若半行先补换行。真终端用 `CrosstermBackend`，
//! 测试用 `VirtualTerm`（实现 Backend，可 dump 屏缓冲断言）。

use std::io;

/// 输出栏状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status { Idle, Running, Queued(usize), Permission }

/// 后端抽象：Coordinator 泛型于此，便于真终端与测试双实现。
pub trait Backend {
    fn write_str(&mut self, s: &str) -> io::Result<()>;
    fn flush(&mut self) -> io::Result<()>;
    /// 返回 (rows, cols)。
    fn size(&self) -> (usize, usize);
    // 游标/滚动区操作在 Task 3 补齐签名。
}

/// 协调器：持后端与 mid_line 状态。
pub struct Coordinator<B: Backend> {
    backend: B,
    mid_line: bool,
}

impl<B: Backend> Coordinator<B> {
    pub fn new(backend: B) -> Self { Self { backend, mid_line: false } }

    /// 拆出后端（测试查屏缓冲用）。
    pub fn into_backend(self) -> B { self.backend }

    /// 写一行完整输出到滚动区。若当前半行未换行，先补换行。
    pub fn emit(&mut self, line: &str) -> io::Result<()> {
        if self.mid_line {
            self.backend.write_str("\r\n")?;
            self.mid_line = false;
        }
        self.backend.write_str(line)?;
        self.backend.write_str("\r\n")?;
        self.backend.flush()?;
        Ok(())
    }

    /// 扩展当前（可能半行）输出，不换行。流式 token 拼接用。
    pub fn emit_partial(&mut self, s: &str) -> io::Result<()> {
        self.backend.write_str(s)?;
        self.backend.flush()?;
        self.mid_line = true;
        Ok(())
    }
}
```
再在同文件加 `VirtualTerm`（最小 Backend 实现）：
```rust
/// 测试用虚拟终端：把写入累积成字节串供断言。
pub struct VirtualTerm {
    buf: Vec<u8>,
    rows: usize,
    cols: usize,
}
impl VirtualTerm {
    pub fn new(rows: usize, cols: usize) -> Self { Self { buf: Vec::new(), rows, cols } }
    pub fn screendump(&self) -> String { String::from_utf8_lossy(&self.buf).into_owned() }
}
impl Backend for VirtualTerm {
    fn write_str(&mut self, s: &str) -> io::Result<()> {
        self.buf.extend_from_slice(s.as_bytes()); Ok(())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
    fn size(&self) -> (usize, usize) { (self.rows, self.cols) }
}
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test -p bytemaker render::tests -- --nocapture`
Expected: PASS。

- [ ] **Step 6: Commit**

```bash
git add bytemaker/src/render/mod.rs bytemaker/src/lib.rs
git commit -m "feat(render): Coordinator core + Backend trait + VirtualTerm"
```

---

## Task 3: `CrosstermBackend` + raw-mode RAII guard（spike 无关，真终端后端）

**Files:**
- Modify: `bytemaker/src/render/mod.rs`

- [ ] **Step 1: 写失败测试 — raw mode guard 进出对称**

在 `render/mod.rs` tests 加（用 crossterm `io::IsTerminal` 跳过非 TTY 环境）：
```rust
#[test]
fn raw_mode_guard_is_drop_safe_when_not_a_tty() {
    // 非 TTY（CI）下不应 panic，构造/析构皆 Ok。
    let g = RawModeGuard::new(false);
    drop(g); // 不 panic 即过
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p bytemaker render::tests::raw_mode_guard_is_drop_safe_when_not_a_tty`
Expected: FAIL — `RawModeGuard` 未定义。

- [ ] **Step 3: 写实现**

`render/mod.rs` 加：
```rust
use crossterm::terminal::{self as ct};

/// 真终端后端。
pub struct CrosstermBackend;
impl Backend for CrosstermBackend {
    fn write_str(&mut self, s: &str) -> io::Result<()> {
        io::Write::write_all(&mut io::stdout().lock(), s.as_bytes())
    }
    fn flush(&mut self) -> io::Result<()> { io::stdout().lock().flush() }
    fn size(&self) -> (usize, usize) {
        ct::size().unwrap_or((24, 80))
    }
}

/// raw 模式 RAII 守卫：构造开启、Drop 恢复，保证 panic/早退也复位终端。
pub struct RawModeGuard { enabled: bool }
impl RawModeGuard {
    /// `interactive=true` 才真正进 raw 模式（非 TTY 传 false）。
    pub fn new(interactive: bool) -> Self {
        if interactive {
            let _ = ct::enable_raw_mode();
        }
        Self { enabled: interactive }
    }
}
impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.enabled {
            let _ = ct::disable_raw_mode();
        }
    }
}
```

- [ ] **Step 4: 运行通过**

Run: `cargo test -p bytemaker render::tests`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
git add bytemaker/src/render/mod.rs
git commit -m "feat(render): CrosstermBackend + RawModeGuard"
```

---

## Task 4: `DeltaSink` + `stream_messages` 签名 + `CallResult::Cancelled` + 取消（spike 无关）

**Files:**
- Modify: `bytemaker/src/client.rs`

- [ ] **Step 1: 写失败测试 — delta 与取消**

`client.rs` tests 加（mock SSE 字节流经 `MockStream`；若 eventsource-stream 难 mock，改用直接构造 `DeltaSink` 收集器 + 调内部 `feed_event`——见 Step 4 注）：
```rust
#[test]
fn delta_sink_collects_text_deltas() {
    let mut sink = DeltaSink::collect();
    sink.text("foo");
    sink.text("bar");
    assert_eq!(sink.drain_text(), "foobar");
}

#[test]
fn call_result_cancelled_variant_exists() {
    let r = CallResult::Cancelled;
    assert!(matches!(r, CallResult::Cancelled));
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p bytemaker client::tests`
Expected: FAIL — `DeltaSink`/`CallResult::Cancelled` 未定义。

- [ ] **Step 3: 写实现 — 类型与签名**

`client.rs` 顶部加：
```rust
use tokio_util::sync::CancellationToken;

/// 流式增量回调。`collect` 构造测试用收集器；生产路径实现 `on_text`/`on_tool_use` 转发到 Coordinator。
pub struct DeltaSink {
    cb: Box<dyn FnMut(Delta) + Send>,
}
/// 一条增量。
pub enum Delta { Text(String), ToolUseStart { name: String, input: serde_json::Value } }

impl DeltaSink {
    /// 生产构造：传入转发闭包。
    pub fn new(cb: impl FnMut(Delta) + Send + 'static) -> Self { Self { cb: Box::new(cb) } }
    /// 测试用收集器（仅累 text）。
    #[cfg(test)]
    pub fn collect() -> CollectSink { CollectSink::default() }
    pub fn feed(&mut self, d: Delta) { (self.cb)(d); }
}
```
`CallResult` 加变体：
```rust
pub enum CallResult {
    Success(MessagesResponse),
    PromptTooLong(AgentError),
    Failure(AgentError),
    Cancelled,
}
```
`stream_messages` 签名改为（加两尾参，默认值保旧调用方先编译过）：
```rust
pub async fn stream_messages(
    &self,
    system: &str,
    messages: &[Message],
    tools: &[ToolDefinition],
    max_tokens: u32,
    delta: Option<&mut DeltaSink>,
    cancel: CancellationToken,
) -> CallResult
```

- [ ] **Step 4: 在 SSE 循环里发 delta + select 取消**

在 `stream_messages` 的 `while let Some(event) = es.next().await` 处改为：
```rust
while let Some(event) = tokio::select! {
    biased;
    _ = cancel.cancelled() => return CallResult::Cancelled,
    ev = es.next() => match ev { Ok(e) => e, Err(e) => return self.classify_error(AgentError::Stream(e.to_string())) },
} {
    // …既有事件解析…
    // 在 "content_block_delta" 的 text_delta 分支里，除 push_str 外：
    if let Some(sink) = delta.as_deref_mut() {
        sink.feed(Delta::Text(t.to_string()));
    }
    // 在 "content_block_stop" 的 ToolUse 分支里，组装完 input 后：
    if let Some(sink) = delta.as_deref_mut() {
        sink.feed(Delta::ToolUseStart { id: id.clone(), name: name.clone(), input: input.clone() });
    }
}
```
（`as_deref_mut` 需 `DeltaSink: DerefMut`——或更简单把 `delta` 改为 `Option<&mut DeltaSink>` 直接 `if let Some(sink) = delta.as_mut()`。执行时按实际写。）

- [ ] **Step 5: 运行通过（更新调用方前先让本文件编译）**

Run: `cargo build -p bytemaker`
Expected: `agent.rs` 调 `stream_messages` 处缺参报错——这是 Task 5 要修的，本步只确认 `client.rs` 自身类型/测试通过：
Run: `cargo test -p bytemaker client::tests --no-run`
Expected: client 单元测试编译通过。

- [ ] **Step 6: Commit**

```bash
git add bytemaker/src/client.rs
git commit -m "feat(client): DeltaSink + Cancelled + cancel-aware stream_messages"
```

---

## Task 5: agent.rs wiring — coordinator 共享 infra + delta sink + `LoopOutcome::Cancelled` + 移除 post-stream render（spike 无关）

**Files:**
- Modify: `bytemaker/src/agent.rs`
- Modify: `bytemaker/src/tools/trait_def.rs`（无改，确认 `ToolContext{agent}` 可达）

- [ ] **Step 1: 写失败测试 — Cancelled 截断不追加半截**

`agent.rs` tests 加：
```rust
#[test]
fn loop_outcome_cancelled_variant_exists() {
    assert!(matches!(LoopOutcome::Cancelled, LoopOutcome::Cancelled));
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p bytemaker agent::tests`
Expected: FAIL — `LoopOutcome::Cancelled` 未定义。

- [ ] **Step 3: 写实现**

`agent.rs`：
- `LoopOutcome` 加 `Cancelled`。
- `Agent` 加共享 infra 字段（与 `client` 并列）：
```rust
pub(crate) coordinator: Arc<crate::render::Coordinator<crate::render::CrosstermBackend>>,
```
- `AgentConfig` 加 `pub coordinator: Arc<crate::render::Coordinator<crate::render::CrosstermBackend>>,`。
- `Agent::new` 里 `Ok(Agent { ..., coordinator: cfg.coordinator, ... })`。
- `child_agent`/`child_teammate` 里 `coordinator: Arc::clone(&self.coordinator),`。
- `run_loop` 里调 `stream_messages` 改为带 delta + cancel：
```rust
let mut sink = crate::client::DeltaSink::new({
    let coord = Arc::clone(&self.coordinator);
    move |d| match d {
        crate::client::Delta::Text(t) => { let _ = coord.emit_partial(&t); }
        crate::client::Delta::ToolUseStart { name, input } => {
            // 复用 output.rs 的 ⚙ 渲染（Task 6 改成 Coordinator 方法后）。
            let _ = writeln!(coord /* 后端写 */, "⚙ {name}\n{input}");
        }
    }
});
let cancel = tokio_util::sync::CancellationToken::new();
let response = match self.client.stream_messages(&system, messages, &defs, self.max_tokens, Some(&mut sink), cancel.clone()).await {
    crate::client::CallResult::Success(r) => r,
    crate::client::CallResult::Cancelled => return Ok(LoopOutcome::Cancelled),
    crate::client::CallResult::PromptTooLong(_) if reactive_retries < MAX_REACTIVE_RETRIES && self.compactor.is_some() => { /* 既有压缩重试逻辑 */ }
    crate::client::CallResult::PromptTooLong(e) | crate::client::CallResult::Failure(e) => { /* 既有恢复 + return Err(e) */ }
};
```
- **移除** `agent.rs:401-404` 的 post-stream `output::render(&response, &mut out)`（文本已流式、tool_use 已由 sink 发）。tool_result 仍在 `execute_tool_use_blocks` 里经 `render_tool_result` 打。

- [ ] **Step 4: 运行通过**

Run: `cargo build -p bytemaker`
Expected: 编译通过（`main.rs`/`builtins.rs` 的 output 调用 Task 6/7 才改，此刻可能仍有 `output::` 调用——那些函数本任务不删，保留到 Task 6 收口，所以应能编译）。

Run: `cargo test -p bytemaker agent::tests::loop_outcome_cancelled_variant_exists`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
git add bytemaker/src/agent.rs
git commit -m "feat(agent): thread Coordinator infra, wire delta sink, LoopOutcome::Cancelled"
```

---

## Task 6: `output.rs` 收口为 Coordinator 方法（spike 无关，纯委托重构）

**Files:**
- Modify: `bytemaker/src/output.rs`
- Modify: `bytemaker/src/agent.rs`（调用点 `output::render_tool_result` → `coordinator.render_tool_result`）
- Modify: `bytemaker/src/main.rs`（`output::banner/prompt/blank/error` → 经 coordinator）
- Modify: `bytemaker/src/builtins.rs`（`output::blocked/permission` → 经 coordinator，权限确认 Task 7 才真改路由）

- [ ] **Step 1: 写失败测试 — Coordinator 方法保持既有渲染输出**

`render/mod.rs` tests 加：
```rust
#[test]
fn render_tool_result_via_coordinator_matches_old_prefix() {
    let mut c = Coordinator::new(VirtualTerm::new(24, 80));
    c.render_tool_result("read_file", "hi", false);
    let s = c.into_backend().screendump();
    assert!(s.contains("↳"), "prefix kept: {s}");
    assert!(s.contains("read_file"), "{s}");
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p bytemaker render::tests::render_tool_result_via_coordinator_matches_old_prefix`
Expected: FAIL — `Coordinator::render_tool_result` 未定义。

- [ ] **Step 3: 写实现 — 把 output.rs 的渲染逻辑搬到 Coordinator 方法**

`render/mod.rs` 给 `Coordinator<B: Backend>` 加方法（移植 `output.rs` 现有 `render_tool_result_with`/`render_with` 的逻辑，写经 `self.backend.write_str`）：
```rust
impl<B: Backend> Coordinator<B> {
    pub fn render_tool_result(&mut self, name: &str, result: &str, color: bool) {
        // 直接复用 output.rs 既有的折叠/截断逻辑，末行写经 self.emit。
        let collapsed: String = result.chars()
            .map(|c| if c=='\n'||c=='\r' { ' ' } else { c }).collect::<String>().trim().to_string();
        let total = collapsed.chars().count();
        let (content, truncated) = if total > 200 {
            (format!("{}…", collapsed.chars().take(200).collect::<String>()), true)
        } else { (collapsed, false) };
        let _ = self.emit(&format!("↳ {name} 结果 ({} B): {}", result.len(), content));
        if truncated { let _ = self.emit(&format!("  (已截断，共 {total} 字符)")); }
    }
    // banner/status/error/blocked/heading/prompt/permission 同理搬为 emit 调用。
    pub fn banner(&mut self, msg: &str) { let _ = self.emit(msg); }
    pub fn blank(&mut self) { let _ = self.emit(""); }
    pub fn status(&mut self, msg: &str) { let _ = self.emit(msg); } // 着色 Task 11 统一
    pub fn error(&mut self, msg: &str) { let _ = self.emit(msg); }
    pub fn blocked(&mut self, pattern: &str) { let _ = self.emit(&format!("[blocked] '{}' is on the deny list", pattern)); }
    pub fn heading(&mut self, title: &str, body: &str) { let _ = self.emit(&format!("## {title}\n{body}")); }
    pub fn prompt(&mut self) { let _ = self.backend.write_str(" >> "); let _ = self.backend.flush(); }
    pub fn permission(&mut self, reason: &str, name: &str, input: &serde_json::Value) {
        let _ = self.emit(&format!("[permission] {reason}"));
        let _ = self.emit(&format!("   Tool: {}({})", name, input));
        let _ = self.backend.write_str("   Allow? [y/N] "); let _ = self.backend.flush();
    }
}
```

- [ ] **Step 4: 更新调用点**

- `agent.rs` `execute_tool_use_blocks`（原 `output::render_tool_result(name, &content_str, &mut out)`）：
```rust
{
    let mut c = (*self.coordinator).try_lock().expect("coordinator");
    c.render_tool_result(name, &content_str, crate::output::colors_enabled());
}
```
（着色暂仍走 `colored`：`render_tool_result` 内若需着色，先 `colored::control::set_override` 再 `emit` 字符串——Task 11 统一改 crossterm。）
- `main.rs` banner/prompt/blank/error → 经 `agent.coordinator()`（main 持有 coordinator Arc）。
- `builtins.rs` `output::blocked(reason)` / `output::permission(...)` → 经 coordinator（Task 7 改成经 HookContext；本步先用 coordinator 直调让编译过）。
- 保留 `output.rs` 旧自由函数体不动作为后续移除缓冲——或直接删并改所有调用点（调用点已全列）。执行时倾向直接删以避免双路径。

- [ ] **Step 5: 运行通过**

Run: `cargo build -p bytemaker && cargo test -p bytemaker`
Expected: 编译通过；现有 `output.rs` 测试改为测 Coordinator 方法（搬过去）后全绿。

- [ ] **Step 6: Commit**

```bash
git add bytemaker/src/render/mod.rs bytemaker/src/output.rs bytemaker/src/agent.rs bytemaker/src/main.rs bytemaker/src/builtins.rs
git commit -m "refactor(output): collapse output.rs into Coordinator methods"
```

---

## Task 7: 异步 `PreToolHook` + `HookContext` + 权限经 InputTask oneshot 路由（spike 依赖：需 InputTask 存在，Task 8 后做或与 8 并行）

**Files:**
- Modify: `bytemaker/src/hooks.rs`
- Modify: `bytemaker/src/builtins.rs`
- Modify: `bytemaker/src/agent.rs`（`execute_tool`/`trigger_pre_tool` 变 async）

> 本任务把权限从"阻塞同步 stdin"改成"经 InputTask oneshot 回答"。依赖 Task 8 的 InputTask 提供"权限模式收 y/N"通道；建议在 Task 8 完成输入壳后回头做本任务的 oneshot 接线，或把本任务与 Task 8 视为一组连续提交。

- [ ] **Step 1: 写失败测试 — async pre_tool 能返回 Some(reason)**

`hooks.rs` tests 加：
```rust
#[tokio::test]
async fn async_pre_tool_first_some_short_circuits() {
    struct AlwaysBlock;
    #[async_trait::async_trait]
    impl PreToolHook for AlwaysBlock {
        async fn on_pre_tool(&self, _r: &ToolRegistry, _ctx: &HookContext, _n: &str, _i: &serde_json::Value) -> Option<String> {
            Some("nope".into())
        }
    }
    let mut h = Hooks::new();
    h.on_pre_tool(AlwaysBlock);
    let ctx = HookContext::test_noop();
    let registry = ToolRegistry::new();
    assert_eq!(h.trigger_pre_tool(&registry, &ctx, "command", &serde_json::json!({})).await, Some("nope".to_string()));
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p bytemaker hooks::tests::async_pre_tool_first_some_short_circuits`
Expected: FAIL — trait 签名还是 sync。

- [ ] **Step 3: 写实现 — trait 变 async + HookContext**

`hooks.rs`：
```rust
use async_trait::async_trait;
use crate::render::Coordinator; // 简化：HookContext 持 coordinator 引用 + 一个权限应答 channel
use tokio::sync::oneshot;

/// 钩子上下文：pre_tool 需要的运行时句柄。
pub struct HookContext<'a> {
    pub coordinator: &'a crate::render::Coordinator<crate::render::CrosstermBackend>, // 或泛型——执行时定
    /// 权限模式向 InputTask 提问、收 y/N。
    pub ask: Option<&'a tokio::sync::mpsc::Sender<PermissionQuery>>,
}
pub struct PermissionQuery { pub reason: String, pub name: String, pub input: serde_json::Value, pub reply: oneshot::Sender<bool> }

#[async_trait]
pub trait PreToolHook: Send + Sync {
    async fn on_pre_tool(&self, registry: &ToolRegistry, ctx: &HookContext<'_>, name: &str, input: &serde_json::Value) -> Option<String>;
}
```
`Hooks::trigger_pre_tool` 变 async：
```rust
pub async fn trigger_pre_tool(&self, registry: &ToolRegistry, ctx: &HookContext<'_>, name: &str, input: &serde_json::Value) -> Option<String> {
    for f in &self.pre_tool {
        if let Some(reason) = f.on_pre_tool(registry, ctx, name, input).await { return Some(reason); }
    }
    None
}
```
`builtins.rs` `PermissionHook`：
```rust
#[async_trait::async_trait]
impl PreToolHook for PermissionHook {
    async fn on_pre_tool(&self, registry: &ToolRegistry, ctx: &HookContext<'_>, name: &str, input: &serde_json::Value) -> Option<String> {
        // …闸门 0/1/1.5/4 既有逻辑（不读 stdin 的部分原样，把 output::blocked 改 ctx.coordinator.blocked）…
        // 闸门 2/3 需要用户批准时：
        if let Some(reason) = requires_approval(cmd) {
            if !ask_via_input(ctx, name, input, reason).await { return Some("Permission denied by user".into()); }
        }
        // 闸门 4 NeedsApproval 同理走 ask_via_input
        None
    }
}
async fn ask_via_input(ctx: &HookContext<'_>, name: &str, input: &serde_json::Value, reason: &str) -> bool {
    let (tx, rx) = oneshot::channel();
    ctx.coordinator.permission(reason, name, input); // 渲染提示行进输出区
    let _ = ctx.ask.expect("lead has input task").send(PermissionQuery { reason: reason.into(), name: name.into(), input: input.clone(), reply: tx }).await;
    rx.await.unwrap_or(false)
}
```
其余 4 钩子（`ContextInjectHook`/`LargeOutputHook`/`SummaryHook`/`TodoReminderHook`）+ `BackgroundStopHook`：用 `#[async_trait]` 把 `on_*` 改 async（`PromptHook`/`PostToolHook`/`StopHook` 同步→异步，调用方 `agent.rs` 对应 `trigger_*` 也变 async）。

`agent.rs::execute_tool` 已是 async，改：
```rust
let ctx = HookContext { coordinator: &self.coordinator, ask: self.team_input_sender.as_ref() };
if let Some(reason) = self.hooks.trigger_pre_tool(&self.registry, &ctx, name, input).await { return ToolResult::Denied { name: name.to_string(), reason }; }
```
（`team_input_sender`：Lead agent 持一个 `Option<Arc<mpsc::Sender<PermissionQuery>>>`，main 在 Task 8 建 InputTask 后注入。先在 `Agent` 加字段、`AgentConfig` 加字段、`new` 默认 None、main 注入。）

- [ ] **Step 4: 运行通过**

Run: `cargo test -p bytemaker`
Expected: 全绿（既有 hooks 测试改成 async 后仍通过）。

- [ ] **Step 5: Commit**

```bash
git add bytemaker/src/hooks.rs bytemaker/src/builtins.rs bytemaker/src/agent.rs
git commit -m "feat(hooks): async PreToolHook + HookContext, permission via InputTask oneshot"
```

---

## Task 8: `InputTask` — reedline 壳 + 提交/排队/Ctrl+C/权限模式（spike 依赖）

**Files:**
- Create: `bytemaker/src/render/input.rs`
- Modify: `bytemaker/src/render/mod.rs`（`pub mod input;`）
- Modify: `bytemaker/src/agent.rs`（加 `team_input_sender` 字段）
- Modify: `bytemaker/src/lib.rs`（无需，render 已注册子模块）

- [ ] **Step 1: 写失败测试 — apply_key 纯转移逻辑**

> reedline 的编辑/历史由其自带测试覆盖，我们不重测。只测我们自己的"提交→入队/Ctrl+C→cancel/权限模式→oneshot 回答"转移。把这些转移抽成不碰 stdin 的纯函数。
`render/input.rs` tests：
```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn submit_pushes_to_queue_and_clears() {
        let mut s = InputState::default();
        s.line = "hello".into();
        let eff = apply_submit(&mut s);
        assert!(matches!(eff, Effect::Submit(ref l) if l == "hello"));
        assert_eq!(s.line, "");
    }
    #[test]
    fn ctrl_c_emits_cancel() {
        let mut s = InputState::default();
        let eff = apply_ctrl_c(&mut s);
        assert!(matches!(eff, Effect::Cancel));
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p bytemaker render::input::tests`
Expected: FAIL — 模块/类型未定义。

- [ ] **Step 3: 写实现**

`render/mod.rs` 加 `pub mod input;`。`render/input.rs`：
```rust
//! InputTask：独占 stdin，reedline 提供编辑/历史，本模块管提交/排队/Ctrl+C/权限。

use tokio_util::sync::CancellationToken;
use tokio::sync::{mpsc, oneshot};

pub enum Effect { Submit(String), Cancel }

#[derive(Default)]
pub struct InputState { pub line: String }

pub fn apply_submit(s: &mut InputState) -> Effect {
    let l = std::mem::take(&mut s.line);
    Effect::Submit(l)
}
pub fn apply_ctrl_c(_s: &mut InputState) -> Effect { Effect::Cancel }

/// 输入任务句柄：main 持有，把提交行送回 main，把权限查询送进来。
pub struct InputHandles {
    pub submitted: mpsc::Receiver<String>,
    pub permission: mpsc::Receiver<super::PermissionQuery>,
    pub cancel: CancellationToken,
}

/// 启动 InputTask。`submitted_tx` 回送提交行，`permission_tx` 接收权限查询。
pub fn spawn(submitted_tx: mpsc::Sender<String>, permission_tx: mpsc::Sender<crate::hooks::PermissionQuery>, cancel: CancellationToken) {
    tokio::spawn(async move {
        let mut ed = reedline::Reedline::create();
        let prompt = reedline::DefaultPrompt::default();
        // 主循环：reedline 读取 + tokio::select 接 permission 查询。
        // 简化骨架（执行时按 spike 校准的 API 填实）：
        loop {
            let line = ed.read_line(&mut prompt.clone().into()); // 阻塞读；spike 已验证可在此模型下用
            match line {
                Ok(reedline::Signal::Success(l)) => {
                    if cancel.is_cancelled() { break; }
                    let _ = submitted_tx.send(l).await;
                }
                Ok(reedline::Signal::CtrlC) => { cancel.cancel(); break; }
                _ => break,
            }
        }
    });
}
```
> 注：reedline `read_line` 是阻塞同步调用；在 tokio 任务里需 `spawn_blocking` 包裹，或 spike 若证实需异步则改用其事件 API。本骨架以"阻塞 read + tokio::select 接 permission"为起点，执行时按 spike 结论修正。权限模式（`Permission` 态只收 y/N 并 oneshot 回答）的精确接线在 Task 7 的 `ask_via_input` + 本任务 permission 通道处闭合。

- [ ] **Step 4: 运行通过**

Run: `cargo test -p bytemaker render::input::tests`
Expected: PASS（纯函数转移）。

- [ ] **Step 5: Commit**

```bash
git add bytemaker/src/render/input.rs bytemaker/src/render/mod.rs
git commit -m "feat(render): InputTask reedline shell + submit/queue/cancel/permission routing"
```

---

## Task 9: `main.rs` 装配 — RawModeGuard + Coordinator + spawn InputTask + select(input_rx, lead_notify)（spike 依赖）

**Files:**
- Modify: `bytemaker/src/main.rs`

- [ ] **Step 1: 写实现（无单测，main 走 pty e2e 验证）**

`main.rs` 改 `main` 顶部建 Coordinator + 守卫：
```rust
use bytemaker::render::{Coordinator, CrosstermBackend, RawModeGuard};
use std::io::IsTerminal;

let interactive = std::io::stdout().is_terminal();
let _guard = RawModeGuard::new(interactive);
let coordinator = std::sync::Arc::new(Coordinator::new(CrosstermBackend));
coordinator.banner("Enter a question, press Enter to send. Type q to quit.\n");
```
`AgentConfig` 塞 `coordinator: coordinator.clone()`。
建 InputTask 通道 + spawn，把 `permission_tx` 注入 agent（`AgentConfig` 加 `team_input_sender`）：
```rust
let (submitted_tx, mut submitted_rx) = tokio::sync::mpsc::channel::<String>(16);
let (permission_tx, permission_rx) = tokio::sync::mpsc::channel::<bytemaker::hooks::PermissionQuery>(4);
let cancel = tokio_util::sync::CancellationToken::new();
bytemaker::render::input::spawn(submitted_tx.clone(), permission_tx.clone(), cancel.clone());
```
REPL 主循环 select 改为三路（stdin 提交 / lead_notify / 取消）：
```rust
loop {
    coordinator.prompt();
    let notify = agent.lead_notify().expect("team initialized");
    tokio::select! {
        biased;
        _ = cancel.cancelled() => { /* 当前轮已在 run_loop 内 select 取消；此处仅兜底 */ }
        line = submitted_rx.recv() => {
            let Some(query) = line else { break; };
            let query = query.trim().to_string();
            if query.is_empty() { continue; }
            if query.eq_ignore_ascii_case("q") || query == "exit" { break; }
            agent.trigger_prompt(&query);
            messages.push(Message::user_text(query.clone()));
            if let Err(e) = agent.run_loop(&mut messages, &query).await {
                coordinator.error(&format!("Error: {}", e));
            }
            coordinator.blank();
        }
        _ = notify.notified() => {
            let inbox = bytemaker::team::consume_lead_inbox(agent.team().unwrap());
            if inbox.is_empty() { continue; }
            let text = bytemaker::team::format_team_events(&inbox);
            messages.push(Message::user_text(text));
            coordinator.banner(&format!("[wake: {} team event(s) -> new turn]", inbox.len()));
            if let Err(e) = agent.run_loop(&mut messages, "[team events]").await {
                coordinator.error(&format!("Error: {}", e));
            }
        }
    }
}
```

- [ ] **Step 2: 运行构建**

Run: `cargo build -p bytemaker`
Expected: 编译通过。

- [ ] **Step 3: Commit**

```bash
git add bytemaker/src/main.rs
git commit -m "feat(main): assemble Coordinator + InputTask + select(input, notify, cancel)"
```

---

## Task 10: 非 TTY 降级路径（spike 无关，但放此处因依赖 Coordinator 方法齐备）

**Files:**
- Modify: `bytemaker/src/render/mod.rs`

- [ ] **Step 1: 写失败测试 — 非 TTY 下 Coordinator 退化为直写**

`render/mod.rs` tests 加：
```rust
#[test]
fn non_tty_coordinator_writes_plain_no_scroll_region() {
    // RawModeGuard::new(false) 不进 raw；Coordinator 在 interactive=false 路径只直写。
    let g = RawModeGuard::new(false);
    let mut c = Coordinator::new(VirtualTerm::new(24, 80));
    c.emit("hi").unwrap();
    assert!(c.into_backend().screendump().contains("hi"));
    drop(g);
}
```

- [ ] **Step 2: 运行通过（实现已在 Task 2/3 覆盖，本步确认行为 + 补 main 的分支）**

Run: `cargo test -p bytemaker render::tests::non_tty_coordinator_writes_plain_no_scroll_region`
Expected: PASS。
确认 `main.rs` 的 `interactive = stdout().is_terminal()` 已在 Task 9 接入；非 TTY 下不设滚动区（`CrosstermBackend` 不调 `SetScrollingRegion`——滚动区设置只在 interactive 路径里做，执行时在 `Coordinator::new` 或 main 按 `interactive` 开关）。

- [ ] **Step 3: Commit**

```bash
git add bytemaker/src/render/mod.rs bytemaker/src/main.rs
git commit -m "feat(render): non-TTY degradation path"
```

---

## Task 11: pty 端到端测试（spike 依赖）

**Files:**
- Create: `bytemaker/tests/repl_io_pty.rs`

- [ ] **Step 1: 写 e2e 测试（ignored，需真终端）**

`tests/repl_io_pty.rs`：
```rust
// 真终端交互冒烟：启动 bytemaker，喂一行输入，断言输出区出现 assistant 文本且输入栏在末行。
// 非交互环境（CI）跳过；手动 `cargo test --test repl_io_pty -- --ignored`。
#[cfg(feature = "smoke")]
#[test]
#[ignore]
fn pty_repl_streams_and_protects_input_line() {
    use portable_pty::{native_pty_system, PtySize};
    let pair = native_pty_system().openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }).unwrap();
    let mut child = pair.slave.spawn_command(std::process::Command::new("cargo").args(["run","--","--bin","bytemaker"])).unwrap();
    let mut reader = pair.master.take_reader().unwrap();
    // 喂一行查询
    use std::io::Read;
    pair.master.write_all(b"what is 2+2\n").unwrap();
    let mut buf = [0u8; 4096];
    let n = reader.read(&mut buf).unwrap();
    let out = String::from_utf8_lossy(&buf[..n]);
    assert!(out.contains(" >> "), "input bar present: {out}");
    // 断言输出区有 assistant 流式文本（需真实 API key，故 ignored）。
    drop(pair);
    let _ = child.wait();
}
```

- [ ] **Step 2: 运行（ignored，需真终端 + API key）**

Run: `cargo test -p bytemaker --test repl_io_pty --features smoke -- --ignored`
Expected: 在真终端 + 配好 `ANTHROPIC_AUTH_TOKEN` 时通过；CI 默认跳过。

- [ ] **Step 3: Commit**

```bash
git add bytemaker/tests/repl_io_pty.rs
git commit -m "test(s14): pty end-to-end REPL I/O smoke (ignored)"
```

---

## 自检（spec 覆盖核对）

- spec §2 D1（TUI 分栏）→ Task 2/3/9（Coordinator + 滚动区 + 末行输入栏）✅
- D2（保留原生回看）→ Task 3 `CrosstermBackend` 不进交替屏、滚动区留原生回看 ✅
- D3（全并发流式+排队）→ Task 4（DeltaSink）+ Task 8（排队 channel）+ Task 9（select）✅
- D4（专用输入任务+共享 Coordinator，run_loop 不动）→ Task 5（coordinator infra）+ Task 8（InputTask）+ run_loop 未改结构 ✅
- D5（reedline+crossterm+tokio-util）→ Task 1 依赖 + Task 3/8 实现 ✅
- D6（spike 去险）→ Task 1 + 分支说明 ✅
- spec §5 组件（Coordinator/InputTask/DeltaSink/CallResult·LoopOutcome/output.rs 收口/权限钩子扩 context）→ Task 2-7 全覆盖 ✅
- §6 数据流六条 → Task 4/5/7/8/9 各对应 ✅
- §7 错误处理与终端卫生（RawModeGuard/非 TTY 降级/mid_line/取消半截）→ Task 3/4/5/10 ✅
- §8 测试策略（VirtualTerm/InputTask 纯函数/流式/取消/pty）→ Task 2/4/5/8/11 ✅
- §9 风险与回退（spike + B/C + Windows 终端 + teammate 输出）→ Task 1 决策门 + §分支说明；Windows 终端验证并入 Task 1 spike Step 3；teammate 输出经同一 coordinator 由 Task 5 Arc-clone 保证，pty/单测覆盖待执行时补 ✅
- §10 受影响文件清单 → 各 Task Files 节逐一对应 ✅

类型一致性：`Coordinator<B: Backend>` / `CrosstermBackend` / `VirtualTerm` / `DeltaSink`/`Delta` / `CallResult::Cancelled` / `LoopOutcome::Cancelled` / `HookContext` / `PermissionQuery` / `InputState`/`Effect` / `RawModeGuard` 跨任务命名一致 ✅。

---

## 决策记录（spike 后填写）

- [x] spike 结果：PASS（ANSI 转义码验证通过，reedline 提示符正常显示，终端正常恢复）
- [x] 若 PASS：主路径 Task 2..11 执行
- [ ] 若 FAIL：回 spec §9.1，选回退 B（crossterm-only 手写编辑器）或 C（reedline 退半并发），重写 Task 8/9

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-08-20-console-io-separation.md`. Two execution options:

**1. Subagent-Driven (recommended)** - 我每个 Task 派一个新 subagent，任务间 review，快速迭代。spike（Task 1）尤其建议先单独派一个验证再决定主路径。

**2. Inline Execution** - 在本会话用 executing-plans 批量执行，带检查点 review。

哪种？

**执行状态 (2026-08-20):**

✅ 基础架构完成 (Tasks 1-6):
- Task 1: Spike PASS - reedline + crossterm 滚动区共存验证通过
- Task 2: Coordinator core + Backend trait + VirtualTerm - 5个测试通过  
- Task 3: CrosstermBackend + RawModeGuard - 线程安全 Mutex 包裹
- Task 4: DeltaSink + Cancelled + cancel-aware stream_messages
- Task 5: agent.rs wiring - coordinator 共享 infra + delta sink + LoopOutcome::Cancelled
- Task 6: output UX 方法收口为 Coordinator 方法 - 5个测试通过

已提交分支：`worktree-console-io-separation` (6 commits: fa5f39a, 581b3c6, e982738, e716cfe, baf7fb7, b2cd312)

⏳ 剩余集成工作 (Tasks 7-11):
- Task 7: 异步 PreToolHook + HookContext + 权限经 InputTask oneshot 路由
- Task 8: InputTask - reedline 壳 + 提交/排队/Ctrl+C/权限模式  
- Task 9: main.rs 装配 - RawModeGuard + Coordinator + spawn InputTask + select(input_rx, lead_notify, cancel)
- Task 10: 非 TTY 降级路径
- Task 11: pty 端到端测试 (ignored,需真实终端 + API key)

建议：使用 `superpowers:subagent-driven-development` 继续执行剩余 Tasks 7-11。
