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
pub mod artifact;
pub mod bandwidth;
pub mod command;
pub mod comparison;
pub mod config;
pub mod entity;
pub mod error;
pub mod evidence;
pub mod health;
pub mod keyexpr;
pub mod metric_guard;
pub mod payload;
pub mod publisher_registry;
pub mod qos;
pub mod query_detail;
pub mod semconv;
pub mod serialization;
pub mod session;
pub mod stream;
pub mod telemetry;

// Re-export commonly used types at the crate root
pub use alert::{Alert, AlertKind, AlertSeverity, AlertState};
pub use artifact::{
    ArtifactKind, ArtifactOptions, ArtifactRequest, ArtifactState, ArtifactStatus, Delivery, Entry,
    KindAdvert, KindStatus, Manifest, TreeIndex, TreeSummary,
};
pub use bandwidth::{
    BandwidthKey, BandwidthRecord, BandwidthSource, ByteSemantics, LABEL_SEMANTICS, LABEL_SOURCE,
    ProtoScope, bandwidth_query_key,
};
pub use command::{
    Command, artifact_blob_prefix, artifact_cancel_key, artifact_request_key, artifact_status_key,
    artifact_store_prefix, artifact_tree_prefix, command_key, status_key,
};
pub use comparison::ComparisonOp;
pub use config::{
    ArtifactLimits, ArtifactReportLimits, ArtifactSnapshotLimits, BaseConfig, CommonArtifactLimits,
    IdentityConfig, LinkProfile, LogFormat, LoggingConfig, SnapshotDir, ZenohConfig, load_config,
    parse_config,
};
pub use entity::{AliasRecord, HostEntity, MemberClaim, NameVal, PdnsRecord};
pub use error::{Error, Result};
pub use evidence::{CloudFacts, HostEvidence, NameObservation};
pub use health::{
    DeviceLiveness, DeviceStatus, ErrorReport, ErrorType, HealthSnapshot, HealthStatus, SensorInfo,
};
pub use keyexpr::{
    alias_key, all_alerts_wildcard, all_device_liveliness_wildcard, all_entity_wildcard,
    all_evidence_wildcard, all_health_wildcard, all_liveliness_wildcard,
    all_name_evidence_wildcard, all_pdns_wildcard, all_state_wildcard, all_telemetry_wildcard,
    catalog_claim_key, catalog_claims_wildcard, catalog_rpc_key, correlator_alive_key,
    entities_query_key, entity_key, fleet_blob_prefix, fleet_command_key, fleet_rpc_key,
    host_evidence_key, is_telemetry_key, media_preview_key, media_video_key, name_observation_key,
    names_query_key, origin_rpc_key, parse_full_key, parse_key, pdns_key, refine_full_key,
    refine_key, validate_relative_selector,
};
pub use payload::{PAYLOAD_TYPES, decode_payload, schema_location};
pub use publisher_registry::PublisherRegistry;
pub use qos::QosClass;
pub use query_detail::{
    AssetRecord, CaptureRecord, CgroupNode, CgroupPid, DnsRecord, ElephantRecord,
    EncryptedDnsRecord, FlowRecord, HistBucket, Histogram, HttpHostRecord, Ja4hRecord,
    LatencyReport, LogRecord, MatrixRecord, NameInfo, NeighborRecord, NetflowFieldValue,
    NetflowRecord, ProcessRecord, QuicRecord, RouteRecord, SocketRecord, SshRecord, TalkerRecord,
    TimerRecord, TlsRecord, UnitDetail, UnitRecord,
};
pub use serialization::{Format, decode, decode_auto, encode};
pub use session::connect;
pub use stream::{FrameMeta, StreamControl, StreamDescriptor, StreamStatus};
pub use telemetry::{Protocol, TelemetryPoint, TelemetryValue, current_timestamp_millis};
/// The registry's *parse* direction, re-exported so consumers get it without a
/// direct `zensight-keyspace` dependency (RFC 08 §1, issue #475).
pub use zensight_keyspace::CommonState;

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
