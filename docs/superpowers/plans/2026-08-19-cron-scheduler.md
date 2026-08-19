# s12 Cron Scheduler Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 实现一个 cron 调度器，使用 5 字段 cron 表达式在指定时间将 prompt 注入到 agent 循环。

**Architecture:** 单个 tokio interval 每 1 秒轮询到期任务并入队，agent 循环顶部消费队列并注入 `[Scheduled]` 消息。使用 `Arc<Mutex<>>` 保护共享状态，durable 任务持久化到 `.scheduled_tasks.json`。

**Tech Stack:** Rust, tokio, serde, chrono（时间处理）

## Global Constraints

- 使用 `Result<T, String>` 进行错误处理
- 工具命名：`schedule_cron`、`list_crons`、`cancel_cron`
- 持久化文件：`.scheduled_tasks.json`
- Cron 任务 ID 格式：`cron_{8位hex}`
- 所有运行时逻辑在一个 tokio interval 中实现

---

### Task 1: 创建 CronJob 数据结构和模块框架

**Files:**
- Create: `rust-agent/src/cron_scheduler.rs`

**Interfaces:**
- Produces: `CronJob` 结构体定义

- [ ] **Step 1: 创建模块框架和 CronJob 数据结构**

```rust
/*
cron_scheduler.rs - Cron Scheduler (s12)

定时任务调度器：使用 cron 表达式在指定时间将 prompt 注入到 agent 循环。
*/

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

/// Cron 任务
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CronJob {
    /// 任务 ID, 格式 cron_[0-9a-f]{8}
    pub id: String,
    /// 5字段 cron 表达式
    pub cron: String,
    /// 触发后注入的 prompt
    pub prompt: String,
    /// 是否循环执行
    pub recurring: bool,
    /// 是否持久化到磁盘
    pub durable: bool,
    /// 是否已入队但未交付
    pub pending_delivery: bool,
    /// 最后触发时间 "YYYY-MM-DD HH:MM"
    pub last_fired: Option<String>,
}

/// 共享状态
#[derive(Default)]
struct CronState {
    /// id -> job
    jobs: HashMap<String, CronJob>,
    /// 待交付的任务队列
    delivery_queue: VecDeque<CronJob>,
}
```

- [ ] **Step 2: 编译验证**

Run: `cd rust-agent && cargo check`
Expected: 编译成功，无错误

- [ ] **Step 3: Commit**

```bash
git add rust-agent/src/cron_scheduler.rs
git commit -m "feat(s12): add CronJob data structure"
```

---

### Task 2: 实现 Cron 表达式字段验证

**Files:**
- Modify: `rust-agent/src/cron_scheduler.rs`

**Interfaces:**
- Produces: `validate_cron_field()` 函数

- [ ] **Step 1: 添加字段验证辅助函数**

在 `CronState` 定义后添加：

```rust
/// 验证 cron 字段是否在有效范围内
fn validate_cron_field(field: &str, min: i32, max: i32) -> Result<(), String> {
    if field == "*" {
        return Ok(());
    }
    if field.starts_with("*/") {
        let step = &field[2..];
        if step.parse::<u32>().map(|n| n > 0).unwrap_or(false) {
            return Ok(());
        }
        return Err(format!("Invalid step: {}", field));
    }
    if field.contains(',') {
        for part in field.split(',') {
            validate_cron_field(part.trim(), min, max)?;
        }
        return Ok(());
    }
    if field.contains('-') {
        let parts: Vec<&str> = field.split('-').collect();
        if parts.len() != 2 {
            return Err(format!("Invalid range: {}", field));
        }
        let start = parts[0].parse::<i32>();
        let end = parts[1].parse::<i32>();
        match (start, end) {
            (Ok(s), Ok(e)) if s <= e && s >= min && e <= max => Ok(()),
            (Ok(_), Ok(e)) if e > max => Err(format!("Range {} exceeds maximum {}", field, max)),
            (Ok(s), Ok(_)) if s < min => Err(format!("Range {} below minimum {}", field, min)),
            (Ok(_), Ok(_)) => Err(format!("Range start > end: {}", field)),
            _ => Err(format!("Invalid range values: {}", field)),
        }
    } else {
        match field.parse::<i32>() {
            Ok(v) if v >= min && v <= max => Ok(()),
            Ok(_) => Err(format!("Value {} outside [{}-{}]", field, min, max)),
            Err(_) => Err(format!("Invalid field: {}", field)),
        }
    }
}

/// 验证完整的 cron 表达式
pub fn validate_cron(cron_expr: &str) -> Result<(), String> {
    let fields: Vec<&str> = cron_expr.trim().split_whitespace().collect();
    if fields.len() != 5 {
        return Err(format!("Expected 5 fields, got {}", fields.len()));
    }
    
    let field_rules = [
        ("minute", 0, 59),
        ("hour", 0, 23),
        ("day-of-month", 1, 31),
        ("month", 1, 12),
        ("day-of-week", 0, 6),
    ];
    
    for (field, (name, min, max)) in fields.iter().zip(field_rules.iter()) {
        validate_cron_field(field, *min, *max)
            .map_err(|e| format!("{}: {}", name, e))?;
    }
    
    Ok(())
}
```

