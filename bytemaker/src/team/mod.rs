pub mod lock;
pub mod bus;
pub mod assignment;
pub mod protocols;
// pub mod runtime;    // Task 10
// pub mod tools;      // Task 9
pub mod worktree;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::client::{ContentBlock, Message};
use crate::task_system::task::{Task, TaskStatus};
use crate::task_system::store::TaskStore;
use crate::team::assignment::{Assignment, AssignmentRegistry};
use crate::team::bus::MessageRecord;
use crate::team::lock::TaskStoreLock;
use crate::team::protocols::{GateStatus, ProtocolRegistry, ProtocolStatus, ProtocolType};
use crate::team::worktree::task_worktree_cwd;

/// Shared team state (one Arc): mailboxes, assignments, protocol state,
/// active-teammate registry, a Notify for waking the Lead REPL, the task
/// store, the repo workdir, and the cross-process TaskStoreLock.
pub struct TeamCtx {
    pub bus: crate::team::bus::MessageBus,
    pub assignments: AssignmentRegistry,
    pub protocols: ProtocolRegistry,
    pub active: Mutex<HashMap<String, TeammateStatus>>,
    pub lead_notify: tokio::sync::Notify,
    pub task_store: Arc<TaskStore>,
    pub workdir: PathBuf,
    pub lock: TaskStoreLock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeammateStatus {
    Working,
    WaitingApproval,
    Idle,
    Stopping,
}

impl TeamCtx {
    pub fn new(workdir: PathBuf, task_store: Arc<TaskStore>) -> std::io::Result<Self> {
        let tasks_dir = task_store_dir(&task_store);
        Ok(Self {
            bus: crate::team::bus::MessageBus::new(workdir.clone()),
            assignments: AssignmentRegistry::new(),
            protocols: ProtocolRegistry::new(),
            active: Mutex::new(HashMap::new()),
            lead_notify: tokio::sync::Notify::new(),
            task_store,
            workdir,
            lock: TaskStoreLock::new(&tasks_dir)?,
        })
    }

