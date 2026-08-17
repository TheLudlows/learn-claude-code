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
mod output;
mod permission;
mod skills;
mod subagent;
mod todo;
mod tools;

use client::{Client, ContentBlock, Message};
use dotenv::dotenv;
use hooks::{assemble_post_tool_messages, context_inject_hook, large_output_hook, summary_hook, todo_reminder_hook, Hooks};
use permission::permission_hook;
use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use tools::{dispatch_tool, get_tool_definitions};

/// 执行单个工具调用（含 PreToolUse 拦截）。
///
/// 返回真实工具输出（被 PreToolUse 拦截时返回拦截原因作为 tool_result）。
/// PostToolUse 不在此处理：其返回值由 agent_loop 经 assemble_post_tool_messages
/// 作为独立 user 消息注入，不再覆盖 tool_result。
async fn execute_tool(
    client: &Client,
    name: &str,
    input: &serde_json::Value,
    hooks: &Hooks,
) -> String {
    // PreToolUse 拦截
    if let Some(reason) = hooks.trigger_pre_tool(name, input) {
        return reason;
    }

    // 执行工具（PostToolUse 提醒由调用方注入，见 agent_loop）
    if name == "task" {
        if let Some(prompt) = input.get("prompt").and_then(|p| p.as_str()) {
            subagent::run_subagent_loop(client, prompt, hooks).await.unwrap_or_else(|e| format!("Subagent error: {}", e))
        } else {
            "Error: missing prompt".to_string()
        }
    } else {
        dispatch_tool(name, input)
    }
}

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

        // 打印这一轮的 LLM 内容（text + tool_use）；client 自身不打印。
        {
            let mut out = io::stdout().lock();
            output::render(&response, &mut out);
        }

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

        // 执行工具调用
        let mut tool_results = Vec::new();
        let mut reminders: Vec<String> = Vec::new();
        for block in &response.content {
            if let ContentBlock::ToolUse { id, name, input } = block {
                let tool_output = execute_tool(client, name, input, hooks).await;
                // 打印工具执行结果（此前只喂回 LLM，用户看不到工具返回了什么）
                {
                    let mut out = io::stdout().lock();
                    output::render_tool_result(name, &tool_output, &mut out);
                }
                // PostToolUse: 提醒作为独立 user 消息注入，不进 tool_result
                if let Some(msg) = hooks.trigger_post_tool(name, input, &tool_output) {
                    reminders.push(msg);
                }
                tool_results.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: tool_output,
                });
            }
        }

        // 添加工具结果（真实输出）+ PostToolUse 提醒（独立 user 消息）
        messages.extend(assemble_post_tool_messages(tool_results, reminders));
    }

    Ok(())
}

/// 把 API key 打码：仅留前 4 与后 4 字符，避免完整密钥泄露到 stdout。
fn mask_key(k: &str) -> String {
    let chars: Vec<char> = k.chars().collect();
    if chars.len() > 8 {
        let head: String = chars[..4].iter().collect();
        let tail: String = chars[chars.len() - 4..].iter().collect();
        format!("{}…{}", head, tail)
    } else {
        "***".to_string()
    }
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
    println!("base_url: {}, model: {}, key: {}", base_url, model, mask_key(&api_key));

    let client = Client::new(api_key, base_url, model);

    let cwd = env::current_dir()
        .unwrap_or_else(|_| ".".into())
        .to_string_lossy()
        .to_string();

    // s07: 启动时扫描技能目录，把「名称+描述」编入 system prompt，完整正文按需 load_skill 取。
    // SKILLS_DIR 缺省（或空串）时回退到 cwd/skills；目录不存在则注册表为空（agent 仍可运行，只是无技能）。
    let skills_dir = env::var("SKILLS_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("{}/skills", cwd));
    let loader = skills::SkillLoader::scan(PathBuf::from(&skills_dir));
    let skill_count = loader.len();
    skills::set_instance(loader);
    println!(
        "Loaded {} skill(s) from {}",
        skill_count, skills_dir
    );

    // 组装 system prompt：固定的 agent 指令 + 技能目录（非空才加）+ load_skill 提示。
    // 目录只在 system prompt 里（每次调用都付这点开销）；完整正文在 load_skill 的 tool_result 里按需加载。
    let catalog = skills::catalog();
    let system = if catalog.is_empty() {
        format!(
            "You are a coding agent at {} on {}. Before starting any multi-step task, use todo_write to plan your steps. Update status as you go. You can use tools as needed.",
            cwd, env::consts::OS
        )
    } else {
        format!(
            "You are a coding agent at {} on {}. Before starting any multi-step task, use todo_write to plan your steps. Update status as you go. You can use tools as needed.\n\n\
             Skills available:\n{}\n\n\
             Use load_skill to read the full instructions when a skill applies.",
            cwd, env::consts::OS, catalog
        )
    };

    // s04: 注册钩子 —— 循环只调 trigger_*, 具体逻辑全在回调里
    let mut hooks = Hooks::new();
    hooks.on_prompt(context_inject_hook);
    hooks.on_pre_tool(permission_hook); // s03 三道闸门, 搬成 PreToolUse 回调
    hooks.on_post_tool(large_output_hook);
    hooks.on_stop(summary_hook);
    hooks.on_post_tool(todo_reminder_hook);

    let mut messages: Vec<Message> = Vec::new();

    // 初始化 TodoManager 并设置全局实例
    let todo_manager = todo::TodoManager::new();
    todo::set_instance(todo_manager);

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
