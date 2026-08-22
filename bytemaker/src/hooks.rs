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

回调用 trait 对象 (Box<dyn TraitX>, 各 trait 带 Send + Sync 超trait): 每个事件一个 trait,
钩子以结构体实现, 注册时装箱。相比裸 fn 指针多一次堆分配, 但换取了
钩子可携带 owned 状态 (如 TodoReminderHook 的计数器, 不再依赖 static 全局),
且 Send + Sync 超trait 保证 Box<dyn> 可跨 async 边界。循环只调 trigger_*,
具体逻辑全在回调里 —— 这正是 s04 的要点。
*/

use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::oneshot;

use crate::client::{ContentBlock, Message};
use crate::tools::registry::ToolRegistry;

/// 钩子运行时上下文：pre_tool 需要的 output + input 抽象接口。
pub struct HookContext {
    /// 抽象的输出接口
    pub output: Arc<dyn crate::io::Output>,
    /// 抽象的输入接口
    pub input: Arc<dyn crate::io::Input>,
}

impl HookContext {
    /// 测试用：使用内存 I/O 的测试上下文
    #[cfg(test)]
    pub(crate) fn test_noop() -> Self {
        let io = crate::io::IO::memory();
        Self {
            output: Arc::clone(&io.output),
            input: Arc::clone(&io.input),
        }
    }
}

/// 一条权限查询：InputTask 经 reply 回答权限确认。
pub struct PermissionQuery {
    pub reason: String,
    pub name: String,
    pub input: serde_json::Value,
    pub reply: oneshot::Sender<bool>,
}

// ---- 回调 trait ----
#[async_trait]
pub trait PromptHook: Send + Sync {
    async fn on_prompt(&self, query: &str);
}
#[async_trait]
pub trait PreToolHook: Send + Sync {
    async fn on_pre_tool(
        &self,
        registry: &ToolRegistry,
        ctx: &HookContext,
        name: &str,
        input: &serde_json::Value,
    ) -> Option<String>;
}
#[async_trait]
pub trait PostToolHook: Send + Sync {
    async fn on_post_tool(&self, name: &str, input: &serde_json::Value, output: &str) -> Option<String>;
}
#[async_trait]
pub trait StopHook: Send + Sync {
    async fn on_stop(&self, messages: &[Message]) -> Option<String>;
}

/// 钩子注册表: 事件 -> 回调列表。
#[derive(Default)]
pub struct Hooks {
    user_prompt: Vec<Box<dyn PromptHook>>,
    pre_tool: Vec<Box<dyn PreToolHook>>,
    post_tool: Vec<Box<dyn PostToolHook>>,
    stop: Vec<Box<dyn StopHook>>,
}

