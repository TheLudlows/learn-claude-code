use crate::error::AgentError;
use crate::tools::trait_def::ToolDefinition;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

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

impl Message {
    /// 单条 user 文本消息（最常见的形状：role=`user` + 一个 Text 块）。
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// 单条 assistant 文本消息。
    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// assistant 消息，直接包裹已有的 content 块（如把 API 响应回填进对话）。
    pub fn assistant_content(content: Vec<ContentBlock>) -> Self {
        Self {
            role: "assistant".to_string(),
            content,
        }
    }

    /// user 消息，直接包裹已有的 content 块（tool 结果 / 提醒等透传）。
    pub fn user_blocks(content: Vec<ContentBlock>) -> Self {
        Self {
            role: "user".to_string(),
            content,
        }
    }

    /// 返回一个 builder，用于在一则消息里混排 Text / ToolUse / ToolResult 块。
    pub fn builder() -> MessageBuilder {
        MessageBuilder::new()
    }
}

/// 按块拼装 `Message` 的 builder。
///
/// 常见形状优先用 `Message::user_text` / `assistant_text` / `assistant_content`
/// / `user_blocks` 等命名构造器；当一则消息里需要混排多个 Text / ToolUse /
/// ToolResult 块时，用本 builder 链式追加，最后 `.build()` 成 `Message`。
#[derive(Default)]
pub struct MessageBuilder {
    role: String,
    content: Vec<ContentBlock>,
}

impl MessageBuilder {
    /// 空 builder（role 与 content 均为空）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 设定角色为 user（等价于 `.role("user")`）。
    pub fn user(mut self) -> Self {
        self.role = "user".to_string();
        self
    }

    /// 设定角色为 assistant（等价于 `.role("assistant")`）。
    pub fn assistant(mut self) -> Self {
        self.role = "assistant".to_string();
        self
    }

    /// 设定角色。
    pub fn role(mut self, role: impl Into<String>) -> Self {
        self.role = role.into();
        self
    }

    /// 追加一个 Text 块。
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.content.push(ContentBlock::Text { text: text.into() });
        self
    }

    /// 追加一个 ToolUse 块。
    pub fn tool_use(
        mut self,
        id: impl Into<String>,
        name: impl Into<String>,
        input: serde_json::Value,
    ) -> Self {
        self.content.push(ContentBlock::ToolUse {
            id: id.into(),
            name: name.into(),
            input,
        });
        self
    }

    /// 追加一个 ToolResult 块。
    pub fn tool_result(
        mut self,
        tool_use_id: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        self.content.push(ContentBlock::ToolResult {
            tool_use_id: tool_use_id.into(),
            content: content.into(),
        });
        self
    }

    /// 追加任意一个 content 块。
    pub fn block(mut self, block: ContentBlock) -> Self {
        self.content.push(block);
        self
    }

    /// 用给定块整体替换 content。
    pub fn content(mut self, blocks: Vec<ContentBlock>) -> Self {
        self.content = blocks;
        self
    }

    /// 构造 `Message`。
    pub fn build(self) -> Message {
        Message {
            role: self.role,
            content: self.content,
        }
    }
}

/// Anthropic API 响应（流式累加后的完整结果）
#[derive(Debug)]
pub struct MessagesResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: String,
}

/// `stream_messages` 的三态返回值。
///
/// 将成功、上下文超限、其他错误分开，调用方可直接 pattern match：
/// - `Success` — 模型正常响应
/// - `PromptTooLong` — API 拒绝（上下文过长），调用方可压缩后重试
/// - `Failure` — 其他错误（网络、认证、解析等）
///
/// 不需要区分 prompt-too-long 的调用方用 `.into_response()?` 即可。
#[derive(Debug)]
pub enum CallResult {
    Success(MessagesResponse),
    PromptTooLong(AgentError),
    Failure(AgentError),
    Cancelled,
}

impl CallResult {
    /// 转为 `Result<MessagesResponse, AgentError>`，不区分 prompt-too-long。
    pub fn into_response(self) -> Result<MessagesResponse, AgentError> {
        match self {
            Self::Success(r) => Ok(r),
            Self::PromptTooLong(e) | Self::Failure(e) => Err(e),
            Self::Cancelled => Err(AgentError::Stream("Cancelled".to_string())),
        }
    }

    /// 是否为上下文超限（O(1) 判别，不扫字符串）。
    pub fn is_prompt_too_long(&self) -> bool {
        matches!(self, Self::PromptTooLong(_))
    }
}