- [ ] **Step 2: 编译并运行测试**

Run: `cd rust-agent && cargo test --lib cron_scheduler`
Expected: 编译成功（暂无测试）

- [ ] **Step 3: Commit**

```bash
git add rust-agent/src/cron_scheduler.rs
git commit -m "feat(s12): add cron expression validation"
```

---

### Task 3: 实现 Cron 字段匹配逻辑

**Files:**
- Modify: `rust-agent/src/cron_scheduler.rs`

**Interfaces:**
- Produces: `cron_field_matches()`, `cron_matches()` 函数

- [ ] **Step 1: 添加字段匹配函数**

在 `validate_cron()` 后添加：

```rust
/// 检查单个 cron 字段是否匹配指定值
fn cron_field_matches(field: &str, value: u32) -> bool {
    if field == "*" {
        return true;
    }
    if field.starts_with("*/") {
        let step = field[2..].parse::<u32>().unwrap_or(1);
        return value % step == 0;
    }
    if field.contains(',') {
        return field.split(',').any(|part| cron_field_matches(part.trim(), value));
    }
    if field.contains('-') {
        let parts: Vec<&str> = field.split('-').collect();
        if parts.len() == 2 {
            if let (Ok(start), Ok(end)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                return value >= start && value <= end;
            }
        }
        return false;
    }
    field.parse::<u32>().map(|v| v == value).unwrap_or(false)
}

/// 检查 cron 表达式是否匹配给定时间
pub fn cron_matches(cron_expr: &str, moment: &chrono::DateTime<chrono::Local>) -> bool {
    let fields: Vec<&str> = cron_expr.trim().split_whitespace().collect();
    if fields.len() != 5 {
        return false;
    }
    
    let (minute, hour, day, month, weekday) = (
        fields[0], fields[1], fields[2], fields[3], fields[4]
    );
    
    // chrono weekday: Mon=0..Sun=6, cron: Sun=0..Sat=6
    let cron_weekday = (moment.weekday().num_days_from_monday() + 1) % 7;
    
    if !cron_field_matches(minute, moment.minute()) {
        return false;
    }
    if !cron_field_matches(hour, moment.hour()) {
        return false;
    }
    if !cron_field_matches(month, moment.month()) {
        return false;
    }
    
    let day_matches = cron_field_matches(day, moment.day());
    let weekday_matches = cron_field_matches(weekday, cron_weekday);
    
    // day 和 weekday 是 OR 关系
    match (day, weekday) {
        ("*", "*") => true,
        ("*", _) => weekday_matches,
        (_, "*") => day_matches,
        _ => day_matches || weekday_matches,
    }
}
```

- [ ] **Step 2: 编译验证**

Run: `cd rust-agent && cargo check`
Expected: 编译成功

- [ ] **Step 3: Commit**

```bash
git add rust-agent/src/cron_scheduler.rs
git commit -m "feat(s12): add cron matching logic"
```

---

### Task 4: 实现 CronManager 核心结构

**Files:**
- Modify: `rust-agent/src/cron_scheduler.rs`

**Interfaces:**
- Produces: `CronManager` 结构体及其基本方法

- [ ] **Step 1: 添加 CronManager 结构体和 ID 生成**

在 `cron_matches()` 后添加：

```rust
use fastrand;

/// Cron 任务管理器
#[derive(Clone)]
pub struct CronManager {
    state: Arc<Mutex<CronState>>,
    workdir: PathBuf,
}

impl CronManager {
    const MAX_ID_RETRIES: usize = 100;
    const DURABLE_FILE: &str = ".scheduled_tasks.json";

    /// 创建管理器
    pub fn new(workdir: PathBuf) -> Self {
        Self {
            state: Arc::new(Mutex::new(CronState::default())),
            workdir,
        }
    }

    /// 生成唯一的任务 ID
    fn generate_id(&self) -> String {
        let state = self.state.lock().expect("state mutex poisoned");
        for _ in 0..Self::MAX_ID_RETRIES {
            let id = format!("cron_{:08x}", fastrand::u32(..));
            if !state.jobs.contains_key(&id) {
                return id;
            }
        }
        String::new() // 极低概率
    }

    /// 获取工作目录
    pub fn workdir(&self) -> &PathBuf {
        &self.workdir
    }
}
```

- [ ] **Step 2: 编译验证**

Run: `cd rust-agent && cargo check`
Expected: 编译成功

- [ ] **Step 3: Commit**

```bash
git add rust-agent/src/cron_scheduler.rs
git commit -m "feat(s12): add CronManager core structure"
```

---

### Task 5: 实现任务调度、取消和列表

**Files:**
- Modify: `rust-agent/src/cron_scheduler.rs`

**Interfaces:**
- Consumes: `validate_cron()`, `generate_id()`
- Produces: `schedule()`, `cancel()`, `list()` 方法

- [ ] **Step 1: 实现任务管理方法**

在 `CronManager` impl 块末尾添加：

