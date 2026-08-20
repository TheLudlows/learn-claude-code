/*
cron_scheduler.rs - Cron Scheduler (s12)

定时任务调度器：使用 cron 表达式在指定时间将 prompt 注入到 agent 循环。

实现说明：
- cron 表达式解析/匹配使用 croner crate（5 字段 Vixie cron，默认 OR 语义）。
- 调度由单个 tokio::spawn 后台任务驱动，每 60 秒检查一次到期任务。
- 到期任务推入 delivery_queue，由主循环 (main.rs) 拉取交付。
*/

use chrono::{Local, Timelike};
use croner::Cron;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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

/// 共享状态（纯业务数据，不再持有外部调度器）
struct CronState {
    /// id -> job
    jobs: HashMap<String, CronJob>,
    /// 待交付的任务队列
    delivery_queue: VecDeque<CronJob>,
}

/// Cron 任务管理器
#[derive(Clone)]
pub struct CronManager {
    state: Arc<Mutex<CronState>>,
    workdir: PathBuf,
}

impl CronManager {
    const MAX_ID_RETRIES: usize = 100;
    const DURABLE_FILE: &str = ".scheduled_tasks.json";

    /// 创建管理器，同时启动后台调度循环。
    pub async fn new(workdir: PathBuf) -> Result<Self, String> {
        let state = Arc::new(Mutex::new(CronState {
            jobs: HashMap::new(),
            delivery_queue: VecDeque::new(),
        }));

        // 启动后台 tick 循环（每 60 秒检查到期任务）
        tokio::spawn(tick_loop(Arc::clone(&state), workdir.clone()));

        Ok(Self { state, workdir })
    }

