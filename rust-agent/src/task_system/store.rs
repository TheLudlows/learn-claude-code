/*
store.rs - TaskStore for file persistence

Manages .tasks/ directory, file I/O, and task persistence.
*/

use thiserror::Error;
use regex::Regex;
use std::path::{Path, PathBuf};
use std::env;

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
        let resolved = path.canonicalize().ok();

        if let Some(resolved) = resolved {
            if !resolved.starts_with(&self.directory) {
                return Err(TaskStoreError::InvalidId(task_id.to_string()));
            }
        }

        Ok(path)
    }

    /// Checks if a task exists
    pub fn exists(&self, task_id: &str) -> bool {
        self.task_path(task_id)
            .map(|p| p.exists())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}