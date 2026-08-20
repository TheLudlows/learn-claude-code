/*
error.rs - Unified error types for the agent

Provides a typed error enum to replace `Box<dyn Error>` and ad-hoc error strings.
Covers LLM API errors, tool errors, file system errors, and validation errors.
*/

/// Unified error type for the agent.
///
/// Variants:
/// - `Api`: HTTP-level error from the LLM provider (non-2xx status).
/// - `Network`: Transport / connection error (DNS, TLS, timeout at TCP level).
/// - `Stream`: SSE stream-level error (protocol violation, server-sent error event).
/// - `Timeout`: Operation exceeded its deadline.
/// - `InvalidResponse`: Malformed response (bad JSON, missing fields).
/// - `ToolNotFound`: Tool was not found in the registry.
/// - `ToolRejected`: Tool was rejected (e.g., in subagent context).
/// - `ToolDenied`: Tool execution was denied by a pre-tool hook.
/// - `ToolExecution`: Tool execution failed with a specific error message.
/// - `PathTraversal`: Path attempt to escape the workspace.
/// - `FileSystem`: File system related error.
/// - `Validation`: Input validation failed.
/// - `Other`: Catch-all for errors not yet classified (io, config, etc.).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum AgentError {
    /// HTTP error from the LLM API (non-2xx response).
    #[error("API error (HTTP {status}): {body}")]
    Api { status: u16, body: String },

    /// Network / transport error.
    #[error("Network error: {0}")]
    Network(String),

    /// SSE stream-level error.
    #[error("Stream error: {0}")]
    Stream(String),

    /// Operation timed out.
    #[error("Operation timed out after {seconds}s")]
    Timeout { seconds: u64 },

    /// Invalid / malformed response.
    #[error("Invalid response: {0}")]
    InvalidResponse(String),

    /// Tool was not found in the registry.
    #[error("Tool '{name}' not found. Available tools: {available}")]
    ToolNotFound { name: String, available: String },

    /// Tool was rejected (e.g., in subagent context).
    #[error("Tool '{name}' rejected: {reason}")]
    ToolRejected { name: String, reason: String },

    /// Tool execution was denied by a pre-tool hook.
    #[error("Tool '{name}' denied: {reason}")]
    ToolDenied { name: String, reason: String },

    /// Tool execution failed with a specific error message.
    #[error("Tool '{name}' execution failed: {reason}")]
    ToolExecution { name: String, reason: String },

    /// Path attempt to escape the workspace.
    #[error("Path '{path}' escapes workspace")]
    PathTraversal { path: String },

    /// File system related error.
    #[error("File system error: {0}")]
    FileSystem(String),

    /// Input validation failed.
    #[error("Validation error: {0}")]
    Validation(String),

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


impl From<std::io::Error> for AgentError {
    fn from(e: std::io::Error) -> Self {
        Self::FileSystem(e.to_string())
    }
}

impl From<reqwest::Error> for AgentError {
    fn from(e: reqwest::Error) -> Self {
        Self::Network(e.to_string())
    }
}

impl From<std::env::VarError> for AgentError {
    fn from(e: std::env::VarError) -> Self {
        Self::Validation(format!("Environment variable error: {}", e))
    }
}

impl From<serde_json::Error> for AgentError {
    fn from(e: serde_json::Error) -> Self {
        Self::Validation(format!("JSON error: {}", e))
    }
}

impl From<Box<dyn std::error::Error>> for AgentError {
    fn from(e: Box<dyn std::error::Error>) -> Self {
        Self::Other(e.to_string())
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
        // From<io::Error> 把 io 错误映射为 FileSystem（error.rs:100），测试据此断言。
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let err: AgentError = io_err.into();
        assert!(matches!(err, AgentError::FileSystem(_)));
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

}
