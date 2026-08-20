# bytemaker 控制台输入/输出分离 — 设计

- 日期: 2026-08-20
- 分支: `refactor/agent-object-abstraction`
- 状态: 已与用户确认设计方向，待实现计划
- 关联代码: `bytemaker/src/{main,output,agent,client,builtins,hooks}.rs`

## 1. 背景与目标

bytemaker 当前是一个 tokio REPL（`main.rs`）。所有输出经 `output.rs` 写 `stdout`，
所有输入读 `stdin`，两者共用同一条终端流，没有"受保护的输入行"。具体痛点：

- `run_loop`（`agent.rs:314`）跑的时候（LLM 调用 + 工具执行 + 多个子轮），用户若提前
  敲下一句，字符和滚动的输出交叠；一轮结束后 `output::prompt()` 重新写 ` >> `，刚才
  打了一半的字悬在新提示符上方。
- `output::prompt()` 写完不换行就 `flush` 等输入（`output.rs:177`），任何输出都顶到
  同一行。
- 两处 stdin 读取者——主循环异步 `BufReader(stdin).lines()`（`main.rs:64`）与权限
  钩子里同步 `stdin().read_line()`（`builtins.rs:172`）——分属不同抽象层，靠"恰好不
  并发"维持正确，脆弱。
- `client.rs::stream_messages`（`client.rs:246`）把整条响应累加成 `MessagesResponse`
  后整体返回，再由 `agent.rs:403` 一次打印——无实时流式。

**目标**: 把输入和输出在控制台分离，给用户一条"输出永不覆盖"的固定输入行，并支持
实时流式输出与并发排队输入。

## 2. 决策（已与用户确认）

| # | 决策 | 选择 | 关键理由 |
|---|------|------|----------|
| D1 | 总体形态 | **TUI 分栏布局** | 固定底部输入栏 + 上方输出区，输出永不进入输入区 |
| D2 | 渲染地基 | **保留原生回看**（crossterm 原始模式，输出区即真终端滚动） | 不丢终端原生 scrollback；只固定底部输入行 |
| D3 | 并发程度 | **全并发**: 逐 token 实时流式 + agent 工作时可打字、回车排队 | 真正的输入/输出分离；流式时输入行持续受压是设计的考验本身 |
| D4 | 架构方案 | **专用输入任务 + 共享 Coordinator**（run_loop 保持顺序 for 循环不动） | 对 s13 `refactor/agent-object-abstraction` 改动最小、不冒进 |
| D5 | 输入库 | **reedline**（行编辑/历史/补全）+ **crossterm**（原语/滚动区/着色）+ **tokio-util CancellationToken** | 不重复造轮子；ratatui 不用（交替屏杀掉原生回看） |
| D6 | 去险 | **先做半天 spike** 验证 reedline 与 crossterm 滚动区能否共存 | reedline 天然是轮次制，与全并发共存需验证；不成则回退 B/C |

## 3. 非目标（YAGNI）

- 不做整屏交替屏 TUI（不用 ratatui 的主路径）。
- 不做多窗格仪表盘（左侧活动栏等）——两区即可（输出滚动区 + 输入栏 + 输入栏内状态提示）。
- 不做 unicode 宽字符/组合字符的完美光标（spike 走 reedline 自带处理；回退 B 的手写
  编辑器只做 char 级）。
- 不做跨会话持久历史（先内存；reedline `FileBackedHistory` 留作未来开关）。
- 不引入额外权限模型——沿用现有三道闸门，只改其 I/O 通路。

## 4. 架构与并发拓扑

三个 actor，共享一个 `Coordinator`（即设计稿里的 `Renderer`，改名以避免和
`output::render` 混淆）。

