//! ZenSight Common Library
//!
//! This crate provides shared types and utilities for ZenSight observability sensors:
//!
//! - [`telemetry`] - Common telemetry data model (`TelemetryPoint`, `TelemetryValue`, `Protocol`)
//! - [`serialization`] - JSON/CBOR encoding and decoding
//! - [`config`] - Configuration loading (JSON5 format)
//! - [`session`] - Zenoh session management
//! - [`keyexpr`] - Key expression builders and parsers
//! - [`error`] - Error types

pub mod alert;
pub mod command;
pub mod comparison;
pub mod config;
pub mod error;
pub mod health;
pub mod keyexpr;
pub mod query_detail;
pub mod serialization;
pub mod session;
pub mod telemetry;

// Re-export commonly used types at the crate root
pub use alert::{Alert, AlertKind, AlertSeverity, AlertState};
pub use command::{Command, command_key, status_key};
pub use comparison::ComparisonOp;
pub use config::{BaseConfig, LogFormat, LoggingConfig, ZenohConfig, load_config, parse_config};
pub use error::{Error, Result};
pub use health::{
    CorrelationEntry, DeviceLiveness, DeviceStatus, ErrorReport, ErrorType, HealthSnapshot,
    HealthStatus, SensorInfo,
};
pub use keyexpr::{
    KEY_PREFIX, KeyExprBuilder, ParseError, ParsedKeyExpr, all_alerts_wildcard,
    all_correlation_wildcard, all_errors_wildcard, all_health_wildcard, all_liveness_wildcard,
    all_sensors_wildcard, all_telemetry_wildcard, parse_key_expr,
};
pub use query_detail::{NeighborRecord, RouteRecord, SocketRecord};
pub use serialization::{Format, decode, decode_auto, encode};
pub use session::connect;
pub use telemetry::{Protocol, TelemetryPoint, TelemetryValue, current_timestamp_millis};

/// Initialize tracing with the given configuration.
///
/// Supports two output formats:
/// - `LogFormat::Text` (default): Human-readable text format
/// - `LogFormat::Json`: Structured JSON format for log aggregation systems
///
/// # Example
///
/// ```ignore
/// use zensight_common::{LoggingConfig, LogFormat, init_tracing};
///
/// let config = LoggingConfig {
///     level: "info".to_string(),
///     format: LogFormat::Json,
/// };
/// init_tracing(&config)?;
/// ```
pub fn init_tracing(config: &LoggingConfig) -> Result<()> {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.level));

    match config.format {
        LogFormat::Text => {
            tracing_subscriber::registry()
                .with(fmt::layer())
                .with(filter)
                .try_init()
                .map_err(|e| Error::Config(format!("Failed to initialize tracing: {}", e)))?;
        }
        LogFormat::Json => {
            tracing_subscriber::registry()
                .with(fmt::layer().json())
                .with(filter)
                .try_init()
                .map_err(|e| Error::Config(format!("Failed to initialize tracing: {}", e)))?;
        }
    }

    Ok(())
}
