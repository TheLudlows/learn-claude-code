use crate::tools::ToolDefinition;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::io::{self, Write};

/// Anthropic API 请求体
#[derive(Serialize)]
struct MessagesRequest {
    model: String,
    max_tokens: u32,
    stream: bool,
    system: String,
    messages: Vec<Message>,
    tools: Vec<ToolDefinition>,
}

/// 消息
#[derive(Serialize, Deserialize, Clone)]
pub struct Message {
    pub role: String,
    pub content: Vec<ContentBlock>,
}

/// 内容块
///
/// 请求和响应共用：序列化时按 `type` 打 tag（text / tool_use / tool_result），
/// 反序列化/累加时同理。main.rs 构造 Text / ToolResult，读取 ToolUse / Text。
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum ContentBlock {
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

/// Anthropic API 响应（流式累加后的完整结果）
#[derive(Debug)]
pub struct MessagesResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: String,
}

/// 封装 Anthropic API 交互。
pub struct Client {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
}

impl Client {
    pub fn new(api_key: String, base_url: String, model: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url,
            api_key,
            model,
        }
    }
    /// 流式调用 /v1/messages。
    ///
    /// text delta 边收边打到 stdout（live），tool_use 的 input_json delta 累加成
    /// 完整 JSON，最后返回 `MessagesResponse`。`agent_loop` 拿到后照旧判断 stop_reason。
    pub async fn stream_messages(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
        max_tokens: u32,
    ) -> Result<MessagesResponse, Box<dyn std::error::Error>> {
        // base_url 末尾的 '/' 会拼出 `//v1/messages`，先 trim 掉。
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let request = MessagesRequest {
            model: self.model.clone(),
            max_tokens,
            stream: true,
            system: system.to_string(),
            messages: messages.to_vec(),
            tools: tools.to_vec(),
        };

        let response = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            //.header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await?;

        // 非成功状态：先把 body 原样打出来，别让 serde 的 "error decoding response body"
        // 把真正的错误信息盖掉。那 92 字节里才写着 400 的真实原因。
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!("HTTP {} {} — {}", status, self.base_url, body).into());
        }

        // 流式解析 SSE（手写，不引 SSE crate，符合项目极简依赖风格）。
        // SSE 每个事件由若干行组成，以空行结束；我们只关心 `data:` 行。
        let mut stream = response.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut event_data = String::new();

        let mut content: Vec<ContentBlock> = Vec::new();
        let mut stop_reason = String::new();

        // 当前正在累加的 content block
        enum BlockAcc {
            Text(String),
            ToolUse {
                id: String,
                name: String,
                partial_json: String,
            },
        }
        let mut current: Option<BlockAcc> = None;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            buf.extend_from_slice(&chunk);

            // 处理所有完整行。按字节 `\n` 切分是安全的：`\n` (0x0A) 不会出现在
            // UTF-8 多字节序列的中间，所以每段完整行都是合法的 UTF-8。
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = buf.drain(..pos + 1).collect();
                let line = String::from_utf8_lossy(&line_bytes);
                let line = line.trim_end();

                if let Some(data) = line.strip_prefix("data: ") {
                    event_data.push_str(data);
                } else if let Some(data) = line.strip_prefix("data:") {
                    event_data.push_str(data);
                } else if line.is_empty() {
                    // 事件边界：解析累积的 data
                    if event_data.is_empty() {
                        continue;
                    }
                    let ev: serde_json::Value = match serde_json::from_str(&event_data) {
                        Ok(v) => v,
                        Err(e) => {
                            return Err(format!(
                                "invalid SSE event JSON: {} (raw: {})",
                                e, event_data
                            )
                            .into());
                        }
                    };
                    event_data.clear();
                    let ty = ev.get("type").and_then(|t| t.as_str()).unwrap_or("");

                    match ty {
                        "content_block_start" => {
                            let cb = ev
                                .get("content_block")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null);
                            let bty = cb.get("type").and_then(|t| t.as_str()).unwrap_or("");
                            match bty {
                                "text" => current = Some(BlockAcc::Text(String::new())),
                                "tool_use" => {
                                    let id = cb
                                        .get("id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let name = cb
                                        .get("name")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    current = Some(BlockAcc::ToolUse {
                                        id,
                                        name,
                                        partial_json: String::new(),
                                    });
                                }
                                _ => {}
                            }
                        }
                        "content_block_delta" => {
                            if let Some(acc) = current.as_mut() {
                                let delta = ev
                                    .get("delta")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Null);
                                let dty = delta
                                    .get("type")
                                    .and_then(|t| t.as_str())
                                    .unwrap_or("");
                                match (acc, dty) {
                                    (BlockAcc::Text(text_buf), "text_delta") => {
                                        let t = delta
                                            .get("text")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("");
                                        if text_buf.is_empty() {
                                            print!("\x1b[35magent\x1b[0m{}", t);
                                        } else {
                                            print!("{}", t);
                                        }
                                        io::stdout().flush().ok();
                                        text_buf.push_str(t);
                                    }
                                    (BlockAcc::ToolUse { partial_json, .. }, "input_json_delta") => {
                                        let p = delta
                                            .get("partial_json")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("");
                                        partial_json.push_str(p);
                                    }
                                    _ => {}
                                }
                            }
                        }
                        "content_block_stop" => {
                            if let Some(acc) = current.take() {
                                match acc {
                                    BlockAcc::Text(text) => {
                                        content.push(ContentBlock::Text { text });
                                    }
                                    BlockAcc::ToolUse {
                                        id,
                                        name,
                                        partial_json,
                                    } => {
                                        let input: serde_json::Value = if partial_json.is_empty() {
                                            serde_json::Value::Null
                                        } else {
                                            serde_json::from_str(&partial_json)
                                                .unwrap_or(serde_json::Value::Null)
                                        };
                                        content.push(ContentBlock::ToolUse { id, name, input });
                                    }
                                }
                            }
                        }
                        "message_delta" => {
                            if let Some(sr) = ev
                                .get("delta")
                                .and_then(|d| d.get("stop_reason"))
                                .and_then(|v| v.as_str())
                            {
                                stop_reason = sr.to_string();
                            }
                        }
                        "message_stop" => {}
                        "message_start" | "ping" => {}
                        "error" => {
                            let msg = ev
                                .get("error")
                                .and_then(|e| e.get("message"))
                                .and_then(|m| m.as_str())
                                .unwrap_or("stream error");
                            return Err(format!("stream error: {}", msg).into());
                        }
                        _ => {}
                    }
                }
                // `event:` / `:` 注释行忽略
            }
        }

        // 流可能在没有最后空行的情况下结束；残留若是 error 事件也要报出来
        if !event_data.is_empty() {
            if let Ok(ev) = serde_json::from_str::<serde_json::Value>(&event_data) {
                if ev.get("type").and_then(|t| t.as_str()) == Some("error") {
                    let msg = ev
                        .get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("stream error");
                    return Err(format!("stream error: {}", msg).into());
                }
            }
        }

        // 如果有文本输出，打印换行和所有响应
        let has_text = content.iter().any(|block| matches!(block, ContentBlock::Text { .. }));
        if has_text {
            println!();
        }
        println!("=== All Response Content ===");
        for block in &content {
            match block {
                ContentBlock::Text { text } => {
                    println!("Text: {}", text);
                }
                ContentBlock::ToolUse { id, name, input } => {
                    println!("ToolUse: id={}, name={}, input={}", id, name, serde_json::to_string(input).unwrap_or_default());
                }
                ContentBlock::ToolResult { tool_use_id, content: result_content } => {
                    println!("ToolResult: tool_use_id={}, content={}", tool_use_id, result_content);
                }
            }
        }
        println!("===========================");
        Ok(MessagesResponse { content, stop_reason })
    }
}
