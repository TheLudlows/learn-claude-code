use crate::error::AgentError;
use crate::tools::trait_def::ToolDefinition;
use eventsource_stream::{EventStreamError, Eventsource};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

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
    /// 累加 text 与 tool_use 的 input_json delta，最后返回 `MessagesResponse`。
    /// 本函数不打印任何内容——展示交给 `output::render`，由调用方拿到响应后调用。
    /// `agent_loop` 拿到后照旧判断 stop_reason。
    pub async fn stream_messages(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
        max_tokens: u32,
    ) -> Result<MessagesResponse, AgentError> {
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
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await?;

        // 非成功状态：先把 body 原样打出来，别让 serde 的 "error decoding response body"
        // 把真正的错误信息盖掉。那 92 字节里才写着 400 的真实原因。
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(AgentError::Api {
                status: status.as_u16(),
                body: format!("{} — {}", self.base_url, body),
            });
        }

        // 流式解析 SSE：用 eventsource-stream 处理协议层（行分割、事件边界），
        // 我们只负责解析每个事件的 JSON data。
        let bytes_stream = response.bytes_stream();
        let mut es = bytes_stream.eventsource();

        let mut content: Vec<ContentBlock> = Vec::new();
        // 初始化为 "unknown" 而非空串：若流异常结束且未收到 message_delta 事件，
        // 空串会因 `"" != "tool_use"` 静默退出循环，掩盖协议错误。
        // "unknown" 同样不触发工具分支，但留下可识别的哨兵值，便于上层诊断。
        let mut stop_reason = String::from("unknown");

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

        while let Some(event) = es.next().await {
            let event = event
                .map_err(|e: EventStreamError<reqwest::Error>| AgentError::Stream(e.to_string()))?;

            // SSE 每个事件由若干行组成；eventsource-stream 已处理行分割和事件边界，
            // 我们只拿到完整的 data 字段。Anthropic 每个事件只发一条 `data:` 行。
            let ev: serde_json::Value = match serde_json::from_str(&event.data) {
                Ok(v) => v,
                Err(e) => {
                    return Err(AgentError::InvalidResponse(format!(
                        "SSE JSON parse error: {} (raw: {})",
                        e, event.data
                    )));
                }
            };
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
                    return Err(AgentError::Stream(msg.to_string()));
                }
                _ => {}
            }
        }

        Ok(MessagesResponse { content, stop_reason })
    }
}
