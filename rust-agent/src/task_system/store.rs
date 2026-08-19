/*
store.rs - TaskStore for file persistence

Manages .tasks/ directory, file I/O, and task persistence.
*/

use thiserror::Error;
use regex::Regex;
use std::path::PathBuf;
#[cfg(test)]
use std::path::Path;
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

    pub fn list(&self) -> Result<Vec<Task>, TaskStoreError> {
        if !self.directory.exists() {
            return Ok(Vec::new());
        }

        let mut tasks = Vec::new();
        for entry in std::fs::read_dir(&self.directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            if name_str.ends_with(".json") && self.id_pattern.is_match(&name_str.strip_suffix(".json").unwrap()) {
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

/// 测试专用构造器（feature `testing`）：绕过工作区校验直接装配 TaskStore。
///
/// 集成测试（`tests/`）是外部 crate，无法访问 `#[cfg(test)]` 的 `create_test_store`，
/// 因此通过 feature 门控暴露此构造器，供其针对任意临时目录构造存储。
/// 生产构建（未启用 `testing`）不编译此项，不会泄露越界构造能力。
#[cfg(feature = "testing")]
impl TaskStore {
    pub fn new_for_test(directory: PathBuf) -> Self {
        TaskStore {
            directory,
            id_pattern: Regex::new(r"^task_[0-9a-f]{8}$").unwrap(),
        }
    }

    /// 测试专用：返回存储目录，供集成测试断言构造结果。
    pub fn directory(&self) -> &std::path::Path {
        &self.directory
    }
}

/// 测试专用：在给定目录下构造一个绕过工作区校验的 TaskStore。
///
/// `TaskStore::new` 会把 directory canonicalize 后与 `current_dir` 比较以阻止越界，
/// 但单元测试的临时目录不在工作区内，因此提供此构造器直接装配私有字段。
/// `tools` 模块的测试也复用此助手，保证两处行为一致。
#[cfg(test)]
pub(crate) fn create_test_store(dir: &Path) -> TaskStore {
    let store_dir = dir.join("tasks");
    std::fs::create_dir_all(&store_dir).unwrap();
    TaskStore {
        directory: store_dir,
        id_pattern: regex::Regex::new(r"^task_[0-9a-f]{8}$").unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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

        // Collect all IDs
        let ids = vec![t1.id.clone(), t2.id.clone(), t3.id.clone()];
        let mut sorted_ids = ids.clone();
        sorted_ids.sort();

        // Tasks should be sorted by ID
        for (i, task) in tasks.iter().enumerate() {
            assert_eq!(task.id, sorted_ids[i]);
        }
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
}