/// 流式增量回调。
pub struct DeltaSink {
    cb: Box<dyn FnMut(Delta) + Send>,
}
/// 一条增量。
pub enum Delta { Text(String), ToolUseStart { id: String, name: String, input: serde_json::Value } }

impl DeltaSink {
    /// 生产构造：传入转发闭包。
    pub fn new(cb: impl FnMut(Delta) + Send + 'static) -> Self { Self { cb: Box::new(cb) } }
    /// 测试用收集器（仅累 text）。
    #[cfg(test)]
    pub fn collect() -> CollectSink { CollectSink::default() }
    pub fn feed(&mut self, d: Delta) { (self.cb)(d); }
}

/// 测试用文本收集器。
#[cfg(test)]
#[derive(Default)]
pub struct CollectSink {
    text: String,
}
#[cfg(test)]
impl CollectSink {
    pub fn drain_text(&mut self) -> String { std::mem::take(&mut self.text) }
    pub fn text(&mut self, t: &str) { self.text.push_str(t); }
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

    /// 把 AgentError 分类为 CallResult（prompt_too_long → PromptTooLong，否则 Failure）。
    fn classify_error(&self, err: AgentError) -> CallResult {
        if err.is_prompt_too_long() {
            CallResult::PromptTooLong(err)
        } else {
            CallResult::Failure(err)
        }
    }

    /// 流式调用 /v1/messages。
    ///
    /// 累加 text 与 tool_use 的 input_json delta，最后返回 `CallResult`。
    /// 本函数不直接打印——流式 delta 经 `DeltaSink` 回调交由调用方（Coordinator）渲染。
    /// `agent_loop` 拿到后照旧判断 stop_reason。
    pub async fn stream_messages(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
        max_tokens: u32,
        mut delta: Option<&mut DeltaSink>,
        cancel: CancellationToken,
    ) -> CallResult {
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

        // 粗估输入规模，用于诊断上下文增长：system 长度 + 历史消息里的文本/工具结果
        // 正文字符数（ToolUse 的 input 是 JSON Value，序列化成本高且非主要内容，跳过）。
        let system_chars = system.chars().count();
        let msg_chars: usize = messages
            .iter()
            .flat_map(|m| m.content.iter())
            .map(|b| match b {
                ContentBlock::Text { text } => text.chars().count(),
                ContentBlock::ToolResult { content, .. } => content.chars().count(),
                ContentBlock::ToolUse { .. } => 0,
            })
            .sum();
        tracing::info!(
            "[req] model={}, messages={}, tools={}, max_tokens={}, system_chars={}, input_chars={}",
            self.model,
            messages.len(),
            tools.len(),
            max_tokens,
            system_chars,
            system_chars + msg_chars
        );

        let response = match self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return self.classify_error(e.into()),
        };

