/*
main.rs - REPL 入口（s13 / 控制台 I/O 分离）

核心循环与共享状态都移入 lib 的 `Agent`（agent.rs）：main 只做 CLI 装配——
读 env、构造 Agent、启动 cron、REPL 调 `agent.run_loop`。原 `execute_tool`/`agent_loop`
/`set_instance`/`init_manager`/`start_runtime` 全部并入 Agent，此处不再出现。

控制台 I/O 分离（spec 2026-08-20）：
- 交互模式（真 TTY）：`RawModeGuard` 进 raw 模式 + 设滚动区（末行留输入栏）；
  `InputTask`（reedline）独占 stdin，经单一命令通道收 `ReadLine` / `AskPermission`；
  main 在 `select!{ line_rx, lead_notify }` 上等待，team 事件打断输入等待时
  **保留在途的 ReadLine**（用户在 team 回合期间键入的行成为下一轮用户回合）。
- 非交互模式（管道/CI）：退化到 tokio 行读取，不进 raw、不设滚动区、不 spawn
  InputTask；权限钩子无 ask 通道 → 需批准的命令直接拒绝（不挂起）。
两种模式的用户回合 / team 唤醒逻辑共享 `run_user_turn` / `run_team_wake`。
*/

use std::env;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use bytemaker::agent::{Agent, AgentConfig, LoopOutcome};
use bytemaker::client::Message;
use bytemaker::error::AgentError;
use bytemaker::output;
use bytemaker::render::input::InputCmd;
use bytemaker::render::{Coordinator, CrosstermBackend, RawModeGuard};
use bytemaker::team;
use dotenv::dotenv;
use tokio::io::AsyncBufReadExt;
use tokio::sync::oneshot;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), AgentError> {
    dotenv().ok();
    // 诊断日志（[memory]/[snip_compact]/[persist] 等）：默认 INFO，RUST_LOG=warn 静默，
    // =debug 更细。UX 行不走 tracing，仍由 output.rs 着色 / Coordinator 写。
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    // logo 在进 raw 模式前打印（cooked 模式下 `\n` 正常；进 raw 后所有输出走 Coordinator 的 `\r\n`）。
    output::logo();

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

    // 控制台 I/O 分离：交互模式进 raw + 设滚动区；非 TTY 全部跳过。
    let interactive = std::io::stdout().is_terminal();
    let _guard = RawModeGuard::new(interactive);

    let coordinator: Arc<Mutex<Coordinator<CrosstermBackend>>> =
        Arc::new(Mutex::new(Coordinator::new(CrosstermBackend::new())));
    {
        let mut c = coordinator.lock().unwrap();
        c.banner("Enter a question, press Enter to send. Type q to quit.\n");
        c.banner(&format!(
            "base_url: {}, model: {}, key: {}",
            base_url, model, "***"
        ));
    }

    // 交互模式才 spawn InputTask；其命令发送端克隆给 agent，供权限钩子下发 AskPermission。
    let cmd_tx = if interactive {
        Some(bytemaker::render::input::spawn())
    } else {
        None
    };
    let cfg = AgentConfig {
        api_key,
        base_url,
        model,
        workdir: cwd.clone(),
        skills_dir: PathBuf::from(&skills_dir),
        coordinator: coordinator.clone(),
        team_input_sender: cmd_tx.clone(),
    };
    let agent = Agent::new(cfg).await?;
    agent.start_cron_runtime().await?;
    coordinator.lock().unwrap().banner(&format!(
        "Loaded {} skill(s) from {}",
        agent.skills_len(),
        skills_dir
    ));

    let mut messages: Vec<Message> = Vec::new();

    if let Some(cmd_tx) = cmd_tx {
        // ---- 交互模式：reedline InputTask ----
        run_interactive(&agent, &mut messages, &coordinator, cmd_tx).await?;
    } else {
        // ---- 非交互模式：tokio 行读取 ----
        run_noninteractive(&agent, &mut messages, &coordinator).await;
    }

    Ok(())
}