```
┌─ main 任务 (持有 Agent + REPL) ──────────────────────┐
│  raw mode 开启 → 建 Coordinator → 塞进 AgentConfig      │
│  spawn InputTask                                        │
│  loop { select! { input_rx → run_loop; lead_notify } }  │
└────────────────────────────────────────────────────────┘
        │ Agent.coordinator (Arc<Coordinator>, 共享 infra)     │
        │ client.stream_messages(.., delta_sink, cancel)     │
        ▼                                                     ▼
┌─ InputTask (独占 stdin) ─┐      ┌─ Coordinator (内部 Mutex 守 stdout+游标状态) ─┐
│ reedline 读取循环          │      │ emit()/emit_partial()/redraw_input()/         │
│ Enter→input_tx / Ctrl+C→  │─────▶│   set_status()                                 │
│ cancel / 重绘输入行       │      │ 输出区=ANSI 滚动区(保留原生回看)               │
└───────────────────────────┘      │ 末行=固定输入栏(区外,永不滚)                  │
                                    └────────────────────────────────────────────────┘
```

- `Coordinator` 作为**共享 infra** 加入 `Agent`（与 `client`/`registry` 并列），
  `child_agent`/`child_teammate` `Arc::clone` 之——子 agent、teammate 的输出也走同一
  Coordinator，不旁路 stdout。契合 s13 消除全局单例的方向（**不重新引入 OnceLock**）。
- `run_loop` **保持顺序 for 循环不动**；流式靠回调注入、取消靠 `CancellationToken`
  传入 `stream_messages`，SSE 循环内 `tokio::select!` 在 `es.next()` 与
  `token.cancelled()` 之间。
- `stream_messages` 签名加两个尾参：`delta: Option<&mut DeltaSink>`、
  `cancel: CancellationToken`。

## 5. 组件清单

### 5.1 `Coordinator`（新，`src/render/mod.rs`）
- 持 `StdoutLock`（raw 模式）、`input_line/input_cursor/input_status`（供重绘）、
  `mid_line: bool`（当前输出是否半行未换行，定位用）。
- 核心 `emit(line)`：在滚动区内写 `\r\n` → 终端原生滚动 → 重绘区外输入栏 → 还原光标。
- `emit_partial(s)`：扩展当前半行（流式 token 拼接用）。
- `redraw_input()`：清输入栏行重写 ` >> {line}{status}`。
- `set_status(Idle | Running{queued} | Permission)`。
- 滚动区由 crossterm `SetScrollingRegion` 划定：行 1..N-1 输出区、行 N 输入栏。
- 非 TTY 降级：`stdout` 非 `IsTerminal` 时跳过 raw 模式与滚动区，退化为直接
  `println!`/`write!`，行为对齐现状（保 `output.rs` 现有测试与 CI 不破）。

### 5.2 `InputTask`（新，`src/render/input.rs`）
- tokio 任务，持 `Arc<Coordinator>` clone 以重绘输入栏；跑 reedline 的读取循环
  （reedline 提供行编辑/历史/补全/emacs-vi）。
- 提交行 → `input_tx.send(line)` + 入历史 + 清行。
- Ctrl+C → `cancel.cancel()` + 清行 + 重绘。
- 权限模式：Coordinator `set_status(Permission)` 时只收 y/N，经 oneshot 回钩子。
- 运行中可继续打字；Enter 在 `Running` 态也收，入带缓冲 channel，状态栏显
  "运行中排队 N"。main 的 `select!` 在 run_loop 返回后立即取队首开下一轮。
- reedline 只碰第 N 行；流式输出写到 1..N-1 区内，互不覆盖（见第 6 节风险）。

### 5.3 `DeltaSink`（新，`client.rs` 或 `render/mod.rs`）
- 枚举 `Text(String)` / `ToolUseStart{name, input}`。
- `stream_messages` 在 `content_block_delta(text_delta)` 调 `Text`、在
  `content_block_stop(tool_use)` 调 `ToolUseStart`。
- agent 把 sink 转发到 `coordinator.emit_partial` / `emit`。
- 取消后不追加半截 assistant 内容进 `messages`。

### 5.4 `CallResult`/`LoopOutcome` 扩展
- `CallResult` 加 `Cancelled` 变体；`LoopOutcome` 加 `Cancelled`。
- agent 在 `Cancelled` 时截断本轮、不追加未完成内容。

### 5.5 `output.rs` 收口到 Coordinator
- `banner/status/error/blocked/heading/prompt/permission/render/render_tool_result` 改为
  `Coordinator` 的方法（或取 `&Coordinator` 的自由函数）。