```rust
    /// 调度一个 cron 任务
    pub fn schedule(&self, cron: &str, prompt: &str, recurring: bool, durable: bool) -> Result<CronJob, String> {
        validate_cron(cron)?;
        if prompt.trim().is_empty() {
            return Err("Prompt cannot be empty".to_string());
        }

        let id = self.generate_id();
        if id.is_empty() {
            return Err("Failed to allocate task id".to_string());
        }

        let job = CronJob {
            id: id.clone(),
            cron: cron.to_string(),
            prompt: prompt.to_string(),
            recurring,
            durable,
            pending_delivery: false,
            last_fired: None,
        };

        {
            let mut state = self.state.lock().expect("state mutex poisoned");
            state.jobs.insert(id.clone(), job.clone());
        }

        if durable {
            self.save_durable()?;
        }

        Ok(job)
    }

    /// 取消一个 cron 任务
    pub fn cancel(&self, job_id: &str) -> Result<String, String> {
        let (removed_job, was_durable) = {
            let mut state = self.state.lock().expect("state mutex poisoned");
            let job = state.jobs.get(job_id).ok_or_else(|| format!("Job {} not found", job_id))?;
            let was_durable = job.durable;
            
            // 从队列中移除
            state.delivery_queue.retain(|j| j.id != job_id);
            
            let removed_job = state.jobs.remove(job_id).unwrap();
            (removed_job, was_durable)
        };

        if was_durable {
            if let Err(e) = self.save_durable() {
                // 恢复
                let mut state = self.state.lock().expect("state mutex poisoned");
                state.jobs.insert(job_id.to_string(), removed_job);
                return Err(format!("Failed to save after cancel: {}", e));
            }
        }

        Ok(format!("Cancelled {}", job_id))
    }

    /// 列出所有 cron 任务
    pub fn list(&self) -> Vec<CronJob> {
        let state = self.state.lock().expect("state mutex poisoned");
        state.jobs.values().cloned().collect()
    }
```

- [ ] **Step 2: 编译验证**

Run: `cd rust-agent && cargo check`
Expected: 编译成功

- [ ] **Step 3: Commit**

```bash
git add rust-agent/src/cron_scheduler.rs
git commit -m "feat(s12): add schedule, cancel, list methods"
```

---

### Task 6: 实现持久化

**Files:**
- Modify: `rust-agent/src/cron_scheduler.rs`

**Interfaces:**
- Consumes: `CronJob`
- Produces: `save_durable()`, `load_durable()` 方法

- [ ] **Step 1: 添加持久化方法**

在 `CronManager` impl 块中，`list()` 后添加：

```rust
    /// 保存 durable 任务到磁盘
    fn save_durable(&self) -> Result<(), String> {
        let state = self.state.lock().expect("state mutex poisoned");
        let payload: Vec<CronJob> = state.jobs.values()
            .filter(|j| j.durable)
            .cloned()
            .collect();
        drop(state);

        let json = serde_json::to_string_pretty(&payload)
            .map_err(|e| format!("Failed to serialize: {}", e))?;

        let file_path = self.workdir.join(Self::DURABLE_FILE);
        let temp_path = file_path.with_extension(format!("tmp.{}", std::process::id()));

        std::fs::write(&temp_path, json)
            .map_err(|e| format!("Failed to write temp file: {}", e))?;

        std::fs::rename(&temp_path, &file_path)
            .map_err(|e| format!("Failed to rename temp file: {}", e))?;

        Ok(())
    }

    /// 从磁盘加载 durable 任务
    pub fn load_durable(&self) -> Result<usize, String> {
        let file_path = self.workdir.join(Self::DURABLE_FILE);
        if !file_path.exists() {
            return Ok(0);
        }

        let content = std::fs::read_to_string(&file_path)
            .map_err(|e| format!("Failed to read: {}", e))?;

        let payload: Vec<CronJob> = serde_json::from_str(&content)
            .map_err(|e| format!("Failed to parse: {}", e))?;

        let mut loaded = 0;
        let mut state = self.state.lock().expect("state mutex poisoned");
        for job in payload {
            if let Err(e) = validate_cron(&job.cron) {
                eprintln!("  [cron] skipped invalid saved job: {}", e);
                continue;
            }
            if !job.id.starts_with("cron_") {
                eprintln!("  [cron] skipped invalid job ID: {}", job.id);
                continue;
            }
            if job.prompt.trim().is_empty() {
                eprintln!("  [cron] skipped job with empty prompt: {}", job.id);
                continue;
            }

            state.jobs.insert(job.id.clone(), job.clone());
            if job.pending_delivery {
                state.delivery_queue.push_back(job);
            }
            loaded += 1;
        }

        if loaded > 0 {
            println!("  [cron] loaded {} durable job(s)", loaded);
        }

        Ok(loaded)
    }
```

- [ ] **Step 2: 编译验证**

Run: `cd rust-agent && cargo check`
Expected: 编译成功

- [ ] **Step 3: Commit**

```bash
git add rust-agent/src/cron_scheduler.rs
git commit -m "feat(s12): add persistence (save_durable, load_durable)"
```

