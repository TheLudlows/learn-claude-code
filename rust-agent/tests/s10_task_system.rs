/*
s10_task_system.rs - Integration tests for s10 Task System

These mirror the Python s10 implementation's test cases against the Rust port.

设计说明（相对计划的修正）：
- 计划的 `mock_context()` 用 `&()` 填充 `ToolContext` 字段，但该结构要求
  `&Client`/`&Hooks`/`&ToolRegistry`，无法编译。这里用真实（但空）实例构造。
- 计划依赖 `init_task_store` + `set_current_dir` 驱动工具结构体的 `execute`，
  但 `init_task_store` 基于 `OnceLock`（首次调用即固化、进程级），叠加
  `set_current_dir` 在并行测试线程间会相互踩踏、产生 flaky。这里改用
  feature `testing` 暴露的 `set_store_for_test` 注入每用例独立的临时存储。
- 计划的符号链接越界测试在 Windows 上需管理员/开发者模式才能建符号链接，
  故拆为：通用路径用普通外部目录验证 `TaskStore::new` 的越界拒绝；
  符号链接变体仅在 unix 下运行。
- 所有存储构造走 `TaskStore::new_for_test`，绕过工作区校验，避免 `set_current_dir`。

运行：cargo test --features testing --offline --test s10_task_system
*/

#![cfg(feature = "testing")]

use rust_agent::task_system::{
    claim_task, clear_store_for_test, complete_task, set_store_for_test, TaskStore, TaskStatus,
};
use rust_agent::client::Client;
use rust_agent::hooks::Hooks;
use rust_agent::tools::registry::ToolRegistry;
use rust_agent::tools::trait_def::ToolContext;
use std::path::PathBuf;
use tempfile::TempDir;

/// 构造一个指向临时目录的 TaskStore（绕过工作区校验）。
fn test_store() -> (TaskStore, TempDir) {
    let tmp = TempDir::new().unwrap();
    let store_dir = tmp.path().join("tasks");
    std::fs::create_dir_all(&store_dir).unwrap();
    let store = TaskStore::new_for_test(store_dir);
    (store, tmp)
}

/// 构造一个真实但空的 ToolContext（任务工具不读取 ctx，空实例即可）。
fn tool_context<'a>(
    client: &'a Client,
    hooks: &'a Hooks,
    registry: &'a ToolRegistry,
) -> ToolContext<'a> {
    ToolContext {
        client,
        hooks,
        registry,
    }
}

/// RAII 守卫：注入测试存储，离开作用域时清除覆盖，避免污染其他用例。
struct StoreGuard;

impl Drop for StoreGuard {
    fn drop(&mut self) {
        clear_store_for_test();
    }
}

/// 注入临时存储并返回守卫。
fn inject_store(store: TaskStore) -> StoreGuard {
    set_store_for_test(store);
    StoreGuard
}

#[test]
fn test_dependencies_gate_claim_and_completion_checks_owner() {
    let (store, _tmp) = test_store();

    // 带依赖的任务
    let schema = store
        .create("create schema".to_string(), "".to_string(), vec![])
        .unwrap();
    let api = store
        .create("write API".to_string(), "".to_string(), vec![schema.id.clone()])
        .unwrap();

    // schema 未完成时不能认领 API
    assert_eq!(
        claim_task(&store, &api.id, "agent"),
        format!("Blocked by: [\"{}\"]", schema.id)
    );

    // 认领并完成 schema（应解锁 API）
    assert!(claim_task(&store, &schema.id, "agent").contains("Claimed"));
    assert!(complete_task(&store, &schema.id, "agent").contains("Unblocked: write API"));

    // 现在可以认领 API
    assert!(claim_task(&store, &api.id, "agent").contains("Claimed"));

    // 错误 owner 不能完成
    assert!(complete_task(&store, &api.id, "other").contains("owned by agent, not other"));

    // 正确 owner 完成
    assert!(complete_task(&store, &api.id, "agent").contains("Completed"));

    // 验证终态
    let loaded = store.load(&api.id).unwrap();
    assert_eq!(loaded.status, TaskStatus::Completed);
}

