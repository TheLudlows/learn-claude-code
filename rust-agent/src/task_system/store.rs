/*
store.rs - TaskStore for file persistence

Manages .tasks/ directory, file I/O, and task persistence.
*/

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