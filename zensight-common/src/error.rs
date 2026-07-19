use thiserror::Error;

/// Common error type for ZenSight components.
#[derive(Debug, Error)]
pub enum Error {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Zenoh error: {0}")]
    Zenoh(#[from] zenoh::Error),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("CBOR serialization error: {0}")]
    Cbor(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Invalid key expression: {0}")]
    KeyExpr(String),

    /// A registry `type` name with no entry in the RFC 08 §5 type table
    /// (`schema::SCHEMAS`) — the payload cannot be decoded because nothing
    /// in this build knows what it is.
    #[error("Unknown payload type {0:?} — not in the RFC 08 §5 type table")]
    UnknownPayloadType(String),
}

impl From<ciborium::ser::Error<std::io::Error>> for Error {
    fn from(e: ciborium::ser::Error<std::io::Error>) -> Self {
        Error::Cbor(e.to_string())
    }
}

impl From<ciborium::de::Error<std::io::Error>> for Error {
    fn from(e: ciborium::de::Error<std::io::Error>) -> Self {
        Error::Cbor(e.to_string())
    }
}

/// Result type alias using ZenSight's Error.
pub type Result<T> = std::result::Result<T, Error>;
