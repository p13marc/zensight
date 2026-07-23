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
pub mod event;
pub mod evidence;
pub mod health;
pub mod interfaces;
pub mod keyexpr;
pub mod metric_guard;
pub mod publisher_registry;
pub mod qos;
pub mod query_detail;
pub mod registry;
pub mod rpc;
pub mod schema;
pub mod semconv;
pub mod serialization;
pub mod session;
pub mod state;
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
pub use entity::{
    AliasRecord, AssertionKind, HostEntity, MemberClaim, NameVal, OperatorAssertion, PdnsRecord,
};
pub use error::{Error, Result};
pub use event::EventRecord;
pub use evidence::{CloudFacts, HostEvidence, NameObservation};
pub use health::{
    DeviceLiveness, DeviceStatus, ErrorReport, ErrorType, HealthSnapshot, HealthStatus, SensorInfo,
};
pub use interfaces::{IfStatus, InterfaceCounters, InterfaceEntry, InterfaceRates, InterfaceTable};
pub use keyexpr::{
    alias_key, all_alerts_wildcard, all_alias_wildcard, all_assertion_wildcard,
    all_device_liveliness_wildcard, all_entity_wildcard, all_evidence_wildcard,
    all_health_wildcard, all_liveliness_wildcard, all_name_evidence_wildcard, all_pdns_wildcard,
    all_state_wildcard, all_telemetry_wildcard, assertion_key, catalog_claim_key,
    catalog_claims_wildcard, catalog_rpc_key, correlator_alive_key, entities_query_key, entity_key,
    fleet_blob_prefix, fleet_command_key, fleet_rpc_key, host_evidence_key, is_telemetry_key,
    media_preview_key, media_video_key, name_observation_key, names_query_key, origin_rpc_key,
    parse_full_key, parse_key, pdns_key, refine_full_key, refine_key, validate_relative_selector,
};
pub use publisher_registry::PublisherRegistry;
pub use qos::QosClass;
pub use query_detail::{
    AssetRecord, CaptureRecord, CgroupNode, CgroupPid, DnsRecord, ElephantRecord,
    EncryptedDnsRecord, FlowRecord, HistBucket, Histogram, HttpHostRecord, Ja4hRecord,
    LatencyReport, LogRecord, MatrixRecord, NameInfo, NeighborRecord, NetflowFieldValue,
    NetflowRecord, ProcessRecord, QuicRecord, RouteRecord, SocketRecord, SshRecord, TalkerRecord,
    TimerRecord, TlsRecord, UnitDetail, UnitRecord,
};
pub use rpc::{
    ERR_BUSY, ERR_GATED, ERR_INVALID_ARGS, ERR_NOT_FOUND, ERR_UNAUTHORIZED, ERR_UNSUPPORTED,
    RpcError, RpcRequest, RpcResult,
};
pub use serialization::{Format, decode, decode_auto, encode};
pub use session::connect;
pub use state::ZensightState;
pub use stream::{FrameMeta, StreamControl, StreamDescriptor, StreamStatus};
pub use telemetry::{Protocol, TelemetryPoint, TelemetryValue, current_timestamp_millis};
/// The registry's *parse* direction, re-exported so consumers get it without a
/// direct `zenkey` dependency (RFC 08 §1, issue #475).
pub use zenkey::CommonState;

/// ZenSight's application profile (RFC 11 §1): the application name and the
/// RFC 06 §1 origin salt, plus the once-per-process host-origin mint. The
/// deployment *base* is deliberately not an application constant — it names
/// the deployment, not the software, and is optional session configuration
/// (`zenoh.namespace` / `ZENSIGHT_ZENOH_NAMESPACE`; see [`CONVENTIONAL_BASE`]).
pub static PROFILE: zenkey::AppProfile = zenkey::AppProfile::new("zensight", "zensight-host-id-v1");

/// The *conventional example* base (session namespace, RFC 03 §1.1).
///
/// **Not a default.** The base names a deployment; the default is *empty* —
/// no session namespace, keys at the bus root, Zenoh's own default — and a
/// deployment sets one (`zenoh.namespace` in config, or
/// `ZENSIGHT_ZENOH_NAMESPACE`) to isolate itself on shared infrastructure.
/// Keys built through `zenkey` are base-relative (they start at `v1`); the
/// base is added on egress and stripped on ingress by the Zenoh session
/// namespace. Only heuristic diagnostics (which can catch nothing but the
/// conventional spelling) and un-namespaced debug tools may read this —
/// application code building keys never spells the base.
pub const CONVENTIONAL_BASE: &str = "zensight";

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
