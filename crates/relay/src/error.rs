//! Relay error type.

use std::fmt;

/// Errors produced by the relay layer.
#[derive(Debug)]
pub enum RelayError {
    /// A retention or relay configuration is internally inconsistent.
    Config(String),
    /// The storage backend failed.
    Backend(String),
    /// A serialized event id could not be parsed.
    InvalidEventId(String),
}

impl fmt::Display for RelayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RelayError::Config(message) => write!(f, "invalid relay config: {message}"),
            RelayError::Backend(message) => write!(f, "relay backend error: {message}"),
            RelayError::InvalidEventId(message) => write!(f, "invalid event id: {message}"),
        }
    }
}

impl std::error::Error for RelayError {}

impl RelayError {
    /// Convenience constructor for a configuration error.
    pub fn config(message: impl Into<String>) -> Self {
        RelayError::Config(message.into())
    }

    /// Convenience constructor for a backend error.
    pub fn backend(message: impl Into<String>) -> Self {
        RelayError::Backend(message.into())
    }
}

/// Result alias for the relay layer.
pub type Result<T> = std::result::Result<T, RelayError>;
