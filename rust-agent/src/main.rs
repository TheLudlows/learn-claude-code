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

  + memory.rs: recall into system per request + extract/consolidate on exit (s09)
  + agent_loop gains memory param; system -> base_system, rebuilt per request

API 交互(请求构造 + 流式解析)在 client.rs;工具与分发在 tools.rs。
记忆在 memory.rs(跨会话);压缩在 compact.rs(会话内)。

Key insight: the loop stays the same; compaction runs transparently before each call,
memory recall runs once per request, extract/consolidate run only on true exit.
*/

use rust_agent::client::{CallResult, Client, ContentBlock, Message};
use rust_agent::compact::{ContextCompactor, MAX_REACTIVE_RETRIES};
use rust_agent::error::AgentError;
use rust_agent::memory::{build_system, MemoryStore};
use rust_agent::builtins::{ContextInjectHook, LargeOutputHook, PermissionHook, SummaryHook, TodoReminderHook};
use rust_agent::hooks::{assemble_post_tool_messages, Hooks};
use rust_agent::tools::{workdir, ToolContext, ToolRegistry, ToolResult};
use rust_agent::cron_scheduler::{init_manager, start_runtime, get_manager, acknowledge_jobs, restore_jobs};
use dotenv::dotenv;
use std::env;
use std::io;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

/// 执行单个工具调用（含 PreToolUse 拦截）。
///
/// 返回 ToolResult，区分四种情况：
/// - Denied: pre_tool hook 拦截（权限拒绝等）
/// - Output: 工具真正执行了（成功或内部报错）
/// - Rejected: 子 agent 上下文调用受限工具（此处 for_subagent=false，不会发生）
/// - NotFound: registry 里找不到这个工具
///
/// PostToolUse 不在此处理：其返回值由 agent_loop 经 assemble_post_tool_messages
/// 作为独立 user 消息注入，不再覆盖 tool_result。
async fn execute_tool(
    client: &Client,
    registry: &ToolRegistry,
    name: &str,
    input: &serde_json::Value,
    hooks: &Hooks,
) -> ToolResult {
    // PreToolUse 拦截
    if let Some(reason) = hooks.trigger_pre_tool(registry, name, input) {
        return ToolResult::Denied {
            name: name.to_string(),
            reason,
        };
    }

    // Create ToolContext for tool execution
    let ctx = ToolContext {
        client,
        hooks,
        registry,
    };
    registry.dispatch(name, &ctx, input, false).await
}

