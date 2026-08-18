/*
builtins.rs - 内置钩子集合

main.rs 默认注册的钩子集中于此(权限检查 PermissionHook 在 Task 3 迁入):
  ContextInjectHook  UserPromptSubmit  记录工作目录
  LargeOutputHook    PostToolUse       输出过大提醒
  SummaryHook        Stop              收尾工具次数统计
  TodoReminderHook   PostToolUse       3 轮未 todo_write 时提醒

均实现 hooks.rs 中对应 trait, 通过 hooks.on_* 注册。
*/

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::client::{ContentBlock, Message};
use crate::hooks::{PostToolHook, PromptHook, StopHook};
use crate::tools::workdir;

/// PostToolUse: 在 3 轮未使用 todo_write 时注入提醒。
/// 计数器为 owned 字段, 不再依赖 static 全局 —— 不同 Hooks 实例互不干扰。
pub struct TodoReminderHook {
    rounds_since_todo: AtomicUsize,
}

impl TodoReminderHook {
    pub fn new() -> Self {
        Self {
            rounds_since_todo: AtomicUsize::new(0),
        }
    }
}

impl Default for TodoReminderHook {
    fn default() -> Self {
        Self::new()
    }
}

impl PostToolHook for TodoReminderHook {
    fn on_post_tool(&self, name: &str, _input: &serde_json::Value, _output: &str) -> Option<String> {
        if name == "todo_write" {
            self.rounds_since_todo.store(0, Ordering::SeqCst);
            None
        } else {
            let count = self.rounds_since_todo.fetch_add(1, Ordering::SeqCst) + 1;
            if count >= 3 {
                self.rounds_since_todo.store(0, Ordering::SeqCst);
                Some("<reminder>Update your todos.</reminder>".to_string())
            } else {
                None
            }
        }
    }
}

/// UserPromptSubmit: 记录当前工作目录。
pub struct ContextInjectHook;

impl PromptHook for ContextInjectHook {
    fn on_prompt(&self, _query: &str) {
        println!(
            "\x1b[90m[HOOK] UserPromptSubmit: working in {}\x1b[0m",
            workdir().display()
        );
    }
}

/// PostToolUse: 输出过大时提醒。
pub struct LargeOutputHook;

impl PostToolHook for LargeOutputHook {
    fn on_post_tool(&self, name: &str, _input: &serde_json::Value, output: &str) -> Option<String> {
        if output.len() > 100_000 {
            println!(
                "\x1b[33m[HOOK] Large output from {}: {} chars\x1b[0m",
                name,
                output.len()
            );
        }
        None
    }
}

/// Stop: 收尾统计本轮用过的工具次数。
pub struct SummaryHook;

impl StopHook for SummaryHook {
    fn on_stop(&self, messages: &[Message]) -> Option<String> {
        let tool_count = messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter(|b| matches!(b, ContentBlock::ToolResult { .. }))
            .count();
        println!(
            "\x1b[90m[HOOK] Stop: session used {} tool calls\x1b[0m",
            tool_count
        );
        None
    }
}