impl Hooks {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_prompt<H: PromptHook + 'static>(&mut self, h: H) {
        self.user_prompt.push(Box::new(h));
    }
    pub fn on_pre_tool<H: PreToolHook + 'static>(&mut self, h: H) {
        self.pre_tool.push(Box::new(h));
    }
    pub fn on_post_tool<H: PostToolHook + 'static>(&mut self, h: H) {
        self.post_tool.push(Box::new(h));
    }
    pub fn on_stop<H: StopHook + 'static>(&mut self, h: H) {
        self.stop.push(Box::new(h));
    }

    /// 用户输入后、进入 LLM 前触发。返回值不参与控制流。
    pub async fn trigger_prompt(&self, query: &str) {
        for f in &self.user_prompt {
            f.on_prompt(query).await;
        }
    }

    /// 工具执行前触发。第一个返回 Some(reason) 的回调短路 -> 该工具被拦截。
    pub async fn trigger_pre_tool(
        &self,
        registry: &ToolRegistry,
        ctx: &HookContext,
        name: &str,
        input: &serde_json::Value,
    ) -> Option<String> {
        for f in &self.pre_tool {
            if let Some(reason) = f.on_pre_tool(registry, ctx, name, input).await {
                return Some(reason);
            }
        }
        None
    }

    /// 工具执行后触发。返回 Some(msg) -> 由调用方作为独立 user 消息注入（不覆盖 tool_result）。
    pub async fn trigger_post_tool(&self, name: &str, input: &serde_json::Value, output: &str) -> Option<String> {
        for f in &self.post_tool {
            if let Some(msg) = f.on_post_tool(name, input, output).await {
                return Some(msg);
            }
        }
        None
    }

    /// 循环即将退出时触发。返回 Some(msg) -> 注入 msg 并继续, 不退出。
    pub async fn trigger_stop(&self, messages: &[Message]) -> Option<String> {
        for f in &self.stop {
            if let Some(msg) = f.on_stop(messages).await {
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
        out.push(Message::user_blocks(tool_results));
    }

    if !reminders.is_empty() {
        out.push(Message::user_blocks(
            reminders.into_iter().map(|r| ContentBlock::Text { text: r }).collect(),
        ));
    }

    // 兜底：两者皆空时（stop_reason 被报为 tool_use 但 content 里没有 ToolUse 块，
    // 且无 PostToolUse 提醒），仍要回喂一条非空 user 消息——否则 Anthropic API 会以
    // "content cannot be empty" 返回 400。
    if out.is_empty() {
        out.push(Message::user_text("(no tool calls to execute)"));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Message;

    struct AlwaysBlock;
    #[async_trait::async_trait]
    impl PreToolHook for AlwaysBlock {
        async fn on_pre_tool(&self, _r: &ToolRegistry, _ctx: &HookContext, _n: &str, _i: &serde_json::Value) -> Option<String> {
            Some("nope".to_string())
        }
    }
    struct NeverBlock;
    #[async_trait::async_trait]
    impl PreToolHook for NeverBlock {
        async fn on_pre_tool(&self, _r: &ToolRegistry, _ctx: &HookContext, _n: &str, _i: &serde_json::Value) -> Option<String> {
            None
        }
    }
    struct PanicIfCalled;
    #[async_trait::async_trait]
    impl PreToolHook for PanicIfCalled {
        async fn on_pre_tool(&self, _r: &ToolRegistry, _ctx: &HookContext, _n: &str, _i: &serde_json::Value) -> Option<String> {
            panic!("second hook must not run after a block")
        }
    }

    #[tokio::test]
    async fn empty_registry_allows() {
        let h = Hooks::new();
        let ctx = HookContext::test_noop();
        let registry = ToolRegistry::new();
        assert!(h.trigger_pre_tool(&registry, &ctx, "command", &serde_json::json!({})).await.is_none());
    }

    #[tokio::test]
    async fn pre_tool_first_some_short_circuits() {
        let mut h = Hooks::new();
        h.on_pre_tool(AlwaysBlock);
        h.on_pre_tool(PanicIfCalled); // 没短路就会 panic
        let ctx = HookContext::test_noop();
        let registry = ToolRegistry::new();
        assert_eq!(
            h.trigger_pre_tool(&registry, &ctx, "command", &serde_json::json!({})).await,
            Some("nope".to_string())
        );
    }

    #[tokio::test]
    async fn none_passes_through() {
        let mut h = Hooks::new();
        h.on_pre_tool(NeverBlock);
        h.on_pre_tool(NeverBlock);
        let ctx = HookContext::test_noop();
        let registry = ToolRegistry::new();
        assert!(h.trigger_pre_tool(&registry, &ctx, "command", &serde_json::json!({})).await.is_none());
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

    #[tokio::test]
    async fn stop_some_forces_continue() {
        struct Force;
        #[async_trait::async_trait]
        impl StopHook for Force {
            async fn on_stop(&self, _m: &[Message]) -> Option<String> {
                Some("keep going".to_string())
            }
        }
        let mut h = Hooks::new();
        h.on_stop(Force);
        assert_eq!(h.trigger_stop(&[]).await, Some("keep going".to_string()));
    }
}
