//! Failure classes shared by the domain operations and the MCP adapter.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Code {
    InvalidInput,
    NotFound,
    Protected,
    AlreadyExists,
    StaleRevision,
    RequestIdReuse,
    CursorExpired,
    SemanticUnavailable,
    Unavailable,
    /// the change may be committed, and only a retry of the same request ID can tell
    OutcomeUncertain,
}

/// One rejected element of a request, such as a single change in a batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Problem {
    pub code: Code,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_revision: Option<String>,
}

impl Problem {
    pub fn new(code: Code, id: &str, message: impl Into<String>) -> Self {
        Self {
            code,
            id: Some(id.to_string()),
            message: message.into(),
            current_revision: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct WikiError {
    pub code: Code,
    pub message: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<Problem>,
}

impl WikiError {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: Vec::new(),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(Code::InvalidInput, message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(Code::Unavailable, message)
    }

    /// Reject a whole batch. The first problem names the class of the failure.
    pub fn rejected(details: Vec<Problem>) -> Self {
        let code = details.first().map_or(Code::InvalidInput, |p| p.code);
        Self {
            code,
            message: format!(
                "{} problem(s) found, and no change was applied",
                details.len()
            ),
            details,
        }
    }
}

pub type Result<T> = std::result::Result<T, WikiError>;
