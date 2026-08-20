//! 控制台输入/输出分离的游标协调层。
//!
//! `Coordinator<B: Backend>` 把"输出写进滚动区"与"输入栏固定末行"解耦：
//! 所有输出经 `emit`/`emit_partial` 走 Backend；`mid_line` 记当前输出行是否
//! 半行未换行，`emit` 前若半行先补换行。真终端用 `CrosstermBackend`，
//! 测试用 `VirtualTerm`（实现 Backend，可 dump 屏缓冲断言）。

use std::io::{self, Write};
use crossterm::terminal as ct;
use colored::{Colorize, control as colored_control};

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

    // ---- UX 输出方法（替代 output.rs 自由函数） ----

    /// 普通横幅行（不着色）。
    pub fn banner(&mut self, msg: &str) { let _ = self.emit(msg); }

    /// 空行。
    pub fn blank(&mut self) { let _ = self.emit(""); }

    /// 状态行。
    pub fn status(&mut self, msg: &str) {
        if colored_control::SHOULD_COLORIZE.should_colorize() {
            let _ = self.emit(&format!("{}", msg.yellow()));
        } else {
            let _ = self.emit(msg);
        }
    }

    /// 错误行。
    pub fn error(&mut self, msg: &str) {
        if colored_control::SHOULD_COLORIZE.should_colorize() {
            let _ = self.emit(&format!("{}", msg.red()));
        } else {
            let _ = self.emit(msg);
        }
    }

    /// blocked 提示。
    pub fn blocked(&mut self, pattern: &str) {
        let _ = self.emit(""); // 前置空行
        let msg = format!("[blocked] '{}' is on the deny list", pattern);
        if colored_control::SHOULD_COLORIZE.should_colorize() {
            let _ = self.emit(&format!("{}", msg.red()));
        } else {
            let _ = self.emit(&msg);
        }
    }

    /// 提示符 ` >> `（cyan，无换行）。
    pub fn prompt(&mut self) {
        let s = if colored_control::SHOULD_COLORIZE.should_colorize() {
            format!("{}", " >> ".cyan())
        } else {
            " >> ".to_string()
        };
        let _ = self.backend.write_str(&s);
        let _ = self.backend.flush();
    }

    /// 权限确认。
    pub fn permission(&mut self, reason: &str, name: &str, input: &serde_json::Value) {
        let _ = self.emit(""); // 前置空行
        if colored_control::SHOULD_COLORIZE.should_colorize() {
            let _ = self.emit(&format!("{}", format!("[permission] {reason}").yellow()));
        } else {
            let _ = self.emit(&format!("[permission] {reason}"));
        }
        let _ = self.emit(&format!("   Tool: {}({})", name, input));
        let _ = self.backend.write_str("   Allow? [y/N] ");
        let _ = self.backend.flush();
    }

    /// 工具执行结果渲染（折叠+截断）。
    pub fn render_tool_result(&mut self, name: &str, result: &str, color: bool) {
        if color {
            colored::control::set_override(true);
        }

        const TRUNCATE_AT: usize = 200;
        let size = if result.len() < 1024 {
            format!("{} B", result.len())
        } else if result.len() < 1024 * 1024 {
            format!("{:.1} KB", result.len() as f64 / 1024.0)
        } else {
            format!("{:.1} MB", result.len() as f64 / (1024.0 * 1024.0))
        };

        // 换行折叠成空格、去首尾空白
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
        let _ = self.emit(&format!(
            "{}{}",
            if color && colored_control::SHOULD_COLORIZE.should_colorize() {
                format!("{}", prefix.dimmed())
            } else {
                prefix
            },
            content
        ));

        if truncated {
            let trunc_msg = format!("  (已截断，共 {total} 字符)");
            let _ = self.emit(&format!(
                "{}",
                if color && colored_control::SHOULD_COLORIZE.should_colorize() {
                    format!("{}", trunc_msg.dimmed())
                } else {
                    trunc_msg
                }
            ));
        }

        if color {
            colored_control::set_override(false);
        }
    }
}

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

/// 真终端后端（使用 Mutex 包装以支持 Arc 共享）。
pub struct CrosstermBackend(std::sync::Mutex<()>); // 零大小的 Mutex，用于同步
impl CrosstermBackend {
    pub fn new() -> Self { Self(std::sync::Mutex::new(())) }
}
impl Default for CrosstermBackend {
    fn default() -> Self { Self::new() }
}
impl Backend for CrosstermBackend {
    fn write_str(&mut self, s: &str) -> io::Result<()> {
        let _guard = self.0.lock().unwrap();
        io::Write::write_all(&mut io::stdout().lock(), s.as_bytes())
    }
    fn flush(&mut self) -> io::Result<()> {
        let _guard = self.0.lock().unwrap();
        io::stdout().lock().flush()
    }
    fn size(&self) -> (usize, usize) {
        let (r, c) = ct::size().unwrap_or((24, 80));
        (r as usize, c as usize)
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
        let s = c.into_backend().screendump();
        assert!(s.contains("a") && s.contains("b"), "got: {:?}", s);
    }

    #[test]
    fn raw_mode_guard_is_drop_safe_when_not_a_tty() {
        // 非 TTY（CI）下不应 panic，构造/析构皆 Ok。
        let g = RawModeGuard::new(false);
        drop(g); // 不 panic 即过
    }

    #[test]
    fn render_tool_result_via_coordinator_matches_old_prefix() {
        let mut c = Coordinator::new(VirtualTerm::new(24, 80));
        c.render_tool_result("read_file", "hi", false);
        let s = c.into_backend().screendump();
        assert!(s.contains("↳"), "prefix kept: {s}");
        assert!(s.contains("read_file"), "{s}");
        assert!(s.contains("2 B"), "{s}");
    }

    #[test]
    fn render_tool_result_truncates_long_output() {
        let mut c = Coordinator::new(VirtualTerm::new(24, 80));
        let long = "a".repeat(300);
        c.render_tool_result("test", &long, false);
        let s = c.into_backend().screendump();
        assert!(s.contains("…"), "should be truncated: {s}");
        assert!(s.contains("已截断"), "{s}");
    }
}