---

### Task 7: 实现到期检测和队列管理

**Files:**
- Modify: `rust-agent/src/cron_scheduler.rs`

**Interfaces:**
- Consumes: `cron_matches()`
- Produces: `poll_due_jobs()`, `consume_queue()`, `acknowledge_jobs()` 方法

- [ ] **Step 1: 添加到期检测和队列管理方法**

在 `CronManager` impl 块中，`load_durable()` 后添加：

```rust
    /// 检查到期任务并入队
    pub fn poll_due_jobs(&self, moment: &chrono::DateTime<chrono::Local>) {
        let minute_marker = moment.format("%Y-%m-%d %H:%M").to_string();

        let mut state = self.state.lock().expect("state mutex poisoned");
        let job_ids: Vec<String> = state.jobs.keys().cloned().collect();

        for id in job_ids {
            if let Some(job) = state.jobs.get_mut(&id) {
                if job.pending_delivery {
                    continue;
                }
                if job.last_fired.as_ref() == Some(&minute_marker) {
                    continue;
                }
                if cron_matches(&job.cron, moment) {
                    job.pending_delivery = true;
                    job.last_fired = Some(minute_marker.clone());
                    state.delivery_queue.push_back(job.clone());
                    
                    if job.durable {
                        drop(state);
                        let _ = self.save_durable();
                        state = self.state.lock().expect("state mutex poisoned");
                    }
                    
                    println!("  [cron] due {}: {}", job.id, &job.prompt[..job.prompt.len().min(60)]);
                }
            }
        }
    }

    /// 消费待交付队列
    pub fn consume_queue(&self) -> Vec<CronJob> {
        let mut state = self.state.lock().expect("state mutex poisoned");
        state.delivery_queue.drain(..).collect()
    }

    /// 确认任务已交付
    pub fn acknowledge_jobs(&self, jobs: &[CronJob]) -> Result<(), String> {
        let mut state = self.state.lock().expect("state mutex poisoned");

        for delivered in jobs {
            if let Some(current) = state.jobs.get_mut(&delivered.id) {
                if current.recurring {
                    current.pending_delivery = false;
                } else {
                    state.jobs.remove(&delivered.id);
                }
            }
        }

        if jobs.iter().any(|j| j.durable) {
            drop(state);
            self.save_durable()?;
        }

        Ok(())
    }

    /// 恢复未交付的任务到队列
    pub fn restore_jobs(&self, jobs: &[CronJob]) {
        let mut state = self.state.lock().expect("state mutex poisoned");
        let queued_ids: std::collections::HashSet<String> = 
            state.delivery_queue.iter().map(|j| j.id.clone()).collect();

        for delivered in jobs {
            if let Some(current) = state.jobs.get_mut(&delivered.id) {
                current.pending_delivery = true;
                if !queued_ids.contains(&delivered.id) {
                    state.delivery_queue.push_back(current.clone());
                }
            }
        }
    }

    /// 检查是否有待交付任务
    pub fn has_queue(&self) -> bool {
        let state = self.state.lock().expect("state mutex poisoned");
        !state.delivery_queue.is_empty()
    }
```

- [ ] **Step 2: 编译验证**

Run: `cd rust-agent && cargo check`
Expected: 编译成功

- [ ] **Step 3: Commit**

```bash
git add rust-agent/src/cron_scheduler.rs
git commit -m "feat(s12): add due job polling and queue management"
```

---

### Task 8: 实现运行时

**Files:**
- Modify: `rust-agent/src/cron_scheduler.rs`

**Interfaces:**
- Consumes: `poll_due_jobs()`, `consume_queue()`, `acknowledge_jobs()`
- Produces: `start_runtime()`, `collect_and_inject()` 函数

- [ ] **Step 1: 添加运行时函数**

在文件末尾添加：

