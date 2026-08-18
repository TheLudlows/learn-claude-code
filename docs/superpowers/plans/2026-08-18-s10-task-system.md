# s10 Task System Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement a file-persisted task system with dependency tracking, ownership, and state management in rust-agent

**Architecture:** Three-module design - task.rs (data), store.rs (persistence), tools.rs (harness integration) - with global Arc<TaskStore> for thread-safe shared state

**Tech Stack:** Rust, serde (JSON), regex (ID validation), fastrand (random IDs), tokio (async tools)

## Global Constraints

- Task ID format: `task_[0-9a-f]{8}` (8 hex digits)
- Storage directory: `.tasks/` within workspace
- Task status enum values: Pending, InProgress, Completed (snake_case JSON)
- Owner default: "agent" when claiming
- Max ID retry attempts: 100
- All tools use PermissionCheck::Pass
- Python s10 compatibility: JSON output format matches Python implementation

---

### Task 1: Add Dependencies to Cargo.toml

**Files:**
- Modify: `rust-agent/Cargo.toml`

**Interfaces:**
- Produces: Available `fastrand` and `regex` crates

- [ ] **Step 1: Add fastrand and regex dependencies**

```toml
[dependencies]
# ... existing dependencies ...
fastrand = "2.1"
regex = "1"
```

Add these two lines to the `[dependencies]` section in `rust-agent/Cargo.toml`.

- [ ] **Step 2: Verify dependencies compile**

Run: `cd rust-agent && cargo check`
Expected: No errors, dependencies resolved successfully

- [ ] **Step 3: Commit**

```bash
git add rust-agent/Cargo.toml
git commit -m "feat(s10): add fastrand and regex dependencies"
```

---

### Task 2: Create Task Data Structure

**Files:**
- Create: `rust-agent/src/task_system/task.rs`
- Create: `rust-agent/src/task_system/mod.rs`

**Interfaces:**
- Produces: `Task` struct with `id: String, subject: String, description: String, status: TaskStatus, owner: Option<String>, blocked_by: Vec<String>`
- Produces: `TaskStatus` enum with `Pending, InProgress, Completed` variants
- Produces: `Task::can_claim(&self, incomplete_deps: &[String]) -> bool`
- Produces: `Task::can_complete(&self, owner: &str) -> bool`

- [ ] **Step 1: Create task.rs with Task and TaskStatus**

```rust
/*
task.rs - Task data structure for s10 Task System

Defines Task struct and TaskStatus enum with serialization support.
*/

use serde::{Deserialize, Serialize};

/// 任务状态
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

/// 任务数据结构
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    /// 任务 ID，格式：task_xxxxxxxx（8位十六进制）
    pub id: String,
    /// 任务简短标题
    pub subject: String,
    /// 任务详细描述
    pub description: String,
    /// 当前状态
    pub status: TaskStatus,
    /// 负责该任务的 agent
    pub owner: Option<String>,
    /// 前置任务 ID 列表
    pub blocked_by: Vec<String>,
}

impl Task {
    /// 检查任务是否可以被认领
    pub fn can_claim(&self, incomplete_deps: &[String]) -> bool {
        self.status == TaskStatus::Pending && incomplete_deps.is_empty()
    }

    /// 检查任务是否可以被完成
    pub fn can_complete(&self, owner: &str) -> bool {
        self.status == TaskStatus::InProgress
            && self.owner.as_deref() == Some(owner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_status_serialization() {
        let status = TaskStatus::Pending;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, "\"pending\"");

        let deserialized: TaskStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, TaskStatus::Pending);
    }

    #[test]
    fn test_task_serialization() {
        let task = Task {
            id: "task_12345678".to_string(),
            subject: "Test task".to_string(),
            description: "A test task".to_string(),
            status: TaskStatus::Pending,
            owner: None,
            blocked_by: vec!["task_87654321".to_string()],
        };

        let json = serde_json::to_string(&task).unwrap();
        assert!(json.contains("\"id\":\"task_12345678\""));
        assert!(json.contains("\"status\":\"pending\""));
        assert!(json.contains("\"blocked_by\""));
    }

    #[test]
    fn test_can_claim() {
        let task = Task {
            id: "task_12345678".to_string(),
            subject: "Test".to_string(),
            description: "".to_string(),
            status: TaskStatus::Pending,
            owner: None,
            blocked_by: vec![],
        };
        assert!(task.can_claim(&[]));

        let task_with_deps = Task {
            id: "task_12345678".to_string(),
            subject: "Test".to_string(),
            description: "".to_string(),
            status: TaskStatus::Pending,
            owner: None,
            blocked_by: vec!["task_other".to_string()],
        };
        assert!(!task_with_deps.can_claim(&["task_other".to_string()]));
    }

    #[test]
    fn test_can_complete() {
        let task = Task {
            id: "task_12345678".to_string(),
            subject: "Test".to_string(),
            description: "".to_string(),
            status: TaskStatus::InProgress,
            owner: Some("agent".to_string()),
            blocked_by: vec![],
        };
        assert!(task.can_complete("agent"));
        assert!(!task.can_complete("other"));
    }
}
```

- [ ] **Step 2: Create mod.rs to export task module**

```rust
/*
mod.rs - Task System module

Exports Task and TaskStatus for use by other modules.
*/

pub mod task;

pub use task::{Task, TaskStatus};
```

- [ ] **Step 3: Run tests**

Run: `cd rust-agent && cargo test task_system::task`
Expected: All tests pass (4 tests)

- [ ] **Step 4: Commit**

```bash
git add rust-agent/src/task_system/
git commit -m "feat(s10): add Task data structure with tests"
```

---

### Task 3: Create TaskStore Error Types

**Files:**
- Modify: `rust-agent/src/task_system/mod.rs`
- Create: `rust-agent/src/task_system/store.rs`

**Interfaces:**
- Produces: `TaskStoreError` enum with variants: `InvalidId(String), NotFound(String), EscapesWorkspace, Io(std::io::Error), Json(serde_json::Error), InvalidStatus(String)`
- Produces: `TaskStoreError::Display` implementation for user-friendly error messages

- [ ] **Step 1: Create store.rs with error types**

```rust
/*
store.rs - TaskStore for file persistence

Manages .tasks/ directory, file I/O, and task persistence.
*/

use std::path::PathBuf;
use std::env;
use thiserror::Error;

/// TaskStore error types
#[derive(Error, Debug)]
pub enum TaskStoreError {
    #[error("Invalid task ID: {0}")]
    InvalidId(String),

    #[error("Task not found: {0}")]
    NotFound(String),

    #[error("Task store escapes workspace")]
    EscapesWorkspace,

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Invalid task status: {0}")]
    InvalidStatus(String),
}
```

