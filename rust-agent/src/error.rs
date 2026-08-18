/*
error.rs - Unified error types for the agent

Provides a typed error enum to replace `Box<dyn Error>` and ad-hoc error strings.
Covers LLM API errors; tool-layer errors still return `String` for simplicity,
but can migrate to `AgentError` incrementally.
*/

use std::fmt;

/// Unified error type for the agent.
///
/// Variants:
/// - `Api`: HTTP-level error from the LLM provider (non-2xx status).
/// - `Network`: Transport / connection error (DNS, TLS, timeout at TCP level).
/// - `Stream`: SSE stream-level error (protocol violation, server-sent error event).
/// - `Timeout`: Operation exceeded its deadline.
/// - `InvalidResponse`: Malformed response (bad JSON, missing fields).
/// - `Other`: Catch-all for errors not yet classified (io, config, etc.).
#[derive(Debug)]
pub enum AgentError {
    /// HTTP error from the LLM API (non-2xx response).
    Api {
        status: u16,
        body: String,
    },
    /// Network / transport error.
    Network(reqwest::Error),
    /// SSE stream-level error.
    Stream(String),
    /// Operation timed out.
    Timeout {
        seconds: u64,
    },
    /// Invalid / malformed response.
    InvalidResponse(String),
    /// Catch-all.
    Other(String),
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Api { status, body } => write!(f, "API error (HTTP {}): {}", status, body),
            Self::Network(e) => write!(f, "Network error: {}", e),
            Self::Stream(msg) => write!(f, "Stream error: {}", msg),
            Self::Timeout { seconds } => write!(f, "Operation timed out after {}s", seconds),
            Self::InvalidResponse(msg) => write!(f, "Invalid response: {}", msg),
            Self::Other(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for AgentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Network(e) => Some(e),
            _ => None,
        }
    }
}

// ---- From conversions ----

impl From<reqwest::Error> for AgentError {
    fn from(e: reqwest::Error) -> Self {
        Self::Network(e)
    }
}

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
}
