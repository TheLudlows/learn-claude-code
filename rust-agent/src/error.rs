/*
error.rs - Unified error types for the agent

Provides a typed error enum to replace `Box<dyn Error>` and ad-hoc error strings.
Covers LLM API errors; tool-layer errors still return `String` for simplicity,
but can migrate to `AgentError` incrementally.
*/

/// Unified error type for the agent.
///
/// 使用 `thiserror` derive 宏自动生成 `Display`、`Error`、`From` 实现，
/// 替代原来 72 行手写样板代码。
///
/// Variants:
/// - `Api`: HTTP-level error from the LLM provider (non-2xx status).
/// - `Network`: Transport / connection error (DNS, TLS, timeout at TCP level).
/// - `Stream`: SSE stream-level error (protocol violation, server-sent error event).
/// - `Timeout`: Operation exceeded its deadline.
/// - `InvalidResponse`: Malformed response (bad JSON, missing fields).
/// - `Other`: Catch-all for errors not yet classified (io, config, etc.).
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// HTTP error from the LLM API (non-2xx response).
    #[error("API error (HTTP {status}): {body}")]
    Api { status: u16, body: String },

    /// Network / transport error.
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),

    /// SSE stream-level error.
    #[error("Stream error: {0}")]
    Stream(String),

    /// Operation timed out.
    #[error("Operation timed out after {seconds}s")]
    Timeout { seconds: u64 },

    /// Invalid / malformed response.
    #[error("Invalid response: {0}")]
    InvalidResponse(String),

    /// Catch-all.
    #[error("{0}")]
    Other(String),
}

impl AgentError {
    /// Check if this error indicates the prompt was too long for the model's context.
    ///
    /// Used by the agent loop to trigger reactive compaction: when the API rejects
    /// a request due to context length, we can summarize history and retry once.
    pub fn is_prompt_too_long(&self) -> bool {
        let msg = match self {
            Self::Api { body, .. } => body.to_lowercase(),
            Self::Stream(msg) => msg.to_lowercase(),
            Self::Other(msg) => msg.to_lowercase(),
            _ => return false,
        };
        msg.contains("prompt_too_long")
            || msg.contains("too many tokens")
            || msg.contains("request_too_large")
    }
}

// ---- From conversions (非 #[from] 可处理的类型) ----
//
// 以下 From 实现将各种错误类型统一转为 `Other(String)`，
// 因为 `Other` 变体是 `String` 而非源错误类型，thiserror 的 `#[from]` 无法自动推导。

impl From<std::io::Error> for AgentError {
    fn from(e: std::io::Error) -> Self {
        Self::Other(e.to_string())
    }
}

/// Allows `?` to convert `Result<T, Box<dyn Error>>` into `Result<T, AgentError>`.
impl From<Box<dyn std::error::Error>> for AgentError {
    fn from(e: Box<dyn std::error::Error>) -> Self {
        Self::Other(e.to_string())
    }
}

impl From<std::env::VarError> for AgentError {
    fn from(e: std::env::VarError) -> Self {
        Self::Other(format!("Environment variable error: {}", e))
    }
}

impl From<serde_json::Error> for AgentError {
    fn from(e: serde_json::Error) -> Self {
        Self::Other(format!("JSON error: {}", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_api_error() {
        let err = AgentError::Api {
            status: 429,
            body: "rate limited".into(),
        };
        assert_eq!(err.to_string(), "API error (HTTP 429): rate limited");
    }

    #[test]
    fn display_timeout() {
        let err = AgentError::Timeout { seconds: 30 };
        assert_eq!(err.to_string(), "Operation timed out after 30s");
    }

    #[test]
    fn display_stream_error() {
        let err = AgentError::Stream("bad JSON".into());
        assert_eq!(err.to_string(), "Stream error: bad JSON");
    }

    #[test]
    fn display_other() {
        let err = AgentError::Other("something went wrong".into());
        assert_eq!(err.to_string(), "something went wrong");
    }

    #[test]
    fn from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let err: AgentError = io_err.into();
        assert!(matches!(err, AgentError::Other(_)));
        assert!(err.to_string().contains("file missing"));
    }

    #[test]
    fn from_boxed_error() {
        let boxed: Box<dyn std::error::Error> = "generic failure".into();
        let err: AgentError = boxed.into();
        assert!(matches!(err, AgentError::Other(_)));
        assert!(err.to_string().contains("generic failure"));
    }

    #[test]
    fn is_prompt_too_long_api_variant() {
        let err = AgentError::Api {
            status: 400,
            body: "prompt_too_long: request exceeds maximum context".into(),
        };
        assert!(err.is_prompt_too_long());

        let err2 = AgentError::Api {
            status: 400,
            body: "too many tokens in request".into(),
        };
        assert!(err2.is_prompt_too_long());

        let err3 = AgentError::Api {
            status: 400,
            body: "request_too_large".into(),
        };
        assert!(err3.is_prompt_too_long());

        // Case insensitive
        let err4 = AgentError::Api {
            status: 400,
            body: "PROMPT_TOO_LONG".into(),
        };
        assert!(err4.is_prompt_too_long());
    }

    #[test]
    fn is_prompt_too_long_negative() {
        let err = AgentError::Api {
            status: 401,
            body: "unauthorized".into(),
        };
        assert!(!err.is_prompt_too_long());

        let err2 = AgentError::Stream("connection closed".into());
        assert!(!err2.is_prompt_too_long());

        let err3 = AgentError::Timeout { seconds: 30 };
        assert!(!err3.is_prompt_too_long());

        let err4 = AgentError::Other("some other error".into());
        assert!(!err4.is_prompt_too_long());
    }

    #[test]
    fn from_serde_json_error() {
        let json_err = serde_json::from_str::<serde_json::Value>("invalid json").unwrap_err();
        let err: AgentError = json_err.into();
        assert!(matches!(err, AgentError::Other(_)));
        assert!(err.to_string().contains("JSON error"));
    }
}
