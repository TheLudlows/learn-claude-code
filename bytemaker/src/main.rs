/*
main.rs - REPL 入口（s13）

核心循环与共享状态都移入 lib 的 `Agent`（agent.rs）：main 只做 CLI 装配——
读 env、构造 Agent、启动 cron、REPL 调 `agent.run_loop`。原 `execute_tool`/`agent_loop`
/`set_instance`/`init_manager`/`start_runtime` 全部并入 Agent，此处不再出现。
*/

use bytemaker::agent::{Agent, AgentConfig};
use bytemaker::client::{ContentBlock, Message};
use bytemaker::error::AgentError;
use bytemaker::output;
use dotenv::dotenv;
use std::env;
use std::io;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), AgentError> {
    dotenv().ok();
    // 诊断日志（[memory]/[snip_compact]/[persist] 等）：默认 INFO，RUST_LOG=warn 静默，
    // =debug 更细。UX 行不走 tracing，仍由 output.rs 着色。
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
    output::banner("Enter a question, press Enter to send. Type q to quit.\n");

    let api_key = env::var("ANTHROPIC_AUTH_TOKEN")
        .or_else(|_| env::var("ANTHROPIC_API_KEY"))?;
    let base_url = env::var("ANTHROPIC_BASE_URL")
        .unwrap_or_else(|_| "https://api.anthropic.com".to_string());
    let model = env::var("MODEL_ID")?;
    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    let skills_dir = env::var("SKILLS_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("{}/skills", cwd.to_string_lossy()));

    output::banner(&format!(
        "base_url: {}, model: {}, key: {}",
        base_url, model, "***"
    ));

    let cfg = AgentConfig {
        api_key,
        base_url,
        model,
        workdir: cwd.clone(),
        skills_dir: PathBuf::from(&skills_dir),
    };
    let agent = Agent::new(cfg).await?;
    agent.start_cron_runtime().await?;
    output::banner(&format!(
        "Loaded {} skill(s) from {}",
        agent.skills_len(),
        skills_dir
    ));

    let mut messages: Vec<Message> = Vec::new();

    loop {
        output::prompt();

        let mut query = String::new();
        io::stdin().read_line(&mut query)?;
        let query = query.trim().to_string();

        if query.is_empty() {
            continue;
        }
        if query.eq_ignore_ascii_case("q") || query == "exit" {
            break;
        }

        // 用户输入后、进入 LLM 前触发 UserPromptSubmit。
        agent.trigger_prompt(&query);

        messages.push(Message {
            role: "user".to_string(),
            content: vec![ContentBlock::Text {
                text: query.clone(),
            }],
        });

        if let Err(e) = agent.run_loop(&mut messages, &query).await {
            output::error(&format!("Error: {}", e));
        }

        output::blank();
    }

    Ok(())
}
