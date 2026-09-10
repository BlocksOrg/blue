//! Shared error type for the harness client crates.

use std::path::PathBuf;

/// The one error type used across the client crates. Backend services
/// (`services/*`) define their own error types.
#[derive(Debug, thiserror::Error)]
pub enum GhError {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("io error: {0}")]
    IoBare(#[from] std::io::Error),

    #[error("config error: {0}")]
    Config(String),

    #[error("unknown harness: {requested:?} (known: {known})")]
    UnknownHarness { requested: String, known: String },

    /// A harness was requested that is not in the service's `allowed_harnesses`.
    #[error("harness `{requested}` is not allowed by policy (allowed: {allowed})")]
    HarnessNotAllowed { requested: String, allowed: String },

    #[error("harness `{0}` was not found on PATH")]
    HarnessNotInstalled(String),

    #[error("service error: {0}")]
    Service(String),

    /// The control service rejected the session (401/403). Distinct from
    /// `Service` because a rejected session must never fail-soft to cache:
    /// a stale cache cannot re-authenticate the user, and serving it hides
    /// the one thing they need to be told.
    #[error("{0}")]
    Unauthorized(String),

    /// The control service answered, understood the request, and needs the
    /// user to run a command before it can be served (409). Also never
    /// fail-soft: the cache cannot satisfy the prerequisite either.
    #[error("{0}")]
    ActionRequired(String),

    #[error("serialization error: {0}")]
    Serde(String),

    #[error("{0}")]
    Other(String),
}

impl GhError {
    pub fn config(msg: impl Into<String>) -> Self {
        GhError::Config(msg.into())
    }
    pub fn other(msg: impl Into<String>) -> Self {
        GhError::Other(msg.into())
    }
    pub fn service(msg: impl Into<String>) -> Self {
        GhError::Service(msg.into())
    }
    pub fn unauthorized(msg: impl Into<String>) -> Self {
        GhError::Unauthorized(msg.into())
    }
    pub fn action_required(msg: impl Into<String>) -> Self {
        GhError::ActionRequired(msg.into())
    }
}

pub type Result<T> = std::result::Result<T, GhError>;
