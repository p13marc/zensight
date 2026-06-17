//! Error types for the sensor framework.

use thiserror::Error;

/// Result type alias using [`SensorError`].
pub type Result<T> = std::result::Result<T, SensorError>;

/// Errors that can occur in a sensor.
#[derive(Error, Debug)]
pub enum SensorError {
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

    /// Zenoh publish error.
    #[error("Zenoh publish error on '{key}': {message}")]
    ZenohPublish { key: String, message: String },

    /// Zenoh subscription error.
    #[error("Zenoh subscription error on '{key_expr}': {message}")]
    ZenohSubscription { key_expr: String, message: String },

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

impl SensorError {
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

    /// Create a Zenoh publish error.
    pub fn zenoh_publish(key: impl Into<String>, message: impl Into<String>) -> Self {
        Self::ZenohPublish {
            key: key.into(),
            message: message.into(),
        }
    }

    /// Create a Zenoh subscription error.
    pub fn zenoh_subscription(key_expr: impl Into<String>, message: impl Into<String>) -> Self {
        Self::ZenohSubscription {
            key_expr: key_expr.into(),
            message: message.into(),
        }
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

impl From<zenoh::Error> for SensorError {
    fn from(err: zenoh::Error) -> Self {
        Self::ZenohSession(err.to_string())
    }
}

impl From<serde_json::Error> for SensorError {
    fn from(err: serde_json::Error) -> Self {
        Self::Serialization(err.to_string())
    }
}

impl From<json5::Error> for SensorError {
    fn from(err: json5::Error) -> Self {
        Self::ConfigParse(err.to_string())
    }
}