```rust
use crate::client::{ContentBlock, Message};
use std::sync::atomic::{AtomicBool, Ordering};

/// 全局运行时停止标志
static RUNTIME_STOP: AtomicBool = AtomicBool::new(false);
static RUNTIME_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static RUNTIME_HANDLE: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>> = std::sync::Mutex::new(None);

/// 全局 CronManager 实例
static CRON_MANAGER: std::sync::OnceLock<std::sync::Arc<CronManager>> = std::sync::OnceLock::new();

/// 初始化全局 CronManager
pub fn init_manager(workdir: PathBuf) -> std::sync::Arc<CronManager> {
    let manager = std::sync::Arc::new(CronManager::new(workdir));
    let _ = CRON_MANAGER.set(manager.clone());
    
    // 加载持久化任务
    let _ = manager.load_durable();
    
    manager
}

/// 获取全局 CronManager
pub fn get_manager() -> Option<std::sync::Arc<CronManager>> {
    CRON_MANAGER.get().cloned()
}

/// 启动运行时
pub async fn start_runtime() {
    if RUNTIME_STARTED.load(Ordering::SeqCst) {
        return;
    }

    let manager = get_manager().expect("CronManager not initialized");
    RUNTIME_STOP.store(false, Ordering::SeqCst);

    let handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            interval.tick().await;
            if RUNTIME_STOP.load(Ordering::SeqCst) {
                break;
            }
            manager.poll_due_jobs(&chrono::Local::now());
        }
    });

    let mut guard = RUNTIME_HANDLE.lock().expect("handle mutex poisoned");
    *guard = Some(handle);
    RUNTIME_STARTED.store(true, Ordering::SeqCst);
}

/// 停止运行时
pub async fn stop_runtime() {
    RUNTIME_STOP.store(true, Ordering::SeqCst);
    
    if let Some(handle) = RUNTIME_HANDLE.lock().expect("handle mutex poisoned").take() {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), handle).await;
    }
    
    RUNTIME_STARTED.store(false, Ordering::SeqCst);
}

/// 收集并注入待交付任务到消息列表
pub fn collect_and_inject(messages: &mut Vec<Message>) -> Option<usize> {
    let manager = get_manager()?;
    if !manager.has_queue() {
        return None;
    }

    let jobs = manager.consume_queue();
    if jobs.is_empty() {
        return None;
    }

    let count = jobs.len();
    for job in &jobs {
        messages.push(Message {
            role: "user".to_string(),
            content: vec![ContentBlock::Text {
                text: format!("[Scheduled] {}", job.prompt),
            }],
        });
        println!("  [cron] delivered {}: {}", job.id, &job.prompt[..job.prompt.len().min(60)]);
    }

    Some(count)
}

/// 确认任务已交付
pub fn acknowledge_jobs(jobs: &[CronJob]) -> Result<(), String> {
    let manager = get_manager().ok_or_else(|| "CronManager not initialized".to_string())?;
    manager.acknowledge_jobs(jobs)
}

/// 恢复未交付的任务
pub fn restore_jobs(jobs: &[CronJob]) {
    if let Some(manager) = get_manager() {
        manager.restore_jobs(jobs);
    }
}
```

- [ ] **Step 2: 编译验证**

Run: `cd rust-agent && cargo check`
Expected: 编译成功

- [ ] **Step 3: Commit**

```bash
git add rust-agent/src/cron_scheduler.rs
git commit -m "feat(s12): add runtime (start_runtime, collect_and_inject)"
```

---

### Task 9: 实现三个工具

**Files:**
- Modify: `rust-agent/src/cron_scheduler.rs`

**Interfaces:**
- Consumes: `CronManager`, `Result<T, String>`
- Produces: `ScheduleCronTool`, `ListCronsTool`, `CancelCronTool`

- [ ] **Step 1: 添加工具结构体和 trait 实现**

在文件末尾添加：

```rust
use crate::tools::trait_def::{PermissionCheck, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::Value;

/// ScheduleCron 工具
pub struct ScheduleCronTool;

#[async_trait]
impl Tool for ScheduleCronTool {
    fn name(&self) -> &str {
        "schedule_cron"
    }

    fn description(&self) -> &str {
        "Schedule a prompt with a 5-field cron expression."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "cron": {"type": "string"},
                "prompt": {"type": "string"},
                "recurring": {"type": "boolean", "default": true},
                "durable": {"type": "boolean", "default": true}
            },
            "required": ["cron", "prompt"]
        })
    }

    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        let manager = get_manager();
        let Some(manager) = manager else {
            return "Error: CronManager not initialized".to_string();
        };

        let cron = input.get("cron").and_then(|v| v.as_str()).unwrap_or("");
        let prompt = input.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
        let recurring = input.get("recurring").and_then(|v| v.as_bool()).unwrap_or(true);
        let durable = input.get("durable").and_then(|v| v.as_bool()).unwrap_or(true);

        match manager.schedule(cron, prompt, recurring, durable) {
            Ok(job) => format!("Scheduled {}: {} -> {}", job.id, job.cron, job.prompt),
            Err(e) => format!("Error: {}", e),
        }
    }

    fn available_for_subagent(&self) -> bool {
        true
    }
}

/// ListCrons 工具
pub struct ListCronsTool;

#[async_trait]
impl Tool for ListCronsTool {
    fn name(&self) -> &str {
        "list_crons"
    }

    fn description(&self) -> &str {
        "List scheduled cron jobs."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, _input: &Value) -> String {
        let manager = get_manager();
        let Some(manager) = manager else {
            return "Error: CronManager not initialized".to_string();
        };

        let jobs = manager.list();
        if jobs.is_empty() {
            return "No cron jobs.".to_string();
        }

        jobs.iter()
            .map(|job| {
                let frequency = if job.recurring { "recurring" } else { "one-shot" };
                let storage = if job.durable { "durable" } else { "session" };
                format!("{}: {} -> {} [{}, {}]", job.id, job.cron, 
                    &job.prompt[..job.prompt.len().min(60)], frequency, storage)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn available_for_subagent(&self) -> bool {
        true
    }
}

/// CancelCron 工具
pub struct CancelCronTool;

#[async_trait]
impl Tool for CancelCronTool {
    fn name(&self) -> &str {
        "cancel_cron"
    }

    fn description(&self) -> &str {
        "Cancel a cron job by ID."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "job_id": {"type": "string"}
            },
            "required": ["job_id"]
        })
    }

    fn check_permission(&self, _input: &Value) -> PermissionCheck {
        PermissionCheck::Pass
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, input: &Value) -> String {
        let manager = get_manager();
        let Some(manager) = manager else {
            return "Error: CronManager not initialized".to_string();
        };

        let job_id = input.get("job_id").and_then(|v| v.as_str()).unwrap_or("");
        manager.cancel(job_id)
    }

    fn available_for_subagent(&self) -> bool {
        true
    }
}
```

