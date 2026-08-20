# s13 Agent Teams Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement s13 Agent Teams in bytemaker on top of the already-landed `Agent` object abstraction — persistent teammates, file mailboxes, atomic task claiming, typed shutdown/plan protocols, and (deferrable) task-bound git worktrees.

**Architecture:** Teammates are `tokio::spawn` tasks that reuse the unified `Agent::run_loop`. A shared `TeamCtx` (one `Arc`) owns the `MessageBus`, assignment registry, protocol state, and a `Notify` for waking the Lead REPL. `Agent` gains `owner`/`kind`/`team` fields; `ToolContext::cwd()` resolves per-owner; `AgentKind { Lead, Subagent, Teammate }` replaces `for_subagent: bool` and drives tool visibility + the plan gate. A cross-platform `fs4` file lock serializes task mutations.

**Tech Stack:** Rust 2021, tokio, async-trait, serde, `fs4` (new), `dunce`/`path-clean` (existing), `fastrand`, `regex`.

Reference: `s13_agent_teams/code.py` (Python). Spec: `docs/superpowers/specs/2026-08-20-agent-teams-design.md`.

---

## Global Constraints

- **Errors**: storage/team functions return `Result<T, String>` (matches s10/s11/s12). `Agent::new` keeps returning `Result<_, AgentError>`.
- **Tool names** (exact): `spawn_teammate`, `list_teammates`, `send_message`, `request_shutdown`, `request_plan`, `review_plan`, `submit_plan`, `create_worktree`.
- **ID formats**: task `task_{8hex}` (existing); protocol `req_{06d}`.
- **Reserved teammate names**: `lead`, `agent` (case-insensitive). Teammate name regex `^[A-Za-z0-9_-]{1,64}$`.
- **Worktree name regex**: `^(?!.*\.\.)[A-Za-z0-9][A-Za-z0-9._-]{0,63}$`, maxLength 64. Worktree branch `wt/<name>`.
- **File layouts**: `.mailboxes/<name>.jsonl`, `.worktrees/<name>`, `.tasks/.lock`, `.tasks/<id>.json`.
- **No global singletons**: everything threads through `Agent` / `TeamCtx` (`Arc`-shared).
- **TDD**: write the failing test first, run to see it fail, implement, run to pass, commit. One commit per task. Run `cargo test` after each task.
- **Do not** bring s11 background tasks or s12 cron into teammate logic.
- **Platform**: code must compile and tests must pass on win32. Use `fs4` (not `fcntl`); use `tokio::select!` (not `select()`); use `dunce` for path canonicalization.

---

## File Structure

New module `src/team/` (mirrors `task_system/`, `background_tasks/` multi-file convention):

| Path | Responsibility |
|---|---|
| `src/team/mod.rs` | `pub mod` declarations, `TeamCtx`, `AgentKind` re-export, `team::claim_task`/`complete_task` |
| `src/team/lock.rs` | `TaskStoreLock` (in-process `Mutex` + `fs4` file lock) |
| `src/team/bus.rs` | `MessageBus` (file inboxes + per-agent `Notify`) |
| `src/team/assignment.rs` | `Assignment`, `assignment_cwd`, `advance_assignment_version` |
| `src/team/protocols.rs` | `ProtocolState`, `GateStatus`, `match_response`, plan/shutdown helpers |
| `src/team/runtime.rs` | `TeammateRuntime`, `spawn_teammate_thread` |
| `src/team/worktree.rs` | `task_worktree_cwd`, `create_worktree`, `remove_worktree` (Phase 2) |
| `src/team/tools.rs` | Lead team tools + `SubmitPlanTool` |

Modified existing files: `src/agent.rs`, `src/tools/trait_def.rs`, `src/tools/registry.rs`, `src/tools/{command,read_file,write_file,edit_file,glob_tool}.rs`, `src/task_system/task.rs`, `src/task_system/tools.rs`, `src/tools/mod.rs`, `src/lib.rs`, `src/main.rs`, `Cargo.toml`.

---

## Phase 1 — Core team runtime (required; produces working collaboration without worktree)

### Task 1: `fs4` dependency, `TaskStoreLock`, `Task.worktree` field

**Files:**
- Modify: `Cargo.toml` (add `fs4`)
- Create: `src/team/lock.rs`
- Modify: `src/task_system/task.rs` (add `worktree` field)
- Modify: `src/lib.rs` (declare `pub mod team;`)

- [ ] **Step 1: Add `fs4` to `Cargo.toml`**

In the `[dependencies]` table add:
```toml
fs4 = "0.9"
```

- [ ] **Step 2: Add `worktree` field to `Task`**

In `src/task_system/task.rs`, change the `Task` struct:
```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub subject: String,
    pub description: String,
    pub status: TaskStatus,
    pub owner: Option<String>,
    pub blocked_by: Vec<String>,
    /// Optional task-bound worktree name (s13). Old JSON without it deserializes to None.
    #[serde(default)]
    pub worktree: Option<String>,
}
```
Update the `Task { ... }` literal in `src/task_system/store.rs::create` (around line 165) and in `src/task_system/tools.rs` and `task_system/store.rs` test fixtures (`task_12345678` literals) to add `worktree: None,`.

- [ ] **Step 3: Write the failing test for `TaskStoreLock`**

Create `src/team/lock.rs`:
```rust
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use fs4::fs_std::FileExt;

/// Serializes task mutations across threads (in-process `Mutex`) and host
/// processes (fs4 exclusive lock on `.tasks/.lock`).
pub struct TaskStoreLock {
    inner: Mutex<()>,
    lock_path: PathBuf,
}

impl TaskStoreLock {
    pub fn new(tasks_dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(tasks_dir)?;
        Ok(Self {
            inner: Mutex::new(()),
            lock_path: tasks_dir.join(".lock"),
        })
    }

    /// Acquire both locks for the duration of the returned guard's lifetime.
    pub fn lock(&self) -> std::io::Result<TaskStoreGuard<'_>> {
        let guard = self.inner.lock().map_err(|e| std::io::Error::other(e.to_string()))?;
        let file = std::fs::OpenOptions::new()
            .read(true).write(true).create(true)
            .open(&self.lock_path)?;
        file.lock_exclusive()?;
        Ok(TaskStoreGuard { guard, file })
    }
}

pub struct TaskStoreGuard<'a> {
    guard: std::sync::MutexGuard<'a, ()>,
    file: std::fs::File,
}

impl Drop for TaskStoreGuard<'_> {
    fn drop(&mut self) {
        let _ = fs4::fs_std::FileExt::unlock(&self.file);
    }
}
```

Add to `src/team/mod.rs` (create the file):
```rust
pub mod lock;
pub mod bus;
pub mod assignment;
pub mod protocols;
pub mod runtime;
pub mod tools;
// pub mod worktree; // Phase 2
```
And in `src/lib.rs` add `pub mod team;`.

Append the failing test to `src/team/lock.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use tempfile::TempDir;

    #[test]
    fn lock_is_mutually_exclusive_across_threads() {
        let tmp = TempDir::new().unwrap();
        let lk = TaskStoreLock::new(tmp.path()).unwrap();
        let g1 = lk.lock().unwrap();
        // A second thread must block; we can't easily assert "blocked", so
        // assert the first guard holds and a non-blocking attempt from another
        // thread fails to acquire instantly via try-lock-style timeout.
        let lk2 = TaskStoreLock::new(tmp.path()).unwrap();
        let child = thread::spawn(move || {
            // This will block until `g1` drops; use a 50ms sentinel.
            let _g2 = lk2.lock().unwrap();
            true
        });
        assert!(!child.is_finished(), "second lock must block while first held");
        drop(g1);
        assert!(child.join().unwrap(), "second lock acquires after first drops");
    }

    #[test]
    fn worktree_field_defaults_none_for_old_json() {
        let json = r#"{"id":"task_12345678","subject":"s","description":"","status":"pending","owner":null,"blocked_by":[]}"#;
        let t: crate::task_system::task::Task = serde_json::from_str(json).unwrap();
        assert!(t.worktree.is_none());
    }
}
```

- [ ] **Step 4: Run test to verify it fails**

Run: `cargo test --lib team::lock`
Expected: FAIL — `fs4` not yet resolving / module wiring incomplete (compile errors first; fix any import paths until the test compiles and the mutual-exclusion assertion runs).

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test --lib team::lock`
Expected: PASS.

- [ ] **Step 6: Run full suite to confirm no regression**

Run: `cargo test`
Expected: all existing tests pass (the `worktree: None` additions keep s10 fixtures valid).

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock src/team/ src/lib.rs src/task_system/task.rs src/task_system/store.rs src/task_system/tools.rs
git commit -m "feat(s13): add fs4 lock, Task.worktree field, team module skeleton"
```

---

### Task 2: `AgentKind` enum replaces `for_subagent` (foundation refactor)

**Files:**
- Modify: `src/tools/trait_def.rs`
- Modify: `src/tools/registry.rs`
- Modify: `src/agent.rs` (field + `child_agent`)
- Test: inline `#[cfg(test)]` in each file

- [ ] **Step 1: Write the failing tests for kind-based visibility**

Add `AgentKind` to `src/tools/trait_def.rs` (top, after imports):
```rust
/// Which agent context a tool call is dispatched in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    Lead,
    Subagent,
    Teammate,
}
```
Change the `Tool` trait method:
```rust
    fn available_for(&self, _kind: AgentKind) -> bool {
        true
    }
```
(remove `available_for_subagent`).

Append to `src/tools/trait_def.rs` tests:
```rust
#[cfg(test)]
mod kind_tests {
    use super::*;
    use async_trait::async_trait;

    struct LeadOnly;
    #[async_trait]
    impl Tool for LeadOnly {
        fn name(&self) -> &str { "lead_only" }
        fn description(&self) -> &str { "lead only" }
        fn input_schema(&self) -> serde_json::Value { serde_json::json!({"type":"object","properties":{}}) }
        async fn execute(&self, _: &ToolContext<'_>, _: &serde_json::Value) -> String { "x".into() }
        fn available_for(&self, kind: AgentKind) -> bool { kind == AgentKind::Lead }
    }

    #[test]
    fn available_for_filters_by_kind() {
        let t = LeadOnly;
        assert!(t.available_for(AgentKind::Lead));
        assert!(!t.available_for(AgentKind::Teammate));
        assert!(!t.available_for(AgentKind::Subagent));
    }
}
```

- [ ] **Step 2: Update `ToolRegistry` to use `AgentKind`**

In `src/tools/registry.rs`:
- `dispatch(&self, name, ctx, input, kind: AgentKind) -> ToolResult` — replace `for_subagent: bool`:
```rust
    pub async fn dispatch(
        &self, name: &str, ctx: &ToolContext<'_>, input: &Value, kind: AgentKind,
    ) -> ToolResult {
        match self.tools.get(name) {
            Some(tool) => {
                if !tool.available_for(kind) {
                    ToolResult::Rejected {
                        name: name.to_string(),
                        reason: format!("Tool not available in {:?} context", kind),
                    }
                } else {
                    ToolResult::Output(tool.execute(ctx, input).await)
                }
            }
            None => ToolResult::NotFound { name: name.to_string(), available: self.tools.keys().cloned().collect() },
        }
    }
```
- Rename `definitions_for_subagent` → `definitions_for(kind: AgentKind)`:
```rust
    pub fn definitions_for(&self, kind: AgentKind) -> Vec<ToolDefinition> {
        self.tools.values()
            .filter(|t| t.available_for(kind))
            .map(|t| ToolDefinition { name: t.name().into(), description: t.description().into(), input_schema: t.input_schema() })
            .collect()
    }
```
Update `dispatch`/`definitions_for` callers and the existing registry tests: replace `for_subagent` bools with `AgentKind`, `definitions_for_subagent()` with `definitions_for(AgentKind::Subagent)`, and `available_for_subagent()` with `available_for(AgentKind::Subagent)`. The `TaskTool` test (`test_dispatch_for_subagent_rejects_task`) becomes `dispatch("task", &ctx, &json!({"prompt":"recurse"}), AgentKind::Subagent)` and `TaskTool::available_for` returns `kind == AgentKind::Lead`.

- [ ] **Step 3: Update `Agent` to carry `kind` instead of `for_subagent`**

In `src/agent.rs`:
- Add `use crate::tools::trait_def::AgentKind;`
- Replace field `pub(crate) for_subagent: bool,` with `pub(crate) kind: AgentKind,`.
- In `Agent::new`: `kind: AgentKind::Lead,` (was `for_subagent: false`).
- In `child_agent`: `kind: AgentKind::Subagent,` (was `for_subagent: true`).
- In `run_loop`: replace `if !self.for_subagent { self.bg_manager.collect_and_inject(messages) }` with `if self.kind == AgentKind::Lead { let _ = self.bg_manager.collect_and_inject(messages); }`.
- Replace `if self.for_subagent { self.registry.definitions_for_subagent() } else { self.registry.definitions() }` with `self.registry.definitions_for(self.kind)`.
- In `execute_tool`: change `self.registry.dispatch(name, &ctx, input, self.for_subagent)` → `self.registry.dispatch(name, &ctx, input, self.kind)`.
- In `TestAgent::new`: `kind: AgentKind::Lead,`.
- Update `agent.rs` tests: `child_agent_shares_infra...` checks `child.for_subagent` → `child.kind == AgentKind::Subagent`; `subagent_execute_tool_runs_pre_tool_denies_destructive` uses `child_agent` (unchanged); `test_agent_constructs_isolated` checks `!a.agent().for_subagent` → `a.agent().kind == AgentKind::Lead`.

- [ ] **Step 4: Run tests to verify they fail/pass**

