use async_trait::async_trait;
use serde_json::{json, Value};
use crate::tools::trait_def::{AgentKind, PermissionCheck, Tool, ToolContext};
use crate::team::protocols::{GateStatus, ProtocolState, ProtocolStatus, ProtocolType};
use crate::team::{TeamCtx, TeammateStatus};

/// Teammate-only: submit a plan for Lead approval. Records work_version +
/// task_id so a later claim/complete invalidates the approval.
pub fn submit_plan(team: &TeamCtx, owner: &str, plan: &str) -> String {
    let task_id = team.assignments.get(owner).map(|a| a.task_id.clone());
    let work_version = team.assignments.version(owner);

    // Fast path: if a plan is already pending, refuse before allocating an id.
    if team.protocols.gate(owner) == GateStatus::Pending {
        return "A plan is already waiting for review.".into();
    }
    // Allocate the request id WITHOUT holding the pending lock — new_request_id
    // locks pending internally, so taking it under our own pending lock would
    // deadlock (std Mutex is not reentrant).
    let request_id = team.protocols.new_request_id();
    {
        let mut pending = team.protocols.pending.lock().unwrap();
        // Re-check under the lock in case of a concurrent submit.
        if team.protocols.gate(owner) == GateStatus::Pending {
            return "A plan is already waiting for review.".into();
        }
        pending.insert(
            request_id.clone(),
            ProtocolState {
                request_id: request_id.clone(),
                ptype: ProtocolType::PlanApproval,
                sender: owner.into(),
                target: "lead".into(),
                status: ProtocolStatus::Pending,
                payload: plan.into(),
                work_version: Some(work_version),
                task_id,
            },
        );
    }
    team.protocols
        .plan_request_ids
        .lock()
        .unwrap()
        .insert(owner.into(), request_id.clone());
    team.bus.send(
        owner,
        "lead",
        plan,
        "plan_approval_request",
        Some(json!({ "request_id": request_id })),
    );
    team.protocols.set_gate(owner, GateStatus::Pending);
    team.active
        .lock()
        .unwrap()
        .insert(owner.into(), TeammateStatus::WaitingApproval);
    // Wake the Lead so it sees the plan_approval_request.
    team.lead_notify.notify_one();
    "Plan submitted. Wait for Lead's decision.".into()
}

/// Teammate tool: submit a plan for Lead approval before mutating files/bash.
pub struct SubmitPlanTool;

#[async_trait]
impl Tool for SubmitPlanTool {
    fn name(&self) -> &str {
        "submit_plan"
    }
    fn description(&self) -> &str {
        "Submit a work plan for Lead approval before changing files or running bash."
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{"plan":{"type":"string"}},"required":["plan"]})
    }
    fn check_permission(&self, _: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }
    fn available_for(&self, kind: AgentKind) -> bool {
        kind == AgentKind::Teammate
    }
    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String {
        let plan = input.get("plan").and_then(|v| v.as_str()).unwrap_or("");
        let Some(team) = &ctx.agent.team else {
            return "Error: not in team context".into();
        };
        submit_plan(team, ctx.owner(), plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_system::store::create_test_store;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn ctx() -> (tempfile::TempDir, Arc<TeamCtx>) {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(create_test_store(tmp.path()));
        let team = Arc::new(TeamCtx::new(tmp.path().to_path_buf(), store).unwrap());
        (tmp, team)
    }

    #[test]
    fn submit_plan_sets_pending_gate() {
        let (_tmp, team) = ctx();
        let r = submit_plan(&team, "alice", "step 1; step 2");
        assert!(r.contains("submitted"));
        assert_eq!(team.protocols.gate("alice"), GateStatus::Pending);
        assert!(team.bus.peek("lead"));
    }

    #[test]
    fn submit_plan_rejects_duplicate() {
        let (_tmp, team) = ctx();
        submit_plan(&team, "alice", "plan a");
        let r = submit_plan(&team, "alice", "plan b");
        assert!(r.contains("already waiting"));
    }
}
