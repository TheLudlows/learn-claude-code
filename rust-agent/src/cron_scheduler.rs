/*
cron_scheduler.rs - Cron Scheduler (s12)

定时任务调度器：使用 cron 表达式在指定时间将 prompt 注入到 agent 循环。
*/

use crate::client::{ContentBlock, Message};
use chrono::{DateTime, Datelike, Local, Timelike};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
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

    /// 检查到期任务并入队
    pub fn poll_due_jobs(&self, moment: &chrono::DateTime<chrono::Local>) {
        let minute_marker = moment.format("%Y-%m-%d %H:%M").to_string();

        let mut state = self.state.lock().expect("state mutex poisoned");
        let job_ids: Vec<String> = state.jobs.keys().cloned().collect();

        for id in job_ids {
            let job_clone = {
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
                        let job_clone = job.clone();
                        job_clone
                    } else {
                        continue;
                    }
                } else {
                    continue;
                }
            };

            state.delivery_queue.push_back(job_clone.clone());

            if job_clone.durable {
                drop(state);
                let _ = self.save_durable();
                state = self.state.lock().expect("state mutex poisoned");
            }

            println!("  [cron] due {}: {}", job_clone.id, &job_clone.prompt[..job_clone.prompt.len().min(60)]);
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
            let current_clone = {
                if let Some(current) = state.jobs.get_mut(&delivered.id) {
                    current.pending_delivery = true;
                    current.clone()
                } else {
                    continue;
                }
            };

            if !queued_ids.contains(&delivered.id) {
                state.delivery_queue.push_back(current_clone);
            }
        }
    }

    /// 检查是否有待交付任务
    pub fn has_queue(&self) -> bool {
        let state = self.state.lock().expect("state mutex poisoned");
        !state.delivery_queue.is_empty()
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
        match manager.cancel(job_id) {
            Ok(msg) => msg,
            Err(e) => format!("Error: {}", e),
        }
    }

    fn available_for_subagent(&self) -> bool {
        true
    }
}