- 调用点有限（main/builtins/agent/todo），逐一改经 `agent.coordinator()` 或 main 局部
  `coordinator`。
- 着色：默认 crossterm `style` 统一并替掉 `colored`（见 §11）；仅当迁移成本过高时
  保留 `colored`，届时 Coordinator 适配两条着色路径。

### 5.6 权限钩子扩 context
- `PreToolHook::on_pre_tool` 现拿 `&ToolRegistry`；为让权限走 Coordinator + InputTask
  （而非 `builtins.rs:172` 阻塞 `stdin.read_line`），扩展为拿 `&HookContext`
  （含 `coordinator` + 输入应答 oneshot）。
- `PermissionHook` 渲染提示行进输出区，把输入栏切"权限模式"收 y/N，经 oneshot 取答
  ——钩子变 async。
- `TeammatePermissionHook` 仍非交互直接拒。

## 6. 数据流

- **正常轮**: main 从 `input_rx` 收查询 → `agent.run_loop` → `stream_messages` 逐 delta
  经 `coordinator.emit_partial` 实时打字、tool_use 经 `emit` 打 `⚙`、tool_result 经
  `render_tool_result` 打 `↳`。结束设 `Idle` 重绘输入栏。
- **流式 + 并发打字**: 流式在 main 任务栈内推进（`stream_messages` 的 `.await` 间
  yield），InputTask 独立轮询按键、只重绘输入栏；两者经 `Coordinator` 的 Mutex 串行化
  stdout，输入栏始终在区外不受覆盖。
- **排队提交**: InputTask 在 `Running` 态也收 Enter，整行入 `input_rx`（带缓冲
  channel），状态栏显排队数；main 在 run_loop 返回后立即取队首开下一轮。
- **权限确认**: 钩子渲染 `[permission]` 行进输出区 →
  `coordinator.set_status(Permission)` → InputTask 进权限模式只收 y/N → oneshot 回
  钩子继续/拦截。
- **Ctrl+C 中断**: InputTask `cancel.cancel()`；`stream_messages` 内 `select!` 命中
  `token.cancelled()` → 返回 `Cancelled` → run_loop 截断本轮 → 回 main 等下一输入。
- **团队/cron 事件**: `lead_notify` 与 `[cron]` 仍经 `coordinator.emit` 进输出区；
  `[wake: N]` 行同。事件可在 run_loop 进行时到达，入 `input_rx` 之外的通知队列，
  轮空后处理。

## 7. 错误处理与终端卫生

- **raw mode 守卫**: `Coordinator::new` 开启 raw 模式，`Drop` 里恢复 cooked + 关滚动区
  + 显示光标，确保 panic/早退也复位终端。RAII guard 类型，不靠裸 `disable_raw_mode`
  散落。
- **非 TTY 降级**: `stdout` 非 `IsTerminal` 时（CI/管道/测试）跳过 raw 模式与滚动区，
  `Coordinator` 退化为直接 `println!`/`write!`，行为对齐现状。`NO_COLOR` 仍遵守。
- **半行流式**: `mid_line` 标记当前输出行未换行；`emit` 前若半行先补换行；输入栏重绘
  基于行首坐标，不依赖半行长度，杜绝错位。
- **取消后半截**: `Cancelled` 时不把未完成的 assistant 内容追加进 `messages`，避免下
  一轮拿到残破 tool_use。

## 8. 测试策略

- **Coordinator 单测**: 实现 `VirtualTerm`（`Vec<u8>` 屏缓冲 + 游标坐标 + 行宽），
  `Coordinator` 写往它而非真 stdout。断言：`emit` 在半行后正确补换行、输入栏始终位于
  末行区外、滚动区外不被写、`redraw_input` 还原光标列。纯函数，确定性。
- **InputTask 逻辑单测**: reedline 驱动后的"提交/排队/Ctrl+C/权限模式"转移，喂模拟
  按键序列断言行为，不碰真 stdin（reedline 本身的编辑/历史由其自带测试覆盖，我们不
  重复测）。
