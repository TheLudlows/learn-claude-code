# s10 Task System Design Specification

**Date:** 2026-08-18
**Status:** Approved
**Related:** s10_task_system/README.zh.md

## Overview

Implement a file-persisted task system in rust-agent with dependency tracking, ownership, and state management. This enables coordination of multi-step work across sessions with proper task graph enforcement.

## Architecture

```
rust-agent/
├── src/
│   ├── task_system/
│   │   ├── mod.rs          # Module exports
│   │   ├── task.rs         # Task data structure and status enum
│   │   ├── store.rs        # TaskStore for file persistence
│   │   └── tools.rs        # Tool implementations
│   └── tools/
│       └── mod.rs          # Register task tools
```

## Core Components

### 1. Task Data Structure

Located in `src/task_system/task.rs`:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    pub id: String,              // Format: task_xxxxxxxx (8 hex digits)
    pub subject: String,         // Brief title
    pub description: String,     // Detailed description
    pub status: TaskStatus,      // Current state
    pub owner: Option<String>,   // Agent claiming this task
    pub blocked_by: Vec<String>, // Dependency task IDs
}
```

**Design decisions:**
- Use `enum` for status (type-safe vs Python's string)
- Use `snake_case` serde for JSON compatibility with Python
- `blocked_by` field name (snake_case) for JSON compatibility

### 2. TaskStore

Located in `src/task_system/store.rs`:

**Responsibilities:**
- File storage in `.tasks/` directory
- Path safety validation (prevent escape from workspace)
- ID generation with collision retry (max 100 attempts)
- Atomic file creation using `create_new(true)`

**Error types:**
```rust
pub enum TaskStoreError {
    InvalidId(String),
    NotFound(String),
    EscapesWorkspace,
    Io(std::io::Error),
    Json(serde_json::Error),
    InvalidStatus(String),
}
```

**Key methods:**
- `new(directory)`: Initialize store with workspace validation
- `create(subject, description, blocked_by)`: Create task with dependency validation
- `load(task_id)`: Load task with ID validation
- `save(task)`: Persist task state
- `list()`: Return all tasks sorted by ID
- `exists(task_id)`: Check if task file exists

### 3. Tools

Located in `src/task_system/tools.rs`:

| Tool | Purpose |
|------|---------|
| `create_task` | Create task with optional dependencies |
| `list_tasks` | List all tasks with status/owner/deps |
| `get_task` | Get full task details by ID |
| `claim_task` | Claim pending task (pending → in_progress) |
| `complete_task` | Complete task (in_progress → completed), return unblocked |

**Global state:**
```rust
static TASK_STORE: OnceLock<Arc<TaskStore>> = OnceLock::new();
```
Initialized via `init_task_store()` called from `main()`.

**Helper functions:**
- `incomplete_dependencies(store, task)`: Returns list of unmet dependencies

### 4. State Machine

```
pending ──claim──→ in_progress ──complete──→ completed
```

**Transitions:**
1. **claim_task**: pending → in_progress
   - Verify status is `Pending`
   - Verify all dependencies are `Completed`
   - Set `owner = "agent"`

2. **complete_task**: in_progress → completed
   - Verify status is `InProgress`
   - Verify `owner` matches caller
   - Calculate and return newly unblocked tasks

## Dependencies

Add to `Cargo.toml`:

```toml
fastrand = "2.1"   # Random ID generation
regex = "1"        # ID format validation (task_[0-9a-f]{8})
```

Both already available in the project.

## Integration Points

### Module Registration (`src/lib.rs`)

```rust
pub mod task_system;
```

### Tool Registration (`src/tools/mod.rs`)

```rust
use crate::task_system::{CreateTaskTool, ListTasksTool, GetTaskTool, ClaimTaskTool, CompleteTaskTool};

pub fn build_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();

    // ... existing tools ...

    registry.register(Box::new(CreateTaskTool));
    registry.register(Box::new(ListTasksTool));
    registry.register(Box::new(GetTaskTool));
    registry.register(Box::new(ClaimTaskTool));
    registry.register(Box::new(CompleteTaskTool));

    registry
}
```

### Initialization (`src/main.rs`)

```rust
fn main() {
    // ... existing initialization ...

    if let Err(e) = rust_agent::task_system::init_task_store() {
        eprintln!("Warning: Failed to initialize task store: {}", e);
    }

    // ... main loop ...
}
```

## Testing Strategy

### Unit Tests

**Location:** `src/task_system/{task,store,tools}.rs` - `#[cfg(test)]` modules

Coverage:
- Task serialization/deserialization
- TaskStore create/load/save operations
- ID collision retry mechanism
- Dependency validation
- State transition validation

### Integration Tests

**Location:** `tests/s10_task_system.rs`

Test cases mirroring Python implementation:
1. `test_dependencies_gate_claim_and_completion_checks_owner`
2. `test_invalid_and_missing_task_ids_become_tool_results`
3. `test_create_retries_instead_of_overwriting_an_existing_id`
4. `test_create_rejects_unknown_dependencies`
5. `test_task_store_rejects_a_symlink_outside_the_workspace`

## Security Considerations

1. **Path validation:** TaskStore validates `.tasks/` is within workspace
2. **ID validation:** Regex ensures format `task_[0-9a-f]{8}`
3. **ID collision:** Atomic write with `create_new(true)` prevents overwrites
4. **Dependency check:** Prevents claiming tasks with unmet dependencies
5. **Owner check:** Prevents completing tasks owned by other agents

## Compatibility with Python s10

| Python Feature | Rust Implementation |
|----------------|---------------------|
| `Task` dataclass | `Task` struct with `TaskStatus` enum |
| `TaskStore` class | `TaskStore` struct |
| Regex ID validation | `regex` crate |
| `secrets.token_hex(4)` | `fastrand::u32(..)` |
| `open(..., "x")` | `File::options().create_new(true)` |
| `OnceLock` pattern | `OnceLock<Arc<TaskStore>>` |
| JSON persistence | `serde_json` |

JSON output format matches Python for cross-language compatibility (snake_case).

## Success Criteria

- [ ] Task files created in `.tasks/` directory
- [ ] Tasks can be created with dependencies
- [ ] `claim_task` blocks until dependencies complete
- [ ] `complete_task` returns newly unblocked tasks
- [ ] Owner validation prevents unauthorized completion
- [ ] Invalid task IDs return error messages
- [ ] All Python test cases pass equivalent Rust tests
- [ ] Path escape attempts are rejected

## Migration Notes

- Existing `src/tools/task.rs` (subagent tool) is unrelated to this feature
- No breaking changes to existing code
- Task storage only activates on successful `init_task_store()`