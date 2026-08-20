// 可抛弃原型：验证 reedline 读取循环能否与 crossterm 滚动区共存。
// 成功标准见 Step 3。运行: cargo run --example spike_reedline_scroll
use crossterm::{
    cursor::{MoveTo},
    terminal::{self, enable_raw_mode, disable_raw_mode},
    execute,
};
use reedline::{Reedline, Signal, DefaultPrompt};
use std::io::{self, Write};

fn main() -> io::Result<()> {
    let mut out = io::stdout();
    let (cols, rows) = terminal::size()?;

    // 进入 raw 模式
    enable_raw_mode()?;

    // 使用 ANSI 转义码设置滚动区
    // ESC[<top>;<bottom>r 设置滚动区域
    let scroll_bottom = rows - 1;  // 末行留给 reedline
    write!(out, "\x1b[0;{}r", scroll_bottom)?;
    out.flush()?;

    // 在滚动区写若干行，模拟流式输出。
    // 直接写内容，让滚动区自动滚动
    for i in 0..5 {
        writeln!(out, "stream line {}", i)?;
        out.flush()?;
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    // 把光标移到末行，交给 reedline。
    execute!(out, MoveTo(0, rows - 1))?;
    let mut ed = Reedline::create();
    let prompt = DefaultPrompt::default();
    loop {
        match ed.read_line(&prompt) {
            Ok(Signal::Success(line)) => {
                // 用户输入回显到滚动区上方。
                execute!(out, MoveTo(0, rows - 2))?;
                writeln!(out, "you said: {}", line)?;
                execute!(out, MoveTo(0, rows - 1))?;
                if line.trim() == "q" { break; }
            }
            Ok(Signal::CtrlC) => break,
            Ok(Signal::CtrlD) => break,
            _ => break,
        }
    }
    // 恢复终端状态 - 禁用滚动区
    write!(out, "\x1b[r")?;
    out.flush()?;
    disable_raw_mode()?;
    Ok(())
}