- **流式 delta 单测**: 给 `stream_messages` 传收集型 `DeltaSink`（或 mock SSE 字节
  流），断言 text delta 与 tool_use 块边界产出正确的 `Text`/`ToolUseStart` 序列。
- **取消单测**: mock SSE 流 + 提前 `cancel.cancel()`，断言返回 `CallResult::Cancelled`
  且 `messages` 未被追加。
- **端到端**: 现有 `tests/s13_agent_teams.rs` 之外，加 pty 驱动测试（`portable-pty` 或
  conpty）跑真终端交互——非 TTY 路径的集成仍走普通单测覆盖。

## 9. 风险与回退

### 9.1 去险 spike（实现第 0 步）
半天可抛弃原型，单验证一件事：**reedline inline 重绘能否与外部设定的 crossterm 滚动
区共存**。重点测：
- resize/清行时 reedline 是否扰乱滚动区或第 N 行；
- 流式写到 1..N-1 区内时，第 N 行 reedline 提示符是否稳；
- reedline 读取循环能否与 main 任务的流式写入经 Mutex 串行而不死锁。

**成了** → 按全并发铺开（最佳：不造轮子 + 全并发）。
**不成** → 回退：
- **回退 B**: 不用 reedline，crossterm 事件上手写 char 级编辑器，保全并发、依赖最轻，
  代价是行编辑为自造小轮子。
- **回退 C**: 用 reedline 走轮次制/半并发模型（流式输出在轮间，不在输入期），完全不
  造轮子，但放弃"边流式边打字"。

spike 结论回写本节，并据此选定主路径。

### 9.2 其他风险
- **hook 变 async 的连锁**: `PreToolHook` 变 async 会牵动所有实现（5 个内置钩子 +
  `BackgroundStopHook`）。逐个迁移，保持同步实现可用 `async` 直接包一层。
- **teammate 输出**: teammate 在后台运行，其输出也进同一 Coordinator——注意不要让
  teammate 的并发输出和 Lead 的输入栏抢同一行（滚动区设计已解耦，但需测）。
- **Windows 终端**: conpty/Windows Terminal 对滚动区与 raw 模式的支持需在 spike 中顺
  带验证（项目运行于 Windows 11）。

## 10. 受影响文件清单

- 新增: `src/render/mod.rs`（Coordinator）、`src/render/input.rs`（InputTask/reedline 壳）
- `Cargo.toml`: 加 `crossterm`、`reedline`、`tokio-util`（CancellationToken）、
  `portable-pty`（dev）
- `client.rs`: `stream_messages` 签名加 `DeltaSink` + `CancellationToken`；`CallResult`
  加 `Cancelled`；SSE 循环内 `select!` 取消 + 发 delta
- `agent.rs`: `coordinator` 作为共享 infra 字段（Arc-clone 到 child）；wiring 流式 sink
  与 cancel；移除 `agent.rs:403` 的 post-stream `output::render`（文本已流式、tool_use
  已由 sink 发、tool_result 仍 `render_tool_result`）；处理 `LoopOutcome::Cancelled`
- `output.rs`: 自由函数收口为 Coordinator 方法
- `main.rs`: 开 raw 模式 + 建 Coordinator + 塞 `AgentConfig`；spawn InputTask；`select!`
  在 `input_rx` 与 `lead_notify` 之间；RAII guard
- `builtins.rs`: `PermissionHook` 取 `HookContext`、变 async；`ask_user` 由
  InputTask 路由的 y/N 替代
- `hooks.rs`: `PreToolHook` trait 签名扩展（或加 `HookContext` 参数）
- `tools/trait_def.rs`: 确保 `ToolContext` 可达 `coordinator`
- 测试: Coordinator/InputTask/流式/取消单测 + pty 端到端

## 11. 已应用的默认（未逐一问用户，写入请过目）

- 行编辑/历史/emacs-vi 由 reedline 提供（spike 成功前提）。
- Ctrl+C 中断当前流式轮（全并发自然配套）。
- 权限 y/N 复用底部输入行（不另开模态）。
- 输入历史先内存，不跨会话持久。
- 着色优先 crossterm `style` 统一、替掉 `colored`（细节留实现）。
