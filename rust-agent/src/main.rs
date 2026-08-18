/*
s08_context_compact.rs - Context Compact (Rust)

The agent loop from s07 gains a ContextCompactor that runs before every model call:

    User prompt
         |
         v
    UserPromptSubmit            <- trigger_prompt()
         |
    +---- compact.prepare() ----+   budget -> snip -> micro -> [compact_history]
    |                            |
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

  + compact.rs: four-step pipeline + reactive retry
  + agent_loop gains compactor + active_request params
  + prompt_too_long triggers reactive_compact (1 retry)

API 交互(请求构造 + 流式解析)在 client.rs;工具与分发在 tools.rs。

Key insight: the loop stays the same; compaction runs transparently before each call.
*/

use rust_agent::client::{Client, ContentBlock, Message};
use rust_agent::compact::{ContextCompactor, MAX_REACTIVE_RETRIES};
use rust_agent::error::AgentError;
use rust_agent::hooks::{assemble_post_tool_messages, context_inject_hook, large_output_hook, summary_hook, todo_reminder_hook, Hooks};
use rust_agent::permission::permission_hook;
use rust_agent::tools::{workdir, ToolContext, ToolRegistry};
use dotenv::dotenv;
use std::env;
use std::io::{self, Write};
use std::path::PathBuf;

/// 执行单个工具调用（含 PreToolUse 拦截）。
///
/// 返回真实工具输出（被 PreToolUse 拦截时返回拦截原因作为 tool_result）。
/// PostToolUse 不在此处理：其返回值由 agent_loop 经 assemble_post_tool_messages
/// 作为独立 user 消息注入，不再覆盖 tool_result。
async fn execute_tool(
    client: &Client,
    registry: &ToolRegistry,
    name: &str,
    input: &serde_json::Value,
    hooks: &Hooks,
) -> String {
    // PreToolUse 拦截
    if let Some(reason) = hooks.trigger_pre_tool(registry, name, input) {
        return reason;
    }

    // Create ToolContext for tool execution
    let ctx = ToolContext {
        client,
        hooks,
        registry,
    };

    // 执行工具（PostToolUse 提醒由调用方注入，见 agent_loop）
    registry.dispatch(name, &ctx, input, false).await.unwrap_or_else(|| "Error: tool not found".to_string())
}

/// Agent 核心循环
///
/// 循环结构不变: 调用 LLM -> 追加助手响应 -> 若 stop_reason 是 tool_use 就执行工具、
/// 把 tool_result 喂回去 -> 直到模型说结束。s04 的变化是: 不再硬编码 check_permission,
/// 而是在固定节点上 trigger_hooks(PreToolUse / PostToolUse / Stop)。
///
/// s08 变化: 每次调用模型前先 compactor.prepare()（budget->snip->micro->超阈值才摘要）；
/// stream_messages 包进 match, prompt_too_long 时 reactive_compact 重试一次。
async fn agent_loop(
    client: &Client,
    registry: &ToolRegistry,
    system: &str,
    messages: &mut Vec<Message>,
    hooks: &Hooks,
    compactor: &ContextCompactor,
    active_request: &str,
) -> Result<(), AgentError> {
    let mut reactive_retries = 0u32;
    loop {
        // s08: 每次调用模型前运行压缩管线
        compactor
            .prepare(client, messages, active_request)
            .await?;

        let response = match client
            .stream_messages(system, messages, &registry.definitions(), 8000)
            .await
        {
            Ok(r) => {
                reactive_retries = 0;
                r
            }
            Err(e) => {
                // s08: prompt_too_long 时尝试 reactive_compact 重试一次
                if e.is_prompt_too_long() && reactive_retries < MAX_REACTIVE_RETRIES {
                    println!("\x1b[33m[reactive compact]\x1b[0m");
                    compactor
                        .reactive_compact(client, messages, active_request)
                        .await?;
                    reactive_retries += 1;
                    continue;
                }
                return Err(e);
            }
        };

        // 打印这一轮的 LLM 内容（text + tool_use）；client 自身不打印。
        {
            let mut out = io::stdout().lock();
            rust_agent::output::render(&response, &mut out);
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
        let mut tool_results = Vec::new();
        let mut reminders: Vec<String> = Vec::new();
        for block in &response.content {
            if let ContentBlock::ToolUse { id, name, input } = block {
                let tool_output = execute_tool(client, registry, name, input, hooks).await;
                // 打印工具执行结果（此前只喂回 LLM，用户看不到工具返回了什么）
                {
                    let mut out = io::stdout().lock();
                    rust_agent::output::render_tool_result(name, &tool_output, &mut out);
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
async fn main() -> Result<(), AgentError> {
    dotenv().ok();
    println!("Enter a question, press Enter to send. Type q to quit.\n");

    let api_key = env::var("ANTHROPIC_AUTH_TOKEN")
        .or_else(|_| env::var("ANTHROPIC_API_KEY"))?;
    let base_url = env::var("ANTHROPIC_BASE_URL")
        .unwrap_or_else(|_| "https://api.anthropic.com".to_string());
    let model = env::var("MODEL_ID")?;
    println!("base_url: {}, model: {}, key: {}", base_url, model, mask_key(&api_key));

    let client = Client::new(api_key, base_url, model);

    let cwd = workdir().to_string_lossy().to_string();

    // s08: 上下文压缩器。目录与 Python s08 一致：.transcripts/ 与 .task_outputs/tool-results/。
    let compactor = ContextCompactor::new(
        PathBuf::from(&cwd).join(".transcripts"),
        PathBuf::from(&cwd).join(".task_outputs").join("tool-results"),
    );
    let skills_dir = env::var("SKILLS_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("{}/skills", cwd));
    let loader = rust_agent::skills::SkillLoader::scan(PathBuf::from(&skills_dir));
    let skill_count = loader.len();
    rust_agent::skills::set_instance(loader);
    println!(
        "Loaded {} skill(s) from {}",
        skill_count, skills_dir
    );

    // 组装 system prompt：固定的 agent 指令 + 技能目录（非空才加）+ load_skill 提示。
    // 目录只在 system prompt 里（每次调用都付这点开销）；完整正文在 load_skill 的 tool_result 里按需加载。
    let catalog = rust_agent::skills::catalog();
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

    // Build tool registry
    let registry = rust_agent::tools::build_registry();

    let mut messages: Vec<Message> = Vec::new();

    // 初始化 TodoManager 并设置全局实例
    let todo_manager = rust_agent::todo::TodoManager::new();
    rust_agent::todo::set_instance(todo_manager);

    loop {
        print!("\x1b[36m >> \x1b[0m");
        io::stdout().flush()?;

        let mut query = String::new();
        io::stdin().read_line(&mut query)?;
        let query = query.trim().to_string();

        if query.is_empty() {
            continue;
        }
        if query.eq_ignore_ascii_case("q") || query == "exit" {
            break;
        }

        // s04: 用户输入后、进入 LLM 前触发 UserPromptSubmit
        hooks.trigger_prompt(&query);

        messages.push(Message {
            role: "user".to_string(),
            content: vec![ContentBlock::Text {
                text: query.clone(),
            }],
        });

        if let Err(e) = agent_loop(
            &client,
            &registry,
            &system,
            &mut messages,
            &hooks,
            &compactor,
            &query,
        )
        .await
        {
            eprintln!("Error: {}", e);
        }

        println!();
    }

    Ok(())
}
