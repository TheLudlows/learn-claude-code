use std::sync::Arc;

use crate::agent::Agent;
use crate::client::{ContentBlock, Message};
use crate::team::protocols::GateStatus;
use crate::team::{
    claim_next_task, drain_inbox, extract_last_assistant_text,
    release_completed_assignment, release_teammate_assignment, teammate_system_prompt,
    TeamCtx, TeammateStatus, IDLE_SCAN_INTERVAL,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Continue,
    Idle,
    Stop,
}

/// One persistent teammate: its agent, conversation, and shared team context.
/// Moved into `tokio::spawn`; must be `Send` (Agent/Hooks are Send+Sync, verified).
pub struct TeammateRuntime {
    pub name: String,
    pub agent: Agent,
    pub messages: Vec<Message>,
    pub team: Arc<TeamCtx>,
}

impl TeammateRuntime {
    /// Build the initial messages, including an [Assigned task] block if the
    /// teammate was spawned with an already-claimed task. `child_agent` is built
    /// by the caller via `lead_agent.child_teammate(...)` — the runtime does NOT
    /// reference the Lead agent, avoiding a TeamCtx → Agent → TeamCtx Arc cycle.
    pub fn new(name: String, _role: &str, prompt: String, team: Arc<TeamCtx>, child_agent: Agent) -> Self {
        let mut messages = vec![Message {
            role: "user".to_string(),
            content: vec![ContentBlock::Text { text: prompt }],
        }];
        if let Some(a) = team.assignments.get(&name) {
            if let Ok(task) = team.task_store.load(&a.task_id) {
                let block = format!(
                    "\n\n[Assigned task {}] {}\n{}\nWork directory: {}",
                    task.id,
                    task.subject,
                    task.description,
                    a.cwd.display()
                );
                if let Some(ContentBlock::Text { text }) = messages[0].content.get_mut(0) {
                    text.push_str(&block);
                }
            }
        }
        Self { name, agent: child_agent, messages, team }
    }

    /// Run the teammate until it stops (shutdown accepted or work done + idle exit).
    pub async fn run(mut self) {
        let mut phase = Phase::Continue;
        while phase != Phase::Stop {
            if phase == Phase::Idle {
                if !self.wait_for_work().await {
                    break;
                }
            }
            phase = self.work().await;
        }
        release_teammate_assignment(&self.team, &self.name);
        self.team.active.lock().unwrap().remove(&self.name);
    }

    /// Run one model turn. Returns the next phase (Stop on shutdown ack, Idle
    /// while a plan is pending approval, Idle after delivering a result).
    async fn work(&mut self) -> Phase {
        // Clone `name` so the cross-`await` borrows are two disjoint fields
        // (self.agent immutable, self.messages mutable) rather than three.
        let name = self.name.clone();
        let _ = self.agent.run_loop(&mut self.messages, &name).await;

        if matches!(
            self.team.active.lock().unwrap().get(&self.name).copied(),
            Some(TeammateStatus::Stopping)
        ) {
            return Phase::Stop;
        }
        let gate = self.team.protocols.gate(&self.name);
        if gate == GateStatus::Pending {
            self.team
                .active
                .lock()
                .unwrap()
                .insert(self.name.clone(), TeammateStatus::WaitingApproval);
            return Phase::Idle;
        }
        if let Some(summary) = extract_last_assistant_text(&self.messages) {
            self.team.bus.send(&self.name, "lead", &summary, "result", None);
            self.team.lead_notify.notify_one();
        }
        release_completed_assignment(&self.team, &self.name);
        self.team
            .active
            .lock()
            .unwrap()
            .insert(self.name.clone(), TeammateStatus::Idle);
        self.team
            .bus
            .send(&self.name, "lead", "Waiting for more work.", "idle_notification", None);
        self.team.lead_notify.notify_one();
        Phase::Idle
    }

