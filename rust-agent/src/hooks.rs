/*
hooks.rs - 钩子系统 (s04)

循环不把扩展逻辑写进体内, 而是在四个固定节点上触发回调:
  UserPromptSubmit  用户输入提交后、进入 LLM 前
  PreToolUse        工具执行前 (s03 的权限检查移到这里)
  PostToolUse       工具执行后
  Stop              循环即将退出时

返回值语义:
  PreToolUse  返回 Some(reason) -> 阻止本次工具, reason 直接当 tool_result
  Stop        返回 Some(msg)    -> 注入 msg 并继续循环, 不退出
  UserPromptSubmit / PostToolUse 的返回值不参与控制流。

回调用裸 fn 指针(Copy、零开销), 对应 Python "按名注册函数" 的风格,
也免去 Box<dyn Fn> 的堆分配与 Send/Sync 约束。循环只调 trigger_*,
具体逻辑全在回调里 —— 这正是 s04 的要点。
*/

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::client::Message;
use crate::tools::workdir;

// ---- 回调类型 ----
pub type PromptHook = fn(&str);
pub type PreToolHook = fn(&str, &serde_json::Value) -> Option<String>;
pub type PostToolHook = fn(&str, &serde_json::Value, &str) -> Option<String>;
pub type StopHook = fn(&[Message]) -> Option<String>;

/// 钩子注册表: 事件 -> 回调列表。
#[derive(Default)]
pub struct Hooks {
    user_prompt: Vec<PromptHook>,
    pre_tool: Vec<PreToolHook>,
    post_tool: Vec<PostToolHook>,
    stop: Vec<StopHook>,
}

impl Hooks {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_prompt(&mut self, f: PromptHook) {
        self.user_prompt.push(f);
    }
    pub fn on_pre_tool(&mut self, f: PreToolHook) {
        self.pre_tool.push(f);
    }
    pub fn on_post_tool(&mut self, f: PostToolHook) {
        self.post_tool.push(f);
    }
    pub fn on_stop(&mut self, f: StopHook) {
        self.stop.push(f);
    }

    /// 用户输入后、进入 LLM 前触发。返回值不参与控制流。
    pub fn trigger_prompt(&self, query: &str) {
        for f in &self.user_prompt {
            f(query);
        }
    }

    /// 工具执行前触发。第一个返回 Some(reason) 的回调短路 -> 该工具被拦截。
    pub fn trigger_pre_tool(&self, name: &str, input: &serde_json::Value) -> Option<String> {
        for f in &self.pre_tool {
            if let Some(reason) = f(name, input) {
                return Some(reason);
            }
        }
        None
    }

    /// 工具执行后触发。返回值不参与控制流。
    pub fn trigger_post_tool(&self, name: &str, input: &serde_json::Value, output: &str) {
        for f in &self.post_tool {
            f(name, input, output);
        }
    }

    /// 循环即将退出时触发。返回 Some(msg) -> 注入 msg 并继续, 不退出。
    pub fn trigger_stop(&self, messages: &[Message]) -> Option<String> {
        for f in &self.stop {
            if let Some(msg) = f(messages) {
                return Some(msg);
            }
        }
        None
    }
}

/// 自上次 todo_write 以来的轮次计数器
static ROUNDS_SINCE_TODO: AtomicUsize = AtomicUsize::new(0);

/// PostToolUse: 在 3 轮未使用 todo_write 时注入提醒
pub fn todo_reminder_hook(
    name: &str,
    _input: &serde_json::Value,
    _output: &str,
) -> Option<String> {
    if name == "todo_write" {
        ROUNDS_SINCE_TODO.store(0, Ordering::SeqCst);
        None
    } else {
        let count = ROUNDS_SINCE_TODO.fetch_add(1, Ordering::SeqCst) + 1;
        if count >= 3 {
            ROUNDS_SINCE_TODO.store(0, Ordering::SeqCst);
            Some("<reminder>Update your todos.</reminder>".to_string())
        } else {
            None
        }
    }
}

// ---- 示例 hook (权限检查见 permission::permission_hook) ----

/// UserPromptSubmit: 记录当前工作目录。
pub fn context_inject_hook(_query: &str) {
    println!(
        "\x1b[90m[HOOK] UserPromptSubmit: working in {}\x1b[0m",
        workdir().display()
    );
}

/// PreToolUse: 记录每次工具调用。
pub fn log_hook(name: &str, input: &serde_json::Value) -> Option<String> {
    let preview: String = input
        .as_object()
        .map(|o| {
            o.values()
                .take(2)
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let preview: String = preview.chars().take(60).collect();
    println!("\x1b[90m[HOOK] {}({})\x1b[0m", name, preview);
    None
}

/// PostToolUse: 输出过大时提醒。
pub fn large_output_hook(name: &str, _input: &serde_json::Value, output: &str) -> Option<String> {
    if output.len() > 100_000 {
        println!(
            "\x1b[33m[HOOK] Large output from {}: {} chars\x1b[0m",
            name,
            output.len()
        );
    }
    None
}

/// Stop: 收尾统计本轮用过的工具次数。
pub fn summary_hook(messages: &[Message]) -> Option<String> {
    let tool_count = messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter(|b| matches!(b, crate::client::ContentBlock::ToolResult { .. }))
        .count();
    println!(
        "\x1b[90m[HOOK] Stop: session used {} tool calls\x1b[0m",
        tool_count
    );
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Message;

    fn always_block(_n: &str, _i: &serde_json::Value) -> Option<String> {
        Some("nope".to_string())
    }
    fn never_block(_n: &str, _i: &serde_json::Value) -> Option<String> {
        None
    }
    fn panic_if_called(_n: &str, _i: &serde_json::Value) -> Option<String> {
        panic!("second hook must not run after a block")
    }

    #[test]
    fn empty_registry_allows() {
        let h = Hooks::new();
        assert!(h.trigger_pre_tool("bash", &serde_json::json!({})).is_none());
    }

    #[test]
    fn pre_tool_first_some_short_circuits() {
        let mut h = Hooks::new();
        h.on_pre_tool(always_block);
        h.on_pre_tool(panic_if_called); // 没短路就会 panic
        assert_eq!(
            h.trigger_pre_tool("bash", &serde_json::json!({})),
            Some("nope".to_string())
        );
    }

    #[test]
    fn none_passes_through() {
        let mut h = Hooks::new();
        h.on_pre_tool(never_block);
        h.on_pre_tool(never_block);
        assert!(h.trigger_pre_tool("bash", &serde_json::json!({})).is_none());
    }

    #[test]
    fn stop_some_forces_continue() {
        fn force(_m: &[Message]) -> Option<String> {
            Some("keep going".to_string())
        }
        let mut h = Hooks::new();
        h.on_stop(force);
        assert_eq!(h.trigger_stop(&[]), Some("keep going".to_string()));
    }
}