/// 交互模式 REPL：reedline InputTask + select{line, lead_notify}。
async fn run_interactive(
    agent: &Agent,
    messages: &mut Vec<Message>,
    coordinator: &Arc<Mutex<Coordinator<CrosstermBackend>>>,
    cmd_tx: tokio::sync::mpsc::Sender<InputCmd>,
) -> Result<(), AgentError> {
    loop {
        // 请求 InputTask 读一行查询（reedline 自行渲染 ` >> ` 提示符）。
        let (line_tx, mut line_rx) = oneshot::channel();
        if cmd_tx.send(InputCmd::ReadLine(line_tx)).await.is_err() {
            break; // InputTask 线程已退出
        }
        let notify = agent.lead_notify().expect("team initialized");
        loop {
            tokio::select! {
                biased;
                res = &mut line_rx => {
                    let line = match res {
                        Ok(Some(l)) => l,
                        _ => break, // EOF / Ctrl+C：InputTask 已退出，本路也结束
                    };
                    let query = line.trim().to_string();
                    if query.is_empty() {
                        break; // 空行：重新发 ReadLine
                    }
                    if query.eq_ignore_ascii_case("q") || query == "exit" {
                        let _ = cmd_tx.send(InputCmd::Shutdown).await;
                        return Ok(());
                    }
                    if run_user_turn(agent, messages, &query, coordinator).await {
                        let _ = cmd_tx.send(InputCmd::Shutdown).await;
                        return Ok(());
                    }
                    break; // 本轮结束：重新发 ReadLine
                }
                _ = notify.notified() => {
                    // team 事件打断输入等待。在途的 ReadLine 保留在 InputTask 线程上
                    // （用户在 team 回合期间键入的行将在 team 回合后由本 select 取回）。
                    if run_team_wake(agent, messages, coordinator).await {
                        let _ = cmd_tx.send(InputCmd::Shutdown).await;
                        return Ok(());
                    }
                    continue; // 重新 select 同一个 line_rx（仍在途，或用户已键入则就绪）
                }
            }
        }
    }
    Ok(())
}

/// 非交互模式 REPL：tokio stdin 行读取 + select{line, lead_notify}。
async fn run_noninteractive(
    agent: &Agent,
    messages: &mut Vec<Message>,
    coordinator: &Arc<Mutex<Coordinator<CrosstermBackend>>>,
) {
    let mut reader = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    loop {
        coordinator.lock().unwrap().prompt();
        let notify = agent.lead_notify().expect("team initialized");
        tokio::select! {
            biased;
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
                if run_user_turn(agent, messages, &query, coordinator).await {
                    break;
                }
            }
            _ = notify.notified() => {
                if run_team_wake(agent, messages, coordinator).await {
                    break;
                }
            }
        }
    }
}

/// 跑一轮用户回合。返回 true 表示应退出（Cancelled）；false 表示继续。
async fn run_user_turn(
    agent: &Agent,
    messages: &mut Vec<Message>,
    query: &str,
    coordinator: &Arc<Mutex<Coordinator<CrosstermBackend>>>,
) -> bool {
    // 用户输入后、进入 LLM 前触发 UserPromptSubmit。
    agent.trigger_prompt(query).await;
    messages.push(Message::user_text(query.to_string()));
    match agent.run_loop(messages, query).await {
        Ok(LoopOutcome::Cancelled) => true,
        Ok(_) => {
            coordinator.lock().unwrap().blank();
            false
        }
        Err(e) => {
            coordinator.lock().unwrap().error(&format!("Error: {}", e));
            false
        }
    }
}

/// 处理 team 唤醒：排空 Lead 收件箱里的 typed 事件，作为新回合喂回。
/// 返回 true 表示应退出（Cancelled）；false 表示继续。
async fn run_team_wake(
    agent: &Agent,
    messages: &mut Vec<Message>,
    coordinator: &Arc<Mutex<Coordinator<CrosstermBackend>>>,
) -> bool {
    let inbox = team::consume_lead_inbox(agent.team().expect("team initialized"));
    if inbox.is_empty() {
        return false;
    }
    let text = team::format_team_events(&inbox);
    messages.push(Message::user_text(text));
    coordinator.lock().unwrap().banner(&format!(
        "[wake: {} team event(s) -> new turn]",
        inbox.len()
    ));
    match agent.run_loop(messages, "[team events]").await {
        Ok(LoopOutcome::Cancelled) => true,
        Ok(_) => false,
        Err(e) => {
            coordinator.lock().unwrap().error(&format!("Error: {}", e));
            false
        }
    }
}