- [ ] **Step 2: 编译验证**

Run: `cd rust-agent && cargo check`
Expected: 编译成功

- [ ] **Step 3: Commit**

```bash
git add rust-agent/src/cron_scheduler.rs
git commit -m "feat(s12): add three cron tools"
```

---

### Task 10: 集成到 lib.rs

**Files:**
- Modify: `rust-agent/src/lib.rs`

**Interfaces:**
- Produces: 模块导出声明

- [ ] **Step 1: 在 lib.rs 中添加 cron_scheduler 模块**

查看 lib.rs 中的模块声明部分，添加：

```rust
pub mod cron_scheduler;
```

- [ ] **Step 2: 编译验证**

Run: `cd rust-agent && cargo check`
Expected: 编译成功

- [ ] **Step 3: Commit**

```bash
git add rust-agent/src/lib.rs
git commit -m "feat(s12): export cron_scheduler module"
```

---

### Task 11: 集成到 main.rs

**Files:**
- Modify: `rust-agent/src/main.rs`

**Interfaces:**
- Consumes: `init_manager()`, `start_runtime()`, `collect_and_inject()`, `acknowledge_jobs()`, `restore_jobs()`

- [ ] **Step 1: 添加 use 声明**

在文件顶部的 use 声明中添加：

```rust
use rust_agent::cron_scheduler::{collect_and_inject, acknowledge_jobs, restore_jobs};
```

- [ ] **Step 2: 在 main() 中初始化 CronManager**

在 `todo_manager` 初始化后添加：

```rust
    // s12: 初始化 CronManager 并启动运行时
    let cron_manager = rust_agent::cron_scheduler::init_manager(PathBuf::from(&cwd));
    rust_agent::cron_scheduler::start_runtime().await;
```

- [ ] **Step 3: 在 agent_loop 中添加收集和注入**

在 `agent_loop` 函数中，在 `reactive_retries = 0u32;` 后添加：

```rust
        // s12: 循环顶部收集待交付的定时任务
        let scheduled_start = messages.len();
        let scheduled_jobs = collect_and_inject(messages);
        let mut waiting_for_ack: Vec<rust_agent::cron_scheduler::CronJob> = Vec::new();
        if let Some(jobs) = scheduled_jobs {
            waiting_for_ack = jobs; // jobs 返回的是 Option<usize>，需要修改 collect_and_inject 返回值
        }
```

等等，我需要修正这个。`collect_and_inject` 返回 `Option<usize>`，需要重新设计。让我重新实现。

- [ ] **Step 3 (修正): 在 agent_loop 中添加收集和注入**

在 `agent_loop` 函数中，在 `reactive_retries = 0u32;` 后添加：

```rust
        // s12: 循环顶部收集待交付的定时任务
        let scheduled_start = messages.len();
        let scheduled_jobs = {
            let manager = rust_agent::cron_scheduler::get_manager();
            if let Some(mgr) = manager {
                Some(mgr.consume_queue())
            } else {
                None
            }
        };

        if let Some(jobs) = scheduled_jobs {
            for job in &jobs {
                messages.push(Message {
                    role: "user".to_string(),
                    content: vec![ContentBlock::Text {
                        text: format!("[Scheduled] {}", job.prompt),
                    }],
                });
                println!("  [cron] delivered {}: {}", job.id, &job.prompt[..job.prompt.len().min(60)]);
            }
            waiting_for_ack = jobs;
        }
```

同时在 agent_loop 参数中添加 `waiting_for_ack: Vec<rust_agent::cron_scheduler::CronJob>` 变量声明。

- [ ] **Step 4: 在模型调用成功后确认任务**

在 `messages.append({"role": "assistant"...})` 后添加：

```rust
        if !waiting_for_ack.is_empty() {
            if let Err(e) = acknowledge_jobs(&waiting_for_ack) {
                println!("  [cron] acknowledgement failed: {}", e);
            }
            waiting_for_ack.clear();
        }
```

- [ ] **Step 5: 在模型调用失败时恢复任务**

在 `Err(e)` 处理中添加：

```rust
            if !waiting_for_ack.is_empty() {
                // 移除已注入的消息
                messages.truncate(scheduled_start);
                restore_jobs(&waiting_for_ack);
            }
```

- [ ] **Step 6: 编译验证**

Run: `cd rust-agent && cargo check`
Expected: 编译成功

- [ ] **Step 7: Commit**

```bash
git add rust-agent/src/main.rs
git commit -m "feat(s12): integrate cron scheduler into main loop"
```

---

### Task 12: 集成到 tools registry

