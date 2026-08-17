/*
hooks.rs - 钩子系统 (s04)

循环不把扩展逻辑写进体内, 而是在四个固定节点上触发回调:
  UserPromptSubmit  用户输入提交后、进入 LLM 前
  PreToolUse        工具执行前 (s03 的权限检查移到这里)
  PostToolUse       工具执行后
  Stop              循环即将退出时

返回值语义:
  PreToolUse  返回 Some(reason) -> 阻止本次工具, reason 直接当 tool_result
  PostToolUse 返回 Some(msg)    -> 由循环作为独立 user 消息注入（不覆盖 tool_result）
  Stop        返回 Some(msg)    -> 注入 msg 并继续循环, 不退出
  UserPromptSubmit 的返回值不参与控制流。

回调用裸 fn 指针(Copy、零开销), 对应 Python "按名注册函数" 的风格,
也免去 Box<dyn Fn> 的堆分配与 Send/Sync 约束。循环只调 trigger_*,
具体逻辑全在回调里 —— 这正是 s04 的要点。
*/

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::client::{ContentBlock, Message};
use crate::tools::registry::ToolRegistry;
use crate::tools::workdir;

// ---- 回调类型 ----
pub type PromptHook = fn(&str);
pub type PreToolHook = fn(&ToolRegistry, &str, &serde_json::Value) -> Option<String>;
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
    pub fn trigger_pre_tool(&self, registry: &ToolRegistry, name: &str, input: &serde_json::Value) -> Option<String> {
        for f in &self.pre_tool {
            if let Some(reason) = f(registry, name, input) {
                return Some(reason);
            }
        }
        None
    }

    /// 工具执行后触发。返回 Some(msg) -> 由调用方作为独立 user 消息注入（不覆盖 tool_result）。
    pub fn trigger_post_tool(&self, name: &str, input: &serde_json::Value, output: &str) -> Option<String> {
        for f in &self.post_tool {
            if let Some(msg) = f(name, input, output) {
                return Some(msg);
            }
        }
        None
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

/// 把本轮工具结果与 PostToolUse 提醒组装成要追加的 user 消息。
///
/// tool_result 始终是真实工具输出（不被提醒覆盖）；若 PostToolUse 返回了提醒，
/// 则作为独立 user 消息追加在后 —— 与 Stop 钩子（agent_loop / run_subagent_loop）
/// 的注入方式一致。
pub fn assemble_post_tool_messages(
    tool_results: Vec<ContentBlock>,
    reminders: Vec<String>,
) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();

    if !tool_results.is_empty() {
        out.push(Message {
            role: "user".to_string(),
            content: tool_results,
        });
    }

    if !reminders.is_empty() {
        out.push(Message {
            role: "user".to_string(),
            content: reminders
                .into_iter()
                .map(|r| ContentBlock::Text { text: r })
                .collect(),
        });
    }

    // 兜底：两者皆空时（stop_reason 被报为 tool_use 但 content 里没有 ToolUse 块，
    // 且无 PostToolUse 提醒），仍要回喂一条非空 user 消息——否则 Anthropic API 会以
    // "content cannot be empty" 返回 400。
    if out.is_empty() {
        out.push(Message {
            role: "user".to_string(),
            content: vec![ContentBlock::Text {
                text: "(no tool calls to execute)".to_string(),
            }],
        });
    }

    out
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

    fn always_block(_r: &ToolRegistry, _n: &str, _i: &serde_json::Value) -> Option<String> {
        Some("nope".to_string())
    }
    fn never_block(_r: &ToolRegistry, _n: &str, _i: &serde_json::Value) -> Option<String> {
        None
    }
    fn panic_if_called(_r: &ToolRegistry, _n: &str, _i: &serde_json::Value) -> Option<String> {
        panic!("second hook must not run after a block")
    }

    #[test]
    fn empty_registry_allows() {
        let h = Hooks::new();
        let registry = ToolRegistry::new();
        assert!(h.trigger_pre_tool(&registry, "command", &serde_json::json!({})).is_none());
    }

    #[test]
    fn pre_tool_first_some_short_circuits() {
        let mut h = Hooks::new();
        h.on_pre_tool(always_block);
        h.on_pre_tool(panic_if_called); // 没短路就会 panic
        let registry = ToolRegistry::new();
        assert_eq!(
            h.trigger_pre_tool(&registry, "command", &serde_json::json!({})),
            Some("nope".to_string())
        );
    }

    #[test]
    fn none_passes_through() {
        let mut h = Hooks::new();
        h.on_pre_tool(never_block);
        h.on_pre_tool(never_block);
        let registry = ToolRegistry::new();
        assert!(h.trigger_pre_tool(&registry, "command", &serde_json::json!({})).is_none());
    }

    #[test]
    fn post_tool_reminder_is_separate_user_message_not_tool_result() {
        let tool_results = vec![ContentBlock::ToolResult {
            tool_use_id: "t1".to_string(),
            content: "real command output".to_string(),
        }];
        let msgs = assemble_post_tool_messages(
            tool_results,
            vec!["<reminder>Update your todos.</reminder>".to_string()],
        );

        // 提醒必须是独立 user 消息，不能塞进 tool_result
        assert_eq!(
            msgs.len(),
            2,
            "reminder must be a separate user message, not folded into tool_result"
        );

        // tool_result 消息原样保留：仍是真实输出
        match &msgs[0].content[0] {
            ContentBlock::ToolResult { content, .. } => {
                assert_eq!(content, "real command output");
            }
            _ => panic!("first message must still hold the real tool_result"),
        }

        // 提醒是新增的 user 消息、Text 块（不是 tool_result）
        assert_eq!(msgs[1].role, "user");
        match &msgs[1].content[0] {
            ContentBlock::Text { text } => {
                assert_eq!(text, "<reminder>Update your todos.</reminder>");
            }
            _ => panic!("reminder must be a Text block, not a tool_result"),
        }
    }

    #[test]
    fn no_reminder_yields_single_tool_results_message() {
        let tool_results = vec![ContentBlock::ToolResult {
            tool_use_id: "t1".to_string(),
            content: "out".to_string(),
        }];
        let msgs = assemble_post_tool_messages(tool_results, vec![]);
        assert_eq!(msgs.len(), 1);
    }

    #[test]
    fn empty_results_and_no_reminder_yields_placeholder_message() {
        // C8 回归：stop_reason 被报为 tool_use 但无 ToolUse 块时，不能产生空 content
        // 消息（否则 Anthropic API 400 "content cannot be empty"）。
        let msgs = assemble_post_tool_messages(vec![], vec![]);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "user");
        assert!(!msgs[0].content.is_empty(), "must not emit empty content");
        match &msgs[0].content[0] {
            ContentBlock::Text { text } => assert!(!text.is_empty()),
            _ => panic!("placeholder must be a Text block"),
        }
    }

    #[test]
    fn empty_results_with_reminder_yields_only_reminder_message() {
        // 无 tool_result 但有提醒：不应再额外塞一条空 tool_result 消息。
        let msgs = assemble_post_tool_messages(
            vec![],
            vec!["<reminder>Update your todos.</reminder>".to_string()],
        );
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "user");
        match &msgs[0].content[0] {
            ContentBlock::Text { text } => {
                assert_eq!(text, "<reminder>Update your todos.</reminder>");
            }
            _ => panic!("must be the reminder Text block"),
        }
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