- [ ] **Step 2: Update mod.rs to export store**

```rust
pub mod task;
pub mod store;

pub use task::{Task, TaskStatus};
pub use store::{TaskStore, TaskStoreError};
```

- [ ] **Step 3: Verify compiles**

Run: `cd rust-agent && cargo check`
Expected: No errors (though TaskStore not yet implemented)

- [ ] **Step 4: Commit**

```bash
git add rust-agent/src/task_system/
git commit -m "feat(s10): add TaskStoreError types"
```

---

### Task 4: Implement TaskStore Core Methods

**Files:**
- Modify: `rust-agent/src/task_system/store.rs`

**Interfaces:**
- Consumes: `TaskStoreError` from Task 3
- Produces: `TaskStore::new(directory: PathBuf) -> Result<Self, TaskStoreError>`
- Produces: `TaskStore::task_path(&self, task_id: &str) -> Result<PathBuf, TaskStoreError>`
- Produces: `TaskStore::exists(&self, task_id: &str) -> bool`

- [ ] **Step 1: Write failing tests for TaskStore initialization**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_store(dir: &Path) -> TaskStore {
        TaskStore::new(dir.to_path_buf()).unwrap()
    }

    #[test]
    fn test_new_validates_workspace() {
        let workdir = env::current_dir().unwrap().canonicalize().unwrap();
        let store = TaskStore::new(workdir.clone());
        assert!(store.is_ok());

        // Try to use outside workspace
        let outside = PathBuf::from("/etc");
        let store = TaskStore::new(outside);
        assert!(matches!(store, Err(TaskStoreError::EscapesWorkspace)));
    }

    #[test]
    fn test_task_path_validates_id_format() {
        let tmp = TempDir::new().unwrap();
        let store = create_test_store(tmp.path());

        // Valid ID
        let path = store.task_path("task_12345678");
        assert!(path.is_ok());

        // Invalid IDs
        assert!(matches!(
            store.task_path("invalid"),
            Err(TaskStoreError::InvalidId(_))
        ));
        assert!(matches!(
            store.task_path("task_123"),
            Err(TaskStoreError::InvalidId(_))
        ));
        assert!(matches!(
            store.task_path("task_123456789"),
            Err(TaskStoreError::InvalidId(_))
        ));
    }

    #[test]
    fn test_exists_returns_false_for_nonexistent() {
        let tmp = TempDir::new().unwrap();
        let store = create_test_store(tmp.path());
        assert!(!store.exists("task_12345678"));
    }
}
```

- [ ] **Step 2: Run test to verify failures**

Run: `cd rust-agent && cargo test test_new_validates_workspace`
Expected: FAIL - TaskStore::new not yet implemented

- [ ] **Step 3: Implement TaskStore::new and task_path**

```rust
use regex::Regex;

pub struct TaskStore {
    directory: PathBuf,
    id_pattern: Regex,
}

impl TaskStore {
    const TASK_ID_PREFIX: &str = "task_";
    const MAX_ID_RETRIES: usize = 100;

    pub fn new(directory: PathBuf) -> Result<Self, TaskStoreError> {
        let directory = directory.canonicalize()
            .map_err(|_| TaskStoreError::EscapesWorkspace)?;

        let workdir = env::current_dir()
            .map_err(|_| TaskStoreError::EscapesWorkspace)?
            .canonicalize()
            .map_err(|_| TaskStoreError::EscapesWorkspace)?;

        if !directory.starts_with(&workdir) {
            return Err(TaskStoreError::EscapesWorkspace);
        }

        Ok(Self {
            directory,
            id_pattern: Regex::new(r"^task_[0-9a-f]{8}$")
                .map_err(|_| TaskStoreError::InvalidId("regex".into()))?,
        })
    }

    fn task_path(&self, task_id: &str) -> Result<PathBuf, TaskStoreError> {
        if !self.id_pattern.is_match(task_id) {
            return Err(TaskStoreError::InvalidId(task_id.to_string()));
        }

        let path = self.directory.join(format!("{}.json", task_id));
        let resolved = path.canonicalize().ok();

        if let Some(resolved) = resolved {
            if !resolved.starts_with(&self.directory) {
                return Err(TaskStoreError::InvalidId(task_id.to_string()));
            }
        }

        Ok(path)
    }