        // 非成功状态：先把 body 原样打出来，别让 serde 的 "error decoding response body"
        // 把真正的错误信息盖掉。那 92 字节里才写着 400 的真实原因。
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return self.classify_error(AgentError::Api {
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
        // token 用量：message_start.message.usage 带 input_tokens（完整值），
        // message_delta.usage 带 output_tokens（累加后的最终值，覆盖 start 的初值）。
        // 某些网关不发 usage 字段，则保持 None，日志记为 `?`。
        let mut input_tokens: Option<u64> = None;
        let mut output_tokens: Option<u64> = None;

        let mut es_stream = es;
        loop {
            let event = tokio::select! {
                biased;
                _ = cancel.cancelled() => return CallResult::Cancelled,
                ev = es_stream.next() => match ev {
                    Some(Ok(e)) => e,
                    Some(Err(e)) => return self.classify_error(AgentError::Stream(e.to_string())),
                    None => return self.classify_error(AgentError::Stream("stream ended unexpectedly".to_string())),
                },
            };

            // SSE 每个事件由若干行组成；eventsource-stream 已处理行分割和事件边界，
            // 我们只拿到完整的 data 字段。Anthropic 每个事件只发一条 `data:` 行。
            let ev: serde_json::Value = match serde_json::from_str(&event.data) {
                Ok(v) => v,
                Err(e) => {
                    return self.classify_error(AgentError::InvalidResponse(format!(
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
                        let delta_val = ev
                            .get("delta")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null);
                        let dty = delta_val
                            .get("type")
                            .and_then(|t| t.as_str())
                            .unwrap_or("");
                        match (acc, dty) {
                            (BlockAcc::Text(text_buf), "text_delta") => {
                                let t = delta_val
                                    .get("text")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                text_buf.push_str(t);
                                // 发送 delta 到 sink
                                if let Some(sink) = delta.as_mut() {
                                    sink.feed(Delta::Text(t.to_string()));
                                }
                            }
                            (BlockAcc::ToolUse { partial_json, .. }, "input_json_delta") => {
                                let p = delta_val
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
                                // 发送 ToolUseStart delta
                                if let Some(sink) = delta.as_mut() {
                                    sink.feed(Delta::ToolUseStart {
                                        id: id.clone(),
                                        name: name.clone(),
                                        input: input.clone(),
                                    });
                                }
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
                    // message_delta.usage.output_tokens 是累加后的最终输出 token 数，
                    // 覆盖 message_start 给出的初值。
                    if let Some(ot) = ev
                        .get("usage")
                        .and_then(|u| u.get("output_tokens"))
                        .and_then(|v| v.as_u64())
                    {
                        output_tokens = Some(ot);
                    }
                }
                "message_stop" => {}
                "message_start" => {
                    // message_start.message.usage 带 input_tokens（完整）与 output_tokens
                    // （初值，之后被 message_delta 的最终值覆盖）。
                    if let Some(usage) = ev.get("message").and_then(|m| m.get("usage")) {
                        if let Some(it) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
                            input_tokens = Some(it);
                        }
                        if let Some(ot) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                            output_tokens = Some(ot);
                        }
                    }
                }
                "ping" => {}
                "error" => {
                    let msg = ev
                        .get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("stream error");
                    return self.classify_error(AgentError::Stream(msg.to_string()));
                }
                _ => {}
            }
        }

        // 拆分块类型 + 收集本轮调用的工具名，让"模型这一轮做了什么"一目了然。
        let text_blocks = content
            .iter()
            .filter(|b| matches!(b, ContentBlock::Text { .. }))
            .count();
        let tool_names: Vec<&str> = content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        tracing::info!(
            "[resp] stop_reason={}, blocks={}, text={}, tool_use={}, tools=[{}], input_tokens={}, output_tokens={}",
            stop_reason,
            content.len(),
            text_blocks,
            tool_names.len(),
            tool_names.join(","),
            input_tokens.map(|n| n.to_string()).unwrap_or_else(|| "?".into()),
            output_tokens.map(|n| n.to_string()).unwrap_or_else(|| "?".into()),
        );

        CallResult::Success(MessagesResponse { content, stop_reason })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_result_into_response_success() {
        let r = MessagesResponse {
            content: vec![],
            stop_reason: "end_turn".into(),
        };
        assert!(CallResult::Success(r).into_response().is_ok());
    }

    #[test]
    fn call_result_into_response_prompt_too_long() {
        let e = AgentError::Api {
            status: 400,
            body: "prompt_too_long".into(),
        };
        assert!(CallResult::PromptTooLong(e).into_response().is_err());
    }

    #[test]
    fn call_result_into_response_failure() {
        let e = AgentError::Other("network down".into());
        assert!(CallResult::Failure(e).into_response().is_err());
    }

    #[test]
    fn call_result_is_prompt_too_long() {
        let e = AgentError::Api {
            status: 400,
            body: "prompt_too_long".into(),
        };
        assert!(CallResult::PromptTooLong(e).is_prompt_too_long());

        let r = MessagesResponse {
            content: vec![],
            stop_reason: "end_turn".into(),
        };
        assert!(!CallResult::Success(r).is_prompt_too_long());

        let e = AgentError::Other("something".into());
        assert!(!CallResult::Failure(e).is_prompt_too_long());
    }

    #[test]
    fn classify_error_prompt_too_long() {
        let client = Client::new("key".into(), "https://api.example.com".into(), "model".into());
        let err = AgentError::Api {
            status: 400,
            body: "error: prompt_too_long".into(),
        };
        assert!(matches!(
            client.classify_error(err),
            CallResult::PromptTooLong(_)
        ));
    }

    #[test]
    fn classify_error_other() {
        let client = Client::new("key".into(), "https://api.example.com".into(), "model".into());
        let err = AgentError::Other("random failure".into());
        assert!(matches!(client.classify_error(err), CallResult::Failure(_)));
    }

    #[test]
    fn delta_sink_collects_text_deltas() {
        let mut sink = DeltaSink::collect();
        sink.text("foo");
        sink.text("bar");
        assert_eq!(sink.drain_text(), "foobar");
    }

    #[test]
    fn call_result_cancelled_variant_exists() {
        let r = CallResult::Cancelled;
        assert!(matches!(r, CallResult::Cancelled));
    }

    #[test]
    fn call_result_into_response_cancelled() {
        assert!(CallResult::Cancelled.into_response().is_err());
    }
}
