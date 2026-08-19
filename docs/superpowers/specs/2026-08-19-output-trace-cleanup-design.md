# 2026-08-19 output-trace-cleanup 设计

## 背景

`rust-agent` 的 `output.rs` 当前用 `owo-colors` 着色，但模块注释里写的是 `colored`（历史不一致）。
`client.rs` 的 `stream_messages` 与 LLM 交互时不打任何 trace，只有 `compact.rs` 的 `[prepare]` 行可见，
调试时看不到请求/响应的轮廓。`render_tool_result_with` 在结果行前故意留了一个空行，用户希望去掉。

## 目标

1. 把着色库从 `owo-colors` 换成更流行的 `colored`。
2. 在 `client.rs` 给 LLM 请求/响应加 info 级 trace（摘要，不打完整 JSON）。
3. 去掉 `render_tool_result_with` 结果行前的空行。

## 非目标

- 不改 `colors_enabled()` 的 `NO_COLOR` 判定逻辑（测试依赖它，且比 `colored` 自带的 TTY 探测更可预测）。
- 不打完整请求/响应 JSON（用户选了摘要档；如需可在后续用 debug 级补）。
- 不动 `[prepare]` 日志本身。

## 设计

### 1. 替换着色库

**文件：** `rust-agent/Cargo.toml`、`rust-agent/src/output.rs`

- `Cargo.toml`：删 `owo-colors = "4"`，加 `colored = "3"`。
- `output.rs`：`use owo_colors::OwoColorize;` → `use colored::Colorize;`。
  两个 trait 的方法名一致（`cyan` / `bold` / `dimmed` / `yellow` / `red`），
  所有 `format!("{}", x.cyan().bold())` 调用点无需改动。
- 模块注释（output.rs:6）里 "着色走 `colored`" 现在与实际一致，无需改。
- `colors_enabled()`（output.rs:21）保持原样：仍由 `NO_COLOR` 控制，公共入口据此开关颜色。

### 2. LLM 请求/响应 trace（info 级）

**文件：** `rust-agent/src/client.rs`，`stream_messages` 内

在 HTTP 调用前后各加一条 `tracing::info!`，风格对齐 `[prepare]`：

- 请求前（构建 `request` 后、`.send()` 前）：
  `[req] model={model}, messages={n}, tools={m}, max_tokens={max_tokens}`
  其中 `n = messages.len()`，`m = tools.len()`。
- 响应后（SSE 解析完成、`Ok(MessagesResponse { .. })` 前）：
  `[resp] stop_reason={stop_reason}, blocks={n}`
  其中 `n = content.len()`。

覆盖所有 5 个 `stream_messages` 调用点（主循环、subagent、memory×3、compact），
无需逐处改。走现有 `tracing_subscriber`（RUST_LOG，默认 INFO），
默认与 `[prepare]` 同显；`RUST_LOG=warn` 静默。

### 3. 去掉结果行前空行

**文件：** `rust-agent/src/output.rs:138`

删除 `render_tool_result_with` 中的 `let _ = writeln!(out);` 及其注释
"与上方内容空一行"。删除后 `task_id: ...` 与 `↳ ... 结果` 连续两行，中间无空行。

现有测试 `render_tool_result_with_short_output_not_truncated`、
`render_tool_result_with_truncates_long_collapsed_output` 不断言前导空行，应保持绿。

## 验证

`cargo build` + `cargo test` + `cargo clippy` 全绿。不实跑 agent（需 API key）。

## 影响范围

- `rust-agent/Cargo.toml`：依赖增删。
- `rust-agent/src/output.rs`：import 换、删一行 `writeln!`。
- `rust-agent/src/client.rs`：加两条 `tracing::info!`。