    /// Block until there is work: a mailbox message or a claimable task.
    /// Returns false if a shutdown was accepted (caller should exit).
    async fn wait_for_work(&mut self) -> bool {
        loop {
            let inbox = self.team.bus.wait_for_messages(&self.name, IDLE_SCAN_INTERVAL).await;
            if !inbox.is_empty() {
                let before = self.messages.len();
                let stop = drain_inbox(&self.team, &self.name, &mut self.messages);
                if stop {
                    return false;
                }
                if self.messages.len() > before {
                    return true;
                }
                continue;
            }
            if let Some(task) = claim_next_task(&self.team, &self.name) {
                let cwd = self
                    .team
                    .assignments
                    .get(&self.name)
                    .map(|a| a.cwd.clone())
                    .unwrap_or_else(|| self.team.workdir.clone());
                let text = format!(
                    "[Auto-claimed task {}] {}\n{}\nWork directory: {}",
                    task.id,
                    task.subject,
                    task.description,
                    cwd.display()
                );
                self.messages.push(Message {
                    role: "user".to_string(),
                    content: vec![ContentBlock::Text { text }],
                });
                return true;
            }
        }
    }
}

/// Validate + (optionally) claim an initial task, then spawn one persistent
/// teammate. In tests the live `tokio::spawn` is skipped so no real runtime runs.
pub fn spawn_teammate_thread(
    lead_agent: &Agent,
    name: &str,
    role: &str,
    prompt: &str,
    task_id: Option<&str>,
    require_plan: bool,
) -> String {
    use crate::team::bus::is_valid_agent_name;
    use crate::team::{claim_task, is_reserved_teammate_name};

    if !is_valid_agent_name(name) {
        return "Invalid teammate name: use 1-64 letters, digits, underscores, or dashes".into();
    }
    if is_reserved_teammate_name(name) {
        return format!("Invalid teammate name: '{}' is reserved by the runtime", name);
    }
    let Some(team) = lead_agent.team() else {
        return "Error: team not initialized".into();
    };
    {
        let active = team.active.lock().unwrap();
        if active.keys().any(|k| k.eq_ignore_ascii_case(name)) {
            return format!("Teammate '{}' already exists", name);
        }
    }
    team.active
        .lock()
        .unwrap()
        .insert(name.into(), TeammateStatus::Working);
    team.protocols.set_gate(
        name,
        if require_plan { GateStatus::Required } else { GateStatus::NotRequired },
    );

    if let Some(tid) = task_id {
        let r = claim_task(team, tid, name);
        if !r.starts_with("Claimed") {
            team.active.lock().unwrap().remove(name);
            team.protocols.set_gate(name, GateStatus::NotRequired);
            return format!("Cannot spawn teammate '{}': {}", name, r);
        }
    }
    let system = teammate_system_prompt(name, role);
    let agent = lead_agent.child_teammate(name, &system, Arc::clone(team));
    let rt = TeammateRuntime::new(name.into(), role, prompt.into(), Arc::clone(team), agent);
    #[cfg(not(test))]
    tokio::spawn(async move { rt.run().await });
    #[cfg(test)]
    {
        drop(rt);
    }
    format!(
        "Teammate '{}' spawned as {}. End this turn; the runtime will deliver its events.",
        name, role
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::TestAgent;
    use crate::team::is_reserved_teammate_name;

    #[test]
    fn reserved_names_rejected() {
        assert!(is_reserved_teammate_name("lead"));
        assert!(is_reserved_teammate_name("Lead"));
        assert!(is_reserved_teammate_name("agent"));
        assert!(!is_reserved_teammate_name("alice"));
    }

    #[test]
    fn spawn_rejects_invalid_and_reserved() {
        let a = TestAgent::new();
        let r = spawn_teammate_thread(a.agent(), "../x", "r", "p", None, false);
        assert!(r.contains("Invalid"));
        let r = spawn_teammate_thread(a.agent(), "lead", "r", "p", None, false);
        assert!(r.contains("reserved"));
    }

    #[test]
    fn spawn_rejects_duplicate() {
        let a = TestAgent::new();
        let r1 = spawn_teammate_thread(a.agent(), "alice", "r", "p", None, false);
        assert!(r1.contains("spawned"));
        let r2 = spawn_teammate_thread(a.agent(), "alice", "r", "p", None, false);
        assert!(r2.contains("already exists"));
    }

    #[test]
    fn spawn_with_bad_task_does_not_spawn() {
        let a = TestAgent::new();
        let r = spawn_teammate_thread(a.agent(), "alice", "r", "p", Some("task_00000000"), false);
        assert!(r.contains("Cannot spawn"), "bad task_id must prevent spawn, got {}", r);
        // no active teammate left
        let team = a.agent().team().unwrap();
        assert!(!team.active.lock().unwrap().contains_key("alice"));
    }
}