    /// 空操作实例：不启动后台调度器，consume_queue 返回空，schedule 操作内存态。
    /// Subagent / Teammate 不需要定时能力，用此占位避免 Option。
    pub fn noop() -> Self {
        Self {
            state: Arc::new(Mutex::new(CronState {
                jobs: HashMap::new(),
                delivery_queue: VecDeque::new(),
            })),
            workdir: PathBuf::new(),
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
    pub fn schedule(
        &self,
        cron: &str,
        prompt: &str,
        recurring: bool,
        durable: bool,
    ) -> Result<CronJob, String> {
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
            state.jobs.insert(id, job.clone());
        }

        if durable {
            self.save_durable()?;
        }

        Ok(job)
    }

    /// 取消一个 cron 任务
    pub fn cancel(&self, job_id: &str) -> Result<String, String> {
        let was_durable = {
            let mut state = self.state.lock().expect("state mutex poisoned");
            let job = state
                .jobs
                .get(job_id)
                .ok_or_else(|| format!("Job {} not found", job_id))?;
            let was_durable = job.durable;
            state.delivery_queue.retain(|j| j.id != job_id);
            state.jobs.remove(job_id);
            was_durable
        };

        if was_durable {
            self.save_durable()?;
        }

        Ok(format!("Cancelled {}", job_id))
    }

    /// 列出所有 cron 任务
    pub fn list(&self) -> Vec<CronJob> {
        let state = self.state.lock().expect("state mutex poisoned");
        state.jobs.values().cloned().collect()
    }

    /// 保存 durable 任务到磁盘
    pub fn save_durable(&self) -> Result<(), String> {
        persist_durable_jobs(&self.state, &self.workdir)
    }

    /// 从磁盘加载 durable 任务（无需重新注册调度器，tick 循环自动覆盖）
    pub async fn load_durable(&self) -> Result<usize, String> {
        let file_path = self.workdir.join(Self::DURABLE_FILE);
        if !file_path.exists() {
            return Ok(0);
        }

        let content =
            std::fs::read_to_string(&file_path).map_err(|e| format!("Failed to read: {}", e))?;

        let payload: Vec<CronJob> =
            serde_json::from_str(&content).map_err(|e| format!("Failed to parse: {}", e))?;

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

    /// 消费待交付队列
    pub fn consume_queue(&self) -> Vec<CronJob> {
        let mut state = self.state.lock().expect("state mutex poisoned");
        state.delivery_queue.drain(..).collect()
    }

    /// 确认任务已交付（保持 async 签名以兼容 agent.rs 调用方）
    pub async fn acknowledge_jobs(&self, jobs: &[CronJob]) -> Result<(), String> {
        let has_durable = {
            let mut state = self.state.lock().expect("state mutex poisoned");
            let mut has_durable = false;
            let mut to_remove: Vec<String> = Vec::new();

            for delivered in jobs {
                if let Some(current) = state.jobs.get_mut(&delivered.id) {
                    has_durable = has_durable || current.durable;
                    if current.recurring {
                        current.pending_delivery = false;
                    } else {
                        to_remove.push(delivered.id.clone());
                    }
                }
            }

            for id in to_remove {
                state.jobs.remove(&id);
            }

            has_durable
        };

        if has_durable {
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
            // 先在嵌套作用域内完成 jobs 的可变借用，再操作 delivery_queue
            let to_push = {
                if let Some(current) = state.jobs.get_mut(&delivered.id) {
                    current.pending_delivery = true;
                    if !queued_ids.contains(&delivered.id) {
                        Some(current.clone())
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            if let Some(job_clone) = to_push {
                state.delivery_queue.push_back(job_clone);
            }
        }
    }

    /// 检查是否有待交付任务
    pub fn has_queue(&self) -> bool {
        let state = self.state.lock().expect("state mutex poisoned");
        !state.delivery_queue.is_empty()
    }

    /// 启动调度器（tick 循环已在 new() 中启动，此方法保留以兼容 agent.rs）
    pub async fn start_scheduler(&self) -> Result<(), String> {
        Ok(())
    }

    /// 停止调度器（best-effort，进程退出即清理）
    pub async fn shutdown_scheduler(&self) {
        // tick 循环随进程退出自动终止
    }
}

// ---------------------------------------------------------------------------
// 自由函数
// ---------------------------------------------------------------------------

/// 后台调度循环：每 60 秒检查到期任务并推入 delivery_queue
async fn tick_loop(state: Arc<Mutex<CronState>>, workdir: PathBuf) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;

        let now = Local::now();
        let minute_marker = now.format("%Y-%m-%d %H:%M").to_string();
        let mut fired_jobs: Vec<CronJob> = Vec::new();

        {
            let mut st = state.lock().expect("state mutex poisoned");

            // 第一遍：标记到期任务并收集 clone（不操作 delivery_queue，避免双重借用）
            for job in st.jobs.values_mut() {
                if job.pending_delivery {
                    continue;
                }
                if !cron_matches(&job.cron, &now) {
                    continue;
                }
                if job.last_fired.as_ref() == Some(&minute_marker) {
                    continue;
                }

                job.pending_delivery = true;
                job.last_fired = Some(minute_marker.clone());
                fired_jobs.push(job.clone());
            }

            // 第二遍：推入交付队列（jobs 的借用已释放）
            for job in &fired_jobs {
                st.delivery_queue.push_back(job.clone());
            }
        }

        // 持久化 + 日志（锁外执行）
        let has_durable = fired_jobs.iter().any(|j| j.durable);
        if has_durable {
            if let Err(e) = persist_durable_jobs(&state, &workdir) {
                eprintln!("  [cron] failed to persist on fire: {}", e);
            }
        }

        for job in &fired_jobs {
            let preview_len = job.prompt.len().min(60);
            println!(
                "  [cron] due {}: {}",
                job.id,
                &job.prompt[..preview_len]
            );
        }
    }
}

/// 把 durable 任务写入磁盘
fn persist_durable_jobs(state: &Arc<Mutex<CronState>>, workdir: &Path) -> Result<(), String> {
    let payload: Vec<CronJob> = {
        let st = state.lock().expect("state mutex poisoned");
        st.jobs.values().filter(|j| j.durable).cloned().collect()
    };

    let json =
        serde_json::to_string_pretty(&payload).map_err(|e| format!("Failed to serialize: {}", e))?;

    let file_path = workdir.join(CronManager::DURABLE_FILE);
    let temp_path = file_path.with_extension(format!("tmp.{}", std::process::id()));

    std::fs::write(&temp_path, json).map_err(|e| format!("Failed to write temp file: {}", e))?;

    std::fs::rename(&temp_path, &file_path).map_err(|e| format!("Failed to rename temp file: {}", e))?;

    Ok(())
}

/// 验证完整的 cron 表达式（5 字段 Vixie cron）
pub fn validate_cron(cron_expr: &str) -> Result<(), String> {
    let fields: Vec<&str> = cron_expr.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(format!("Expected 5 fields, got {}", fields.len()));
    }
    cron_expr.parse::<Cron>().map_err(|e| e.to_string())?;
    Ok(())
}

/// 检查 cron 表达式是否匹配给定时间
pub fn cron_matches(cron_expr: &str, moment: &chrono::DateTime<chrono::Local>) -> bool {
    let schedule = match cron_expr.parse::<Cron>() {
        Ok(s) => s,
        Err(_) => return false,
    };
    // 按分钟粒度匹配：把秒对齐到 0，避免每秒 poll 时因秒数非零而漏判
    let aligned = moment.with_second(0).unwrap_or(*moment);
    schedule.is_time_matching(&aligned).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Tool 实现
// ---------------------------------------------------------------------------

use crate::tools::trait_def::{AgentKind, PermissionCheck, Tool, ToolContext};
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

    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String {
        let Some(manager) = ctx.agent.cron_manager.as_ref() else {
            return "Error: cron not available in subagent".to_string();
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

    /// Cron tools stay available to Lead and subagents, but are withheld from
    /// teammates (s13: "do not bring s12 cron into teammate logic").
    fn available_for(&self, kind: AgentKind) -> bool {
        kind != AgentKind::Teammate
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

    async fn execute(&self, ctx: &ToolContext<'_>, _input: &Value) -> String {
        let Some(manager) = ctx.agent.cron_manager.as_ref() else {
            return "Error: cron not available in subagent".to_string();
        };

        let jobs = manager.list();
        if jobs.is_empty() {
            return "No cron jobs.".to_string();
        }

        jobs.iter()
            .map(|job| {
                let frequency = if job.recurring { "recurring" } else { "one-shot" };
                let storage = if job.durable { "durable" } else { "session" };
                let preview: String = job.prompt.chars().take(60).collect();
                format!(
                    "{}: {} -> {} [{}, {}]",
                    job.id, job.cron, preview, frequency, storage
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn available_for(&self, kind: AgentKind) -> bool {
        kind != AgentKind::Teammate
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

    async fn execute(&self, ctx: &ToolContext<'_>, input: &Value) -> String {
        let Some(manager) = ctx.agent.cron_manager.as_ref() else {
            return "Error: cron not available in subagent".to_string();
        };

        let job_id = input.get("job_id").and_then(|v| v.as_str()).unwrap_or("");
        match manager.cancel(job_id) {
            Ok(msg) => msg,
            Err(e) => format!("Error: {}", e),
        }
    }

    fn available_for(&self, kind: AgentKind) -> bool {
        kind != AgentKind::Teammate
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
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
    fn cron_matches_wildcard() {
        let time = Local.with_ymd_and_hms(2026, 8, 17, 9, 30, 0).unwrap();
        assert!(cron_matches("* * * * *", &time));
    }

    #[test]
    fn cron_matches_exact() {
        let t30 = Local.with_ymd_and_hms(2026, 8, 17, 9, 30, 0).unwrap();
        let t31 = Local.with_ymd_and_hms(2026, 8, 17, 9, 31, 0).unwrap();
        assert!(cron_matches("30 * * * *", &t30));
        assert!(!cron_matches("30 * * * *", &t31));
    }

    #[test]
    fn cron_matches_step() {
        let t30 = Local.with_ymd_and_hms(2026, 8, 17, 9, 30, 0).unwrap();
        let t31 = Local.with_ymd_and_hms(2026, 8, 17, 9, 31, 0).unwrap();
        assert!(cron_matches("*/5 * * * *", &t30)); // 30 % 5 == 0
        assert!(!cron_matches("*/5 * * * *", &t31));
    }

    #[test]
    fn cron_matches_range() {
        let t12 = Local.with_ymd_and_hms(2026, 8, 17, 9, 12, 0).unwrap();
        let t08 = Local.with_ymd_and_hms(2026, 8, 17, 9, 8, 0).unwrap();
        let t18 = Local.with_ymd_and_hms(2026, 8, 17, 9, 18, 0).unwrap();
        assert!(cron_matches("9-17 * * * *", &t12));
        assert!(!cron_matches("9-17 * * * *", &t08));
        assert!(!cron_matches("9-17 * * * *", &t18));
    }

    #[test]
    fn cron_matches_list() {
        let t30 = Local.with_ymd_and_hms(2026, 8, 17, 9, 30, 0).unwrap();
        let t10 = Local.with_ymd_and_hms(2026, 8, 17, 9, 10, 0).unwrap();
        assert!(cron_matches("0,15,30,45 * * * *", &t30));
        assert!(!cron_matches("0,15,30,45 * * * *", &t10));
    }

    #[test]
    fn cron_matches_daily() {
        let time = Local.with_ymd_and_hms(2026, 8, 19, 9, 0, 0).unwrap();
        assert!(cron_matches("0 9 * * *", &time));
        assert!(!cron_matches("0 10 * * *", &time));
    }

    #[test]
    fn cron_matches_weekday() {
        // Monday
        let time = Local.with_ymd_and_hms(2026, 8, 17, 9, 0, 0).unwrap();
        assert!(cron_matches("0 9 * * 1", &time)); // 1=Monday in cron
        assert!(!cron_matches("0 9 * * 6", &time)); // 6=Saturday in cron
    }

    #[test]
    fn cron_matches_day_or_weekday() {
        // 2026-08-17 is a Monday
        let time = Local.with_ymd_and_hms(2026, 8, 17, 9, 0, 0).unwrap();

        // Day match only
        assert!(cron_matches("0 9 17 * *", &time));

        // Weekday match only
        assert!(cron_matches("0 9 * * 1", &time));

        // Neither match
        assert!(!cron_matches("0 9 18 * *", &time));
        assert!(!cron_matches("0 9 * * 2", &time));
    }

    #[tokio::test]
    async fn schedule_and_list() {
        let dir = tempdir().unwrap();
        let manager = CronManager::new(dir.path().to_path_buf()).await.unwrap();

        let job = manager
            .schedule("0 9 * * *", "run tests", true, false)
            .unwrap();
        assert!(job.id.starts_with("cron_"));
        assert_eq!(job.cron, "0 9 * * *");
        assert_eq!(job.prompt, "run tests");

        let jobs = manager.list();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, job.id);
    }

    #[tokio::test]
    async fn schedule_invalid_cron() {
        let dir = tempdir().unwrap();
        let manager = CronManager::new(dir.path().to_path_buf()).await.unwrap();

        assert!(manager.schedule("* * *", "test", true, false).is_err());
    }

    #[tokio::test]
    async fn schedule_empty_prompt() {
        let dir = tempdir().unwrap();
        let manager = CronManager::new(dir.path().to_path_buf()).await.unwrap();

        assert!(manager.schedule("0 9 * * *", "", true, false).is_err());
    }

    #[tokio::test]
    async fn cancel_existing_job() {
        let dir = tempdir().unwrap();
        let manager = CronManager::new(dir.path().to_path_buf()).await.unwrap();

        let job = manager
            .schedule("0 9 * * *", "run tests", true, false)
            .unwrap();
        let result = manager.cancel(&job.id);
        assert!(result.is_ok());

        let jobs = manager.list();
        assert_eq!(jobs.len(), 0);
    }

    #[tokio::test]
    async fn cancel_nonexistent_job() {
        let dir = tempdir().unwrap();
        let manager = CronManager::new(dir.path().to_path_buf()).await.unwrap();

        let result = manager.cancel("cron_deadbeef");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[tokio::test]
    async fn save_and_load_durable() {
        let dir = tempdir().unwrap();
        let manager1 = CronManager::new(dir.path().to_path_buf()).await.unwrap();

        let job = manager1
            .schedule("0 9 * * *", "run tests", true, true)
            .unwrap();

        let jobs = manager1.list();
        assert_eq!(jobs.len(), 1);

        drop(manager1);

        let manager2 = CronManager::new(dir.path().to_path_buf()).await.unwrap();
        let loaded = manager2.load_durable().await.unwrap();
        assert_eq!(loaded, 1);

        let jobs = manager2.list();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, job.id);
        assert_eq!(jobs[0].cron, job.cron);
    }

    #[tokio::test]
    async fn consume_queue() {
        let dir = tempdir().unwrap();
        let manager = CronManager::new(dir.path().to_path_buf()).await.unwrap();

        let job = manager
            .schedule("0 9 * * *", "run tests", true, false)
            .unwrap();

        // 手动入队（测试可访问私有字段）
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

    #[tokio::test]
    async fn acknowledge_recurring_job() {
        let dir = tempdir().unwrap();
        let manager = CronManager::new(dir.path().to_path_buf()).await.unwrap();

        let job = manager
            .schedule("0 9 * * *", "run tests", true, false)
            .unwrap();

        // 标记为待交付（测试可访问私有字段）
        {
            let mut state = manager.state.lock().expect("state mutex poisoned");
            if let Some(j) = state.jobs.get_mut(&job.id) {
                j.pending_delivery = true;
            }
        }

        manager.acknowledge_jobs(&[job]).await.unwrap();

        let jobs = manager.list();
        assert_eq!(jobs.len(), 1);
        assert!(!jobs[0].pending_delivery);
    }

    #[tokio::test]
    async fn acknowledge_oneshot_job() {
        let dir = tempdir().unwrap();
        let manager = CronManager::new(dir.path().to_path_buf()).await.unwrap();

        let job = manager
            .schedule("0 9 * * *", "run tests", false, false)
            .unwrap();

        // 标记为待交付（测试可访问私有字段）
        {
            let mut state = manager.state.lock().expect("state mutex poisoned");
            if let Some(j) = state.jobs.get_mut(&job.id) {
                j.pending_delivery = true;
            }
        }

        manager.acknowledge_jobs(&[job]).await.unwrap();

        let jobs = manager.list();
        assert_eq!(jobs.len(), 0); // one-shot 任务被移除
    }
}
