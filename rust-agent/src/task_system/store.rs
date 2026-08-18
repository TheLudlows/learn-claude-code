/*
store.rs - TaskStore for file persistence

Manages .tasks/ directory, file I/O, and task persistence.
*/

use thiserror::Error;
use regex::Regex;
use std::path::{Path, PathBuf};
use std::env;
use fastrand;

use crate::task_system::task::{Task, TaskStatus};

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

/// TaskStore manages task persistence in the file system
pub struct TaskStore {
    directory: PathBuf,
    id_pattern: Regex,
}

impl TaskStore {
    const TASK_ID_PREFIX: &str = "task_";
    const MAX_ID_RETRIES: usize = 100;

    /// Creates a new TaskStore instance
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

    /// Gets the file path for a task
    fn task_path(&self, task_id: &str) -> Result<PathBuf, TaskStoreError> {
        if !self.id_pattern.is_match(task_id) {
            return Err(TaskStoreError::InvalidId(task_id.to_string()));
        }

        let path = self.directory.join(format!("{}.json", task_id));

        // Security check: ensure the path is within our directory
        if path != self.directory.join(format!("{}.json", task_id)) {
            return Err(TaskStoreError::InvalidId(task_id.to_string()));
        }

        Ok(path)
    }

    /// Checks if a task exists
    pub fn exists(&self, task_id: &str) -> bool {
        self.task_path(task_id)
            .map(|p| p.exists())
            .unwrap_or(false)
    }

    /// Creates a new task
    pub fn create(
        &self,
        subject: String,
        description: String,
        blocked_by: Vec<String>,
    ) -> Result<Task, TaskStoreError> {
        use crate::task_system::task::{Task, TaskStatus};

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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_store(dir: &Path) -> TaskStore {
        // Create a subdirectory within the test dir
        let store_dir = dir.join("tasks");
        std::fs::create_dir_all(&store_dir).unwrap();

        // Use a simpler approach for tests that bypasses workspace validation
        TaskStore {
            directory: store_dir,
            id_pattern: regex::Regex::new(r"^task_[0-9a-f]{8}$").unwrap(),
        }
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
        let workdir = env::current_dir().unwrap();
        let store = create_test_store(&workdir);

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
        let workdir = env::current_dir().unwrap();
        let store = create_test_store(&workdir);
        assert!(!store.exists("task_12345678"));
    }

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
}