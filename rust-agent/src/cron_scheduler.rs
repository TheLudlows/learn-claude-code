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