**Files:**
- Modify: `rust-agent/src/tools/mod.rs`

**Interfaces:**
- Produces: 工具注册

- [ ] **Step 1: 在 build_registry 中注册工具**

在 `build_registry()` 函数末尾添加：

```rust
    registry.register(Box::new(crate::cron_scheduler::ScheduleCronTool));
    registry.register(Box::new(crate::cron_scheduler::ListCronsTool));
    registry.register(Box::new(crate::cron_scheduler::CancelCronTool));
```

- [ ] **Step 2: 编译验证**

Run: `cd rust-agent && cargo check`
Expected: 编译成功

- [ ] **Step 3: Commit**

```bash
git add rust-agent/src/tools/mod.rs
git commit -m "feat(s12): register cron tools in registry"
```

---

### Task 13: 添加单元测试

**Files:**
- Modify: `rust-agent/src/cron_scheduler.rs`

**Interfaces:**
- Consumes: 所有实现的函数

- [ ] **Step 1: 添加测试模块**

在文件末尾添加：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn validate_cron_all_wildcards() {
        assert!(validate_cron("* * * * *").is_ok());
    }

    #[test]
    fn validate_cron_specific_time() {
        assert!(validate_cron("0 9 * * *").is_ok());
    }

    #[test]
    fn validate_cron_step() {
        assert!(validate_cron("*/5 * * * *").is_ok());
    }

    #[test]
    fn validate_cron_range() {
        assert!(validate_cron("0 9-17 * * 1-5").is_ok());
    }

    #[test]
    fn validate_cron_list() {
        assert!(validate_cron("0,15,30,45 * * * *").is_ok());
    }

    #[test]
    fn validate_cron_invalid_field_count() {
        assert!(validate_cron("* * * *").is_err());
    }

    #[test]
    fn validate_cron_invalid_range() {
        assert!(validate_cron("60 * * * *").is_err()); // minute max is 59
    }

    #[test]
    fn validate_cron_invalid_step() {
        assert!(validate_cron("*/0 * * * *").is_err()); // step must be > 0
    }

    #[test]
    fn cron_field_matches_wildcard() {
        assert!(cron_field_matches("*", 30));
    }

    #[test]
    fn cron_field_matches_exact() {
        assert!(cron_field_matches("30", 30));
        assert!(!cron_field_matches("30", 31));
    }

    #[test]
    fn cron_field_matches_step() {
        assert!(cron_field_matches("*/5", 30)); // 30 % 5 == 0
        assert!(!cron_field_matches("*/5", 31));
    }

    #[test]
    fn cron_field_matches_range() {
        assert!(cron_field_matches("9-17", 12));
        assert!(!cron_field_matches("9-17", 8));
        assert!(!cron_field_matches("9-17", 18));
    }

    #[test]
    fn cron_field_matches_list() {
        assert!(cron_field_matches("0,15,30,45", 30));
        assert!(!cron_field_matches("0,15,30,45", 10));
    }

    #[test]
    fn cron_matches_daily() {
        let time = chrono::Local::with_ymd_and_hms(2026, 8, 19, 9, 0, 0).unwrap();
        assert!(cron_matches("0 9 * * *", &time));
        assert!(!cron_matches("0 10 * * *", &time));
    }

    #[test]
    fn cron_matches_weekday() {
        // Monday
        let time = chrono::Local::with_ymd_and_hms(2026, 8, 17, 9, 0, 0).unwrap();
        assert!(cron_matches("0 9 * * 1", &time)); // 1=Monday in cron
        assert!(!cron_matches("0 9 * * 6", &time)); // 6=Saturday in cron
    }

    #[test]
    fn cron_matches_day_or_weekday() {
        // 2026-08-17 is a Monday
        let time = chrono::Local::with_ymd_and_hms(2026, 8, 17, 9, 0, 0).unwrap();
        
        // Day match only
        assert!(cron_matches("0 9 17 * *", &time));
        
        // Weekday match only
        assert!(cron_matches("0 9 * * 1", &time));
        
        // Neither match
        assert!(!cron_matches("0 9 18 * *", &time));
        assert!(!cron_matches("0 9 * * 2", &time));
    }

    #[test]
    fn generate_id_format() {
        let dir = tempdir().unwrap();
        let manager = CronManager::new(dir.path().to_path_buf());
        let id = manager.generate_id();
        assert!(id.starts_with("cron_"));
        assert_eq!(id.len(), 12); // "cron_" + 8 hex
    }

    #[test]
    fn generate_id_unique() {
        let dir = tempdir().unwrap();
        let manager = CronManager::new(dir.path().to_path_buf());
        let mut ids = std::collections::HashSet::new();
        
        for _ in 0..50 {
            let id = manager.generate_id();
            assert!(ids.insert(id), "ID collision");
        }
    }

    #[test]
    fn schedule_and_list() {
        let dir = tempdir().unwrap();
        let manager = CronManager::new(dir.path().to_path_buf());
        
        let job = manager.schedule("0 9 * * *", "run tests", true, false).unwrap();
        assert!(job.id.starts_with("cron_"));
        assert_eq!(job.cron, "0 9 * * *");
        assert_eq!(job.prompt, "run tests");
        
        let jobs = manager.list();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, job.id);
    }

    #[test]
    fn schedule_invalid_cron() {
        let dir = tempdir().unwrap();
        let manager = CronManager::new(dir.path().to_path_buf());
        
        assert!(manager.schedule("* * *", "test", true, false).is_err());
    }

    #[test]
    fn schedule_empty_prompt() {
        let dir = tempdir().unwrap();
        let manager = CronManager::new(dir.path().to_path_buf());
        
        assert!(manager.schedule("0 9 * * *", "", true, false).is_err());
    }

    #[test]
    fn cancel_existing_job() {
        let dir = tempdir().unwrap();
        let manager = CronManager::new(dir.path().to_path_buf());
        
        let job = manager.schedule("0 9 * * *", "run tests", true, false).unwrap();
        let result = manager.cancel(&job.id);
        assert!(result.is_ok());
        
        let jobs = manager.list();
        assert_eq!(jobs.len(), 0);
    }

    #[test]
    fn cancel_nonexistent_job() {
        let dir = tempdir().unwrap();
        let manager = CronManager::new(dir.path().to_path_buf());
        
        let result = manager.cancel("cron_deadbeef");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn save_and_load_durable() {
        let dir = tempdir().unwrap();
        let manager1 = CronManager::new(dir.path().to_path_buf());
        
        let job = manager1.schedule("0 9 * * *", "run tests", true, true).unwrap();
        
        let jobs = manager1.list();
        assert_eq!(jobs.len(), 1);
        
        drop(manager1);
        
        let manager2 = CronManager::new(dir.path().to_path_buf());
        let loaded = manager2.load_durable().unwrap();
        assert_eq!(loaded, 1);
        
        let jobs = manager2.list();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, job.id);
        assert_eq!(jobs[0].cron, job.cron);
    }

    #[test]
    fn consume_queue() {
        let dir = tempdir().unwrap();
        let manager = CronManager::new(dir.path().to_path_buf());
        
        let job = manager.schedule("0 9 * * *", "run tests", true, false).unwrap();
        
        // 手动入队
        {
            let mut state = manager.state.lock().expect("state mutex poisoned");
            state.delivery_queue.push_back(job.clone());
        }
        
        let jobs = manager.consume_queue();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, job.id);
        
        // 队列已清空
        let jobs = manager.consume_queue();
        assert_eq!(jobs.len(), 0);
    }

    #[test]
    fn acknowledge_recurring_job() {
        let dir = tempdir().unwrap();
        let manager = CronManager::new(dir.path().to_path_buf());
        
        let job = manager.schedule("0 9 * * *", "run tests", true, false).unwrap();
        
        // 标记为待交付
        {
            let mut state = manager.state.lock().expect("state mutex poisoned");
            if let Some(j) = state.jobs.get_mut(&job.id) {
                j.pending_delivery = true;
            }
        }
        
        manager.acknowledge_jobs(&[job]).unwrap();
        
        let jobs = manager.list();
        assert_eq!(jobs.len(), 1);
        assert!(!jobs[0].pending_delivery);
    }

    #[test]
    fn acknowledge_oneshot_job() {
        let dir = tempdir().unwrap();
        let manager = CronManager::new(dir.path().to_path_buf());
        
        let job = manager.schedule("0 9 * * *", "run tests", false, false).unwrap();
        
        // 标记为待交付
        {
            let mut state = manager.state.lock().expect("state mutex poisoned");
            if let Some(j) = state.jobs.get_mut(&job.id) {
                j.pending_delivery = true;
            }
        }
        
        manager.acknowledge_jobs(&[job]).unwrap();
        
        let jobs = manager.list();
        assert_eq!(jobs.len(), 0); // one-shot 任务被移除
    }
}
```

- [ ] **Step 2: 添加 chrono 依赖到 Cargo.toml**

确保 `Cargo.toml` 中有：

```toml
chrono = "0.4"
```

- [ ] **Step 3: 运行测试**

Run: `cd rust-agent && cargo test --lib cron_scheduler`
Expected: 所有测试通过

- [ ] **Step 4: Commit**

```bash
git add rust-agent/src/cron_scheduler.rs rust-agent/Cargo.toml
git commit -m "feat(s12): add unit tests for cron scheduler"
```

---

## 自检

### Spec 覆盖检查

- CronJob 数据结构 ✓ Task 1
- Cron 表达式验证 ✓ Task 2
- Cron 匹配逻辑 ✓ Task 3
- CronManager 核心方法 ✓ Task 4-5
- 持久化 ✓ Task 6
- 到期检测和队列管理 ✓ Task 7
- 运行时 ✓ Task 8
- 三个工具 ✓ Task 9
- 集成 ✓ Task 10-12
- 测试 ✓ Task 13

### 占位符扫描

无 TBD、TODO、不完整步骤 ✓

### 类型一致性

- CronJob 结构体定义一致 ✓
- 方法签名一致 ✓
- 错误处理使用 `Result<T, String>` ✓