#[tokio::test]
async fn test_invalid_and_missing_task_ids_become_tool_results() {
    use rust_agent::task_system::{ClaimTaskTool, GetTaskTool};
    use rust_agent::tools::trait_def::Tool;

    let (store, _tmp) = test_store();
    let _guard = inject_store(store);

    let client = Client::new(
        "test-key".to_string(),
        "http://localhost".to_string(),
        "test-model".to_string(),
    );
    let hooks = Hooks::new();
    let registry = ToolRegistry::new();
    let ctx = tool_context(&client, &hooks, &registry);

    // get_task 拒绝非法 ID（路径注入也被 id_pattern 挡掉）
    let tool = GetTaskTool;
    let result = tool
        .execute(&ctx, &serde_json::json!({"task_id": "../outside"}))
        .await;
    assert!(
        result.starts_with("Error: Invalid task ID"),
        "expected invalid-id error, got: {}",
        result
    );

    // claim_task 对不存在的任务返回错误
    let tool = ClaimTaskTool;
    let result = tool
        .execute(&ctx, &serde_json::json!({"task_id": "task_00000000"}))
        .await;
    assert!(
        result.starts_with("Error:"),
        "expected error for missing task, got: {}",
        result
    );
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
fn test_task_store_rejects_a_directory_outside_the_workspace() {
    // TaskStore::new 会把 directory canonicalize 后与 current_dir 比较，
    // 工作区外的目录必须被拒绝。用一个独立的临时目录（不在 cwd 下）验证。
    let outside = TempDir::new().unwrap();
    let outside_tasks = outside.path().join(".tasks");
    std::fs::create_dir_all(&outside_tasks).unwrap();

    let result = TaskStore::new(outside_tasks);
    assert!(
        matches!(result, Err(_)),
        "store pointing outside workspace must be rejected"
    );

    // 确认没有在工作区外创建任何文件
    assert_eq!(outside.path().read_dir().unwrap().count(), 1); // 仅 .tasks 目录本身
}

#[cfg(unix)]
#[test]
fn test_task_store_rejects_a_symlink_outside_the_workspace() {
    use std::os::unix::fs::symlink;

    // 创建 cwd 与一个外部目录，把 cwd/.tasks 软链到外部。
    let workdir = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let tasks_link = workdir.path().join(".tasks");
    symlink(outside.path(), &tasks_link).unwrap();

    // 在 workdir 下运行：new 应检测到 .tasks 解析到工作区外而拒绝。
    let original = std::env::current_dir().unwrap();
    std::env::set_current_dir(workdir.path()).unwrap();
    let result = TaskStore::new(tasks_link);
    std::env::set_current_dir(&original).unwrap();

    assert!(
        matches!(result, Err(_)),
        "store via symlink escaping workspace must be rejected"
    );

    // 外部目录不应被写入任何任务文件
    assert_eq!(outside.path().read_dir().unwrap().count(), 0);
}

#[test]
fn test_end_to_end_workflow() {
    let (store, _tmp) = test_store();

    // 创建任务链：t1 <- t2 <- t3
    let t1 = store
        .create("Task 1".to_string(), "First task".to_string(), vec![])
        .unwrap();
    let t2 = store
        .create("Task 2".to_string(), "Second task".to_string(), vec![t1.id.clone()])
        .unwrap();
    let t3 = store
        .create("Task 3".to_string(), "Third task".to_string(), vec![t2.id.clone()])
        .unwrap();

    // 列表应返回全部三个，按 ID 排序
    let tasks = store.list().unwrap();
    assert_eq!(tasks.len(), 3);

    // 认领 t1
    claim_task(&store, &t1.id, "agent");

    // 完成 t1（应解锁 t2）
    let complete_result = complete_task(&store, &t1.id, "agent");
    assert!(complete_result.contains("Unblocked: Task 2"));

    // 现在可以认领 t2
    claim_task(&store, &t2.id, "agent");

    // t3 还不能认领（t2 未完成）
    let claim_result = claim_task(&store, &t3.id, "agent");
    assert!(claim_result.contains("Blocked by"));

    // 认领状态可读取：t2 应为 InProgress
    let t2_loaded = store.load(&t2.id).unwrap();
    assert_eq!(t2_loaded.status, TaskStatus::InProgress);
    assert_eq!(t2_loaded.owner.as_deref(), Some("agent"));
}

#[test]
fn test_new_for_test_bypasses_workspace_validation() {
    // 直接验证 new_for_test 在任意目录可用（不触发 current_dir 比较）。
    let tmp = TempDir::new().unwrap();
    let dir: PathBuf = tmp.path().join("anywhere").join("tasks");
    std::fs::create_dir_all(&dir).unwrap();
    let store = TaskStore::new_for_test(dir.clone());
    assert_eq!(store.directory(), dir);
}