    pub fn exists(&self, task_id: &str) -> bool {
        self.task_path(task_id)
            .map(|p| p.exists())
            .unwrap_or(false)
    }
}
```

- [ ] **Step 4: Add tempfile dependency to Cargo.toml**

```toml
[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 5: Run tests**

Run: `cd rust-agent && cargo test task_system::store::tests`
Expected: All tests pass (4 tests)

- [ ] **Step 6: Commit**

```bash
git add rust-agent/Cargo.toml rust-agent/src/task_system/
git commit -m "feat(s10): implement TaskStore core methods"
```

---

### Task 5: Implement TaskStore::create

**Files:**
- Modify: `rust-agent/src/task_system/store.rs`

**Interfaces:**
- Consumes: `Task` from Task 2, `TaskStoreError` from Task 3
- Produces: `TaskStore::create(&self, subject: String, description: String, blocked_by: Vec<String>) -> Result<Task, TaskStoreError>`

- [ ] **Step 1: Write failing tests for create**

```rust
#[test]
fn test_create_creates_task_file() {
    let tmp = TempDir::new().unwrap();
    let store = create_test_store(tmp.path());

    let task = store.create(
        "Test task".to_string(),
        "Test description".to_string(),
        vec![],
    ).unwrap();

    assert!(task.id.starts_with("task_"));
    assert_eq!(task.subject, "Test task");
    assert_eq!(task.status, TaskStatus::Pending);
    assert!(task.owner.is_none());
    assert!(task.blocked_by.is_empty());

    // Verify file exists
    assert!(store.exists(&task.id));
}

#[test]
fn test_create_rejects_empty_subject() {
    let tmp = TempDir::new().unwrap();
    let store = create_test_store(tmp.path());

    let result = store.create(
        "".to_string(),
        "".to_string(),
        vec![],
    );
    assert!(matches!(result, Err(TaskStoreError::InvalidId(_))));
}

#[test]
fn test_create_validates_dependencies_exist() {
    let tmp = TempDir::new().unwrap();
    let store = create_test_store(tmp.path());

    let result = store.create(
        "Dependent task".to_string(),
        "".to_string(),
        vec!["task_nonexistent".to_string()],
    );
    assert!(matches!(result, Err(TaskStoreError::NotFound(_))));
}

#[test]
fn test_create_deduplicates_dependencies() {
    let tmp = TempDir::new().unwrap();
    let store = create_test_store(tmp.path());

    let dep = store.create("Dependency".to_string(), "".to_string(), vec![]).unwrap();

    let task = store.create(
        "Task".to_string(),
        "".to_string(),
        vec![dep.id.clone(), dep.id.clone()],
    ).unwrap();

    assert_eq!(task.blocked_by.len(), 1);
    assert_eq!(task.blocked_by[0], dep.id);
}
```

- [ ] **Step 2: Run tests to verify failures**

Run: `cd rust-agent && cargo test test_create`
Expected: FAIL - TaskStore::create not yet implemented

- [ ] **Step 3: Implement TaskStore::create**

```rust
use crate::task_system::task::{Task, TaskStatus};
use fastrand;

impl TaskStore {
    pub fn create(
        &self,
        subject: String,
        description: String,
        blocked_by: Vec<String>,
    ) -> Result<Task, TaskStoreError> {
        let subject = subject.trim().to_string();
        if subject.is_empty() {
            return Err(TaskStoreError::InvalidId("empty subject".into()));
        }

        // 去重依赖列表
        let mut unique_deps = Vec::new();
        for dep in &blocked_by {
            if !unique_deps.contains(dep) {
                unique_deps.push(dep.clone());
            }
        }

        // 验证依赖存在
        for dep in &unique_deps {
            if !self.exists(dep) {
                return Err(TaskStoreError::NotFound(dep.clone()));
            }
        }

        // 创建目录
        std::fs::create_dir_all(&self.directory)?;

        // 生成唯一 ID（最多重试 100 次）
        for _ in 0..Self::MAX_ID_RETRIES {
            let id = format!("task_{:08x}", fastrand::u32(..));
            let path = self.task_path(&id)?;

            // 原子写入：使用 create_new 避免覆盖
            match std::fs::File::options()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_file) => {
                    let task = Task {
                        id: id.clone(),
                        subject: subject.clone(),
                        description: description.clone(),
                        status: TaskStatus::Pending,
                        owner: None,
                        blocked_by: unique_deps.clone(),
                    };

                    let content = serde_json::to_string_pretty(&task)?;
                    std::fs::write(&path, content)?;
                    return Ok(task);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    continue;
                }
                Err(e) => return Err(TaskStoreError::Io(e)),
            }
        }

        Err(TaskStoreError::InvalidId("failed to allocate unique ID".into()))
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cd rust-agent && cargo test test_create`
Expected: All tests pass (4 tests)

- [ ] **Step 5: Commit**

```bash
git add rust-agent/src/task_system/
git commit -m "feat(s10): implement TaskStore::create"
```

---

### Task 6: Implement TaskStore::load and save

**Files:**
- Modify: `rust-agent/src/task_system/store.rs`

**Interfaces:**
- Consumes: `Task` from Task 2
- Produces: `TaskStore::load(&self, task_id: &str) -> Result<Task, TaskStoreError>`
- Produces: `TaskStore::save(&self, task: &Task) -> Result<(), TaskStoreError>`

- [ ] **Step 1: Write failing tests for load and save**

```rust
#[test]
fn test_load_retrieves_task() {
    let tmp = TempDir::new().unwrap();
    let store = create_test_store(tmp.path());

    let created = store.create(
        "Original".to_string(),
        "Description".to_string(),
        vec![],
    ).unwrap();

    let loaded = store.load(&created.id).unwrap();
    assert_eq!(loaded.id, created.id);
    assert_eq!(loaded.subject, "Original");
}

#[test]
fn test_load_validates_id_match() {
    let tmp = TempDir::new().unwrap();
    let store = create_test_store(tmp.path());

    let task = store.create("Test".to_string(), "".to_string(), vec![]).unwrap();

    // Corrupt the file ID
    let path = store.task_path(&task.id).unwrap();
    let mut data = std::fs::read_to_string(&path).unwrap();
    data = data.replace(&task.id, "task_wrongid");
    std::fs::write(&path, data).unwrap();

    let result = store.load(&task.id);
    assert!(matches!(result, Err(TaskStoreError::InvalidId(_))));
}

#[test]
fn test_save_persists_changes() {
    let tmp = TempDir::new().unwrap();
    let store = create_test_store(tmp.path());

    let mut task = store.create("Test".to_string(), "".to_string(), vec![]).unwrap();
    task.status = TaskStatus::Completed;
    task.owner = Some("agent".to_string());

    store.save(&task).unwrap();

    let loaded = store.load(&task.id).unwrap();
    assert_eq!(loaded.status, TaskStatus::Completed);
    assert_eq!(loaded.owner, Some("agent".to_string()));
}
```

- [ ] **Step 2: Run tests to verify failures**

Run: `cd rust-agent && cargo test test_load`
Expected: FAIL - TaskStore::load not yet implemented

- [ ] **Step 3: Implement TaskStore::load and save**

```rust
impl TaskStore {
    pub fn load(&self, task_id: &str) -> Result<Task, TaskStoreError> {
        let path = self.task_path(task_id)?;
        let content = std::fs::read_to_string(&path)?;
        let task: Task = serde_json::from_str(&content)?;

        // 验证 ID 匹配
        if task.id != task_id {
            return Err(TaskStoreError::InvalidId(format!(
                "ID mismatch: file={}, loaded={}", task_id, task.id
            )));
        }

        Ok(task)
    }

    pub fn save(&self, task: &Task) -> Result<(), TaskStoreError> {
        let path = self.task_path(&task.id)?;
        let content = serde_json::to_string_pretty(task)?;
        std::fs::write(&path, content)?;
        Ok(())
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cd rust-agent && cargo test test_load test_save`
Expected: All tests pass (3 tests)

- [ ] **Step 5: Commit**

```bash
git add rust-agent/src/task_system/
git commit -m "feat(s10): implement TaskStore::load and save"
```

---

### Task 7: Implement TaskStore::list

**Files:**
- Modify: `rust-agent/src/task_system/store.rs`

**Interfaces:**
- Produces: `TaskStore::list(&self) -> Result<Vec<Task>, TaskStoreError>`

- [ ] **Step 1: Write failing tests for list**

```rust
#[test]
fn test_list_returns_empty_for_no_tasks() {
    let tmp = TempDir::new().unwrap();
    let store = create_test_store(tmp.path());

    let tasks = store.list().unwrap();
    assert!(tasks.is_empty());
}

#[test]
fn test_list_returns_all_tasks_sorted() {
    let tmp = TempDir::new().unwrap();
    let store = create_test_store(tmp.path());

    let t1 = store.create("First".to_string(), "".to_string(), vec![]).unwrap();
    let t2 = store.create("Second".to_string(), "".to_string(), vec![]).unwrap();
    let t3 = store.create("Third".to_string(), "".to_string(), vec![]).unwrap();

    let tasks = store.list().unwrap();
    assert_eq!(tasks.len(), 3);
    assert_eq!(tasks[0].id, t1.id);
    assert_eq!(tasks[1].id, t2.id);
    assert_eq!(tasks[2].id, t3.id);
}

#[test]
fn test_list_skips_corrupted_files() {
    let tmp = TempDir::new().unwrap();
    let store = create_test_store(tmp.path());

    let valid = store.create("Valid".to_string(), "".to_string(), vec![]).unwrap();

    // Create corrupted file
    let corrupted_path = store.directory.join("task_deadbeef.json");
    std::fs::write(&corrupted_path, "invalid json").unwrap();

    let tasks = store.list().unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, valid.id);
}
```

- [ ] **Step 2: Run tests to verify failures**

Run: `cd rust-agent && cargo test test_list`
Expected: FAIL - TaskStore::list not yet implemented

- [ ] **Step 3: Implement TaskStore::list**

```rust
impl TaskStore {
    pub fn list(&self) -> Result<Vec<Task>, TaskStoreError> {
        if !self.directory.exists() {
            return Ok(Vec::new());
        }

        let mut tasks = Vec::new();
        for entry in std::fs::read_dir(&self.directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            if self.id_pattern.is_match(&name_str) {
                let task_id = name_str.trim_end_matches(".json");
                match self.load(task_id) {
                    Ok(task) => tasks.push(task),
                    Err(_) => continue, // 跳过损坏的任务
                }
            }
        }

        tasks.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(tasks)
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cd rust-agent && cargo test test_list`
Expected: All tests pass (3 tests)

- [ ] **Step 5: Commit**

```bash
git add rust-agent/src/task_system/
git commit -m "feat(s10): implement TaskStore::list"
```

---

### Task 8: Implement Tool Infrastructure

**Files:**
- Create: `rust-agent/src/task_system/tools.rs`
- Modify: `rust-agent/src/task_system/mod.rs`

**Interfaces:**
- Consumes: `TaskStore`, `Task`, `TaskStatus` from previous tasks
- Produces: `init_task_store() -> Result<(), TaskStoreError>`
- Produces: `get_store() -> Arc<TaskStore>`
- Produces: `error_to_output(e: TaskStoreError) -> String`

- [ ] **Step 1: Write tools.rs with global store and helper functions**

```rust
/*
tools.rs - Tool implementations for s10 Task System

Implements create_task, list_tasks, get_task, claim_task, complete_task tools.
*/

use crate::task_system::store::{TaskStore, TaskStoreError};
use crate::task_system::task::{Task, TaskStatus};
use crate::tools::trait_def::{PermissionCheck, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

/// 全局任务存储（使用 Arc 共享）
static TASK_STORE: std::sync::OnceLock<Arc<TaskStore>> = std::sync::OnceLock::new();

/// 初始化任务存储
pub fn init_task_store() -> Result<(), TaskStoreError> {
    let workdir = std::env::current_dir()
        .map_err(|_| TaskStoreError::EscapesWorkspace)?;
    let tasks_dir = workdir.join(".tasks");
    let store = TaskStore::new(tasks_dir)?;
    TASK_STORE.get_or_init(|| Arc::new(store));
    Ok(())
}

/// 获取任务存储
fn get_store() -> Arc<TaskStore> {
    TASK_STORE.get()
        .expect("TaskStore not initialized. Call init_task_store() first.")
        .clone()
}

/// 错误转换为工具输出
fn error_to_output(e: TaskStoreError) -> String {
    format!("Error: {}", e)
}
```

- [ ] **Step 2: Update mod.rs to export tools**

```rust
pub mod task;
pub mod store;
pub mod tools;

pub use task::{Task, TaskStatus};
pub use store::{TaskStore, TaskStoreError};
pub use tools::{
    init_task_store,
    CreateTaskTool,
    ListTasksTool,
    GetTaskTool,
    ClaimTaskTool,
    CompleteTaskTool,
};
```

- [ ] **Step 3: Verify compiles**

Run: `cd rust-agent && cargo check`
Expected: No errors (tool structs not yet exported)

- [ ] **Step 4: Commit**

```bash
git add rust-agent/src/task_system/
git commit -m "feat(s10): add tool infrastructure with global store"
```

---

### Task 9: Implement create_task Tool

**Files:**
- Modify: `rust-agent/src/task_system/tools.rs`

**Interfaces:**
- Produces: `CreateTaskTool` struct implementing `Tool` trait
- Tool name: "create_task"
- Input: `{ subject: string, description?: string, blockedBy?: string[] }`
- Output: "Created {id}: {subject} (blockedBy: ...)" or error

- [ ] **Step 1: Write failing tests for create_task**

```rust
#[cfg(test)]
mod tool_tests {
    use super::*;

    #[test]
    fn test_create_tool_name_and_description() {
        let tool = CreateTaskTool;
        assert_eq!(tool.name(), "create_task");
        assert!(tool.description().contains("Create a task"));
    }

    #[test]
    fn test_create_tool_schema() {
        let tool = CreateTaskTool;
        let schema = tool.input_schema();

        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["subject"]["type"], "string");
        assert_eq!(schema["properties"]["description"]["type"], "string");
        assert_eq!(schema["properties"]["blockedBy"]["type"], "array");

        let required = schema["required"].as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "subject");
    }

    #[test]
    fn test_create_tool_passes_permission() {
        let tool = CreateTaskTool;
        let input = json!({"subject": "test"});
        let check = tool.check_permission(&input);
        assert!(matches!(check, PermissionCheck::Pass));
    }
}
```

- [ ] **Step 2: Run tests to verify failures**

Run: `cd rust-agent && cargo test test_create_tool`
Expected: FAIL - CreateTaskTool not yet implemented

- [ ] **Step 3: Implement CreateTaskTool**

```rust
pub struct CreateTaskTool;

#[async_trait]
impl Tool for CreateTaskTool {
    fn name(&self) -> &str {
        "create_task"
    }

    fn description(&self) -> &str {
        "Create a task with optional dependencies. The task is stored in .tasks/{id}.json"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "subject": {
                    "type": "string",
                    "description": "Brief title for the task"
                },
                "description": {
                    "type": "string",
                    "description": "Detailed description of the task"
                },
                "blockedBy": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "List of task IDs this task depends on"
                }
            },
            "required": ["subject"]
        })
    }

    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        let subject = input.get("subject")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let description = input.get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let blocked_by = input.get("blockedBy")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect())
            .unwrap_or_default();

        let store = get_store();
        match store.create(
            subject.to_string(),
            description.to_string(),
            blocked_by,
        ) {
            Ok(task) => {
                let deps_str = if task.blocked_by.is_empty() {
                    String::new()
                } else {
                    format!(" (blockedBy: {})", task.blocked_by.join(", "))
                };
                format!("Created {}: {}{}", task.id, task.subject, deps_str)
            }
            Err(e) => error_to_output(e),
        }
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cd rust-agent && cargo test test_create_tool`
Expected: All tests pass (3 tests)

- [ ] **Step 5: Commit**

```bash
git add rust-agent/src/task_system/
git commit -m "feat(s10): implement create_task tool"
```

---

### Task 10: Implement list_tasks Tool

**Files:**
- Modify: `rust-agent/src/task_system/tools.rs`

**Interfaces:**
- Produces: `ListTasksTool` struct implementing `Tool` trait
- Tool name: "list_tasks"
- Input: `{}`
- Output: Formatted list of tasks or "No tasks..." message

- [ ] **Step 1: Write failing tests for list_tasks**

```rust
#[test]
fn test_list_tool_name_and_schema() {
    let tool = ListTasksTool;
    assert_eq!(tool.name(), "list_tasks");
    assert_eq!(tool.input_schema()["type"], "object");
}

#[test]
fn test_list_tool_passes_permission() {
    let tool = ListTasksTool;
    let check = tool.check_permission(&json!({}));
    assert!(matches!(check, PermissionCheck::Pass));
}
```

- [ ] **Step 2: Run tests to verify failures**

Run: `cd rust-agent && cargo test test_list_tool`
Expected: FAIL - ListTasksTool not yet implemented

- [ ] **Step 3: Implement ListTasksTool**

```rust
pub struct ListTasksTool;

#[async_trait]
impl Tool for ListTasksTool {
    fn name(&self) -> &str {
        "list_tasks"
    }

    fn description(&self) -> &str {
        "List all tasks with their status, owner, and dependencies"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {}
        })
    }

    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, _input: &Value) -> String {
        let store = get_store();
        match store.list() {
            Ok(tasks) => {
                if tasks.is_empty() {
                    return "No tasks. Use create_task to add some.".to_string();
                }

                let mut lines = Vec::new();
                for task in tasks {
                    let marker = match task.status {
                        TaskStatus::Pending => "[ ]",
                        TaskStatus::InProgress => "[>]",
                        TaskStatus::Completed => "[x]",
                    };
                    let deps_str = if task.blocked_by.is_empty() {
                        String::new()
                    } else {
                        format!(" (blockedBy: {})", task.blocked_by.join(", "))
                    };
                    let owner_str = task.owner.as_ref().map_or(String::new(), |o| format!(" [{}]", o));

                    lines.push(format!(
                        "{} {}: {} [{}]{}{}",
                        marker,
                        task.id,
                        task.subject,
                        serde_json::to_string(&task.status).unwrap(),
                        owner_str,
                        deps_str
                    ));
                }
                lines.join("\n")
            }
            Err(e) => error_to_output(e),
        }
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cd rust-agent && cargo test test_list_tool`
Expected: All tests pass (2 tests)

- [ ] **Step 5: Commit**

```bash
git add rust-agent/src/task_system/
git commit -m "feat(s10): implement list_tasks tool"
```

---

### Task 11: Implement get_task Tool

**Files:**
- Modify: `rust-agent/src/task_system/tools.rs`

**Interfaces:**
- Produces: `GetTaskTool` struct implementing `Tool` trait
- Tool name: "get_task"
- Input: `{ task_id: string }`
- Output: JSON task details or error

- [ ] **Step 1: Write failing tests for get_task**

```rust
#[test]
fn test_get_tool_name_and_schema() {
    let tool = GetTaskTool;
    assert_eq!(tool.name(), "get_task");
    assert_eq!(tool.input_schema()["required"].as_array().unwrap()[0], "task_id");
}
```

- [ ] **Step 2: Run tests to verify failures**

Run: `cd rust-agent && cargo test test_get_tool`
Expected: FAIL - GetTaskTool not yet implemented

- [ ] **Step 3: Implement GetTaskTool**

```rust
pub struct GetTaskTool;

#[async_trait]
impl Tool for GetTaskTool {
    fn name(&self) -> &str {
        "get_task"
    }

    fn description(&self) -> &str {
        "Get a task by ID, returning full task details"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "The task ID to retrieve"
                }
            },
            "required": ["task_id"]
        })
    }

    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        let task_id = input.get("task_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let store = get_store();
        match store.load(task_id) {
            Ok(task) => {
                serde_json::to_string_pretty(&task)
                    .unwrap_or_else(|_| "Error: serialization failed".to_string())
            }
            Err(e) => error_to_output(e),
        }
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cd rust-agent && cargo test test_get_tool`
Expected: All tests pass (1 test)

- [ ] **Step 5: Commit**

```bash
git add rust-agent/src/task_system/
git commit -m "feat(s10): implement get_task tool"
```

---

### Task 12: Implement incomplete_dependencies Helper

**Files:**
- Modify: `rust-agent/src/task_system/tools.rs`

**Interfaces:**
- Produces: `incomplete_dependencies(store: &TaskStore, task: &Task) -> Vec<String>`

- [ ] **Step 1: Write failing tests for incomplete_dependencies**

```rust
#[test]
fn test_incomplete_dependencies_with_no_deps() {
    let tmp = TempDir::new().unwrap();
    let store = TaskStore::new(tmp.path().to_path_buf()).unwrap();
    let task = Task {
        id: "task_12345678".to_string(),
        subject: "Test".to_string(),
        description: "".to_string(),
        status: TaskStatus::Pending,
        owner: None,
        blocked_by: vec![],
    };

    let incomplete = incomplete_dependencies(&store, &task);
    assert!(incomplete.is_empty());
}

#[test]
fn test_incomplete_dependencies_with_completed_deps() {
    let tmp = TempDir::new().unwrap();
    let store = TaskStore::new(tmp.path().to_path_buf()).unwrap();
    let dep = store.create("Dependency".to_string(), "".to_string(), vec![]).unwrap();

    // Complete the dependency
    let mut dep_completed = dep.clone();
    dep_completed.status = TaskStatus::Completed;
    store.save(&dep_completed).unwrap();

    let task = Task {
        id: "task_12345678".to_string(),
        subject: "Test".to_string(),
        description: "".to_string(),
        status: TaskStatus::Pending,
        owner: None,
        blocked_by: vec![dep.id.clone()],
    };

    let incomplete = incomplete_dependencies(&store, &task);
    assert!(incomplete.is_empty());
}

#[test]
fn test_incomplete_dependencies_with_pending_deps() {
    let tmp = TempDir::new().unwrap();
    let store = TaskStore::new(tmp.path().to_path_buf()).unwrap();
    let dep = store.create("Dependency".to_string(), "".to_string(), vec![]).unwrap();

    let task = Task {
        id: "task_12345678".to_string(),
        subject: "Test".to_string(),
        description: "".to_string(),
        status: TaskStatus::Pending,
        owner: None,
        blocked_by: vec![dep.id.clone()],
    };

    let incomplete = incomplete_dependencies(&store, &task);
    assert_eq!(incomplete.len(), 1);
    assert_eq!(incomplete[0], dep.id);
}

#[test]
fn test_incomplete_dependencies_with_missing_deps() {
    let tmp = TempDir::new().unwrap();
    let store = TaskStore::new(tmp.path().to_path_buf()).unwrap();

    let task = Task {
        id: "task_12345678".to_string(),
        subject: "Test".to_string(),
        description: "".to_string(),
        status: TaskStatus::Pending,
        owner: None,
        blocked_by: vec!["task_missing".to_string()],
    };

    let incomplete = incomplete_dependencies(&store, &task);
    assert_eq!(incomplete.len(), 1);
    assert_eq!(incomplete[0], "task_missing");
}
```

- [ ] **Step 2: Run tests to verify failures**

Run: `cd rust-agent && cargo test test_incomplete_dependencies`
Expected: FAIL - incomplete_dependencies not yet implemented

- [ ] **Step 3: Implement incomplete_dependencies**

```rust
/// 辅助函数：获取未完成的依赖列表
fn incomplete_dependencies(store: &TaskStore, task: &Task) -> Vec<String> {
    let mut incomplete = Vec::new();
    for dep_id in &task.blocked_by {
        match store.load(dep_id) {
            Ok(dep_task) => {
                if dep_task.status != TaskStatus::Completed {
                    incomplete.push(dep_id.clone());
                }
            }
            Err(_) => {
                // 依赖任务不存在也算未完成
                incomplete.push(dep_id.clone());
            }
        }
    }
    incomplete
}
```

- [ ] **Step 4: Run tests**

Run: `cd rust-agent && cargo test test_incomplete_dependencies`
Expected: All tests pass (4 tests)

- [ ] **Step 5: Commit**

```bash
git add rust-agent/src/task_system/
git commit -m "feat(s10): implement incomplete_dependencies helper"
```

---

### Task 13: Implement claim_task Tool

**Files:**
- Modify: `rust-agent/src/task_system/tools.rs`

**Interfaces:**
- Produces: `ClaimTaskTool` struct implementing `Tool` trait
- Tool name: "claim_task"
- Input: `{ task_id: string }`
- Output: "Claimed {id} ({subject})" or error with blocked message

- [ ] **Step 1: Write failing tests for claim_task**

```rust
#[test]
fn test_claim_tool_name_and_schema() {
    let tool = ClaimTaskTool;
    assert_eq!(tool.name(), "claim_task");
    assert_eq!(tool.input_schema()["required"].as_array().unwrap()[0], "task_id");
}

#[test]
fn test_claim_blocks_in_progress() {
    let tmp = TempDir::new().unwrap();
    let store = TaskStore::new(tmp.path().to_path_buf()).unwrap();
    let mut task = store.create("Test".to_string(), "".to_string(), vec![]).unwrap();
    task.status = TaskStatus::InProgress;
    store.save(&task).unwrap();

    let result = claim_task(&store, &task.id, "agent");
    assert!(result.contains("is in_progress"));
}

#[test]
fn test_claim_blocks_on_dependencies() {
    let tmp = TempDir::new().unwrap();
    let store = TaskStore::new(tmp.path().to_path_buf()).unwrap();
    let dep = store.create("Dependency".to_string(), "".to_string(), vec![]).unwrap();
    let task = store.create("Test".to_string(), "".to_string(), vec![dep.id]).unwrap();

    let result = claim_task(&store, &task.id, "agent");
    assert!(result.contains("Blocked by"));
}
```

- [ ] **Step 2: Run tests to verify failures**

Run: `cd rust-agent && cargo test test_claim`
Expected: FAIL - claim_task function not yet implemented

- [ ] **Step 3: Implement claim_task function and ClaimTaskTool**

```rust
/// 认领任务
pub fn claim_task(store: &TaskStore, task_id: &str, owner: &str) -> String {
    match store.load(task_id) {
        Ok(mut task) => {
            // 检查状态
            if task.status != TaskStatus::Pending {
                return format!("Task {} is {}, cannot claim",
                    task_id,
                    serde_json::to_string(&task.status).unwrap());
            }

            // 检查依赖
            let incomplete = incomplete_dependencies(store, &task);
            if !incomplete.is_empty() {
                return format!("Blocked by: {}", incomplete);
            }

            // 认领任务
            task.status = TaskStatus::InProgress;
            task.owner = Some(owner.to_string());

            if let Err(e) = store.save(&task) {
                return error_to_output(e);
            }

            format!("Claimed {} ({})", task.id, task.subject)
        }
        Err(e) => error_to_output(e),
    }
}

pub struct ClaimTaskTool;

#[async_trait]
impl Tool for ClaimTaskTool {
    fn name(&self) -> &str {
        "claim_task"
    }

    fn description(&self) -> &str {
        "Claim a pending task whose dependencies are complete. Sets owner and status to in_progress"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "The task ID to claim"
                }
            },
            "required": ["task_id"]
        })
    }

    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        let task_id = input.get("task_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let store = get_store();
        claim_task(&store, task_id, "agent")
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cd rust-agent && cargo test test_claim`
Expected: All tests pass (3 tests)

- [ ] **Step 5: Commit**

```bash
git add rust-agent/src/task_system/
git commit -m "feat(s10): implement claim_task tool"
```

---

### Task 14: Implement complete_task Tool

**Files:**
- Modify: `rust-agent/src/task_system/tools.rs`

**Interfaces:**
- Produces: `CompleteTaskTool` struct implementing `Tool` trait
- Tool name: "complete_task"
- Input: `{ task_id: string }`
- Output: "Completed {id} ({subject})\nUnblocked: ..." or error

- [ ] **Step 1: Write failing tests for complete_task**

```rust
#[test]
fn test_complete_tool_name_and_schema() {
    let tool = CompleteTaskTool;
    assert_eq!(tool.name(), "complete_task");
    assert_eq!(tool.input_schema()["required"].as_array().unwrap()[0], "task_id");
}

#[test]
fn test_complete_blocks_wrong_owner() {
    let tmp = TempDir::new().unwrap();
    let store = TaskStore::new(tmp.path().to_path_buf()).unwrap();
    let mut task = store.create("Test".to_string(), "".to_string(), vec![]).unwrap();
    task.status = TaskStatus::InProgress;
    task.owner = Some("owner1".to_string());
    store.save(&task).unwrap();

    let result = complete_task(&store, &task.id, "owner2");
    assert!(result.contains("owned by owner1, not owner2"));
}

#[test]
fn test_complete_unblocks_downstream_tasks() {
    let tmp = TempDir::new().unwrap();
    let store = TaskStore::new(tmp.path().to_path_buf()).unwrap();
    let schema = store.create("Schema".to_string(), "".to_string(), vec![]).unwrap();
    let api = store.create("API".to_string(), "".to_string(), vec![schema.id.clone()]).unwrap();

    // Claim and complete schema
    claim_task(&store, &schema.id, "agent");
    complete_task(&store, &schema.id, "agent");

    // API should now be claimable
    let claim_result = claim_task(&store, &api.id, "agent");
    assert!(claim_result.contains("Claimed"));
}
```

- [ ] **Step 2: Run tests to verify failures**

Run: `cd rust-agent && cargo test test_complete`
Expected: FAIL - complete_task function not yet implemented

- [ ] **Step 3: Implement complete_task function and CompleteTaskTool**

```rust
/// 完成任务
pub fn complete_task(store: &TaskStore, task_id: &str, owner: &str) -> String {
    // 记录完成前可开始的任务
    let ready_before: std::collections::HashSet<String> = store.list()
        .unwrap_or_default()
        .iter()
        .filter(|t| t.status == TaskStatus::Pending && !t.blocked_by.is_empty())
        .filter(|t| incomplete_dependencies(store, t).is_empty())
        .map(|t| t.id.clone())
        .collect();

    match store.load(task_id) {
        Ok(mut task) => {
            // 检查状态
            if task.status != TaskStatus::InProgress {
                return format!("Task {} is {}, cannot complete",
                    task_id,
                    serde_json::to_string(&task.status).unwrap());
            }

            // 检查 owner
            if task.owner.as_deref() != Some(owner) {
                return format!("Task {} is owned by {}, not {}",
                    task_id,
                    task.owner.as_deref().unwrap_or("none"),
                    owner);
            }

            // 完成任务
            task.status = TaskStatus::Completed;

            if let Err(e) = store.save(&task) {
                return error_to_output(e);
            }

            // 计算刚解锁的任务
            let unblocked: Vec<String> = store.list()
                .unwrap_or_default()
                .iter()
                .filter(|t| t.status == TaskStatus::Pending && !t.blocked_by.is_empty())
                .filter(|t| !ready_before.contains(&t.id))
                .filter(|t| incomplete_dependencies(store, t).is_empty())
                .map(|t| t.subject.clone())
                .collect();

            let mut msg = format!("Completed {} ({})", task.id, task.subject);
            if !unblocked.is_empty() {
                msg.push_str(&format!("\nUnblocked: {}", unblocked.join(", ")));
            }
            msg
        }
        Err(e) => error_to_output(e),
    }
}

pub struct CompleteTaskTool;

#[async_trait]
impl Tool for CompleteTaskTool {
    fn name(&self) -> &str {
        "complete_task"
    }

    fn description(&self) -> &str {
        "Complete the task claimed by this agent. Returns list of newly unblocked tasks"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "The task ID to complete"
                }
            },
            "required": ["task_id"]
        })
    }

    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        let task_id = input.get("task_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let store = get_store();
        complete_task(&store, task_id, "agent")
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cd rust-agent && cargo test test_complete`
Expected: All tests pass (3 tests)

- [ ] **Step 5: Commit**

```bash
git add rust-agent/src/task_system/
git commit -m "feat(s10): implement complete_task tool"
```

---

### Task 15: Register Module in lib.rs

**Files:**
- Modify: `rust-agent/src/lib.rs`

**Interfaces:**
- Produces: `pub mod task_system;` declaration

- [ ] **Step 1: Add task_system module to lib.rs**

```rust
pub mod builtins;
pub mod client;
pub mod compact;
pub mod error;
pub mod hooks;
pub mod memory;
pub mod output;
pub mod skills;
pub mod subagent;
pub mod task_system;  // 新增
pub mod todo;
pub mod tools;
```

- [ ] **Step 2: Verify compiles**

Run: `cd rust-agent && cargo check`
Expected: No errors

- [ ] **Step 3: Commit**

```bash
git add rust-agent/src/lib.rs
git commit -m "feat(s10): register task_system module in lib.rs"
```

---

### Task 16: Register Tools in Tool Registry

**Files:**
- Modify: `rust-agent/src/tools/mod.rs`

**Interfaces:**
- Produces: Tool registration in `build_registry()`

- [ ] **Step 1: Add tool imports and registration**

```rust
use crate::task_system::{CreateTaskTool, ListTasksTool, GetTaskTool, ClaimTaskTool, CompleteTaskTool};

pub fn build_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();

    registry.register(Box::new(command::CommandTool));
    registry.register(Box::new(read_file::ReadFileTool));
    registry.register(Box::new(write_file::WriteFileTool));
    registry.register(Box::new(edit_file::EditFileTool));
    registry.register(Box::new(glob_tool::GlobTool));
    registry.register(Box::new(load_skill::LoadSkillTool));
    registry.register(Box::new(todo_write::TodoWriteTool));
    registry.register(Box::new(task::TaskTool));

    // 新增任务系统工具
    registry.register(Box::new(CreateTaskTool));
    registry.register(Box::new(ListTasksTool));
    registry.register(Box::new(GetTaskTool));
    registry.register(Box::new(ClaimTaskTool));
    registry.register(Box::new(CompleteTaskTool));

    registry
}
```

- [ ] **Step 2: Verify compiles**

Run: `cd rust-agent && cargo check`
Expected: No errors

- [ ] **Step 3: Commit**

```bash
git add rust-agent/src/tools/mod.rs
git commit -m "feat(s10): register task tools in registry"
```

---

### Task 17: Initialize Task Store in main.rs

**Files:**
- Modify: `rust-agent/src/main.rs`

**Interfaces:**
- Produces: `init_task_store()` call in main function

- [ ] **Step 1: Add task store initialization**

Add before the main loop starts:

```rust
// Initialize task store
if let Err(e) = rust_agent::task_system::init_task_store() {
    eprintln!("Warning: Failed to initialize task store: {}", e);
}
```

- [ ] **Step 2: Verify compiles and runs**

Run: `cd rust-agent && cargo run`
Expected: No errors, "Warning:" may appear if workspace issue (expected in some cases)

- [ ] **Step 3: Commit**

```bash
git add rust-agent/src/main.rs
git commit -m "feat(s10): initialize task store in main.rs"
```

---

### Task 18: Write Integration Tests

**Files:**
- Create: `rust-agent/tests/s10_task_system.rs`

**Interfaces:**
- Produces: Integration tests matching Python s10 test cases

- [ ] **Step 1: Create integration test file**

```rust
/*
s10_task_system.rs - Integration tests for s10 Task System

Tests mirroring Python s10 implementation test cases.
*/

use std::fs;
use tempfile::TempDir;
use rust_agent::task_system::{
    TaskStore, TaskStatus, CreateTaskTool, ListTasksTool, GetTaskTool,
    ClaimTaskTool, CompleteTaskTool, claim_task, complete_task
};
use rust_agent::tools::trait_def::{Tool, ToolContext};

/// Helper to create a test store
fn test_store() -> (TaskStore, TempDir) {
    let tmp = TempDir::new().unwrap();
    let store = TaskStore::new(tmp.path().to_path_buf()).unwrap();
    (store, tmp)
}

/// Mock ToolContext for testing
fn mock_context() -> ToolContext<'static> {
    ToolContext {
        client: &(),
        registry: &(),
        hooks: &(),
    }
}

#[test]
fn test_dependencies_gate_claim_and_completion_checks_owner() {
    let (store, _tmp) = test_store();

    // Create tasks with dependency
    let schema = store.create("create schema".to_string(), "".to_string(), vec![]).unwrap();
    let api = store.create("write API".to_string(), "".to_string(), vec![schema.id.clone()]).unwrap();

    // Can't claim API while schema is incomplete
    assert_eq!(
        claim_task(&store, &api.id, "agent"),
        format!("Blocked by: [\"{}\"]", schema.id)
    );

    // Claim and complete schema
    assert!(claim_task(&store, &schema.id, "agent").contains("Claimed"));
    assert!(complete_task(&store, &schema.id, "agent").contains("Unblocked: write API"));

    // Can now claim API
    assert!(claim_task(&store, &api.id, "agent").contains("Claimed"));

    // Can't complete with wrong owner
    assert!(complete_task(&store, &api.id, "other").contains("owned by agent, not other"));

    // Complete with correct owner
    assert!(complete_task(&store, &api.id, "agent").contains("Completed"));

    // Verify final state
    let loaded = store.load(&api.id).unwrap();
    assert_eq!(loaded.status, TaskStatus::Completed);
}

#[test]
fn test_invalid_and_missing_task_ids_become_tool_results() {
    // Initialize global store
    let tmp = TempDir::new().unwrap();
    std::env::set_current_dir(tmp.path()).unwrap();

    rust_agent::task_system::init_task_store().unwrap();

    // Test get_task with invalid ID
    let tool = GetTaskTool;
    let ctx = mock_context();
    let result = tool.execute(&ctx, &serde_json::json!({"task_id": "../outside"})).await;
    assert!(result.starts_with("Error: Invalid task ID"));

    // Test claim_task with missing ID
    let tool = ClaimTaskTool;
    let result = tool.execute(&ctx, &serde_json::json!({"task_id": "task_00000000"})).await;
    assert!(result.starts_with("Error:"));
}

#[test]
fn test_create_rejects_unknown_dependencies() {
    let (store, _tmp) = test_store();

    let result = store.create(
        "write API".to_string(),
        "".to_string(),
        vec!["task_00000000".to_string()],
    );
    assert!(matches!(result, Err(_)));
}

#[test]
fn test_task_store_rejects_a_symlink_outside_the_workspace() {
    // Create temp directory for outside workspace
    let outside_tmp = TempDir::new().unwrap();

    // Create workdir and symlink .tasks to outside
    let workdir_tmp = TempDir::new().unwrap();
    std::env::set_current_dir(workdir_tmp.path()).unwrap();

    let tasks_link = workdir_tmp.path().join(".tasks");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside_tmp.path(), &tasks_link).unwrap();
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(outside_tmp.path(), &tasks_link).unwrap();
    }

    let result = TaskStore::new(tasks_link);
    assert!(matches!(result, Err(_)));

    // Verify no files created outside
    assert_eq!(outside_tmp.path().read_dir().unwrap().count(), 0);
}

#[test]
fn test_end_to_end_workflow() {
    let (store, _tmp) = test_store();

    // Create tasks
    let t1 = store.create("Task 1".to_string(), "First task".to_string(), vec![]).unwrap();
    let t2 = store.create("Task 2".to_string(), "Second task".to_string(), vec![t1.id.clone()]).unwrap();
    let t3 = store.create("Task 3".to_string(), "Third task".to_string(), vec![t2.id.clone()]).unwrap();

    // List tasks
    let tasks = store.list().unwrap();
    assert_eq!(tasks.len(), 3);

    // Claim T1
    claim_task(&store, &t1.id, "agent");

    // Complete T1 (should unblock T2)
    let complete_result = complete_task(&store, &t1.id, "agent");
    assert!(complete_result.contains("Unblocked: Task 2"));

    // Now can claim T2
    claim_task(&store, &t2.id, "agent");

    // Can't claim T3 yet (T2 not complete)
    let claim_result = claim_task(&store, &t3.id, "agent");
    assert!(claim_result.contains("Blocked by"));
}
```

- [ ] **Step 2: Run integration tests**

Run: `cd rust-agent && cargo test s10_task_system`
Expected: All integration tests pass (5 tests)

- [ ] **Step 3: Commit**

```bash
git add rust-agent/tests/
git commit -m "test(s10): add integration tests"
```

---

## Self-Review Checklist

**Spec coverage:**
- ✅ Task data structure (Task 2)
- ✅ TaskStore with file persistence (Tasks 3-7)
- ✅ Five tools (Tasks 9-14)
- ✅ Dependency blocking (Tasks 12-14)
- ✅ Owner validation (Tasks 13-14)
- ✅ State transitions (Tasks 13-14)
- ✅ Path safety (Task 3, integration test)
- ✅ ID validation (Task 3)
- ✅ Integration tests (Task 18)

**Placeholder scan:** No TBD, TODO, or incomplete steps found.

**Type consistency:**
- `TaskStatus::Pending` used consistently
- `blocked_by: Vec<String>` used consistently
- `claim_task(store, task_id, owner)` signature consistent
- `complete_task(store, task_id, owner)` signature consistent

---

**Plan complete and saved to `docs/superpowers/plans/2026-08-18-s10-task-system.md`. Two execution options:**

**1. Subagent-Driven (recommended)** - I dispatch a fresh subagent per task, review between tasks, fast iteration

**2. Inline Execution** - Execute tasks in this session using executing-plans, batch execution with checkpoints

Which approach?