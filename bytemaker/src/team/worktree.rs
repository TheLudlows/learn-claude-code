use std::path::{Path, PathBuf};
use crate::task_system::task::Task;

/// Phase 1 stub: no worktree resolution. Returns (workdir, None) always.
/// Phase 2 (Task 15) replaces this with real git-worktree resolution.
pub fn task_worktree_cwd(workdir: &Path, task: &Task) -> (PathBuf, Option<String>) {
    let _ = task;
    (workdir.to_path_buf(), None)
}
