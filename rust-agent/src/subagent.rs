/*
subagent.rs - Subagent (s06)

子 agent 核心思想：用全新的 messages[] 运行嵌套循环，只返回最终文本。

关键边界：
- 消息隔离：父子对话不共享历史
- 文件系统共享：同一进程和 WORKDIR
- 无递归委托：SUB_TOOLS 不含 task 工具
- 权限共享：使用相同的 hooks 和权限检查
*/

use crate::client::{Client, ContentBlock, Message};
use crate::hooks::Hooks;
use crate::tools::{dispatch_tool, get_subagent_tool_definitions};

/// 子 agent 的最大轮数限制
const MAX_SUBAGENT_TURNS: usize = 30;

/// 子 agent 的 system prompt
const SUB_SYSTEM: &str = "You are a focused coding agent. Complete your task efficiently. Use tools as needed. Return a concise summary of your work. you can ";

/// 提取响应中的最终文本（不含 tool_use）
fn extract_final_text(content: &[ContentBlock]) -> Option<String> {
    content
        .iter()
        .filter_map(|block| {
            if let ContentBlock::Text { text } = block {
                Some(text.clone())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        .into()
}

/// 子 agent 循环
///
/// 运行一个全新的 agent 循环，最多 MAX_SUBAGENT_TURNS 轮。
/// 只返回最终文本给父进程，中间对话被丢弃。
pub async fn run_subagent_loop(
    client: &Client,
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
        for block in &response.content {
            if let ContentBlock::ToolUse { id, name, input } = block {
                print!("\x1b[90m[sub] {} {:?}\x1b[0m\n", name, input);

                // 触发 PreToolUse hook（共享权限检查）
                if let Some(reason) = hooks.trigger_pre_tool(name, input) {
                    tool_results.push(ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: reason,
                    });
                    continue;
                }

                let output = dispatch_tool(name, input);

                // 触发 PostToolUse hook
                if let Some(msg) = hooks.trigger_post_tool(name, input, &output) {
                    tool_results.push(ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: msg,
                    });
                    continue;
                }

                tool_results.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: output,
                });
            }
        }

        // 添加工具结果
        messages.push(Message {
            role: "user".to_string(),
            content: tool_results,
        });
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