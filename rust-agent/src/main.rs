/*
s04_hooks.rs - Hooks (Rust)

The agent loop from s03 does not change shape. The only change: the hard-coded
`permission::check_permission()` call is replaced by `hooks.trigger_pre_tool()`,
and three more extension points are wired in:

    User prompt
         |
         v
    UserPromptSubmit            <- trigger_prompt()
         |
    +----------+      +-------+
    | messages | ---> |  LLM  |
    +----------+      +---+---+
         ^                | stop_reason
         |                v
         |            Stop          <- trigger_stop()  (Some -> inject & continue)
         |
         +------ tool_result ------+
                               |
                  PreToolUse <- trigger_pre_tool()  (Some -> block, reason as tool_result)
                               |
                           dispatch_tool
                               |
                  PostToolUse <- trigger_post_tool()

  + hooks.rs: registry + trigger_* + demo callbacks
  + permission::permission_hook (s03 gates, now a PreToolUse callback)
  + the loop only calls trigger_* — extension logic lives in callbacks

API 交互(请求构造 + 流式解析)在 client.rs;工具与分发在 tools.rs。

Key insight: the loop stays the same; only the four trigger points are wired in.
*/

mod client;
mod hooks;
mod permission;
mod tools;

use client::{Client, ContentBlock, Message};
use dotenv::dotenv;
use hooks::{context_inject_hook, large_output_hook, log_hook, summary_hook, Hooks};
use permission::permission_hook;
use std::env;
use std::io::{self, Write};
use tools::{dispatch_tool, get_tool_definitions};

/// Agent 核心循环
///
/// 循环结构不变: 调用 LLM -> 追加助手响应 -> 若 stop_reason 是 tool_use 就执行工具、
/// 把 tool_result 喂回去 -> 直到模型说结束。s04 的变化是: 不再硬编码 check_permission,
/// 而是在固定节点上 trigger_hooks(PreToolUse / PostToolUse / Stop)。
async fn agent_loop(
    client: &Client,
    system: &str,
    messages: &mut Vec<Message>,
    hooks: &Hooks,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        let response = client
            .stream_messages(system, messages, &get_tool_definitions(), 8000)
            .await?;

        // 添加助手响应(含 text 与 tool_use 块, 原样回传给下一轮)
        messages.push(Message {
            role: "assistant".to_string(),
            content: response.content.clone(),
        });

        // 检查是否需要调用工具
        if response.stop_reason != "tool_use" {
            // s04: 退出前触发 Stop; 返回 Some(msg) 则注入并继续, 不退出
            if let Some(force) = hooks.trigger_stop(messages) {
                messages.push(Message {
                    role: "user".to_string(),
                    content: vec![ContentBlock::Text { text: force }],
                });
                continue;
            }
            break;
        }

        // 执行工具调用: PreToolUse 拦截 -> dispatch -> PostToolUse
        let mut tool_results = Vec::new();
        for block in &response.content {
            if let ContentBlock::ToolUse { id, name, input } = block {
                // s04: hook 取代硬编码的 check_permission; Some(reason) 即拦截
                if let Some(reason) = hooks.trigger_pre_tool(name, input) {
                    tool_results.push(ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: reason,
                    });
                    continue;
                }

                let output = dispatch_tool(name, input);
                hooks.trigger_post_tool(name, input, &output);

                tool_results.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: output,
                });
            }
        }

        // 添加工具结果
        messages.push(Message {
            role: "user".to_string(),
            content: tool_results,
        });
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv().ok();
    println!("Enter a question, press Enter to send. Type q to quit.\n");

    let api_key = env::var("ANTHROPIC_AUTH_TOKEN")
        .or_else(|_| env::var("ANTHROPIC_API_KEY"))?;
    let base_url = env::var("ANTHROPIC_BASE_URL")
        .unwrap_or_else(|_| "https://api.anthropic.com".to_string());
    let model = env::var("MODEL_ID")?;
    println!("api-key: {}, base_url {}, mode {}", api_key, base_url, model);

    let client = Client::new(api_key, base_url, model);

    let cwd = env::current_dir()
        .unwrap_or_else(|_| ".".into())
        .to_string_lossy()
        .to_string();
    let system = format!(
        "You are a coding agent at {} on {}. Use tools to solve tasks. Act, don't explain.",
        cwd, env::consts::OS
    );

    // s04: 注册钩子 —— 循环只调 trigger_*, 具体逻辑全在回调里
    let mut hooks = Hooks::new();
    hooks.on_prompt(context_inject_hook);
    hooks.on_pre_tool(permission_hook); // s03 三道闸门, 搬成 PreToolUse 回调
    hooks.on_pre_tool(log_hook);
    hooks.on_post_tool(large_output_hook);
    hooks.on_stop(summary_hook);

    let mut messages: Vec<Message> = Vec::new();

    loop {
        print!("\x1b[36m You >> \x1b[0m");
        io::stdout().flush()?;

        let mut query = String::new();
        io::stdin().read_line(&mut query)?;
        let query = query.trim();

        if query.eq_ignore_ascii_case("q") || query == "exit" || query.is_empty() {
            break;
        }

        // s04: 用户输入后、进入 LLM 前触发 UserPromptSubmit
        hooks.trigger_prompt(query);

        messages.push(Message {
            role: "user".to_string(),
            content: vec![ContentBlock::Text {
                text: query.to_string(),
            }],
        });

        if let Err(e) = agent_loop(&client, &system, &mut messages, &hooks).await {
            eprintln!("Error: {}", e);
        }

        println!();
    }

    Ok(())
}