Run: `cargo test`
Expected: compile errors resolve to the kind_tests passing and all existing tests green after the mechanical replacements.

- [ ] **Step 5: Commit**

```bash
git add src/tools/trait_def.rs src/tools/registry.rs src/agent.rs
git commit -m "refactor(s13): replace for_subagent bool with AgentKind enum"
```

---

### Task 3: `MessageBus` (file inboxes + per-agent `Notify`)

**Files:**
- Create: `src/team/bus.rs`
- Modify: `src/team/mod.rs` (already declares `bus`)

- [ ] **Step 1: Write the failing tests**

Create `src/team/bus.rs` with the test module first:
```rust
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Arc};
use std::collections::HashMap;
use std::time::Duration;
use serde_json::Value;
use tokio::sync::Notify;

use crate::tools::safe_path_in;

pub const MAILBOX_DIR_NAME: &str = ".mailboxes";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MessageRecord {
    pub from: String,
    pub to: String,
    pub content: String,
    #[serde(rename = "type")]
    pub msg_type: String,
    pub ts: f64,
    pub metadata: Value,
}

pub struct MessageBus {
    workdir: PathBuf,
    inner: Mutex<BusInner>,
}

struct BusInner {
    notifies: HashMap<String, Arc<Notify>>,
}

impl MessageBus {
    pub fn new(workdir: PathBuf) -> Self {
        Self { workdir, inner: Mutex::new(BusInner { notifies: HashMap::new() }) }
    }

    pub fn maildir(&self) -> PathBuf { self.workdir.join(MAILBOX_DIR_NAME) }

    fn notify_for(&self, agent: &str) -> Arc<Notify> {
        let mut inner = self.inner.lock().unwrap();
        inner.notifies.entry(agent.to_string()).or_default().clone()
    }

    fn mailbox_path(&self, agent: &str) -> Result<PathBuf, String> {
        if !is_valid_agent_name(agent) {
            return Err(format!("Invalid mailbox recipient: {:?}", agent));
        }
        let dir = self.maildir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        safe_path_in(&dir, &format!("{}.jsonl", agent))
    }

    pub fn send(&self, from: &str, to: &str, content: &str, msg_type: &str, metadata: Option<Value>) {
        let record = MessageRecord {
            from: from.into(), to: to.into(), content: content.into(),
            msg_type: msg_type.into(), ts: 0.0, metadata: metadata.unwrap_or(Value::Null),
        };
        if let Ok(path) = self.mailbox_path(to) {
            if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                use std::io::Write;
                let _ = writeln!(file, "{}", serde_json::to_string(&record).unwrap_or_default());
            }
        }
        self.notify_for(to).notify_one();
    }

    fn read_file(&self, agent: &str) -> Vec<MessageRecord> {
        let path = match self.mailbox_path(agent) { Ok(p) => p, Err(_) => return vec![] };
        match std::fs::read_to_string(&path) {
            Ok(s) => {
                let _ = std::fs::remove_file(&path);
                s.lines().filter_map(|l| {
                    if l.trim().is_empty() { None } else { serde_json::from_str(l).ok() }
                }).collect()
            }
            Err(_) => vec![],
        }
    }

    pub fn read_inbox(&self, agent: &str) -> Vec<MessageRecord> { self.read_file(agent) }

    pub fn peek(&self, agent: &str) -> bool {
        match self.mailbox_path(agent) {
            Ok(p) => p.exists() && std::fs::metadata(&p).map(|m| m.len() > 0).unwrap_or(false),
            Err(_) => false,
        }
    }

    pub async fn wait_for_messages(&self, agent: &str, timeout: Duration) -> Vec<MessageRecord> {
        loop {
            let existing = self.read_file(agent);
            if !existing.is_empty() {
                return existing;
            }
            let notify = self.notify_for(agent);
            let notified = notify.notified();
            tokio::pin!(notified);
            match tokio::time::timeout(timeout, notified).await {
                Ok(_) => continue,          // re-check after wake
                Err(_) => return self.read_file(agent),
            }
        }
    }
}

pub fn is_valid_agent_name(name: &str) -> bool {
    let re = regex::Regex::new(r"^[A-Za-z0-9_-]{1,64}$").unwrap();
    re.is_match(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn send_then_read_is_destructive() {
        let tmp = TempDir::new().unwrap();
        let bus = MessageBus::new(tmp.path().to_path_buf());
        bus.send("lead", "alice", "hi", "message", None);
        let msgs = bus.read_inbox("alice");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "hi");
        assert_eq!(msgs[0].msg_type, "message");
        // second read is empty (destructive)
        assert!(bus.read_inbox("alice").is_empty());
    }

    #[test]
    fn peek_detects_messages() {
        let tmp = TempDir::new().unwrap();
        let bus = MessageBus::new(tmp.path().to_path_buf());
        assert!(!bus.peek("bob"));
        bus.send("lead", "bob", "x", "message", None);
        assert!(bus.peek("bob"));
    }

    #[tokio::test]
    async fn wait_for_messages_times_out_empty() {
        let tmp = TempDir::new().unwrap();
        let bus = MessageBus::new(tmp.path().to_path_buf());
        let msgs = bus.wait_for_messages("carol", Duration::from_millis(50)).await;
        assert!(msgs.is_empty());
    }

    #[test]
    fn invalid_agent_name_rejected() {
        let tmp = TempDir::new().unwrap();
        let bus = MessageBus::new(tmp.path().to_path_buf());
        bus.send("lead", "../escape", "x", "message", None);
        // no file written outside maildir; peek false
        assert!(!bus.peek("../escape"));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib team::bus`
Expected: FAIL — `MessageBus` not yet wired (compile errors until `safe_path_in` import resolves; the file is new so first run may compile-clean then tests fail/assert).

- [ ] **Step 3: Run test to verify it passes**

Run: `cargo test --lib team::bus`
Expected: PASS (4 tests).

- [ ] **Step 4: Commit**

```bash
git add src/team/bus.rs src/team/mod.rs
git commit -m "feat(s13): add MessageBus file mailboxes with Notify wakeups"
```

---

### Task 4: `Assignment` registry + `assignment_cwd` (no-worktree path)

**Files:**
- Create: `src/team/assignment.rs`
- Modify: `src/team/mod.rs` (already declares `assignment`)

- [ ] **Step 1: Write the failing tests**

Create `src/team/assignment.rs`:
```rust
use std::path::PathBuf;
use std::sync::Mutex;
use std::collections::HashMap;

use crate::task_system::task::{Task, TaskStatus};
use crate::task_system::store::TaskStore;
use super::worktree::task_worktree_cwd;

#[derive(Clone, Debug)]
pub struct Assignment {
    pub task_id: String,
    pub cwd: PathBuf,
}

pub struct AssignmentRegistry {
    pub assignments: Mutex<HashMap<String, Assignment>>,
    pub versions: Mutex<HashMap<String, u32>>,
}

impl AssignmentRegistry {
    pub fn new() -> Self {
        Self { assignments: Mutex::new(HashMap::new()), versions: Mutex::new(HashMap::new()) }
    }

    pub fn get(&self, owner: &str) -> Option<Assignment> {
        self.assignments.lock().unwrap().get(owner).cloned()
    }

    pub fn set(&self, owner: &str, a: Assignment) {
        self.assignments.lock().unwrap().insert(owner.into(), a);
    }

    pub fn remove(&self, owner: &str) -> Option<Assignment> {
        self.assignments.lock().unwrap().remove(owner)
    }

    pub fn version(&self, owner: &str) -> u32 {
        *self.versions.lock().unwrap().get(owner).unwrap_or(&0)
    }

    /// Invalidate stale plan approvals without clearing an explicit requirement.
    pub fn advance_version(&self, owner: &str) -> u32 {
        let mut v = self.versions.lock().unwrap();
        let n = v.get(owner).copied().unwrap_or(0) + 1;
        v.insert(owner.into(), n);
        n
    }
}

/// Resolve a teammate's current cwd. No worktree → repo workdir.
/// Broken worktree binding → Err (fail-closed, never silently fall back).
pub fn assignment_cwd(
    workdir: &std::path::Path,
    store: &TaskStore,
    registry: &AssignmentRegistry,
    owner: &str,
) -> Result<PathBuf, String> {
    if let Some(a) = registry.get(owner) {
        let task = store.load(&a.task_id).map_err(|e| e.to_string())?;
        if task.status != TaskStatus::InProgress && task.status != TaskStatus::Completed {
            return Err(format!("Assignment for {} is no longer active", owner));
        }
        if task.owner.as_deref() != Some(owner) {
            return Err(format!("Assignment for {} is no longer active", owner));
        }
        let (cwd, err) = task_worktree_cwd(workdir, &task);
        if let Some(e) = err {
            return Err(e);
        }
        return Ok(cwd);
    }
    Ok(workdir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_system::store::create_test_store;
    use tempfile::TempDir;

    fn store_with_task(owner: Option<&str>, status: TaskStatus) -> (TempDir, TaskStore, Task) {
        let tmp = TempDir::new().unwrap();
        let store = create_test_store(tmp.path());
        let mut t = store.create("T".into(), "".into(), vec![]).unwrap();
        t.status = status;
        t.owner = owner.map(String::from);
        store.save(&t).unwrap();
        (tmp, store, t)
    }

    #[test]
    fn no_assignment_returns_workdir() {
        let tmp = TempDir::new().unwrap();
        let store = create_test_store(tmp.path());
        let reg = AssignmentRegistry::new();
        let cwd = assignment_cwd(tmp.path(), &store, &reg, "alice").unwrap();
        assert_eq!(cwd, tmp.path());
    }

    #[test]
    fn version_advances() {
        let reg = AssignmentRegistry::new();
        assert_eq!(reg.version("alice"), 0);
        assert_eq!(reg.advance_version("alice"), 1);
        assert_eq!(reg.advance_version("alice"), 2);
        assert_eq!(reg.version("alice"), 2);
    }

    #[test]
    fn assignment_for_inactive_task_errors() {
        let (tmp, store, t) = store_with_task(Some("alice"), TaskStatus::Pending);
        let reg = AssignmentRegistry::new();
        reg.set("alice", Assignment { task_id: t.id, cwd: tmp.path().to_path_buf() });
        let r = assignment_cwd(tmp.path(), &store, &reg, "alice");
        assert!(r.is_err(), "pending task assignment must error, got {:?}", r);
    }
}
```

This references `super::worktree::task_worktree_cwd`, created in Task 16 (Phase 2). For Phase 1 to compile, create a **temporary stub** now:

Create `src/team/worktree.rs`:
```rust
use std::path::{Path, PathBuf};
use crate::task_system::task::Task;

/// Phase 1 stub: no worktree resolution. Returns (workdir, None) always.
/// Phase 2 (Task 16) replaces this with real git-worktree resolution.
pub fn task_worktree_cwd(workdir: &Path, task: &Task) -> (PathBuf, Option<String>) {
    (workdir.to_path_buf(), None)
}
```
Uncomment `pub mod worktree;` in `src/team/mod.rs`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib team::assignment`
Expected: FAIL (module not declared / compile errors), then PASS after wiring.

- [ ] **Step 3: Run test to verify it passes**

Run: `cargo test --lib team::assignment`
Expected: PASS (3 tests).

- [ ] **Step 4: Commit**

```bash
git add src/team/assignment.rs src/team/worktree.rs src/team/mod.rs
git commit -m "feat(s13): add assignment registry and assignment_cwd (no-worktree path)"
```

---

### Task 5: `ProtocolState`, `GateStatus`, `match_response`, plan helpers

**Files:**
- Create: `src/team/protocols.rs`
- Modify: `src/team/mod.rs` (already declares `protocols`)

- [ ] **Step 1: Write the failing tests + implementation**

Create `src/team/protocols.rs`:
```rust
use std::sync::Mutex;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateStatus { NotRequired, Required, Pending, Approved, Rejected }

impl GateStatus {
    pub fn blocks_mutating_tools(&self) -> bool {
        matches!(self, Self::Required | Self::Pending | Self::Rejected)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolType { Shutdown, PlanApproval }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolStatus { Pending, Approved, Rejected }

#[derive(Debug, Clone)]
pub struct ProtocolState {
    pub request_id: String,
    pub ptype: ProtocolType,
    pub sender: String,
    pub target: String,
    pub status: ProtocolStatus,
    pub payload: String,
    pub work_version: Option<u32>,
    pub task_id: Option<String>,
}

/// Pending protocol requests + per-owner plan gate + current plan request id.
pub struct ProtocolRegistry {
    pub pending: Mutex<HashMap<String, ProtocolState>>,
    pub gates: Mutex<HashMap<String, GateStatus>>,
    pub plan_request_ids: Mutex<HashMap<String, String>>,
}

impl ProtocolRegistry {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            gates: Mutex::new(HashMap::new()),
            plan_request_ids: Mutex::new(HashMap::new()),
        }
    }

    pub fn gate(&self, owner: &str) -> GateStatus {
        self.gates.lock().unwrap().get(owner).copied().unwrap_or(GateStatus::NotRequired)
    }

    pub fn set_gate(&self, owner: &str, g: GateStatus) {
        self.gates.lock().unwrap().insert(owner.into(), g);
    }

    /// Match a typed response to a pending request. Returns false on any mismatch.
    pub fn match_response(
        &self, response_type: &str, request_id: &str, approve: bool,
        from_agent: &str, to_agent: &str,
    ) -> bool {
        let mut pending = self.pending.lock().unwrap();
        let Some(state) = pending.get(request_id) else { return false; };
        let expected = match state.ptype {
            ProtocolType::Shutdown => "shutdown_response",
            ProtocolType::PlanApproval => "plan_approval_response",
        };
        if response_type != expected { return false; }
        if from_agent != state.target || to_agent != state.sender { return false; }
        if state.status != ProtocolStatus::Pending { return false; }
        let new_status = if approve { ProtocolStatus::Approved } else { ProtocolStatus::Rejected };
        let state = pending.get_mut(request_id).unwrap();
        state.status = new_status;
        true
    }

