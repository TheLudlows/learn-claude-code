/*
main.rs - REPL 入口（s13）

核心循环与共享状态都移入 lib 的 `Agent`（agent.rs）：main 只做 CLI 装配——
读 env、构造 Agent、启动 cron、REPL 调 `agent.run_loop`。原 `execute_tool`/`agent_loop`
/`set_instance`/`init_manager`/`start_runtime` 全部并入 Agent，此处不再出现。
*/

use bytemaker::agent::{Agent, AgentConfig};
use bytemaker::client::Message;
use bytemaker::error::AgentError;
use bytemaker::output;
use bytemaker::render::{Coordinator, CrosstermBackend};
use dotenv::dotenv;
use std::env;
use std::path::PathBuf;
use tokio::io::AsyncBufReadExt;
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
    output::logo();
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

    let coordinator = std::sync::Arc::new(std::sync::Mutex::new(
        Coordinator::new(CrosstermBackend::new())
    ));
    let cfg = AgentConfig {
        api_key,
        base_url,
        model,
        workdir: cwd.clone(),
        skills_dir: PathBuf::from(&skills_dir),
        coordinator,
        team_input_sender: None,
    };
    let agent = Agent::new(cfg).await?;
    agent.start_cron_runtime().await?;
    output::banner(&format!(
        "Loaded {} skill(s) from {}",
        agent.skills_len(),
        skills_dir
    ));

    let mut messages: Vec<Message> = Vec::new();
    let mut reader = tokio::io::BufReader::new(tokio::io::stdin()).lines();

    loop {
        output::prompt();
        // s13: wake the Lead when a teammate delivers an event (result/idle/plan).
        let notify = agent.lead_notify().expect("team initialized");
        tokio::select! {
            biased;
            // stdin line → a user turn.
            line = reader.next_line() => {
                let line = match line {
                    Ok(Some(s)) => s,
                    _ => break,
                };
                let query = line.trim().to_string();
                if query.is_empty() {
                    continue;
                }
                if query.eq_ignore_ascii_case("q") || query == "exit" {
                    break;
                }
                // 用户输入后、进入 LLM 前触发 UserPromptSubmit。
                agent.trigger_prompt(&query).await;
                messages.push(Message::user_text(query.clone()));
                if let Err(e) = agent.run_loop(&mut messages, &query).await {
                    output::error(&format!("Error: {}", e));
                }
                output::blank();
            }
            // Lead inbox notify → drain typed events into a new turn.
            _ = notify.notified() => {
                let inbox = bytemaker::team::consume_lead_inbox(agent.team().unwrap());
                if inbox.is_empty() {
                    continue;
                }
                let text = bytemaker::team::format_team_events(&inbox);
                messages.push(Message::user_text(text));
                println!("[wake: {} team event(s) -> new turn]", inbox.len());
                if let Err(e) = agent.run_loop(&mut messages, "[team events]").await {
                    output::error(&format!("Error: {}", e));
                }
            }
        }
    }

    Ok(())
}
