//! LLM 内容打印模块 + 统一 UX 输出。
//!
//! client 只负责收集完整 `MessagesResponse`，不碰 stdout；打印全在这里。
//! `render` / `render_tool_result` 写到任意 `io::Write`，便于测试。
//!
//! 着色走 `owo_colors`（替掉裸 ANSI 转义码）；UX 行（提示符 / 状态 / 权限 / 标题
//! 等）经本模块的 helper 统一写 stdout，自动遵守 `NO_COLOR`。诊断行走 `tracing`
//!（RUST_LOG 控制），不在此处。
//!
//! `render_with(.., color=false)` 路径不输出任何转义码，测试据此做确定性
//! 断言；`NO_COLOR` 置位时公共入口 `render` / `render_tool_result` 自动关色。

use crate::client::{ContentBlock, MessagesResponse};
use owo_colors::OwoColorize;
use std::io::{self, Write};

/// 超过此字符数的结果折叠成单行并截断。
const TRUNCATE_AT: usize = 200;

/// `NO_COLOR` 置位时关掉颜色（https://no-color.org，零依赖，方便 CI）。
fn colors_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none()
}

/// 把字节数格式化成 `42 B` / `1.2 KB` 之类。
fn human_size(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        return format!("{bytes} B");
    }
    if b < KB * KB {
        return format!("{:.1} KB", b / KB);
    }
    format!("{:.1} MB", b / (KB * KB))
}

