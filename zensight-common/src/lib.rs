//! ZenSight Common Library
//!
//! This crate provides shared types and utilities for ZenSight observability bridges:
//!
//! - [`telemetry`] - Common telemetry data model (`TelemetryPoint`, `TelemetryValue`, `Protocol`)
//! - [`serialization`] - JSON/CBOR encoding and decoding
//! - [`config`] - Configuration loading (JSON5 format)
//! - [`session`] - Zenoh session management
//! - [`keyexpr`] - Key expression builders and parsers
//! - [`error`] - Error types

pub mod config;
pub mod error;
pub mod keyexpr;
pub mod serialization;
pub mod session;
pub mod telemetry;

// Re-export commonly used types at the crate root
pub use config::{BaseConfig, LoggingConfig, ZenohConfig, load_config, parse_config};
pub use error::{Error, Result};
pub use keyexpr::{
    KEY_PREFIX, KeyExprBuilder, ParsedKeyExpr, all_telemetry_wildcard, parse_key_expr,
};
pub use serialization::{Format, decode, decode_auto, encode};
pub use session::connect;
pub use telemetry::{Protocol, TelemetryPoint, TelemetryValue};

/// Initialize tracing with the given log level.
pub fn init_tracing(config: &LoggingConfig) -> Result<()> {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.level));

    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(filter)
        .try_init()
        .map_err(|e| Error::Config(format!("Failed to initialize tracing: {}", e)))?;

    Ok(())
}
