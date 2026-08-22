/*
main.rs - REPL 入口（逐行 I/O 简化模型）

核心循环与共享状态都移入 lib 的 `Agent`（agent.rs）：main 只做 CLI 装配——
读 env、构造 Agent、启动 cron、REPL 调 `agent.run_loop`。

终端交互（简化后，2026-08-21）：不再用 raw 模式 + 滚动区维持"末行固定输入栏"，
改为普通逐行 I/O。
- 交互模式（真 TTY）：`InputTask`（reedline）独占 stdin，经单一命令通道收
  `ReadLine` / `AskPermission`；main 只 `await` 该行（不再 `select!` notify）。
  team 事件**延迟到下一轮用户回合开头**排空（`run_team_wake` 内部 consume，
  空则 no-op）——不再打断正在打字的回合，避免在 reedline 阻塞读期间流式输出糊屏。
- 非交互模式（管道/CI）：tokio 行读取 + `select!{ line, lead_notify }`，team
  事件立即唤醒（async future 可 drop，无 raw 模式糊屏问题）；不 spawn InputTask。
  权限钩子无 ask 通道 → 需批准的命令直接拒绝（不挂起）。
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
use bytemaker::render::{Coordinator, CrosstermBackend};
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

    // logo：cooked 模式 `\n` 正常（reedline 仅在读期间瞬态开 raw，不影响此处）。
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

    // 创建 I/O 组合
    let io = Arc::new(bytemaker::io::IO::console(coordinator.clone(), cmd_tx.clone()));

    let cfg = AgentConfig {
        api_key,
        base_url,
        model,
        workdir: cwd.clone(),
        skills_dir: PathBuf::from(&skills_dir),
        io: io.clone(),
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
        run_interactive(&agent, &mut messages, Arc::clone(&io.output), cmd_tx).await?;
    } else {
        // ---- 非交互模式：tokio 行读取 ----
        run_noninteractive(&agent, &mut messages, Arc::clone(&io.output)).await;
    }

    // s14: 清理所有 MCP server 进程
    agent.shutdown_mcp().await;

    Ok(())
}

/// 交互模式 REPL：reedline InputTask，纯 await line（不再 select! notify）。
/// team 事件 defer 到下一轮用户回合开头排空（`run_team_wake` 内部 consume，
/// 空则 no-op）——避免在 reedline 阻塞读期间流式输出糊屏。
async fn run_interactive(
    agent: &Agent,
    messages: &mut Vec<Message>,
    output: Arc<dyn bytemaker::io::Output>,
    cmd_tx: tokio::sync::mpsc::Sender<InputCmd>,
) -> Result<(), AgentError> {
    loop {
        // 请求 InputTask 读一行查询（reedline 自行渲染 ` >> ` 提示符）。
        let (line_tx, line_rx) = oneshot::channel();
        if cmd_tx.send(InputCmd::ReadLine(line_tx)).await.is_err() {
            break; // InputTask 线程已退出
        }
        let line = match line_rx.await {
            Ok(Some(l)) => l,
            _ => break, // EOF / Ctrl+C：InputTask 已退出
        };
        let query = line.trim().to_string();
        if query.is_empty() {
            continue; // 空行：重新发 ReadLine
        }
        if query.eq_ignore_ascii_case("q") || query == "exit" {
            let _ = cmd_tx.send(InputCmd::Shutdown).await;
            return Ok(());
        }
        // defer 唤醒：每轮用户回合开头排空 Lead 收件箱。`run_team_wake`
        // 内部自己 `consume_lead_inbox`，空收件箱 → is_empty → 返回 false
        //（no-op）；勿在外层预 consume（会与内部 consume 重复排空成空）。
        if run_team_wake(agent, messages, Arc::clone(&output)).await {
            let _ = cmd_tx.send(InputCmd::Shutdown).await;
            return Ok(());
        }
        if run_user_turn(agent, messages, &query, Arc::clone(&output)).await {
            let _ = cmd_tx.send(InputCmd::Shutdown).await;
            return Ok(());
        }
    }
    Ok(())
}

/// 非交互模式 REPL：tokio stdin 行读取 + select{line, lead_notify}。
async fn run_noninteractive(
    agent: &Agent,
    messages: &mut Vec<Message>,
    output: Arc<dyn bytemaker::io::Output>,
) {
    let mut reader = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    loop {
        output.prompt();
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
                if run_user_turn(agent, messages, &query, Arc::clone(&output)).await {
                    break;
                }
            }
            _ = notify.notified() => {
                if run_team_wake(agent, messages, Arc::clone(&output)).await {
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
    output: Arc<dyn bytemaker::io::Output>,
) -> bool {
    // 用户输入后、进入 LLM 前触发 UserPromptSubmit。
    agent.trigger_prompt(query).await;
    messages.push(Message::user_text(query.to_string()));
    match agent.run_loop(messages, query).await {
        Ok(LoopOutcome::Cancelled) => true,
        Ok(_) => {
            output.blank();
            false
        }
        Err(e) => {
            output.error(&format!("Error: {}", e));
            false
        }
    }
}

/// 处理 team 唤醒：排空 Lead 收件箱里的 typed 事件，作为新回合喂回。
/// 返回 true 表示应退出（Cancelled）；false 表示继续。
async fn run_team_wake(
    agent: &Agent,
    messages: &mut Vec<Message>,
    output: Arc<dyn bytemaker::io::Output>,
) -> bool {
    let inbox = team::consume_lead_inbox(agent.team().expect("team initialized"));
    if inbox.is_empty() {
        return false;
    }
    let text = team::format_team_events(&inbox);
    messages.push(Message::user_text(text));
    output.banner(&format!(
        "[wake: {} team event(s) -> new turn]",
        inbox.len()
    ));
    match agent.run_loop(messages, "[team events]").await {
        Ok(LoopOutcome::Cancelled) => true,
        Ok(_) => false,
        Err(e) => {
            output.error(&format!("Error: {}", e));
            false
        }
    }
}
