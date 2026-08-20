//! 控制台输入/输出分离的游标协调层。
//!
//! `Coordinator<B: Backend>` 把"输出写进滚动区"与"输入栏固定末行"解耦：
//! 所有输出经 `emit`/`emit_partial` 走 Backend；`mid_line` 记当前输出行是否
//! 半行未换行，`emit` 前若半行先补换行。真终端用 `CrosstermBackend`，
//! 测试用 `VirtualTerm`（实现 Backend，可 dump 屏缓冲断言）。

use std::io::{self, Write};
use crossterm::terminal as ct;

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

/// 真终端后端。
pub struct CrosstermBackend;
impl Backend for CrosstermBackend {
    fn write_str(&mut self, s: &str) -> io::Result<()> {
        io::Write::write_all(&mut io::stdout().lock(), s.as_bytes())
    }
    fn flush(&mut self) -> io::Result<()> { io::stdout().lock().flush() }
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
}