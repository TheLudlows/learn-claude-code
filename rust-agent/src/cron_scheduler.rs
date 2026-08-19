/*
cron_scheduler.rs - Cron Scheduler (s12)

定时任务调度器：使用 cron 表达式在指定时间将 prompt 注入到 agent 循环。
*/

use chrono::{DateTime, Datelike, Local, Timelike};
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