    pub fn lead_notify(&self) -> &tokio::sync::Notify {
        &self.lead_notify
    }
}

/// Expose the store's `.tasks` directory so TaskStoreLock can lock `.tasks/.lock`.
fn task_store_dir(store: &TaskStore) -> PathBuf {
    store.directory().to_path_buf()
}

/// Atomically claim one task and bind the owner's cwd. Returns a user-facing string.
pub fn claim_task(team: &TeamCtx, task_id: &str, owner: &str) -> String {
    let _g = match team.lock.lock() {
        Ok(g) => g,
        Err(e) => return format!("Error: {}", e),
    };
    let store = &team.task_store;
    let Ok(mut task) = store.load(task_id) else {
        return format!("Error: Task {} not found", task_id);
    };
    if task.status != TaskStatus::Pending {
        return format!("Task {} is {}, cannot claim", task_id, task.status.as_word());
    }
    if task.owner.is_some() {
        return format!(
            "Task {} is already owned by {}",
            task_id,
            task.owner.as_deref().unwrap_or("?")
        );
    }
    if team.assignments.get(owner).is_some() {
        return format!("Owner {} must finish current work before claiming another task", owner);
    }
    if !incomplete_deps_empty(team, &task) {
        return format!("Blocked by: {:?}", incomplete_deps(team, &task));
    }
    let (cwd, err) = task_worktree_cwd(&team.workdir, &task);
    if let Some(e) = err {
        return format!("Cannot claim {}: {}", task_id, e);
    }
    task.status = TaskStatus::InProgress;
    task.owner = Some(owner.to_string());
    if let Err(e) = store.save(&task) {
        return format!("Error: {}", e);
    }
    team.assignments
        .set(owner, Assignment { task_id: task.id.clone(), cwd });
    team.assignments.advance_version(owner);
    format!("Claimed {} ({})", task.id, task.subject)
}

/// Complete an owned, in-progress task. Respects the plan gate.
pub fn complete_task(team: &TeamCtx, task_id: &str, owner: &str) -> String {
    let _g = match team.lock.lock() {
        Ok(g) => g,
        Err(e) => return format!("Error: {}", e),
    };
    let store = &team.task_store;
    let Ok(mut task) = store.load(task_id) else {
        return format!("Error: Task {} not found", task_id);
    };
    if task.status != TaskStatus::InProgress {
        return format!("Task {} is {}, cannot complete", task_id, task.status.as_word());
    }
    if task.owner.as_deref() != Some(owner) {
        return format!(
            "Task {} is owned by {}, not {}",
            task_id,
            task.owner.as_deref().unwrap_or("none"),
            owner
        );
    }
    if team.protocols.gate(owner).blocks_mutating_tools() {
        return format!("Task {} cannot complete while plan status blocks changes", task_id);
    }
    task.status = TaskStatus::Completed;
    if let Err(e) = store.save(&task) {
        return format!("Error: {}", e);
    }
    format!("Completed {} ({})", task.id, task.subject)
}

fn incomplete_deps(team: &TeamCtx, task: &Task) -> Vec<String> {
    task.blocked_by
        .iter()
        .filter(|d| match team.task_store.load(d) {
            Ok(t) => t.status != TaskStatus::Completed,
            Err(_) => true,
        })
        .cloned()
        .collect()
}

fn incomplete_deps_empty(team: &TeamCtx, task: &Task) -> bool {
    incomplete_deps(team, task).is_empty()
}

// ---- s13 run-loop inbox drain + protocol application (Task 7) ----

/// Drain this teammate's inbox into its messages. Returns true if a shutdown
/// was accepted (the caller should end the loop).
pub fn drain_inbox(team: &TeamCtx, owner: &str, messages: &mut Vec<Message>) -> bool {
    let inbox = team.bus.read_inbox(owner);
    if inbox.is_empty() {
        return false;
    }
    let mut work: Vec<String> = Vec::new();
    let mut should_stop = false;
    for msg in inbox {
        match msg.msg_type.as_str() {
            "shutdown_request" => {
                if apply_shutdown_request(team, owner, &msg) {
                    team.bus.send(
                        owner,
                        "lead",
                        "Shutdown acknowledged.",
                        "shutdown_response",
                        Some(serde_json::json!({
                            "request_id": msg.metadata.get("request_id").cloned().unwrap_or(serde_json::Value::Null),
                            "approve": true,
                        })),
                    );
                    should_stop = true;
                } else {
                    work.push("[Ignored shutdown request: request mismatch]".into());
                }
            }
            "plan_approval_response" => {
                work.push(apply_plan_response(team, owner, &msg));
            }
            "plan_request" => {
                work.push(format!("[Plan required] {}", msg.content));
            }
            _ => work.push(format!("[Message from {}] {}", msg.from, msg.content)),
        }
    }
    if !work.is_empty() {
        messages.push(Message {
            role: "user".to_string(),
            content: vec![ContentBlock::Text { text: work.join("\n") }],
        });
    }
    should_stop
}

/// Accept a pending shutdown request sent by Lead to this teammate.
fn apply_shutdown_request(team: &TeamCtx, owner: &str, msg: &MessageRecord) -> bool {
    let request_id = match msg.metadata.get("request_id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return false,
    };
    let pending = team.protocols.pending.lock().unwrap();
    let Some(state) = pending.get(request_id) else {
        return false;
    };
    if state.ptype != ProtocolType::Shutdown {
        return false;
    }
    if state.sender != "lead" || state.target != owner {
        return false;
    }
    if state.status != ProtocolStatus::Pending {
        return false;
    }
    let stopping = matches!(
        team.active.lock().unwrap().get(owner),
        Some(TeammateStatus::Stopping)
    );
    if stopping {
        return false;
    }
    team.active
        .lock()
        .unwrap()
        .insert(owner.into(), TeammateStatus::Stopping);
    true
}

/// Apply the Lead's plan-approval response if it matches this teammate's current plan.
fn apply_plan_response(team: &TeamCtx, owner: &str, msg: &MessageRecord) -> String {
    let request_id = match msg.metadata.get("request_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return "[Ignored plan response: no request_id]".into(),
    };
    let work_version = team.assignments.version(owner);
    let task_id = team.assignments.get(owner).map(|a| a.task_id);
    let mut pending = team.protocols.pending.lock().unwrap();
    let Some(state) = pending.get(&request_id) else {
        return "[Ignored plan response: request mismatch]".into();
    };
    let expected_id = team
        .protocols
        .plan_request_ids
        .lock()
        .unwrap()
        .get(owner)
        .cloned();
    let approve = msg
        .metadata
        .get("approve")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let valid = state.ptype == ProtocolType::PlanApproval
        && state.sender == owner
        && state.target == "lead"
        && Some(&request_id) == expected_id.as_ref()
        && state.work_version == Some(work_version)
        && state.task_id == task_id
        && state.status == ProtocolStatus::Pending
        && approve;
    if !valid {
        return "[Ignored plan response: request mismatch]".into();
    }
    let new_gate = if approve { GateStatus::Approved } else { GateStatus::Rejected };
    let new_status = if approve { ProtocolStatus::Approved } else { ProtocolStatus::Rejected };
    pending.get_mut(&request_id).unwrap().status = new_status;
    drop(pending);
    team.protocols.set_gate(owner, new_gate);
    team.protocols.plan_request_ids.lock().unwrap().remove(owner);
    team.active
        .lock()
        .unwrap()
        .insert(owner.into(), TeammateStatus::Working);
    format!(
        "[Plan {}] {}",
        if approve { "approved" } else { "rejected" },
        msg.content
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_system::store::create_test_store;
    use tempfile::TempDir;

    fn ctx(tmp: &tempfile::TempDir) -> (Arc<TaskStore>, TeamCtx) {
        let store = Arc::new(create_test_store(tmp.path()));
        let team = TeamCtx::new(tmp.path().to_path_buf(), store.clone()).unwrap();
        (store, team)
    }

    #[test]
    fn claim_succeeds_for_pending_unowned() {
        let tmp = TempDir::new().unwrap();
        let (store, team) = ctx(&tmp);
        let t = store.create("T".into(), "".into(), vec![]).unwrap();
        assert!(claim_task(&team, &t.id, "alice").starts_with("Claimed"));
        assert_eq!(team.assignments.get("alice").unwrap().task_id, t.id);
    }

    #[test]
    fn claim_blocks_second_owner() {
        let tmp = TempDir::new().unwrap();
        let (store, team) = ctx(&tmp);
        let t = store.create("T".into(), "".into(), vec![]).unwrap();
        claim_task(&team, &t.id, "alice");
        let r = claim_task(&team, &t.id, "bob");
        // alice's claim moved the task to InProgress, so bob is rejected at the
        // status gate ("is in_progress, cannot claim"); the "already owned"
        // branch only fires for the inconsistent Pending+owned state.
        assert!(r.contains("cannot claim"), "second owner must be blocked, got {}", r);
    }

    #[test]
    fn claim_rejects_second_task_same_owner() {
        let tmp = TempDir::new().unwrap();
        let (store, team) = ctx(&tmp);
        let t1 = store.create("A".into(), "".into(), vec![]).unwrap();
        let t2 = store.create("B".into(), "".into(), vec![]).unwrap();
        claim_task(&team, &t1.id, "alice");
        let r = claim_task(&team, &t2.id, "alice");
        assert!(r.contains("finish current work"));
    }

    #[test]
    fn complete_checks_owner() {
        let tmp = TempDir::new().unwrap();
        let (store, team) = ctx(&tmp);
        let t = store.create("T".into(), "".into(), vec![]).unwrap();
        claim_task(&team, &t.id, "alice");
        let r = complete_task(&team, &t.id, "bob");
        assert!(r.contains("owned by alice"));
    }

    #[test]
    fn concurrent_claims_one_winner() {
        let tmp = TempDir::new().unwrap();
        let (store, team) = ctx(&tmp);
        let t = store.create("T".into(), "".into(), vec![]).unwrap();
        let tid = t.id.clone();
        let team = Arc::new(team);
        let mut handles = vec![];
        for name in ["alice", "bob", "carol"] {
            let team = Arc::clone(&team);
            let tid = tid.clone();
            handles.push(std::thread::spawn(move || claim_task(&team, &tid, name)));
        }
        let results: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let winners = results.iter().filter(|r| r.starts_with("Claimed")).count();
        assert_eq!(winners, 1, "exactly one concurrent claim must win, got {:?}", results);
    }
}
