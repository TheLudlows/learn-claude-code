//! InputTask：独占 stdin 的输入任务。
//!
//! reedline 的 `read_line` 是阻塞同步调用，无法与 tokio 的 `select!` 交错，
//! 故 InputTask 跑在一个独立 OS 线程上，经单一 `tokio::sync::mpsc` 命令通道
//! 串行处理三类命令：`ReadLine`（读一行用户查询）、`AskPermission`（读 y/N
//! 回答 pre-tool 钩子的权限确认）、`Shutdown`。
//!
//! 关键不变量：用户查询回合期间（main 在 `run_loop` 里）main 不会下发
//! `ReadLine`，故线程处于空闲（`blocking_recv`）。此时 pre-tool 钩子经
//! `ask` 通道发来的 `AskPermission` 能被线程立刻接收并回答——不会与"读
//! 下一行查询"互相阻塞。
//!
//! `apply_submit` / `apply_ctrl_c` 是不碰 stdin 的纯转移函数，便于单测；
//! reedline 自带编辑/历史，不在此重测。
//!
//! 终端卫生：reedline 的 `read_line` 每次进出会自行 `enable_raw_mode` /
//! `disable_raw_mode`（瞬态，仅读期间 raw 开）。逐行 I/O 模型下，输出在
//! 回合内（cooked 模式，`\n` 正常）流式渲染；读期间不产生输出，故无需
//! 滚动区 / 末行锚定，`move_to_input_line` 已移除。

use std::borrow::Cow;

use tokio::sync::{mpsc, oneshot};

use crate::hooks::PermissionQuery;

/// 纯转移效果（不碰 stdin，便于单测）。
pub enum Effect {
    Submit(String),
    Cancel,
}

#[derive(Default)]
pub struct InputState {
    pub line: String,
}

/// 提交当前行：取出缓冲、清空、产出 `Effect::Submit`。
pub fn apply_submit(s: &mut InputState) -> Effect {
    let l = std::mem::take(&mut s.line);
    Effect::Submit(l)
}

/// Ctrl+C：产出 `Effect::Cancel`（不改缓冲）。
pub fn apply_ctrl_c(_s: &mut InputState) -> Effect {
    Effect::Cancel
}

/// 命令线程的输入命令。main 与 pre-tool 钩子经同一通道下发。
pub enum InputCmd {
    /// 读一行用户输入。`Some(line)` 为提交行；`None` 为 EOF / Ctrl+C 取消。
    ReadLine(oneshot::Sender<Option<String>>),
    /// pre-tool 钩子请求权限确认。提示行已由 `Coordinator::permission` 渲染，
    /// 线程只负责读一行并经 `reply` 回答 y/N。
    AskPermission(PermissionQuery),
    /// 关闭线程。
    Shutdown,
}

/// REPL 提示符：左段 ` >> `，其余段空（指标行交给 reedline 默认空白）。
struct ReplPrompt;

impl reedline::Prompt for ReplPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed(" >> ")
    }
    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_indicator(&self, _mode: reedline::PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_history_search_indicator(
        &self,
        _search: reedline::PromptHistorySearch,
    ) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
}

/// 读一行用户输入。`Success` → `Some(line)`；其余（CtrlC/CtrlD/Err/…）→ `None`。
fn read_line(ed: &mut reedline::Reedline, prompt: &ReplPrompt) -> Option<String> {
    match ed.read_line(prompt) {
        Ok(reedline::Signal::Success(line)) => Some(line),
        _ => None,
    }
}

/// 读一行权限确认。`y`（大小写不敏感）→ `true`；其余 → `false`。
fn read_permission(ed: &mut reedline::Reedline, prompt: &ReplPrompt) -> bool {
    match ed.read_line(prompt) {
        Ok(reedline::Signal::Success(l)) => l.trim().eq_ignore_ascii_case("y"),
        _ => false,
    }
}

/// 启动 InputTask：返回命令发送端。线程独占 stdin + reedline，串行处理命令。
///
/// 调用方（main）持 `Sender` 下发 `ReadLine`；pre-tool 钩子持同一 `Sender` 的
/// 克隆（`Agent::team_input_sender`）下发 `AskPermission`。线程在命令间空闲，
/// 故权限确认不会与读取下一行查询互相阻塞。
pub fn spawn() -> mpsc::Sender<InputCmd> {
    let (tx, mut rx) = mpsc::channel::<InputCmd>(64);
    let _ = std::thread::Builder::new()
        .name("bytemaker-input".into())
        .spawn(move || {
            let mut ed = reedline::Reedline::create();
            let prompt = ReplPrompt;
            while let Some(cmd) = rx.blocking_recv() {
                match cmd {
                    InputCmd::ReadLine(reply) => {
                        let res = read_line(&mut ed, &prompt);
                        let is_exit = res.is_none();
                        let _ = reply.send(res);
                        if is_exit {
                            break;
                        }
                    }
                    InputCmd::AskPermission(q) => {
                        let answer = read_permission(&mut ed, &prompt);
                        let _ = q.reply.send(answer);
                    }
                    InputCmd::Shutdown => break,
                }
            }
        });
    tx
}

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
