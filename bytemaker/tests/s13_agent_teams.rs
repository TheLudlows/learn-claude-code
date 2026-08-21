// s13 end-to-end smoke test.
//
// Requires ANTHROPIC_API_KEY / MODEL_ID and a git repo at CWD. Run with:
//   cargo test --test s13_agent_teams --features smoke -- --ignored
// The `smoke` feature re-enables the real `tokio::spawn` inside
// spawn_teammate_thread (otherwise #[cfg(test)] skips it). Failures here are
// flaky-investigation, not unit-suite blockers.

use bytemaker::agent::{Agent, AgentConfig};

#[tokio::test]
#[ignore]
async fn spawn_teammate_delivers_result_and_idle() {
    let api_key = std::env::var("ANTHROPIC_AUTH_TOKEN")
        .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
        .unwrap();
    let base_url = std::env::var("ANTHROPIC_BASE_URL")
        .unwrap_or_else(|_| "https://api.anthropic.com".into());
    let model = std::env::var("MODEL_ID").unwrap();
    let cwd = std::env::current_dir().unwrap();
    let coordinator = std::sync::Arc::new(std::sync::Mutex::new(
        bytemaker::render::Coordinator::new(bytemaker::render::CrosstermBackend::new())
    ));
    let agent = Agent::new(AgentConfig {
        api_key,
        base_url,
        model,
        workdir: cwd.clone(),
        skills_dir: cwd.join("skills"),
        coordinator,
        team_input_sender: None,
    })
    .await
    .unwrap();
    let team = agent.team().unwrap().clone();

    // Lead creates a task, then spawns a teammate on it.
    let task = team
        .task_store
        .create("Echo hello".into(), "Print 'hello' and complete.".into(), vec![])
        .unwrap();
    let spawn = bytemaker::team::runtime::spawn_teammate_thread(
        &agent,
        "alice",
        "coder",
        "Complete the assigned task.",
        Some(task.id.as_str()),
        false,
    );
    assert!(spawn.contains("spawned"), "{}", spawn);

    // Wait (bounded) for the Lead inbox to receive a result + idle notification.
    let mut got_result = false;
    let mut got_idle = false;
    for _ in 0..120 {
        let inbox = bytemaker::team::consume_lead_inbox(&team);
        for m in &inbox {
            if m.msg_type == "result" {
                got_result = true;
            }
            if m.msg_type == "idle_notification" {
                got_idle = true;
            }
        }
        if got_result && got_idle {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert!(got_result, "Lead must receive a result event");
    assert!(got_idle, "Lead must receive an idle_notification");

    // Clean shutdown.
    bytemaker::team::tools::request_shutdown(&team, "alice");
    let _ = bytemaker::team::consume_lead_inbox(&team); // drain shutdown_response
}
