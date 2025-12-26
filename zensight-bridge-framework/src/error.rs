//! Error types for the bridge framework.

use thiserror::Error;

/// Result type alias using [`BridgeError`].
pub type Result<T> = std::result::Result<T, BridgeError>;

/// Errors that can occur in a bridge.
#[derive(Error, Debug)]
pub enum BridgeError {
    /// Configuration error.
    #[error("Configuration error: {0}")]
    Config(String),

    /// Configuration file not found.
    #[error("Configuration file not found: {path}")]
    ConfigNotFound { path: String },

    /// Configuration parse error.
    #[error("Failed to parse configuration: {0}")]
    ConfigParse(String),

    /// Configuration validation error.
    #[error("Configuration validation failed: {0}")]
    ConfigValidation(String),

    /// Zenoh connection error.
    #[error("Zenoh connection error: {0}")]
    ZenohConnection(String),

    /// Zenoh session error.
    #[error("Zenoh session error: {0}")]
    ZenohSession(String),

    /// Serialization error.
    #[error("Serialization error: {0}")]
    Serialization(String),

    /// Publishing error.
    #[error("Failed to publish to {key}: {message}")]
    Publish { key: String, message: String },

    /// Liveliness token error.
    #[error("Liveliness error: {0}")]
    Liveliness(String),

    /// Worker error.
    #[error("Worker error: {0}")]
    Worker(String),

    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Generic error with context.
    #[error("{context}: {source}")]
    WithContext {
        context: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl BridgeError {
    /// Create a configuration error.
    pub fn config(msg: impl Into<String>) -> Self {
        Self::Config(msg.into())
    }

    /// Create a configuration validation error.
    pub fn validation(msg: impl Into<String>) -> Self {
        Self::ConfigValidation(msg.into())
    }

    /// Create a worker error.
    pub fn worker(msg: impl Into<String>) -> Self {
        Self::Worker(msg.into())
    }

    /// Create a liveliness error.
    pub fn liveliness(msg: impl Into<String>) -> Self {
        Self::Liveliness(msg.into())
    }

    /// Wrap an error with context.
    pub fn with_context<E>(context: impl Into<String>, source: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::WithContext {
            context: context.into(),
            source: Box::new(source),
        }
    }
}

impl From<zenoh::Error> for BridgeError {
    fn from(err: zenoh::Error) -> Self {
        Self::ZenohSession(err.to_string())
    }
}

impl From<serde_json::Error> for BridgeError {
    fn from(err: serde_json::Error) -> Self {
        Self::Serialization(err.to_string())
    }
}

impl From<json5::Error> for BridgeError {
    fn from(err: json5::Error) -> Self {
        Self::ConfigParse(err.to_string())
    }
}
