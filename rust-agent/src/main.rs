/*
s02_tool_use.rs - Tool Use (Rust)

The agent loop from s01 does not change. This lesson adds four tools
and a dispatch map:

    +----------+      +-------+      +--------------------------+
    |   User   | ---> |  LLM  | ---> | Tool Dispatch            |
    |  prompt  |      |       |      | bash       -> run_bash   |
    +----------+      +---+---+      | read_file  -> run_read   |
                          ^          | write_file -> run_write  |
                          |          | edit_file  -> run_edit   |
                          +----------| glob       -> run_glob   |
                          tool_result+--------------------------+

  + run_read / run_write / run_edit / run_glob
  + TOOL_HANDLERS (dispatch_tool) instead of a hard-coded run_bash call
  + safe_path to keep file tools inside the workspace

Key insight: the loop stays the same; only tool registration and dispatch grow.
*/

mod tools;

use dotenv::dotenv;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::env;
use std::io::{self, Write};
use tools::{dispatch_tool, get_tool_definitions, ToolDefinition};

/// Anthropic API 请求体
#[derive(Serialize)]
struct MessagesRequest {
    model: String,
    max_tokens: u32,
    system: String,
    messages: Vec<Message>,
    tools: Vec<ToolDefinition>,
}

/// 消息
#[derive(Serialize, Deserialize, Clone)]
struct Message {
    role: String,
    content: Vec<ContentBlock>,
}

/// 内容块
#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "type")]
enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
    },
}

/// Anthropic API 响应
#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
    stop_reason: String,
}

/// Agent 核心循环
async fn agent_loop(
    client: &Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    system: &str,
    messages: &mut Vec<Message>,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        let request = MessagesRequest {
            model: model.to_string(),
            max_tokens: 8000,
            system: system.to_string(),
            messages: messages.clone(),
            tools: get_tool_definitions(),
        };

        let response = client
            .post(&format!("{}/v1/messages", base_url))
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await?;

        let response: MessagesResponse = response.json().await?;

        // 添加助手响应
        messages.push(Message {
            role: "assistant".to_string(),
            content: response.content.clone(),
        });

        // 检查是否需要调用工具
        if response.stop_reason != "tool_use" {
            break;
        }

        // 执行工具调用 - 使用工具分发机制
        let mut tool_results = Vec::new();
        for block in &response.content {
            if let ContentBlock::ToolUse { id, name, input } = block {
                print!("\x1b[33m> {}\x1b[0m", name);
                io::stdout().flush().unwrap();

                let output = dispatch_tool(name, input);
                println!("{}", &output[..output.len().min(200)]);

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

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv().ok();

    println!("rust-agent: s02 Tool Use (Rust)");
    println!("Enter a question, press Enter to send. Type q to quit.\n");

    let api_key = env::var("ANTHROPIC_AUTH_TOKEN")
        .or_else(|_| env::var("ANTHROPIC_API_KEY"))?;
    let base_url = env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| "https://api.anthropic.com".to_string());
    let model = env::var("MODEL_ID")?;
    println!("api-key: {}, base_url {}, mode {}", api_key, base_url, model);
    let client = Client::new();

    let cwd = env::current_dir()
        .unwrap_or_else(|_| ".".into())
        .to_string_lossy()
        .to_string();
    let system = format!("You are a coding agent at {}. Use tools to solve tasks. Act, don't explain.", cwd);

    let mut messages: Vec<Message> = Vec::new();

    loop {
        print!("\x1b[36magent >> \x1b[0m");
        io::stdout().flush().unwrap();

        let mut query = String::new();
        io::stdin().read_line(&mut query)?;
        let query = query.trim();

        if query.eq_ignore_ascii_case("q") || query == "exit" || query.is_empty() {
            break;
        }

        messages.push(Message {
            role: "user".to_string(),
            content: vec![ContentBlock::Text {
                text: query.to_string(),
            }],
        });

        if let Err(e) = agent_loop(&client, &base_url, &api_key, &model, &system, &mut messages).await {
            eprintln!("Error: {}", e);
        }

        // 打印最终响应
        if let Some(last_message) = messages.last() {
            for block in &last_message.content {
                if let ContentBlock::Text { text } = block {
                    println!("{}", text);
                }
            }
        }
        println!();
    }

    Ok(())
}