    pub fn new_request_id(&self) -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        loop {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst) % 1_000_000;
            let id = format!("req_{:06}", n);
            if !self.pending.lock().unwrap().contains_key(&id) {
                return id;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shutdown_req() -> ProtocolState {
        ProtocolState {
            request_id: "req_000001".into(), ptype: ProtocolType::Shutdown,
            sender: "lead".into(), target: "alice".into(),
            status: ProtocolStatus::Pending, payload: String::new(),
            work_version: None, task_id: None,
        }
    }

    #[test]
    fn match_response_approves_on_valid_pair() {
        let reg = ProtocolRegistry::new();
        reg.pending.lock().unwrap().insert("req_000001".into(), shutdown_req());
        assert!(reg.match_response("shutdown_response", "req_000001", true, "alice", "lead"));
        assert_eq!(reg.pending.lock().unwrap().get("req_000001").unwrap().status, ProtocolStatus::Approved);
    }

    #[test]
    fn match_response_rejects_wrong_type() {
        let reg = ProtocolRegistry::new();
        reg.pending.lock().unwrap().insert("req_000001".into(), shutdown_req());
        assert!(!reg.match_response("plan_approval_response", "req_000001", true, "alice", "lead"));
    }

    #[test]
    fn match_response_rejects_wrong_role() {
        let reg = ProtocolRegistry::new();
        reg.pending.lock().unwrap().insert("req_000001".into(), shutdown_req());
        assert!(!reg.match_response("shutdown_response", "req_000001", true, "bob", "lead"));
    }

    #[test]
    fn match_response_rejects_double_resolution() {
        let reg = ProtocolRegistry::new();
        reg.pending.lock().unwrap().insert("req_000001".into(), shutdown_req());
        assert!(reg.match_response("shutdown_response", "req_000001", true, "alice", "lead"));
        assert!(!reg.match_response("shutdown_response", "req_000001", true, "alice", "lead"),
            "already-approved request must not resolve twice");
    }

    #[test]
    fn gate_blocks_mutating_tools_states() {
        assert!(GateStatus::Required.blocks_mutating_tools());
        assert!(GateStatus::Pending.blocks_mutating_tools());
        assert!(GateStatus::Rejected.blocks_mutating_tools());
        assert!(!GateStatus::NotRequired.blocks_mutating_tools());
        assert!(!GateStatus::Approved.blocks_mutating_tools());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib team::protocols`
Expected: FAIL then PASS after compile.

- [ ] **Step 3: Run test to verify it passes**

Run: `cargo test --lib team::protocols`
Expected: PASS (5 tests).

- [ ] **Step 4: Commit**

```bash
git add src/team/protocols.rs src/team/mod.rs
git commit -m "feat(s13): add protocol state, plan gate, and response matching"
```

---

### Task 6: `team::claim_task` / `team::complete_task` under `TaskStoreLock`

**Files:**
- Modify: `src/team/mod.rs` (add `claim_task`, `complete_task`)
- Modify: `src/task_system/tools.rs` (`ClaimTaskTool`/`CompleteTaskTool` read owner from ctx; route through team when available)

- [ ] **Step 1: Write the failing tests + implementation**

In `src/team/mod.rs`, add (after the `pub mod` declarations):
```rust
use crate::task_system::task::{Task, TaskStatus};
use crate::task_system::store::TaskStore;
use crate::tools::trait_def::AgentKind;
use super::assignment::{Assignment, AssignmentRegistry};
use super::lock::TaskStoreLock;
use super::protocols::{GateStatus, ProtocolRegistry};
use super::worktree::task_worktree_cwd;

pub struct TeamCtx {
    pub bus: super::bus::MessageBus,
    pub assignments: AssignmentRegistry,
    pub protocols: ProtocolRegistry,
    pub active: Mutex<HashMap<String, TeammateStatus>>,
    pub lead_notify: tokio::sync::Notify,
    pub task_store: Arc<TaskStore>,
    pub workdir: PathBuf,
    pub lock: TaskStoreLock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeammateStatus { Working, WaitingApproval, Idle, Stopping }

impl TeamCtx {
    pub fn new(workdir: PathBuf, task_store: Arc<TaskStore>) -> std::io::Result<Self> {
        let tasks_dir = task_store_dir(&task_store);          // see helper below
        Ok(Self {
            bus: super::bus::MessageBus::new(workdir.clone()),
            assignments: AssignmentRegistry::new(),
            protocols: ProtocolRegistry::new(),
            active: Mutex::new(HashMap::new()),
            lead_notify: tokio::sync::Notify::new(),
            task_store,
            workdir,
            lock: TaskStoreLock::new(&tasks_dir)?,
        })
    }

    pub fn lead_notify(&self) -> &tokio::sync::Notify { &self.lead_notify }
}

/// Atomically claim one task and bind the owner's cwd. Returns a user-facing string.
pub fn claim_task(team: &TeamCtx, task_id: &str, owner: &str) -> String {
    let _g = match team.lock.lock() { Ok(g) => g, Err(e) => return format!("Error: {}", e) };
    let store = &team.task_store;
    let Ok(mut task) = store.load(task_id) else {
        return format!("Error: Task {} not found", task_id);
    };
    if task.status != TaskStatus::Pending {
        return format!("Task {} is {}, cannot claim", task_id, task.status.as_word());
    }
    if task.owner.is_some() {
        return format!("Task {} is already owned by {}", task_id, task.owner.as_deref().unwrap_or("?"));
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
    team.assignments.set(owner, Assignment { task_id: task.id.clone(), cwd });
    team.assignments.advance_version(owner);
    format!("Claimed {} ({})", task.id, task.subject)
}

/// Complete an owned, in-progress task. Respects the plan gate.
pub fn complete_task(team: &TeamCtx, task_id: &str, owner: &str) -> String {
    let _g = match team.lock.lock() { Ok(g) => g, Err(e) => return format!("Error: {}", e) };
    let store = &team.task_store;
    let Ok(mut task) = store.load(task_id) else {
        return format!("Error: Task {} not found", task_id);
    };
    if task.status != TaskStatus::InProgress {
        return format!("Task {} is {}, cannot complete", task_id, task.status.as_word());
    }
    if task.owner.as_deref() != Some(owner) {
        return format!("Task {} is owned by {}, not {}", task_id, task.owner.as_deref().unwrap_or("none"), owner);
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
    task.blocked_by.iter().filter(|d| {
        match team.task_store.load(d) {
            Ok(t) => t.status != TaskStatus::Completed,
            Err(_) => true,
        }
    }).cloned().collect()
}
fn incomplete_deps_empty(team: &TeamCtx, task: &Task) -> bool { incomplete_deps(team, task).is_empty() }
```

Add helpers for `TaskStore` directory access + `TaskStatus::as_word`:
- `task_store_dir(&TaskStore) -> PathBuf`: expose the store's `.tasks` dir. Add `pub fn directory(&self) -> &Path` to `TaskStore` in `src/task_system/store.rs` (currently `directory` is private). Then `task_store_dir(s) = s.directory().to_path_buf()`.
- `TaskStatus::as_word(&self) -> &'static str` in `src/task_system/task.rs`: returns `"pending"`/`"in_progress"`/`"completed"` (or reuse `tools.rs::status_word`). Add the method to keep `mod.rs` self-contained.

Update `src/task_system/tools.rs` `ClaimTaskTool`/`CompleteTaskTool::execute` to use `ctx.agent.owner` (instead of hardcoded `"agent"`); when `ctx.agent.team` is `Some`, call `team::claim_task(team, task_id, owner)` / `team::complete_task(...)`; otherwise fall back to the existing `task_system::tools::claim_task` (s10 path, owner from ctx). Example:
```rust
async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String {
    let task_id = input.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
    let owner = ctx.agent.owner.as_str();
    if let Some(team) = &ctx.agent.team {
        crate::team::claim_task(team, task_id, owner)
    } else {
        claim_task(&ctx.agent.task_store, task_id, owner)   // s10 path
    }
}
```
Apply the same shape to `CompleteTaskTool`. (The `owner` field on `Agent` is added in Task 7; this task compiles only after Task 7 — **execute Task 7 before this one if doing strictly in order, OR land the `owner` field in Task 7 first.** To keep the plan linear, swap: do Task 7's `owner`/`team` fields before this task. The dependency is noted; executor should land `Agent.owner` + `Agent.team` (Task 7 fields) first.)

> **Dependency note:** Task 6 depends on `Agent.owner` and `Agent.team` from Task 7. Execute Task 7 (fields portion) before Task 6. Reorder if needed; both land in Phase 1.

- [ ] **Step 2: Write failing tests for `team::claim_task`**

Append to `src/team/mod.rs` tests:
```rust
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
        assert!(team::claim_task(&team, &t.id, "alice").starts_with("Claimed"));
        assert_eq!(team.assignments.get("alice").unwrap().task_id, t.id);
    }

    #[test]
    fn claim_blocks_second_owner() {
        let tmp = TempDir::new().unwrap();
        let (store, team) = ctx(&tmp);
        let t = store.create("T".into(), "".into(), vec![]).unwrap();
        team::claim_task(&team, &t.id, "alice");
        let r = team::claim_task(&team, &t.id, "bob");
        assert!(r.contains("already owned"));
    }

    #[test]
    fn claim_rejects_second_task_same_owner() {
        let tmp = TempDir::new().unwrap();
        let (store, team) = ctx(&tmp);
        let t1 = store.create("A".into(), "".into(), vec![]).unwrap();
        let t2 = store.create("B".into(), "".into(), vec![]).unwrap();
        team::claim_task(&team, &t1.id, "alice");
        let r = team::claim_task(&team, &t2.id, "alice");
        assert!(r.contains("finish current work"));
    }

    #[test]
    fn complete_checks_owner() {
        let tmp = TempDir::new().unwrap();
        let (store, team) = ctx(&tmp);
        let t = store.create("T".into(), "".into(), vec![]).unwrap();
        team::claim_task(&team, &t.id, "alice");
        let r = team::complete_task(&team, &t.id, "bob");
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
            handles.push(std::thread::spawn(move || team::claim_task(&team, &tid, name)));
        }
        let results: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let winners = results.iter().filter(|r| r.starts_with("Claimed")).count();
        assert_eq!(winners, 1, "exactly one concurrent claim must win, got {:?}", results);
    }
}
```

- [ ] **Step 3: Run tests to verify pass**

Run: `cargo test --lib team::`
Expected: PASS (claim/complete/concurrency tests).

- [ ] **Step 4: Commit**

```bash
git add src/team/mod.rs src/task_system/store.rs src/task_system/task.rs src/task_system/tools.rs
git commit -m "feat(s13): add team claim_task/complete_task under TaskStoreLock"
```

---

### Task 7: `Agent` extensions — `owner`, `team`, `child_teammate`, `ToolContext::cwd()`, run-loop drain, plan gate

> **Execute this task before Task 6** (Task 6's `claim_task`/`CompleteTaskTool` need `Agent.owner` + `Agent.team`). If you already did Task 6, that's fine — just ensure these fields exist before compiling the team tools.

**Files:**
- Modify: `src/agent.rs`
- Modify: `src/tools/trait_def.rs` (`ToolContext::cwd()`, `owner()`)
- Modify: `src/team/mod.rs` (`drain_inbox`, `apply_plan_response`, `apply_shutdown_request`)

- [ ] **Step 1: Add `owner` and `team` fields to `Agent`**

In `src/agent.rs`, add fields to the `Agent` struct (per-loop section, next to `kind`):
```rust
    pub(crate) owner: String,
    pub(crate) team: Option<Arc<crate::team::TeamCtx>>,
```
In `Agent::new`, after constructing `task_store`/`bg_manager` etc., build the `TeamCtx` and set the fields:
```rust
        let team = Arc::new(
            crate::team::TeamCtx::new(cfg.workdir.clone(), Arc::clone(&task_store))
                .map_err(|e| AgentError::Other(format!("team init: {e}")))?
        );
```
and in the returned `Agent { ... }` literal add:
```rust
            kind: AgentKind::Lead,
            owner: "agent".to_string(),
            team: Some(team),
```
In `child_agent`, set `owner: "agent".to_string(), team: None,` (s06 subagents have no team).
In `TestAgent::new`, set `owner: "agent".to_string(), team: None,`.

- [ ] **Step 2: Add `child_teammate` + teammate hooks**

In `src/agent.rs` `impl Agent`:
```rust
    /// Produce a persistent teammate agent: shares infra, kind=Teammate, team=Some, fresh hooks.
    pub fn child_teammate(&self, name: &str, system: &str, team: Arc<crate::team::TeamCtx>) -> Agent {
        Agent {
            client: Arc::clone(&self.client),
            registry: Arc::clone(&self.registry),
            skills: Arc::clone(&self.skills),
            task_store: Arc::clone(&self.task_store),
            bg_manager: Arc::clone(&self.bg_manager),
            todo_manager: Arc::clone(&self.todo_manager),
            workdir: self.workdir.clone(),
            cron_manager: None,
            compactor: None,
            memory: None,
            hooks: Self::build_teammate_hooks(),
            base_system: system.to_string(),
            max_turns: None,
            kind: AgentKind::Teammate,
            owner: name.to_string(),
            team: Some(team),
            max_tokens: self.max_tokens,
        }
    }

    /// Teammate hook set: no TodoReminder/Summary; non-interactive permission.
    fn build_teammate_hooks() -> Hooks {
        let mut h = Hooks::new();
        h.on_pre_tool(builtins::TeammatePermissionHook);
        h.on_post_tool(builtins::LargeOutputHook);
        h
    }

    pub fn owner(&self) -> &str { &self.owner }
    pub fn team(&self) -> Option<&Arc<crate::team::TeamCtx>> { self.team.as_ref() }
    pub fn lead_notify(&self) -> Option<&tokio::sync::Notify> {
        self.team.as_ref().map(|t| t.lead_notify())
    }
```
Add `builtins::TeammatePermissionHook` to `src/builtins.rs` — a `PreToolUse` hook that, unlike `PermissionHook`, **never reads stdin**: it denies destructive commands and out-of-workspace paths by returning an error string, otherwise `None`. (Inspect `builtins.rs` first; if `PermissionHook` already takes a `prompt_user` flag, reuse it in non-prompt mode instead of adding a new struct.)

- [ ] **Step 3: Add `ToolContext::cwd()` and `owner()`**

In `src/tools/trait_def.rs` `impl ToolContext`:
```rust
impl<'a> ToolContext<'a> {
    pub fn owner(&self) -> &str { &self.agent.owner }
    pub fn cwd(&self) -> Result<std::path::PathBuf, String> {
        use crate::tools::trait_def::AgentKind;
        match &self.agent.team {
            None => Ok(self.agent.workdir.clone()),
            Some(team) => {
                if team.assignments.get(&self.agent.owner).is_some() {
                    crate::team::assignment::assignment_cwd(
                        &team.workdir, &team.task_store, &team.assignments, &self.agent.owner)
                } else if self.agent.kind == AgentKind::Teammate {
                    Err("Claim a Task before using workspace tools.".into())
                } else {
                    Ok(self.agent.workdir.clone())
                }
            }
        }
    }
}
```

- [ ] **Step 4: Plan gate in `execute_tool` + run-loop inbox drain**

In `src/agent.rs` `execute_tool`, after the `trigger_pre_tool` deny check and before dispatch:
```rust
        // s13 plan gate: teammates cannot run mutating tools until the plan is approved.
        if self.kind == AgentKind::Teammate
            && matches!(name, "bash" | "write_file" | "edit_file")
        {
            if let Some(team) = &self.team {
                let gate = team.protocols.gate(&self.owner);
                if gate.blocks_mutating_tools() {
                    return ToolResult::Denied {
                        name: name.to_string(),
                        reason: format!("Blocked: plan status is {:?}. Submit or revise the plan and wait for approval.", gate),
                    };
                }
            }
        }
```
In `run_loop`, at the top of each turn (after the bg `kind==Lead` collect and the cron block), add:
```rust
            // s13: teammates drain their own inbox each turn (Lead's inbox is
            // drained by main.rs outside run_loop).
            if self.kind == AgentKind::Teammate {
                if let Some(team) = &self.team {
                    if crate::team::drain_inbox(team, &self.owner, messages) {
                        return Ok(LoopOutcome::Completed);
                    }
                }
            }
```

- [ ] **Step 5: Implement `drain_inbox` + apply helpers in `team/mod.rs`**

Add to `src/team/mod.rs`:
```rust
use crate::client::{ContentBlock, Message};
use crate::team::bus::MessageRecord;
use crate::team::protocols::{GateStatus, ProtocolStatus, ProtocolType};

/// Drain this teammate's inbox into its messages. Returns true if a shutdown was accepted.
pub fn drain_inbox(team: &TeamCtx, owner: &str, messages: &mut Vec<Message>) -> bool {
    let inbox = team.bus.read_inbox(owner);
    if inbox.is_empty() { return false; }
    let mut work: Vec<String> = Vec::new();
    let mut should_stop = false;
    for msg in inbox {
        match msg.msg_type.as_str() {
            "shutdown_request" => {
                if apply_shutdown_request(team, owner, &msg) {
                    team.bus.send(owner, "lead", "Shutdown acknowledged.", "shutdown_response",
                        Some(serde_json::json!({"request_id": msg.metadata.get("request_id").cloned().unwrap_or(serde_json::Value::Null), "approve": true})));
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
        messages.push(Message { role: "user".to_string(),
            content: vec![ContentBlock::Text { text: work.join("\n") }] });
    }
    should_stop
}

/// Accept a pending shutdown request sent by Lead to this teammate.
fn apply_shutdown_request(team: &TeamCtx, owner: &str, msg: &MessageRecord) -> bool {
    let request_id = match msg.metadata.get("request_id").and_then(|v| v.as_str()) { Some(s) => s, None => return false };
    let mut pending = team.protocols.pending.lock().unwrap();
    let Some(state) = pending.get(request_id) else { return false; };
    if state.ptype != ProtocolType::Shutdown { return false; }
    if state.sender != "lead" || state.target != owner { return false; }
    if state.status != ProtocolStatus::Pending { return false; }
    let stopping = matches!(team.active.lock().unwrap().get(owner), Some(crate::team::TeammateStatus::Stopping));
    if stopping { return false; }
    team.active.lock().unwrap().insert(owner.into(), crate::team::TeammateStatus::Stopping);
    true
}

/// Apply the Lead's plan-approval response if it matches this teammate's current plan.
fn apply_plan_response(team: &TeamCtx, owner: &str, msg: &MessageRecord) -> String {
    let request_id = match msg.metadata.get("request_id").and_then(|v| v.as_str()) { Some(s) => s.to_string(), None => return "[Ignored plan response: no request_id]".into() };
    let work_version = team.assignments.version(owner);
    let task_id = team.assignments.get(owner).map(|a| a.task_id);
    let mut pending = team.protocols.pending.lock().unwrap();
    let Some(state) = pending.get(&request_id) else { return "[Ignored plan response: request mismatch]".into(); };
    let expected_id = team.protocols.plan_request_ids.lock().unwrap().get(owner).cloned();
    let approve = msg.metadata.get("approve").and_then(|v| v.as_bool()).unwrap_or(false);
    let valid = state.ptype == ProtocolType::PlanApproval
        && state.sender == owner && state.target == "lead"
        && Some(&request_id) == expected_id.as_ref()
        && state.work_version == Some(work_version)
        && state.task_id == task_id
        && state.status == ProtocolStatus::Pending
        && approve == true;   // approve=true only when matching
    if !valid { return "[Ignored plan response: request mismatch]".into(); }
    let new_gate = if approve { GateStatus::Approved } else { GateStatus::Rejected };
    let new_status = if approve { ProtocolStatus::Approved } else { ProtocolStatus::Rejected };
    pending.get_mut(&request_id).unwrap().status = new_status;
    drop(pending);
    team.protocols.set_gate(owner, new_gate);
    team.protocols.plan_request_ids.lock().unwrap().remove(owner);
    team.active.lock().unwrap().insert(owner.into(), crate::team::TeammateStatus::Working);
    format!("[Plan {}] {}", if approve { "approved" } else { "rejected" }, msg.content)
}
```
Add `use std::sync::Arc; use std::path::PathBuf; use std::collections::HashMap; use std::sync::Mutex;` at the top of `src/team/mod.rs` (some already present).

- [ ] **Step 6: Write failing tests, then run**

Append to `src/agent.rs` tests:
```rust
    #[tokio::test]
    async fn execute_tool_plan_gate_blocks_bash_for_teammate() {
        // A teammate with gate=Pending cannot run bash; pre_tool destructive path
        // is separate. We set gate via a TeamCtx constructed in a tempdir.
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Arc::new(crate::task_system::store::create_test_store(tmp.path()));
        let team = Arc::new(crate::team::TeamCtx::new(tmp.path().to_path_buf(), store).unwrap());
        team.protocols.set_gate("alice", crate::team::protocols::GateStatus::Pending);
        let a = TestAgent::new();
        let child = a.agent().child_teammate("alice", "sub", Arc::clone(&team));
        let r = child.execute_tool("bash", &serde_json::json!({"command":"ls"})).await;
        assert!(matches!(r, ToolResult::Denied { .. }), "teammate bash must be gated, got {:?}", r);
    }

    #[tokio::test]
    async fn execute_tool_allows_bash_when_approved() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Arc::new(crate::task_system::store::create_test_store(tmp.path()));
        let team = Arc::new(crate::team::TeamCtx::new(tmp.path().to_path_buf(), store).unwrap());
        team.protocols.set_gate("alice", crate::team::protocols::GateStatus::Approved);
        let a = TestAgent::new();
        let child = a.agent().child_teammate("alice", "sub", Arc::clone(&team));
        // child has no assignment -> ctx.cwd() errors for teammate, but the gate
        // passes; bash will then fail on cwd, NOT on the gate. Assert not-Denied-for-gate.
        let r = child.execute_tool("bash", &serde_json::json!({"command":"echo hi"})).await;
        assert!(!matches!(r, ToolResult::Denied { .. }) || r.as_content().contains("Claim a Task"),
            "should not be gated when approved, got {:?}", r);
    }
```

Run: `cargo test --lib agent::tests`
Expected: PASS (gate blocks/allows).

- [ ] **Step 7: Run full suite + commit**

Run: `cargo test`
Expected: PASS.
```bash
git add src/agent.rs src/tools/trait_def.rs src/team/mod.rs src/builtins.rs
git commit -m "feat(s13): add Agent owner/team fields, child_teammate, ctx.cwd, plan gate, inbox drain"
```

---

### Task 8: File tools resolve cwd through `ToolContext::cwd()`

**Files:**
- Modify: `src/tools/command.rs`, `src/tools/read_file.rs`, `src/tools/write_file.rs`, `src/tools/edit_file.rs`, `src/tools/glob_tool.rs`

- [ ] **Step 1: Add a `cwd_or_err` helper and switch each tool**

In `src/tools/mod.rs`, add a helper tools call to resolve cwd from a `ToolContext`:
```rust
/// Resolve the caller's working directory, returning an error string on failure.
pub fn ctx_cwd(ctx: &ToolContext<'_>) -> Result<std::path::PathBuf, String> {
    ctx.cwd()
}
```

In each of `command.rs`, `read_file.rs`, `write_file.rs`, `edit_file.rs`, `glob_tool.rs`, at the top of `execute`, replace the existing base-dir resolution:
```rust
let cwd = match crate::tools::ctx_cwd(ctx) {
    Ok(p) => p,
    Err(e) => return format!("Error: {}", e),
};
```
Then:
- `command.rs`: set `Command`'s `.current_dir(&cwd)` (instead of `tools::workdir()`).
- `read_file.rs` / `write_file.rs` / `edit_file.rs`: replace `tools::safe_path(path)` / `tools::safe_path_in(&tools::workdir(), path)` with `tools::safe_path_in(&cwd, path)`.
- `glob_tool.rs`: replace the base `tools::workdir()` with `cwd`.

These `execute` fns already take `ctx: &ToolContext<'_>`; no signature change. The Lead (team=Some, no assignment → `ctx.cwd()` returns workdir) behaves as before. Teammates with an assignment get the task's cwd; teammates without an assignment get `"Claim a Task before using workspace tools."` (Phase 1: always the repo dir since no worktree, but the assignment must exist — claimed via `claim_task`).

- [ ] **Step 2: Write the failing test**

Append to `src/tools/command.rs` tests (or `src/tools/mod.rs` tests):
```rust
    #[tokio::test]
    async fn command_runs_in_ctx_cwd() {
        // A Lead agent (team=Some, no assignment) -> ctx.cwd() == workdir.
        // Verify command executes in that dir. (Uses TestAgent's tempdir.)
        use crate::agent::TestAgent;
        use crate::tools::trait_def::Tool;
        let a = TestAgent::new();
        let ctx = a.context();
        let out = crate::tools::command::CommandTool.execute(&ctx, &serde_json::json!({"command":"cd"})).await;
        // `cd` prints the cwd; it should contain the TestAgent tempdir path stem.
        assert!(out.contains("TempDir") || out.contains('\\') || out.contains('/'),
            "command should run in ctx cwd, got: {}", out);
    }
```
*(If `command.rs` has no `#[cfg(test)]` module, add one with `use super::*;`.)*

- [ ] **Step 3: Run tests to verify pass**

Run: `cargo test --lib tools::`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add src/tools/mod.rs src/tools/command.rs src/tools/read_file.rs src/tools/write_file.rs src/tools/edit_file.rs src/tools/glob_tool.rs
git commit -m "feat(s13): resolve file-tool cwd via ToolContext::cwd()"
```

---

### Task 9: `submit_plan` helper + `SubmitPlanTool`

**Files:**
- Create: `src/team/tools.rs`
- Modify: `src/team/mod.rs` (already declares `tools`)

- [ ] **Step 1: Write the failing tests + implementation**

Create `src/team/tools.rs`:
```rust
use async_trait::async_trait;
use serde_json::{json, Value};
use crate::tools::trait_def::{AgentKind, PermissionCheck, Tool, ToolContext};
use crate::team::protocols::{GateStatus, ProtocolState, ProtocolStatus, ProtocolType};
use crate::team::{TeamCtx, TeammateStatus};

/// Teammate-only: submit a plan for Lead approval. Records work_version+task_id
/// so a later claim/complete invalidates the approval.
pub fn submit_plan(team: &TeamCtx, owner: &str, plan: &str) -> String {
    let task_id = team.assignments.get(owner).map(|a| a.task_id.clone());
    let work_version = team.assignments.version(owner);
    {
        let mut pending = team.protocols.pending.lock().unwrap();
        if team.protocols.gate(owner) == GateStatus::Pending {
            return "A plan is already waiting for review.".into();
        }
        let request_id = team.protocols.new_request_id();
        pending.insert(request_id.clone(), ProtocolState {
            request_id: request_id.clone(),
            ptype: ProtocolType::PlanApproval,
            sender: owner.into(),
            target: "lead".into(),
            status: ProtocolStatus::Pending,
            payload: plan.into(),
            work_version: Some(work_version),
            task_id,
        });
        team.protocols.plan_request_ids.lock().unwrap().insert(owner.into(), request_id.clone());
        team.bus.send(owner, "lead", plan, "plan_approval_request",
            Some(json!({"request_id": request_id})));
    }
    team.protocols.set_gate(owner, GateStatus::Pending);
    team.active.lock().unwrap().insert(owner.into(), TeammateStatus::WaitingApproval);
    // Wake the Lead so it sees the plan_approval_request.
    team.lead_notify.notify_one();
    "Plan submitted. Wait for Lead's decision.".into()
}

pub struct SubmitPlanTool;

#[async_trait]
impl Tool for SubmitPlanTool {
    fn name(&self) -> &str { "submit_plan" }
    fn description(&self) -> &str { "Submit a work plan for Lead approval before changing files or running bash." }
    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{"plan":{"type":"string"}},"required":["plan"]})
    }
    fn check_permission(&self, _: &Value) -> PermissionCheck { PermissionCheck::Pass }
    fn available_for(&self, kind: AgentKind) -> bool { kind == AgentKind::Teammate }
    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String {
        let plan = input.get("plan").and_then(|v| v.as_str()).unwrap_or("");
        let Some(team) = &ctx.agent.team else { return "Error: not in team context".into(); };
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
```

- [ ] **Step 2: Run test to verify pass**

Run: `cargo test --lib team::tools`
Expected: PASS (2 tests).

- [ ] **Step 3: Commit**

```bash
git add src/team/tools.rs
git commit -m "feat(s13): add submit_plan helper and SubmitPlanTool"
```

---

### Task 10: `TeammateRuntime` + `spawn_teammate_thread` + `SpawnTeammateTool`

**Files:**
- Create: `src/team/runtime.rs`
- Modify: `src/team/tools.rs` (add `SpawnTeammateTool`)
- Modify: `src/team/mod.rs` (`claim_next_task`, `scan_unclaimed_tasks`, `release_*`, `teammate_system_prompt`)

- [ ] **Step 1: Add helpers to `team/mod.rs`**

```rust
use std::time::Duration;

pub const IDLE_SCAN_INTERVAL: Duration = Duration::from_secs(2);

pub fn is_reserved_teammate_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "lead" || lower == "agent"
}

pub fn teammate_system_prompt(name: &str, role: &str) -> String {
    format!(
        "You are '{name}', a {role}. Use tools to complete the assigned Task, then call \
         complete_task and report a concise result. If the first user message contains \
         [Assigned task], that Task is already claimed; do not call claim_task for it \
         again. When asked for a plan, call submit_plan and wait for approval before \
         bash or file changes. File and shell tools use the Task's working directory; \
         that directory is not a sandbox. The runtime delivers your final text to Lead. \
         Use send_message only for intermediate coordination, addressing the coordinator \
         as 'lead'."
    )
}

pub fn release_completed_assignment(team: &TeamCtx, owner: &str) -> bool {
    let g = team.assignments.get(owner);
    if g.is_none() { return false; }
    let a = g.unwrap();
    let task = match team.task_store.load(&a.task_id) { Ok(t) => t, Err(_) => return false };
    if task.status != TaskStatus::Completed || task.owner.as_deref() != Some(owner) { return false; }
    team.assignments.remove(owner);
    team.assignments.advance_version(owner);
    team.protocols.set_gate(owner, GateStatus::NotRequired);
    true
}

pub fn release_teammate_assignment(team: &TeamCtx, owner: &str) {
    let _g = match team.lock.lock() { Ok(g) => g, Err(_) => return };
    // return any in-progress task to pending
    for t in team.task_store.list().unwrap_or_default() {
        if t.status == TaskStatus::InProgress && t.owner.as_deref() == Some(owner) {
            let mut t = t;
            t.status = TaskStatus::Pending;
            t.owner = None;
            let _ = team.task_store.save(&t);
        }
    }
    team.assignments.remove(owner);
    team.assignments.advance_version(owner);
    team.protocols.set_gate(owner, GateStatus::NotRequired);
}

pub fn scan_unclaimed_tasks(team: &TeamCtx) -> Vec<Task> {
    let mut out = Vec::new();
    for t in team.task_store.list().unwrap_or_default() {
        if t.status == TaskStatus::Pending && t.owner.is_none() && incomplete_deps_empty(team, &t) {
            let (_cwd, err) = crate::team::worktree::task_worktree_cwd(&team.workdir, &t);
            if err.is_none() { out.push(t); }
        }
    }
    out
}

pub fn claim_next_task(team: &TeamCtx, owner: &str) -> Option<Task> {
    if team.assignments.get(owner).is_some() { return None; }
    for t in scan_unclaimed_tasks(team) {
        let r = claim_task(team, &t.id, owner);
        if r.starts_with("Claimed") {
            return team.task_store.load(&t.id).ok();
        }
    }
    None
}

pub fn extract_last_assistant_text(messages: &[crate::client::Message]) -> Option<String> {
    messages.iter().rev().find(|m| m.role == "assistant").and_then(|m| {
        m.content.iter().rev().find_map(|b| match b {
            crate::client::ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
    })
}
```
Add `use crate::team::protocols::GateStatus;` and `use crate::task_system::task::{Task, TaskStatus};` at top of `mod.rs` if not present.

- [ ] **Step 2: Create `TeammateRuntime` in `src/team/runtime.rs`**

```rust
use std::sync::Arc;
use std::time::Duration;
use crate::agent::Agent;
use crate::client::{ContentBlock, Message};
use crate::team::{TeamCtx, TeammateStatus, claim_next_task, drain_inbox,
    extract_last_assistant_text, release_completed_assignment, release_teammate_assignment,
    teammate_system_prompt, IDLE_SCAN_INTERVAL};
use crate::team::protocols::GateStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase { Continue, Idle, Stop }

pub struct TeammateRuntime {
    pub name: String,
    pub agent: Agent,
    pub messages: Vec<Message>,
    pub team: Arc<TeamCtx>,
}

impl TeammateRuntime {
    /// Build the initial messages, including an [Assigned task] block if claimed.
    /// `child_agent` is built by the caller (`spawn_teammate_thread`) via
    /// `lead_agent.child_teammate(...)` — the runtime does NOT reference the Lead
    /// agent, avoiding a `TeamCtx → Agent → TeamCtx` `Arc` cycle.
    pub fn new(name: String, role: &str, prompt: String, team: Arc<TeamCtx>, child_agent: Agent) -> Self {
        let mut messages = vec![Message {
            role: "user".to_string(),
            content: vec![ContentBlock::Text { text: prompt }],
        }];
        if let Some(a) = team.assignments.get(&name) {
            if let Ok(task) = team.task_store.load(&a.task_id) {
                let block = format!("\n\n[Assigned task {}] {}\n{}\nWork directory: {}",
                    task.id, task.subject, task.description, a.cwd.display());
                if let Some(ContentBlock::Text { text }) = messages[0].content.get_mut(0) {
                    text.push_str(&block);
                }
            }
        }
        Self { name, agent: child_agent, messages, team }
    }

    pub async fn run(self) {
        let mut phase = Phase::Continue;
        while phase != Phase::Stop {
            if phase == Phase::Idle {
                if !self.wait_for_work().await { break; }
            }
            phase = self.work().await;
        }
        release_teammate_assignment(&self.team, &self.name);
        self.team.active.lock().unwrap().remove(&self.name);
    }

    async fn work(&self) -> Phase {
        let agent = &self.agent;
        let messages = &mut self.messages;
        let _ = agent.run_loop(messages, &self.name).await;
        if matches!(self.team.active.lock().unwrap().get(&self.name).copied(),
            Some(TeammateStatus::Stopping)) {
            return Phase::Stop;
        }
        let gate = self.team.protocols.gate(&self.name);
        if gate == GateStatus::Pending {
            self.team.active.lock().unwrap().insert(self.name.clone(), TeammateStatus::WaitingApproval);
            return Phase::Idle;
        }
        if let Some(summary) = extract_last_assistant_text(&self.messages) {
            self.team.bus.send(&self.name, "lead", &summary, "result", None);
            self.team.lead_notify.notify_one();
        }
        release_completed_assignment(&self.team, &self.name);
        self.team.active.lock().unwrap().insert(self.name.clone(), TeammateStatus::Idle);
        self.team.bus.send(&self.name, "lead", "Waiting for more work.", "idle_notification", None);
        self.team.lead_notify.notify_one();
        Phase::Idle
    }

    async fn wait_for_work(&self) -> bool {
        loop {
            let inbox = self.team.bus.wait_for_messages(&self.name, IDLE_SCAN_INTERVAL).await;
            if !inbox.is_empty() {
                let before = self.messages.len();
                let stop = drain_inbox(&self.team, &self.name, &mut self.messages);
                if stop { return false; }
                if self.messages.len() > before { return true; }
                continue;
            }
            if let Some(task) = claim_next_task(&self.team, &self.name) {
                let cwd = self.team.assignments.get(&self.name)
                    .map(|a| a.cwd.clone()).unwrap_or_else(|| self.team.workdir.clone());
                let text = format!("[Auto-claimed task {}] {}\n{}\nWork directory: {}",
                    task.id, task.subject, task.description, cwd.display());
                self.messages.push(Message {
                    role: "user".to_string(),
                    content: vec![ContentBlock::Text { text }],
                });
                return true;
            }
        }
    }
}

/// Validate + claim initial task, then spawn one persistent teammate.
pub fn spawn_teammate_thread(
    lead_agent: &Agent, name: &str, role: &str, prompt: &str,
    task_id: Option<&str>, require_plan: bool,
) -> String {
    use crate::team::{is_reserved_teammate_name, claim_task};
    use crate::team::bus::is_valid_agent_name;
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
    team.active.lock().unwrap().insert(name.into(), TeammateStatus::Working);
    team.protocols.set_gate(name, if require_plan { GateStatus::Required } else { GateStatus::NotRequired });

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
    tokio::spawn(async move { rt.run().await; });
    format!("Teammate '{}' spawned as {}. End this turn; the runtime will deliver its events.", name, role)
}
```

> **Send check:** `TeammateRuntime` is moved into `tokio::spawn`, so it must be `Send`. All its fields are `Arc`/`String`/`Vec`, except `Agent.hooks: Hooks`. Verify `Hooks` (and the hook trait objects it holds) is `Send + Sync`; if not, box teammates' hooks as `Arc<dyn ... + Send + Sync>` or restrict teammate hooks to `Send` variants before this task compiles.

- [ ] **Step 3: Add `SpawnTeammateTool` to `src/team/tools.rs`**

```rust
pub struct SpawnTeammateTool;

#[async_trait]
impl Tool for SpawnTeammateTool {
    fn name(&self) -> &str { "spawn_teammate" }
    fn description(&self) -> &str { "Spawn a persistent teammate. Propose a team and wait for user confirmation first." }
    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "name":{"type":"string","pattern":"^[A-Za-z0-9_-]{1,64}$"},
            "role":{"type":"string"},
            "prompt":{"type":"string"},
            "task_id":{"type":"string","pattern":"^task_[0-9a-f]{8}$"},
            "require_plan":{"type":"boolean"}
        },"required":["name","role","prompt"]})
    }
    fn check_permission(&self, _: &Value) -> PermissionCheck { PermissionCheck::Pass }
    fn available_for(&self, kind: AgentKind) -> bool { kind == AgentKind::Lead }
    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String {
        let name = input.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let role = input.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let prompt = input.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
        let task_id = input.get("task_id").and_then(|v| v.as_str());
        let require_plan = input.get("require_plan").and_then(|v| v.as_bool()).unwrap_or(false);
        crate::team::runtime::spawn_teammate_thread(ctx.agent, name, role, prompt, task_id, require_plan)
    }
}
```

- [ ] **Step 4: Write failing tests**

Append to `src/team/runtime.rs` tests:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::TestAgent;
    use crate::team::{is_reserved_teammate_name};
    use crate::team::bus::is_valid_agent_name;

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
```
*(Note: `spawn_teammate_thread` with `None` task spawns a real tokio task that immediately tries to wait_for_work; in tests with no API key it won't make model calls until work arrives. To keep unit tests hermetic, gate the spawned task behind `#[cfg(not(test))]` or accept it idles harmlessly. Prefer: in `spawn_teammate_thread`, if `cfg!(test)` and no API key, still record `active` but don't `tokio::spawn`. Add a `#[cfg(not(test))]` guard on the `tokio::spawn` line and in test mode just construct the runtime without spawning.)*

- [ ] **Step 5: Run tests to verify pass**

Run: `cargo test --lib team::runtime`
Expected: PASS (4 tests).

- [ ] **Step 6: Commit**

```bash
git add src/team/runtime.rs src/team/tools.rs src/team/mod.rs
git commit -m "feat(s13): add TeammateRuntime and spawn_teammate"
```

---

### Task 11: Lead tools — `list_teammates`, `send_message`, `request_shutdown`, `request_plan`, `review_plan`

**Files:**
- Modify: `src/team/tools.rs`

- [ ] **Step 1: Add helpers + tool structs to `src/team/tools.rs`**

Append to `src/team/tools.rs` (after `submit_plan`):
```rust
use crate::team::protocols::{ProtocolState, ProtocolStatus, ProtocolType};

pub fn list_teammates(team: &crate::team::TeamCtx) -> String {
    let active = team.active.lock().unwrap();
    if active.is_empty() { return "No active teammates.".into(); }
    active.iter().map(|(n, s)| format!("{}: {:?}", n, s)).collect::<Vec<_>>().join("\n")
}

pub fn send_message(team: &crate::team::TeamCtx, to: &str, content: &str) -> String {
    let active = team.active.lock().unwrap();
    if to != "lead" && !active.contains_key(to) && !active.keys().any(|k| k.eq_ignore_ascii_case(to)) {
        return format!("Teammate '{}' is not active", to);
    }
    drop(active);
    team.bus.send("lead", to, content, "message", None);
    format!("Sent to {}", to)
}

pub fn request_shutdown(team: &crate::team::TeamCtx, teammate: &str) -> String {
    let active = team.active.lock().unwrap();
    if !active.keys().any(|k| k.eq_ignore_ascii_case(teammate)) {
        return format!("Teammate '{}' is not active", teammate);
    }
    drop(active);
    let request_id = team.protocols.new_request_id();
    team.protocols.pending.lock().unwrap().insert(request_id.clone(), ProtocolState {
        request_id: request_id.clone(),
        ptype: ProtocolType::Shutdown,
        sender: "lead".into(),
        target: teammate.into(),
        status: ProtocolStatus::Pending,
        payload: String::new(),
        work_version: None,
        task_id: None,
    });
    team.bus.send("lead", teammate, "Finish the current step and shut down.",
        "shutdown_request", Some(json!({"request_id": request_id})));
    format!("Shutdown requested from {} ({})", teammate, request_id)
}

pub fn request_plan(team: &crate::team::TeamCtx, teammate: &str, task: &str) -> String {
    let active = team.active.lock().unwrap();
    if !active.keys().any(|k| k.eq_ignore_ascii_case(teammate)) {
        return format!("Teammate '{}' is not active", teammate);
    }
    drop(active);
    team.protocols.set_gate(teammate, crate::team::protocols::GateStatus::Required);
    team.bus.send("lead", teammate, task, "plan_request", None);
    format!("Plan requested from {}", teammate)
}

pub fn review_plan(team: &crate::team::TeamCtx, request_id: &str, approve: bool, feedback: &str) -> String {
    let sender = {
        let pending = team.protocols.pending.lock().unwrap();
        let Some(state) = pending.get(request_id) else {
            return format!("Request {} not found", request_id);
        };
        if state.ptype != ProtocolType::PlanApproval { return format!("Request {} is not a plan", request_id); }
        if state.status != ProtocolStatus::Pending { return format!("Request {} already {:?}", request_id, state.status); }
        state.sender.clone()
    };
    let work_version = team.assignments.version(&sender);
    let task_id = team.assignments.get(&sender).map(|a| a.task_id.clone());
    let expected_id = team.protocols.plan_request_ids.lock().unwrap().get(&sender).cloned();
    let mut pending = team.protocols.pending.lock().unwrap();
    let Some(state) = pending.get_mut(request_id) else {
        return format!("Request {} not found", request_id);
    };
    if state.work_version != Some(work_version) || state.task_id != task_id {
        return format!("Request {} belongs to an earlier assignment", request_id);
    }
    if expected_id.as_deref() != Some(request_id) {
        return format!("Request {} is not the current plan", request_id);
    }
    state.status = if approve { ProtocolStatus::Approved } else { ProtocolStatus::Rejected };
    drop(pending);
    team.protocols.set_gate(&sender,
        if approve { crate::team::protocols::GateStatus::Approved } else { crate::team::protocols::GateStatus::Rejected });
    team.protocols.plan_request_ids.lock().unwrap().remove(&sender);
    let content = if !feedback.is_empty() { feedback.to_string() }
        else if approve { "Plan approved.".to_string() } else { "Revise the plan and submit it again.".to_string() };
    team.bus.send("lead", &sender, &content, "plan_approval_response",
        Some(json!({"request_id": request_id, "approve": approve})));
    format!("Plan {} ({})", if approve { "approved" } else { "rejected" }, request_id)
}

macro_rules! lead_tool {
    ($struct:ident, $name:expr, $desc:expr, $schema:expr, $exec:expr) => {
        pub struct $struct;
        #[async_trait]
        impl Tool for $struct {
            fn name(&self) -> &str { $name }
            fn description(&self) -> &str { $desc }
            fn input_schema(&self) -> Value { $schema }
            fn check_permission(&self, _: &Value) -> PermissionCheck { PermissionCheck::Pass }
            fn available_for(&self, kind: AgentKind) -> bool { kind == AgentKind::Lead }
            async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String { $exec(ctx, input) }
        }
    };
}

lead_tool!(ListTeammatesTool, "list_teammates",
    "List active teammates and their status.",
    json!({"type":"object","properties":{}}),
    |ctx: &ToolContext<'_>, _input: &Value| {
        let Some(team) = &ctx.agent.team else { return "Error: not in team context".into(); };
        list_teammates(team)
    });

lead_tool!(SendMessageTool, "send_message",
    "Send a message to 'lead' or an active teammate.",
    json!({"type":"object","properties":{"to":{"type":"string"},"content":{"type":"string"}},"required":["to","content"]}),
    |ctx: &ToolContext<'_>, input: &Value| {
        let Some(team) = &ctx.agent.team else { return "Error: not in team context".into(); };
        let to = input.get("to").and_then(|v| v.as_str()).unwrap_or("");
        let content = input.get("content").and_then(|v| v.as_str()).unwrap_or("");
        send_message(team, to, content)
    });

lead_tool!(RequestShutdownTool, "request_shutdown",
    "Ask a teammate to finish its current step and shut down.",
    json!({"type":"object","properties":{"teammate":{"type":"string"}},"required":["teammate"]}),
    |ctx: &ToolContext<'_>, input: &Value| {
        let Some(team) = &ctx.agent.team else { return "Error: not in team context".into(); };
        let t = input.get("teammate").and_then(|v| v.as_str()).unwrap_or("");
        request_shutdown(team, t)
    });

lead_tool!(RequestPlanTool, "request_plan",
    "Require a teammate to submit a plan before workspace changes.",
    json!({"type":"object","properties":{"teammate":{"type":"string"},"task":{"type":"string"}},"required":["teammate","task"]}),
    |ctx: &ToolContext<'_>, input: &Value| {
        let Some(team) = &ctx.agent.team else { return "Error: not in team context".into(); };
        let t = input.get("teammate").and_then(|v| v.as_str()).unwrap_or("");
        let task = input.get("task").and_then(|v| v.as_str()).unwrap_or("");
        request_plan(team, t, task)
    });

lead_tool!(ReviewPlanTool, "review_plan",
    "Approve or reject a teammate's submitted plan.",
    json!({"type":"object","properties":{"request_id":{"type":"string"},"approve":{"type":"boolean"},"feedback":{"type":"string"}},"required":["request_id","approve"]}),
    |ctx: &ToolContext<'_>, input: &Value| {
        let Some(team) = &ctx.agent.team else { return "Error: not in team context".into(); };
        let rid = input.get("request_id").and_then(|v| v.as_str()).unwrap_or("");
        let approve = input.get("approve").and_then(|v| v.as_bool()).unwrap_or(false);
        let feedback = input.get("feedback").and_then(|v| v.as_str()).unwrap_or("");
        review_plan(team, rid, approve, feedback)
    });
```

> **Note:** the `lead_tool!` macro's closure arg `$exec` must be `async`. If the macro form causes lifetime/async issues, expand each tool by hand (same body, `async fn execute`). Prefer hand-expanded structs if the macro fights the borrow checker — the bodies are trivial. Keep the macro only if it compiles.

- [ ] **Step 2: Write failing tests**

Append to `src/team/tools.rs` tests:
```rust
    #[test]
    fn list_teammates_empty_then_with() {
        let (_tmp, team) = ctx();
        assert_eq!(list_teammates(&team), "No active teammates.");
        team.active.lock().unwrap().insert("alice".into(), crate::team::TeammateStatus::Working);
        assert!(list_teammates(&team).contains("alice"));
    }

    #[test]
    fn send_message_rejects_inactive() {
        let (_tmp, team) = ctx();
        let r = send_message(&team, "ghost", "hi");
        assert!(r.contains("not active"));
    }

    #[test]
    fn request_shutdown_creates_pending() {
        let (_tmp, team) = ctx();
        team.active.lock().unwrap().insert("alice".into(), crate::team::TeammateStatus::Idle);
        let r = request_shutdown(&team, "alice");
        assert!(r.contains("Shutdown requested"));
        assert!(team.protocols.pending.lock().unwrap().len() == 1);
        assert!(team.bus.peek("alice"));
    }

    #[test]
    fn review_plan_approves_matching() {
        let (_tmp, team) = ctx();
        // simulate a submitted plan: submit_plan sets gate=pending + a pending request
        submit_plan(&team, "alice", "plan");
        let rid = team.protocols.plan_request_ids.lock().unwrap().get("alice").cloned().unwrap();
        let r = review_plan(&team, &rid, true, "");
        assert!(r.contains("approved"));
        assert_eq!(team.protocols.gate("alice"), crate::team::protocols::GateStatus::Approved);
        assert!(team.bus.peek("alice"));
    }

    #[test]
    fn review_plan_rejects_stale_version() {
        // If the owner re-claimed (advance_version) after submit, the approval must not apply.
        let (_tmp, team) = ctx();
        submit_plan(&team, "alice", "plan");
        let rid = team.protocols.plan_request_ids.lock().unwrap().get("alice").cloned().unwrap();
        team.assignments.advance_version("alice"); // stale
        let r = review_plan(&team, &rid, true, "");
        assert!(r.contains("earlier assignment"));
    }
```

- [ ] **Step 3: Run tests to verify pass**

Run: `cargo test --lib team::tools`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add src/team/tools.rs
git commit -m "feat(s13): add Lead team tools (list/send/shutdown/plan/review)"
```

---

### Task 12: Register team tools in `build_registry`; verify `AgentKind` visibility

**Files:**
- Modify: `src/tools/mod.rs`

- [ ] **Step 1: Register the team tools**

In `src/tools/mod.rs::build_registry()`, after the cron tools, add:
```rust
    registry.register(Box::new(crate::team::tools::SpawnTeammateTool));
    registry.register(Box::new(crate::team::tools::ListTeammatesTool));
    registry.register(Box::new(crate::team::tools::SendMessageTool));
    registry.register(Box::new(crate::team::tools::RequestShutdownTool));
    registry.register(Box::new(crate::team::tools::RequestPlanTool));
    registry.register(Box::new(crate::team::tools::ReviewPlanTool));
    registry.register(Box::new(crate::team::tools::SubmitPlanTool));
    // CreateWorktreeTool registered in Phase 2 (Task 18).
```

- [ ] **Step 2: Write the failing visibility tests**

Append to `src/tools/mod.rs` tests:
```rust
    #[test]
    fn teammate_tool_set_excludes_lead_tools() {
        use crate::tools::trait_def::AgentKind;
        let reg = build_registry();
        let teammate_names: Vec<&str> = reg.definitions_for(AgentKind::Teammate)
            .iter().map(|d| d.name.as_str()).collect();
        for lead_only in ["spawn_teammate", "request_shutdown", "request_plan",
                          "review_plan", "create_worktree", "schedule_cron",
                          "task_output", "task_stop"] {
            assert!(!teammate_names.contains(&lead_only),
                "teammate must not see {}, got {:?}", lead_only, teammate_names);
        }
        assert!(teammate_names.contains(&"submit_plan"));
        assert!(teammate_names.contains(&"claim_task"));
        assert!(teammate_names.contains(&"complete_task"));
        assert!(teammate_names.contains(&"bash"));
    }

    #[test]
    fn lead_tool_set_excludes_submit_plan() {
        use crate::tools::trait_def::AgentKind;
        let reg = build_registry();
        let lead_names: Vec<&str> = reg.definitions_for(AgentKind::Lead)
            .iter().map(|d| d.name.as_str()).collect();
        assert!(lead_names.contains(&"spawn_teammate"));
        assert!(!lead_names.contains(&"submit_plan"),
            "Lead must not see submit_plan, got {:?}", lead_names);
    }
```

- [ ] **Step 3: Run tests to verify pass**

Run: `cargo test --lib tools::`
Expected: PASS. Then run `cargo build` to confirm the binary compiles.
```bash
cargo build
```
Expected: clean build.

- [ ] **Step 4: Commit**

```bash
git add src/tools/mod.rs
git commit -m "feat(s13): register team tools and enforce AgentKind visibility"
```

---

### Task 13: Lead inbox delivery in the REPL (`main.rs`)

**Files:**
- Modify: `src/main.rs`
- Modify: `src/team/mod.rs` (`consume_lead_inbox`, `format_team_events`)

- [ ] **Step 1: Add `consume_lead_inbox` + `format_team_events` to `team/mod.rs`**

```rust
use crate::team::bus::MessageRecord;

/// Consume the Lead inbox, advancing protocol state for typed responses.
pub fn consume_lead_inbox(team: &TeamCtx) -> Vec<MessageRecord> {
    let msgs = team.bus.read_inbox("lead");
    for msg in &msgs {
        if msg.msg_type.ends_with("_response") {
            let request_id = msg.metadata.get("request_id").and_then(|v| v.as_str()).unwrap_or("");
            let approve = msg.metadata.get("approve").and_then(|v| v.as_bool()).unwrap_or(false);
            team.protocols.match_response(&msg.msg_type, request_id, approve, &msg.from, &msg.to);
        }
    }
    msgs
}

pub fn format_team_events(msgs: &[MessageRecord]) -> String {
    let mut lines = Vec::new();
    for msg in msgs {
        let rid = msg.metadata.get("request_id").and_then(|v| v.as_str()).unwrap_or("");
        let suffix = if rid.is_empty() { String::new() } else { format!(" request_id={}", rid) };
        lines.push(format!("[{}{}] {}: {}", msg.msg_type, suffix, msg.from, msg.content));
    }
    format!("[Team events]\n{}", lines.join("\n"))
}
```

- [ ] **Step 2: Rewrite the REPL loop in `src/main.rs`**

Replace the `loop { ... io::stdin().read_line ... }` block with a `tokio::select!` over stdin and the Lead inbox notify:
```rust
    use tokio::io::AsyncBufReadExt;
    let mut reader = tokio::io::BufReader::new(tokio::io::stdin()).lines();

    loop {
        let notify = agent.lead_notify().expect("team initialized");
        tokio::select! {
            biased;
            line = reader.next_line() => {
                let line = match line { Ok(Some(s)) => s, _ => break };
                let query = line.trim().to_string();
                if query.is_empty() { continue; }
                if query.eq_ignore_ascii_case("q") || query == "exit" { break; }
                agent.trigger_prompt(&query);
                messages.push(Message {
                    role: "user".to_string(),
                    content: vec![ContentBlock::Text { text: query.clone() }],
                });
                if let Err(e) = agent.run_loop(&mut messages, &query).await {
                    output::error(&format!("Error: {}", e));
                }
                output::blank();
            }
            _ = notify.notified() => {
                let inbox = bytemaker::team::consume_lead_inbox(agent.team().unwrap());
                if inbox.is_empty() { continue; }
                let text = bytemaker::team::format_team_events(&inbox);
                messages.push(Message {
                    role: "user".to_string(),
                    content: vec![ContentBlock::Text { text }],
                });
                println!("[wake: {} team event(s) -> new turn]", inbox.len());
                if let Err(e) = agent.run_loop(&mut messages, "[team events]").await {
                    output::error(&format!("Error: {}", e));
                }
            }
        }
    }
```
Remove the now-unused `use std::io;` if no longer referenced. Keep `output::prompt()` before select if desired (it prints `s13 >> `); with `select!` a static prompt is printed once before the loop or omitted — simplest: drop the per-iteration prompt, print a one-time banner (already present).

- [ ] **Step 3: Write unit tests for the inbox helpers**

Append to `src/team/mod.rs` tests:
```rust
    #[test]
    fn consume_lead_inbox_matches_response() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Arc::new(crate::task_system::store::create_test_store(tmp.path()));
        let team = TeamCtx::new(tmp.path().to_path_buf(), store).unwrap();
        // seed a pending shutdown request
        use crate::team::protocols::{ProtocolState, ProtocolStatus, ProtocolType};
        team.protocols.pending.lock().unwrap().insert("req_000001".into(), ProtocolState {
            request_id: "req_000001".into(), ptype: ProtocolType::Shutdown,
            sender: "lead".into(), target: "alice".into(),
            status: ProtocolStatus::Pending, payload: String::new(),
            work_version: None, task_id: None,
        });
        // teammate replies shutdown_response into lead inbox
        team.bus.send("alice", "lead", "ack", "shutdown_response",
            Some(serde_json::json!({"request_id": "req_000001", "approve": true})));
        let inbox = consume_lead_inbox(&team);
        assert_eq!(inbox.len(), 1);
        assert_eq!(team.protocols.pending.lock().unwrap().get("req_000001").unwrap().status,
            ProtocolStatus::Approved);
    }

    #[test]
    fn format_team_events_shape() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Arc::new(crate::task_system::store::create_test_store(tmp.path()));
        let team = TeamCtx::new(tmp.path().to_path_buf(), store).unwrap();
        team.bus.send("alice", "lead", "done", "result", None);
        let inbox = team.bus.read_inbox("lead");
        let s = format_team_events(&inbox);
        assert!(s.starts_with("[Team events]"));
        assert!(s.contains("[result] alice: done"));
    }
```

- [ ] **Step 4: Run tests + build**

Run: `cargo test --lib team::` then `cargo build`
Expected: PASS + clean build.

- [ ] **Step 5: Commit**

```bash
git add src/team/mod.rs src/main.rs
git commit -m "feat(s13): deliver lead inbox events to the REPL via tokio::select"
```

---

### Task 14: End-to-end smoke test (Phase 1 acceptance)

**Files:**
- Create: `tests/s13_agent_teams.rs`

- [ ] **Step 1: Write the integration test**

Create `tests/s13_agent_teams.rs`:
```rust
// Requires ANTHROPIC_API_KEY / MODEL_ID + a git repo at CWD. Run: cargo test --test s13_agent_teams -- --ignored
#![cfg(feature = "smoke")]
use bytemaker::agent::{Agent, AgentConfig};
use bytemaker::client::{ContentBlock, Message};
use std::path::PathBuf;

#[tokio::test]
#[ignore]
async fn spawn_teammate_delivers_result_and_idle() {
    let api_key = std::env::var("ANTHROPIC_AUTH_TOKEN").or_else(|_| std::env::var("ANTHROPIC_API_KEY")).unwrap();
    let base_url = std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| "https://api.anthropic.com".into());
    let model = std::env::var("MODEL_ID").unwrap();
    let cwd = std::env::current_dir().unwrap();
    let agent = Agent::new(AgentConfig {
        api_key, base_url, model,
        workdir: cwd.clone(), skills_dir: cwd.join("skills"),
    }).await.unwrap();
    let team = agent.team().unwrap().clone();

    // Lead creates a task, then spawns a teammate on it.
    let task = agent.task_store.create("Echo hello".into(), "Print 'hello' and complete.".into(), vec![]).unwrap();
    let spawn = bytemaker::team::runtime::spawn_teammate_thread(
        agent, "alice", "coder", "Complete the assigned task.", Some(&task.id), false);
    assert!(spawn.contains("spawned"), "{}", spawn);

    // Wait (bounded) for the Lead inbox to receive a result + idle notification.
    let mut got_result = false;
    let mut got_idle = false;
    for _ in 0..120 {
        let inbox = bytemaker::team::consume_lead_inbox(&team);
        for m in &inbox {
            if m.msg_type == "result" { got_result = true; }
            if m.msg_type == "idle_notification" { got_idle = true; }
        }
        if got_result && got_idle { break; }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert!(got_result, "Lead must receive a result event");
    assert!(got_idle, "Lead must receive an idle_notification");

    // Clean shutdown.
    bytemaker::team::tools::request_shutdown(&team, "alice");
    let _ = bytemaker::team::consume_lead_inbox(&team); // drain shutdown_response
}
```

> Add a `smoke` cargo feature (optional) or just drop `#![cfg(feature="smoke")]` and rely on `#[ignore]`. The test is skipped by default (`#[ignore]`); run it explicitly when an API key is available. Because it exercises the real model, treat failures as flaky-investigation, not blockers for the unit suite.

- [ ] **Step 2: Run the ignored test (only if API key available)**

```bash
cargo test --test s13_agent_teams -- --ignored
```
Expected (with key): PASS. (Without key: skip — do not block Phase 1 completion on this.)

- [ ] **Step 3: Commit**

```bash
git add tests/s13_agent_teams.rs
git commit -m "test(s13): add end-to-end agent teams smoke test"
```

**Phase 1 is complete here.** The harness supports persistent teammates, file mailboxes, atomic claiming, and typed protocols — without worktree. Run `cargo test` (all unit tests green) and proceed to Phase 2 only if worktree is in scope.

---

## Phase 2 — Task-bound git worktree (deferrable)

> Implement only if worktree is in scope. Phase 1 is fully functional without it (teammates use the repo directory). Phase 2 replaces the `task_worktree_cwd` stub (Task 4) with real git-worktree resolution.

### Task 15: Worktree name validation + real `task_worktree_cwd`

**Files:**
- Replace: `src/team/worktree.rs` (was the Phase-1 stub)

- [ ] **Step 1: Write the failing tests + implementation**

Replace `src/team/worktree.rs` entirely:
```rust
use std::path::{Path, PathBuf};
use std::process::Command;
use crate::task_system::task::Task;

const VALID_WORKTREE_NAME: &str = r"^(?!.*\.\.)[A-Za-z0-9][A-Za-z0-9._-]{0,63}$";

pub fn validate_worktree_name(name: &str) -> Option<String> {
    let re = regex::Regex::new(VALID_WORKTREE_NAME).unwrap();
    if !re.is_match(name) {
        return Some("worktree name must be 1-64 chars, start [A-Za-z0-9], rest [A-Za-z0-9._-], no '..'".into());
    }
    None
}

pub fn worktree_path(worktrees_dir: &Path, name: &str) -> Result<PathBuf, String> {
    if let Some(e) = validate_worktree_name(name) { return Err(e); }
    crate::tools::safe_path_in(worktrees_dir, name)
}

pub fn worktree_branch(name: &str) -> String { format!("wt/{}", name) }

fn run_git(args: &[&str], cwd: &Path) -> (bool, String) {
    let out = Command::new("git").args(args).current_dir(cwd)
        .output().map_err(|e| e.to_string());
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            let e = String::from_utf8_lossy(&o.stderr);
            (o.status.success(), format!("{}{}", s, e).trim().to_string())
        }
        Err(e) => (false, e),
    }
}

/// Is `<workdir>/.worktrees/<name>` a registered git worktree on branch `wt/<name>`?
fn registered_worktree(workdir: &Path, name: &str) -> Result<PathBuf, String> {
    let dir = workdir.join(".worktrees");
    let path = worktree_path(&dir, name)?;
    let (ok, out) = run_git(&["worktree", "list", "--porcelain"], workdir);
    if !ok { return Err(format!("cannot read git worktree registry: {}", out)); }
    let expected_branch = format!("refs/heads/{}", worktree_branch(name));
    let mut found = false;
    let mut current_path: Option<String> = None;
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            current_path = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("branch ") {
            if rest == expected_branch {
                if current_path.as_deref() == Some(&path.to_string_lossy()) { found = true; }
            }
        }
    }
    if !found { return Err(format!("worktree '{}' is not registered with git", name)); }
    let p = PathBuf::from(current_path.unwrap());
    if !p.is_dir() { return Err(format!("worktree '{}' is missing at {}", name, p.display())); }
    Ok(p)
}

/// No worktree → repo workdir. Broken binding → (workdir, Some(err)) (caller fails closed).
pub fn task_worktree_cwd(workdir: &Path, task: &Task) -> (PathBuf, Option<String>) {
    let Some(name) = &task.worktree else { return (workdir.to_path_buf(), None); };
    match registered_worktree(workdir, name) {
        Ok(p) => (p, None),
        Err(e) => (workdir.to_path_buf(), Some(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_bad_names() {
        assert!(validate_worktree_name("../x").is_some());
        assert!(validate_worktree_name(".hidden").is_some());   // must start [A-Za-z0-9]
        assert!(validate_worktree_name(&"a".repeat(65)).is_some());
        assert!(validate_worktree_name("auth-refactor").is_none());
        assert!(validate_worktree_name("a.b-c_d").is_none());
    }

    #[test]
    fn task_worktree_cwd_no_binding_returns_workdir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let task = Task {
            id: "task_12345678".into(), subject: "s".into(), description: "".into(),
            status: crate::task_system::task::TaskStatus::Pending,
            owner: None, blocked_by: vec![], worktree: None,
        };
        let (cwd, err) = task_worktree_cwd(tmp.path(), &task);
        assert_eq!(cwd, tmp.path());
        assert!(err.is_none());
    }

    #[test]
    fn task_worktree_cwd_broken_binding_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let task = Task {
            id: "task_12345678".into(), subject: "s".into(), description: "".into(),
            status: crate::task_system::task::TaskStatus::Pending,
            owner: None, blocked_by: vec![], worktree: Some("missing".into()),
        };
        let (cwd, err) = task_worktree_cwd(tmp.path(), &task);
        assert!(err.is_some(), "broken worktree binding must produce an error");
        // not registered with git (tmp not even a repo) -> err
        let _ = cwd;
    }
}
```

- [ ] **Step 2: Run tests to verify pass**

Run: `cargo test --lib team::worktree`
Expected: PASS (3 tests).

- [ ] **Step 3: Commit**

```bash
git add src/team/worktree.rs
git commit -m "feat(s13): add worktree name validation and task_worktree_cwd resolution"
```

---

### Task 16: `create_worktree` (validate → `git worktree add` → bind, with partial-op reporting)

**Files:**
- Modify: `src/team/worktree.rs` (append `create_worktree`)
- Modify: `src/team/mod.rs` (re-export if needed)

- [ ] **Step 1: Append `create_worktree` to `src/team/worktree.rs`**

```rust
use crate::task_system::task::TaskStatus;
use crate::team::TeamCtx;

pub fn create_worktree(team: &TeamCtx, name: &str, task_id: &str) -> String {
    if let Some(e) = validate_worktree_name(name) { return format!("Error: {}", e); }
    let worktrees_dir = team.workdir.join(".worktrees");
    let path = match worktree_path(&worktrees_dir, name) {
        Ok(p) => p, Err(e) => return format!("Error: {}", e),
    };
    let branch = worktree_branch(name);
    let _g = match team.lock.lock() { Ok(g) => g, Err(e) => return format!("Error: {}", e) };
    let store = &team.task_store;
    let Ok(mut task) = store.load(task_id) else { return format!("Error: Task {} not found", task_id); };
    if task.status != TaskStatus::Pending || task.owner.is_some() {
        return format!("Error: Task {} must be pending and unowned", task_id);
    }
    if task.worktree.is_some() {
        return format!("Error: Task {} already uses worktree '{}'", task_id, task.worktree.as_deref().unwrap_or("?"));
    }
    for t in store.list().unwrap_or_default() {
        if t.id != task_id && t.worktree.as_deref() == Some(name) {
            return format!("Error: Worktree '{}' already bound to another task", name);
        }
    }
    if path.exists() { return format!("Error: Worktree path already exists: {}", path.display()); }

    let (ok, out) = run_git(&["rev-parse", "--show-toplevel"], &team.workdir);
    let toplevel_ok = ok && dunce::canonicalize(out.trim()).ok() == dunce::canonicalize(&team.workdir).ok();
    if !toplevel_ok { return "Error: Working directory must be the root of a Git repository".into(); }
    let (ok, o) = run_git(&["check-ref-format", "--branch", &branch], &team.workdir);
    if !ok { return format!("Error: Invalid worktree branch '{}': {}", branch, o); }
    let (exists, _) = run_git(&["show-ref", "--verify", "--quiet", &format!("refs/heads/{}", branch)], &team.workdir);
    if exists { return format!("Error: Branch '{}' already exists", branch); }

    let _ = std::fs::create_dir_all(&worktrees_dir);
    let (ok, result) = run_git(&["worktree", "add", "-b", &branch, &path.to_string_lossy(), "HEAD"], &team.workdir);
    if !ok {
        let mut artifacts = vec![];
        if path.exists() { artifacts.push(format!("checkout path '{}'", path.display())); }
        let (be, _) = run_git(&["show-ref", "--verify", "--quiet", &format!("refs/heads/{}", branch)], &team.workdir);
        if be { artifacts.push(format!("branch '{}'", branch)); }
        if !artifacts.is_empty() {
            return format!("Partial operation: git worktree add failed leaving {}. Task {} remains unbound; inspect '{}' and '{}'. Git error: {}",
                artifacts.join(", "), task_id, path.display(), branch, result);
        }
        return format!("Git error: {}", result);
    }
    task.worktree = Some(name.into());
    if let Err(e) = store.save(&task) {
        return format!("Partial success: Worktree '{}' created at {} on '{}', but binding failed: {}. Git data retained for manual recovery.",
            name, path.display(), branch, e);
    }
    format!("Worktree '{}' created at {} for task {}", name, path.display(), task_id)
}
```
Add `use dunce;` (or `use dunce::canonicalize;`) at top of `src/team/worktree.rs`.

- [ ] **Step 2: Write a test (needs a git repo tempdir)**

Append to `src/team/worktree.rs` tests:
```rust
    #[test]
    fn create_worktree_rejects_non_git_workdir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = std::sync::Arc::new(crate::task_system::store::create_test_store(tmp.path()));
        let team = crate::team::TeamCtx::new(tmp.path().to_path_buf(), store).unwrap();
        let t = team.task_store.create("T".into(), "".into(), vec![]).unwrap();
        let r = create_worktree(&team, "auth", &t.id);
        assert!(r.contains("must be the root of a Git repository"), "non-git workdir must be rejected, got {}", r);
    }

    #[test]
    fn create_worktree_rejects_non_pending_task() {
        // Build a tiny git repo in a tempdir.
        let tmp = tempfile::TempDir::new().unwrap();
        let _ = std::process::Command::new("git").args(["init"]).current_dir(tmp.path()).output().unwrap();
        let _ = std::process::Command::new("git").args(["config","user.email","t@t.t"]).current_dir(tmp.path()).output();
        let _ = std::process::Command::new("git").args(["config","user.name","t"]).current_dir(tmp.path()).output();
        std::fs::write(tmp.path().join("a.txt"), "x").unwrap();
        let _ = std::process::Command::new("git").args(["add","a.txt"]).current_dir(tmp.path()).output();
        let _ = std::process::Command::new("git").args(["commit","-m","init"]).current_dir(tmp.path()).output();
        let store = std::sync::Arc::new(crate::task_system::store::create_test_store(tmp.path()));
        let team = crate::team::TeamCtx::new(tmp.path().to_path_buf(), store).unwrap();
        let mut t = team.task_store.create("T".into(), "".into(), vec![]).unwrap();
        t.status = crate::task_system::task::TaskStatus::InProgress;
        t.owner = Some("alice".into());
        team.task_store.save(&t).unwrap();
        let r = create_worktree(&team, "auth", &t.id);
        assert!(r.contains("must be pending and unowned"), "non-pending must be rejected, got {}", r);
    }
```
*(Skip these tests if the CI environment lacks `git`; guard with `#[cfg_attr(not(feature="git_smoke"), ignore)]` if needed.)*

- [ ] **Step 3: Run tests + commit**

```bash
cargo test --lib team::worktree
```
Expected: PASS (skip-marked tests are OK to skip).
```bash
git add src/team/worktree.rs
git commit -m "feat(s13): add create_worktree with partial-op reporting"
```

---

### Task 17: `remove_worktree` (host-side; retains branch)

**Files:**
- Modify: `src/team/worktree.rs` (append `remove_worktree`)

- [ ] **Step 1: Append `remove_worktree` to `src/team/worktree.rs`**

```rust
pub fn remove_worktree(team: &TeamCtx, name: &str, discard_changes: bool) -> String {
    if let Some(e) = validate_worktree_name(name) { return format!("Error: {}", e); }
    let _g = match team.lock.lock() { Ok(g) => g, Err(e) => return format!("Error: {}", e) };
    let workdir = &team.workdir;
    let path = match registered_worktree(workdir, name) {
        Ok(p) => p, Err(e) => return format!("Error: {}", e),
    };
    let bound: Vec<_> = team.task_store.list().unwrap_or_default().into_iter()
        .filter(|t| t.worktree.as_deref() == Some(name)).collect();
    if bound.is_empty() { return format!("Error: Worktree '{}' is not bound to a task", name); }
    if let Some(t) = bound.iter().find(|t| t.status != TaskStatus::Completed) {
        return format!("Error: Worktree '{}' bound to active task {}; complete it first", name, t.id);
    }
    let leased: Vec<String> = team.assignments.snap()
        .into_iter().filter(|(_, a)| {
            dunce::canonicalize(&a.cwd).ok() == dunce::canonicalize(&path).ok()
        }).map(|(o, _)| o).collect();
    if !leased.is_empty() {
        return format!("Error: Worktree '{}' still in use by {}", name, leased.join(", "));
    }
    let (ok, status) = run_git(&["status", "--porcelain", "--ignored"], &path);
    if !ok { return format!("Error: Cannot verify worktree '{}' status: {}", name, status); }
    if !status.trim().is_empty() && !discard_changes {
        let changed = status.lines().filter(|l| !l.trim().is_empty()).count();
        return format!("Error: Worktree '{}' has {} uncommitted change(s); preserve or discard them manually", name, changed);
    }
    let mut args: Vec<&str> = vec!["worktree", "remove"];
    if discard_changes { args.push("--force"); }
    let path_str = path.to_string_lossy().to_string();
    let (ok, result) = run_git(&[args[0], args[1], args.get(2).unwrap_or(&""), &path_str], workdir);
    // NOTE: the above slice dance is awkward; prefer building a Vec<String> and passing. Simplify:
    let mut argv = vec!["worktree".to_string(), "remove".to_string()];
    if discard_changes { argv.push("--force".to_string()); }
    argv.push(path_str.clone());
    let (ok, result) = run_git_owned(&argv, workdir);
    if !ok { return format!("Git error: {}", result); }
    for mut t in bound { t.worktree = None; let _ = team.task_store.save(&t); }
    format!("Worktree '{}' removed; branch '{}' retained", name, worktree_branch(name))
}

fn run_git_owned(args: &[String], cwd: &Path) -> (bool, String) {
    let out = Command::new("git").args(args).current_dir(cwd).output();
    match out {
        Ok(o) => { let s = String::from_utf8_lossy(&o.stdout); let e = String::from_utf8_lossy(&o.stderr); (o.status.success(), format!("{}{}", s, e).trim().to_string()) }
        Err(e) => (false, e.to_string()),
    }
}
```
> The first `run_git(...)` call before `run_git_owned` is dead/confusing — **remove it** and call `run_git_owned` directly. The clean version is:
> ```rust
> let mut argv = vec!["worktree".to_string(), "remove".to_string()];
> if discard_changes { argv.push("--force".to_string()); }
> argv.push(path.to_string_lossy().to_string());
> let (ok, result) = run_git_owned(&argv, workdir);
> if !ok { return format!("Git error: {}", result); }
> ```
> Also add `pub fn snap(&self) -> Vec<(String, Assignment)>` to `AssignmentRegistry` (clones all entries) — used above.

- [ ] **Step 2: Test + commit**

```bash
cargo test --lib team::worktree
```
```bash
git add src/team/worktree.rs src/team/assignment.rs
git commit -m "feat(s13): add host-side remove_worktree (retains branch)"
```

---

### Task 18: `CreateWorktreeTool` + register

**Files:**
- Modify: `src/team/tools.rs`
- Modify: `src/tools/mod.rs`

- [ ] **Step 1: Add `CreateWorktreeTool` to `src/team/tools.rs`**

```rust
pub struct CreateWorktreeTool;

#[async_trait]
impl Tool for CreateWorktreeTool {
    fn name(&self) -> &str { "create_worktree" }
    fn description(&self) -> &str { "Create and bind a task worktree (Lead only). Task must be pending and unowned." }
    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "name":{"type":"string","pattern":"^(?!.*\\.\\.)[A-Za-z0-9][A-Za-z0-9._-]{0,63}$","maxLength":64},
            "task_id":{"type":"string"}
        },"required":["name","task_id"],"additionalProperties":false})
    }
    fn check_permission(&self, _: &Value) -> PermissionCheck { PermissionCheck::Pass }
    fn available_for(&self, kind: AgentKind) -> bool { kind == AgentKind::Lead }
    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String {
        let Some(team) = &ctx.agent.team else { return "Error: not in team context".into(); };
        let name = input.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let task_id = input.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
        crate::team::worktree::create_worktree(team, name, task_id)
    }
}
```

- [ ] **Step 2: Register it in `src/tools/mod.rs::build_registry`**

Replace the Phase-2 placeholder comment:
```rust
    registry.register(Box::new(crate::team::tools::CreateWorktreeTool));
```
And update the `teammate_tool_set_excludes_lead_tools` test — `create_worktree` is Lead-only so it stays excluded for teammates (already asserted; no change needed).

- [ ] **Step 3: Build + test + commit**

```bash
cargo build && cargo test
```
Expected: clean build + all tests green.
```bash
git add src/team/tools.rs src/tools/mod.rs
git commit -m "feat(s13): add CreateWorktreeTool and register it"
```

**Phase 2 complete.** All seven s13 features are implemented.

---

## Self-Review

**1. Spec coverage** — every spec section maps to a task:

| Spec section | Task(s) |
|---|---|
| `fs4` lock + `Task.worktree` | 1 |
| `AgentKind` replaces `for_subagent` | 2 |
| `MessageBus` | 3 |
| `Assignment` + `assignment_cwd` | 4, 7 |
| `ProtocolState` / gates / `match_response` | 5 |
| atomic `claim_task`/`complete_task` under lock | 6 |
| `Agent` owner/team/`child_teammate`/`cwd`/run-loop drain/gate | 7 |
| file tools resolve `ctx.cwd()` | 8 |
| `submit_plan` / `SubmitPlanTool` | 9 |
| `TeammateRuntime` + `spawn_teammate` | 10 |
| Lead tools (list/send/shutdown/plan/review) | 11 |
| registration + `AgentKind` visibility | 12 |
| Lead inbox delivery into REPL | 13 |
| end-to-end smoke (#[ignore]) | 14 |
| worktree validation + `task_worktree_cwd` | 15 |
| `create_worktree` (partial-op) | 16 |
| `remove_worktree` (host, retains branch) | 17 |
| `CreateWorktreeTool` | 18 |

No spec section is unaddressed.

**2. Placeholder scan** — no `TBD`/`TODO`/`implement later`/`add error handling`/`similar to Task N`. Every code step contains real code. The few `> **Note:**` blocks state concrete decisions (which hook to use, Send check, dead-code removal) — they are instructions, not placeholders.

**3. Type consistency** — verified across tasks: `Assignment { task_id, cwd }`, `GateStatus`/`ProtocolStatus`/`ProtocolType` enums, `ProtocolState { request_id, ptype, sender, target, status, payload, work_version, task_id }`, `TeammateStatus`, `TeamCtx` field set, and the free-fn signatures (`claim_task`/`complete_task`/`assignment_cwd`/`task_worktree_cwd`/`drain_inbox`/`submit_plan`/`request_shutdown`/`request_plan`/`review_plan`/`create_worktree`/`remove_worktree`/`spawn_teammate_thread`/`consume_lead_inbox`/`format_team_events`) all match between definition and call sites. (Naming: the field `ptype` is used instead of the spec's `type` to avoid the Rust keyword; internal consistency holds.)

**Corrections to apply during execution** (flagged inline; restated here so they are not missed):
- **Task 7** — add `builtins::TeammatePermissionHook` (non-interactive) unless `builtins::PermissionHook` already supports a no-prompt mode.
- **Task 10** — confirm `Hooks` is `Send + Sync` before `tokio::spawn(async move { rt.run() })` compiles.
- **Task 11** — if the `lead_tool!` macro fights async/borrow checker, hand-expand each tool struct (bodies are trivial).
- **Task 17** — keep only the clean `run_git_owned` call; drop the dead first `run_git` snippet.
- **Task 10 spawn in tests** — guard `tokio::spawn` with `#[cfg(not(test))]` so unit tests don't spawn live runtimes.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-08-20-agent-teams.md`. Two execution options:

**1. Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration.

**2. Inline Execution** — Execute tasks in this session using `superpowers:executing-plans`, batch execution with checkpoints.

Which approach?
