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