/// Agent 核心循环
///
/// 循环结构不变: 调用 LLM -> 追加助手响应 -> 若 stop_reason 是 tool_use 就执行工具、
/// 把 tool_result 喂回去 -> 直到模型说结束。s04 的变化是: 不再硬编码 check_permission,
/// 而是在固定节点上 trigger_hooks(PreToolUse / PostToolUse / Stop)。
///
/// s08 变化: 每次调用模型前先 compactor.prepare()（budget->snip->micro->超阈值才摘要）；
/// stream_messages 包进 match, prompt_too_long 时 reactive_compact 重试一次。
///
/// s09 变化: 请求开始召回相关记忆拼进 system(每请求一次,非每调用),真退出前
/// (Stop 钩子未 force)extract 持久记忆 + consolidate(≥10 条才合并)。
#[allow(clippy::too_many_arguments)]
async fn agent_loop(
    client: &Client,
    registry: &ToolRegistry,
    base_system: &str,
    messages: &mut Vec<Message>,
    hooks: &Hooks,
    compactor: &ContextCompactor,
    memory: &MemoryStore,
    active_request: &str,
) -> Result<(), AgentError> {
    // s09: 召回相关记忆(模型,失败降级关键词)→ 拼进 system。每请求一次,与压缩正交。
    let recalled = memory.load_memories(client, messages).await;
    let index = memory.read_memory_index();
    let system = build_system(base_system, &index, &recalled);

    let mut reactive_retries = 0u32;
    loop {
        // s11: 循环顶部收集已完成后台任务通知 (被动兜底)
        let _ = rust_agent::background_tasks::collect_and_inject(messages);

        // s12: 循环顶部收集待交付的定时任务
        let scheduled_start = messages.len();
        let scheduled_jobs = get_manager().map(|mgr| mgr.consume_queue());
        let mut waiting_for_ack: Vec<rust_agent::cron_scheduler::CronJob> = Vec::new();

        if let Some(jobs) = scheduled_jobs {
            for job in &jobs {
                messages.push(Message {
                    role: "user".to_string(),
                    content: vec![ContentBlock::Text {
                        text: format!("[Scheduled] {}", job.prompt),
                    }],
                });
                println!("  [cron] delivered {}: {}", job.id, &job.prompt[..job.prompt.len().min(60)]);
            }
            waiting_for_ack = jobs;
        }

        // s08: 每次调用模型前运行压缩管线
        compactor
            .prepare(client, messages, active_request)
            .await?;

        let response = match client
            .stream_messages(&system, messages, &registry.definitions(), 8000)
            .await
        {
            CallResult::Success(r) => {
                reactive_retries = 0;
                r
            }
            // prompt_too_long 且还有重试预算：压缩后重试
            CallResult::PromptTooLong(_) if reactive_retries < MAX_REACTIVE_RETRIES => {
                rust_agent::output::status("[reactive compact]");
                compactor
                    .reactive_compact(client, messages, active_request)
                    .await?;
                reactive_retries += 1;
                continue;
            }
            // prompt_too_long 耗尽重试 或 其他错误：直接返回
            CallResult::PromptTooLong(e) | CallResult::Failure(e) => {
                // s12: 模型调用失败时恢复定时任务
                if !waiting_for_ack.is_empty() {
                    // 移除已注入的消息
                    messages.truncate(scheduled_start);
                    restore_jobs(&waiting_for_ack);
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

        // s12: 模型调用成功后确认定时任务
        if !waiting_for_ack.is_empty() {
            if let Err(e) = acknowledge_jobs(&waiting_for_ack).await {
                println!("  [cron] acknowledgement failed: {}", e);
            }
            waiting_for_ack.clear();
        }

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
            // s09: 真退出前提取持久记忆;有新写入再尝试整理(≥10 条才合并,失败恢复原文件)。
            if memory.extract_memories(client, messages).await > 0
            {
                let _ = memory.consolidate_memories(client).await;
            }
            break;
        }
        let mut tool_results = Vec::new();
        let mut reminders: Vec<String> = Vec::new();
        for block in &response.content {
            if let ContentBlock::ToolUse { id, name, input } = block {
                let tool_result = execute_tool(client, registry, name, input, hooks).await;
                // 打印工具执行结果（此前只喂回 LLM，用户看不到工具返回了什么）
                {
                    let mut out = io::stdout().lock();
                    rust_agent::output::render_tool_result(name, &tool_result.as_content(), &mut out);
                }
                // PostToolUse: 提醒作为独立 user 消息注入，不进 tool_result
                // 只有工具真正执行过才触发 hook（Denied/NotFound/Rejected 不触发）
                if tool_result.was_executed() {
                    if let Some(msg) = hooks.trigger_post_tool(name, input, &tool_result.as_content()) {
                        reminders.push(msg);
                    }
                }
                tool_results.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: tool_result.as_content(),
                });
            }
        }

        messages.extend(assemble_post_tool_messages(tool_results, reminders));
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), AgentError> {
    dotenv().ok();
    // 诊断日志（[memory]/[snip_compact]/[persist] 等）：默认 INFO（与改前可见性一致），
    // RUST_LOG=warn 静默诊断，=debug 更细。UX 行不走 tracing，仍由 output.rs 着色。
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
    rust_agent::output::banner("Enter a question, press Enter to send. Type q to quit.\n");

    let api_key = env::var("ANTHROPIC_AUTH_TOKEN")
        .or_else(|_| env::var("ANTHROPIC_API_KEY"))?;
    let base_url = env::var("ANTHROPIC_BASE_URL")
        .unwrap_or_else(|_| "https://api.anthropic.com".to_string());
    let model = env::var("MODEL_ID")?;
    rust_agent::output::banner(&format!("base_url: {}, model: {}, key: {}", base_url, model, "***"));

    let client = Client::new(api_key, base_url, model);

    let cwd = workdir().to_string_lossy().to_string();

    // s08: 上下文压缩器。目录与 Python s08 一致：.transcripts/ 与 .task_outputs/tool-results/。
    let compactor = ContextCompactor::new(
        PathBuf::from(&cwd).join(".transcripts"),
        PathBuf::from(&cwd).join(".task_outputs").join("tool-results"),
    );
    // s09: 记忆存储。目录 .memory/ 与 Python s09 一致。
    let memory = MemoryStore::new(PathBuf::from(&cwd).join(".memory"));
    let skills_dir = env::var("SKILLS_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("{}/skills", cwd));
    let loader = rust_agent::skills::SkillLoader::scan(PathBuf::from(&skills_dir));
    let skill_count = loader.len();
    rust_agent::skills::set_instance(loader);
    rust_agent::output::banner(&format!("Loaded {} skill(s) from {}", skill_count, skills_dir));

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
    hooks.on_prompt(ContextInjectHook);
    hooks.on_pre_tool(PermissionHook); // s03 三道闸门, 搬成 PreToolUse 回调
    hooks.on_post_tool(LargeOutputHook);
    hooks.on_stop(SummaryHook);
    hooks.on_post_tool(TodoReminderHook::new());
    hooks.on_stop(rust_agent::background_tasks::BackgroundStopHook);

    // Build tool registry
    let registry = rust_agent::tools::build_registry();

    // s10: 任务存储在首次工具调用时懒初始化（见 task_system/tools.rs 的 get_store），
    // 无需在此手动启动；构造失败会以错误信息形式返回给工具，不阻断主循环。

    let mut messages: Vec<Message> = Vec::new();

    // 初始化 TodoManager 并设置全局实例
    let todo_manager = rust_agent::todo::TodoManager::new();
    rust_agent::todo::set_instance(todo_manager);

    // s12: 初始化 CronManager 并启动运行时
    let cron_manager = init_manager(PathBuf::from(&cwd)).await;
    start_runtime().await;
    let _ = cron_manager; // 抑制 unused 警告

    loop {
        rust_agent::output::prompt();

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
            &memory,
            &query,
        )
        .await
        {
            rust_agent::output::error(&format!("Error: {}", e));
        }

        rust_agent::output::blank();
    }

    Ok(())
}