/// 把 tool_use 的入参格式化成 `key: value` 逐行（2 空格缩进）。
/// Null / 空对象 → 空串（调用方据此跳过打印）；其它非对象退化为 pretty JSON。
fn format_input(input: &serde_json::Value) -> String {
    use serde_json::Value;
    use std::fmt::Write as _; // String 上的 writeln! 走 fmt::Write，与模块级 io::Write 区分
    match input {
        Value::Null => String::new(),
        Value::Object(map) if map.is_empty() => String::new(),
        Value::Object(map) => {
            let mut lines = String::new();
            for (k, v) in map {
                let _ = writeln!(lines, "{k}: {v}");
            }
            lines.trim_end().to_string()
        }
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

// ---- LLM 内容渲染（写到任意 io::Write，可测试） ----

/// 把一轮响应里的 LLM 内容写到 `out`（公共入口，按 `NO_COLOR` 自动开关颜色）。
pub fn render<W: Write>(response: &MessagesResponse, out: &mut W) {
    render_with(response, out, colors_enabled());
}

/// 同 `render`，但颜色由参数控制（测试传 `false` 走无转义码路径）。
pub fn render_with<W: Write>(response: &MessagesResponse, out: &mut W, color: bool) {
    let mut first = true;
    for block in &response.content {
        match block {
            ContentBlock::Text { text } if text.trim().is_empty() => {}
            ContentBlock::Text { text } => {
                if !first {
                    let _ = writeln!(out);
                }
                first = false;
                let bar = if color {
                    format!("{}", "▍ ".dimmed())
                } else {
                    "▍ ".to_string()
                };
                let _ = writeln!(out, "{bar}{text}");
            }
            ContentBlock::ToolUse { name, input, .. } => {
                if !first {
                    let _ = writeln!(out);
                }
                first = false;
                let label = format!("⚙ {name}");
                let head = if color {
                    format!("{}", label.cyan().bold())
                } else {
                    label
                };
                let _ = writeln!(out, "{head}");
                let input_str = format_input(input);
                if !input_str.is_empty() {
                    let dim_input = if color {
                        format!("{}", input_str.dimmed())
                    } else {
                        input_str
                    };
                    let _ = writeln!(out, "{dim_input}");
                }
            }
            ContentBlock::ToolResult { .. } => {}
        }
    }
}

/// 把工具执行结果写到 `out`（公共入口，按 `NO_COLOR` 自动开关颜色）。
pub fn render_tool_result<W: Write>(name: &str, result: &str, out: &mut W) {
    render_tool_result_with(name, result, out, colors_enabled());
}

/// 同 `render_tool_result`，但颜色由参数控制（测试传 `false`）。
pub fn render_tool_result_with<W: Write>(
    name: &str,
    result: &str,
    out: &mut W,
    color: bool,
) {
    let size = human_size(result.len());
    // 换行折叠成空格、去首尾空白；按字符截断，避免截断多字节 UTF-8。
    let collapsed: String = result
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string();
    let total = collapsed.chars().count();
    let (content, truncated) = if total > TRUNCATE_AT {
        let s: String = collapsed.chars().take(TRUNCATE_AT).collect();
        (format!("{s}…"), true)
    } else {
        (collapsed, false)
    };

    let prefix = format!("↳ {name} 结果 ({size}): ");
    let _ = writeln!(out); // 与上方内容空一行
    let _ = write!(
        out,
        "{}",
        if color {
            format!("{}", prefix.dimmed())
        } else {
            prefix
        }
    );
    let _ = write!(out, "{content}");
    let _ = writeln!(out);
    if truncated {
        let trunc_msg = format!("  (已截断，共 {total} 字符)");
        let _ = writeln!(
            out,
            "{}",
            if color {
                format!("{}", trunc_msg.dimmed())
            } else {
                trunc_msg
            }
        );
    }
}

// ---- UX 输出（直接写 stdout，自动遵守 NO_COLOR） ----
//
// 散落在 main / builtins / subagent / todo 里的裸 println + \x1b 全收敛到这儿。
// 交互 UX 行用 owo_colors 着色；诊断行（[memory]/[snip_compact] 等）走 tracing，
// 不在此处。

/// 提示符 ` >> `（cyan，无换行，flush 后由调用方读 stdin）。
pub fn prompt() {
    let mut out = io::stdout().lock();
    let _ = write!(
        out,
        "{}",
        if colors_enabled() {
            format!("{}", " >> ".cyan())
        } else {
            " >> ".to_string()
        }
    );
    let _ = out.flush();
}

/// 普通横幅行（不着色）。
pub fn banner(msg: &str) {
    println!("{msg}");
}

/// 空行。
pub fn blank() {
    println!();
}

/// 黄色状态行（如 `[reactive compact]`、`[Subagent started]`）。
pub fn status(msg: &str) {
    if colors_enabled() {
        println!("{}", msg.yellow());
    } else {
        println!("{msg}");
    }
}

/// dim 的子 agent 工具跟踪行 `[sub] {name} {input:?}`。
pub fn sub_trace(name: &str, input: &serde_json::Value) {
    let line = format!("[sub] {} {:?}", name, input);
    if colors_enabled() {
        println!("{}", line.dimmed());
    } else {
        println!("{line}");
    }
}

/// 权限确认三行：黄 `[permission] {reason}` + `   Tool: {name}({input})` +
/// `   Allow? [y/N] `（末行无换行，flush 后由调用方读 stdin）。
/// 仅 `[permission]` 行着色，与原实现一致。
pub fn permission(reason: &str, name: &str, input: &serde_json::Value) {
    let mut out = io::stdout().lock();
    if colors_enabled() {
        let _ = writeln!(out, "\n{}", format!("[permission] {reason}").yellow());
    } else {
        let _ = writeln!(out, "\n[permission] {reason}");
    }
    let _ = writeln!(out, "   Tool: {}({})", name, input);
    let _ = write!(out, "   Allow? [y/N] ");
    let _ = out.flush();
}

/// 红色 `[blocked] '{pattern}' is on the deny list`（前置空行）。
pub fn blocked(pattern: &str) {
    let msg = format!("[blocked] '{}' is on the deny list", pattern);
    if colors_enabled() {
        println!("\n{}", msg.red());
    } else {
        println!("\n{msg}");
    }
}

/// 红色错误行。
pub fn error(msg: &str) {
    if colors_enabled() {
        println!("{}", msg.red());
    } else {
        println!("{msg}");
    }
}

/// 黄色标题 + 正文：`\n## {title}\n{body}`。
pub fn heading(title: &str, body: &str) {
    if colors_enabled() {
        println!("\n{}\n{}", format!("## {title}").yellow(), body);
    } else {
        println!("\n## {title}\n{body}");
    }
}

#[cfg(test)]
mod tests {
    use super::{render, render_with, render_tool_result_with};
    use crate::client::{ContentBlock, MessagesResponse};

    fn resp(content: Vec<ContentBlock>) -> MessagesResponse {
        MessagesResponse {
            content,
            stop_reason: "tool_use".to_string(),
        }
    }

    #[test]
    fn render_with_prefixes_text_with_block_bar() {
        let response = resp(vec![ContentBlock::Text {
            text: "hello world".to_string(),
        }]);
        let mut buf: Vec<u8> = Vec::new();
        render_with(&response, &mut buf, false);
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("▍ hello world"), "text 应带 ▍ 前缀: {out}");
    }

    #[test]
    fn render_with_skips_blank_text() {
        let response = resp(vec![ContentBlock::Text {
            text: "   ".to_string(),
        }]);
        let mut buf: Vec<u8> = Vec::new();
        render_with(&response, &mut buf, false);
        let out = String::from_utf8(buf).unwrap();
        assert!(!out.contains("▍"), "空白文本不应打印: {out}");
    }

    #[test]
    fn render_with_prints_tool_name_and_indented_input() {
        let response = resp(vec![ContentBlock::ToolUse {
            id: "tu_1".to_string(),
            name: "read_file".to_string(),
            input: serde_json::json!({"path": "/a"}),
        }]);
        let mut buf: Vec<u8> = Vec::new();
        render_with(&response, &mut buf, false);
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("⚙ read_file"), "应打印工具名: {out}");
        assert!(out.contains("path:"), "应打印入参 key: {out}");
        assert!(out.contains("\"/a\""), "应打印入参 value: {out}");
    }

    #[test]
    fn render_with_skips_tool_result_block() {
        let response = resp(vec![
            ContentBlock::Text {
                text: "thinking".to_string(),
            },
            ContentBlock::ToolUse {
                id: "tu_1".to_string(),
                name: "read_file".to_string(),
                input: serde_json::json!({"path": "/a"}),
            },
            ContentBlock::ToolResult {
                tool_use_id: "tu_1".to_string(),
                content: "SECRET_RESULT".to_string(),
            },
        ]);
        let mut buf: Vec<u8> = Vec::new();
        render_with(&response, &mut buf, false);
        let out = String::from_utf8(buf).unwrap();
        assert!(
            !out.contains("SECRET_RESULT"),
            "render 不应打印 tool_result: {out}"
        );
    }

    #[test]
    fn render_with_color_true_emits_ansi_escape() {
        let response = resp(vec![ContentBlock::Text {
            text: "hi".to_string(),
        }]);
        let mut buf: Vec<u8> = Vec::new();
        render_with(&response, &mut buf, true);
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("\x1b["), "color=true 应输出 ANSI 转义码: {out}");
    }

    #[test]
    fn render_delegates_and_prints_text() {
        let response = resp(vec![ContentBlock::Text {
            text: "hi there".to_string(),
        }]);
        let mut buf: Vec<u8> = Vec::new();
        render(&response, &mut buf);
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("▍"), "render 应委托到 render_with: {out}");
        assert!(out.contains("hi there"), "render 应打印文本: {out}");
    }

    #[test]
    fn render_tool_result_with_truncates_long_collapsed_output() {
        let long = format!("first line\nsecond line\n{}", "x".repeat(300));
        let mut buf: Vec<u8> = Vec::new();
        render_tool_result_with("read_file", &long, &mut buf, false);
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("↳"), "应带 ↳ 前缀: {out}");
        assert!(out.contains("结果 ("), "应显示大小: {out}");
        assert!(
            out.contains("first line second line"),
            "换行应折叠成空格: {out}"
        );
        assert!(out.contains("…"), "长输出应截断: {out}");
    }

    #[test]
    fn render_tool_result_with_short_output_not_truncated() {
        let mut buf: Vec<u8> = Vec::new();
        render_tool_result_with("task", "done", &mut buf, false);
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("↳"), "应带 ↳ 前缀: {out}");
        assert!(out.contains("done"), "应包含结果内容: {out}");
        assert!(!out.contains("…"), "短输出不应截断: {out}");
        assert!(!out.contains("截断"), "短输出不应有截断提示: {out}");
    }
}
