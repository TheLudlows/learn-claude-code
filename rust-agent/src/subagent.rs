/*
子 agent 核心思想：用全新的 messages[] 运行嵌套循环，只返回最终文本。

关键边界：
- 消息隔离：父子对话不共享历史
- 文件系统共享：同一进程和 WORKDIR
- 无递归委托：SUB_TOOLS 不含 task 工具
- 权限共享：使用相同的 hooks 和权限检查
*/

use crate::client::{Client, ContentBlock, Message};
use crate::hooks::{assemble_post_tool_messages, Hooks};
use crate::tools::registry::ToolRegistry;
use crate::tools_legacy::{dispatch_tool, get_subagent_tool_definitions};

/// 子 agent 的最大轮数限制
const MAX_SUBAGENT_TURNS: usize = 30;

/// 子 agent 的 system prompt
const SUB_SYSTEM: &str = "You are a focused coding agent. Complete your task efficiently. Use tools as needed. Return a concise summary of your work.";

/// 提取响应中的最终文本（不含 tool_use）。
///
/// 无 Text 块时返回 `None`，使调用方的 `else { "(no summary)" }` 分支可达。
/// （原实现末尾 `.into()` 借助 `From<T> for Option<T>` 恒返回 `Some`，连空串也是
/// `Some("")`，导致 `else` 成为死代码、最终无文本时返回空串而非 "(no summary)"。）
fn extract_final_text(content: &[ContentBlock]) -> Option<String> {
    let texts: Vec<String> = content
        .iter()
        .filter_map(|block| {
            if let ContentBlock::Text { text } = block {
                Some(text.clone())
            } else {
                None
            }
        })
        .collect();
    if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n"))
    }
}

/// 子 agent 循环
///
/// 运行一个全新的 agent 循环，最多 MAX_SUBAGENT_TURNS 轮。
/// 只返回最终文本给父进程，中间对话被丢弃。
pub async fn run_subagent_loop(
    client: &Client,
    registry: &ToolRegistry,
    prompt: &str,
    hooks: &Hooks,
) -> Result<String, Box<dyn std::error::Error>> {
    // 子 agent 从全新的消息列表开始
    let mut messages: Vec<Message> = vec![Message {
        role: "user".to_string(),
        content: vec![ContentBlock::Text {
            text: prompt.to_string(),
        }],
    }];

    println!("\x1b[33m[Subagent started]\x1b[0m");

    for _turn in 1..=MAX_SUBAGENT_TURNS {
        let response = client
            .stream_messages(
                SUB_SYSTEM,
                &messages,
                &get_subagent_tool_definitions(),
                8000,
            )
            .await?;

        // 添加助手响应
        messages.push(Message {
            role: "assistant".to_string(),
            content: response.content.clone(),
        });

        // 检查是否需要调用工具
        if response.stop_reason != "tool_use" {
            // s06: 触发 Stop 钩子，如果返回消息则继续循环
            if let Some(force) = hooks.trigger_stop(&messages) {
                messages.push(Message {
                    role: "user".to_string(),
                    content: vec![ContentBlock::Text { text: force }],
                });
                continue;
            }

            // 循环结束，提取并返回最终文本
            if let Some(text) = extract_final_text(&response.content) {
                println!("\x1b[33m[Subagent done]\x1b[0m");
                return Ok(text);
            } else {
                println!("\x1b[33m[Subagent done - no text]\x1b[0m");
                return Ok("(no summary)".to_string());
            }
        }

        // 执行工具调用
        let mut tool_results = Vec::new();
        let mut reminders: Vec<String> = Vec::new();
        for block in &response.content {
            if let ContentBlock::ToolUse { id, name, input } = block {
                println!("\x1b[90m[sub] {} {:?}\x1b[0m", name, input);

                // 触发 PreToolUse hook（共享权限检查）
                if let Some(reason) = hooks.trigger_pre_tool(registry, name, input) {
                    tool_results.push(ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: reason,
                    });
                    continue;
                }

                let output = dispatch_tool(name, input);

                // PostToolUse: 提醒作为独立 user 消息注入，不进 tool_result
                if let Some(msg) = hooks.trigger_post_tool(name, input, &output) {
                    reminders.push(msg);
                }

                tool_results.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: output,
                });
            }
        }

        // 添加工具结果（真实输出）+ PostToolUse 提醒（独立 user 消息）
        messages.extend(assemble_post_tool_messages(tool_results, reminders));
    }

    // 超过最大轮数
    println!(
        "\x1b[33m[Subagent stopped after {} turns without final answer]\x1b[0m",
        MAX_SUBAGENT_TURNS
    );
    Ok(format!(
        "Subagent stopped after {} turns without a final answer.",
        MAX_SUBAGENT_TURNS
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_final_text_none_when_no_text_blocks() {
        // C7 回归：无 Text 块时必须返回 None（原实现恒返回 Some，连空串也是 Some("")）。
        assert_eq!(extract_final_text(&[]), None);

        // 仅 ToolUse 块、无 Text 块 -> None
        let only_tool = vec![ContentBlock::ToolUse {
            id: "t1".to_string(),
            name: "command".to_string(),
            input: serde_json::json!({}),
        }];
        assert_eq!(extract_final_text(&only_tool), None);
    }

    #[test]
    fn extract_final_text_some_when_text_present() {
        let blocks = vec![
            ContentBlock::Text { text: "hello".to_string() },
            ContentBlock::Text { text: "world".to_string() },
        ];
        assert_eq!(extract_final_text(&blocks), Some("hello\nworld".to_string()));
    }
}