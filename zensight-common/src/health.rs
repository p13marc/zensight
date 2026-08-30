//! Health and liveness types for frontend consumption.
//!
//! These types mirror the sensor-framework health types but are designed
//! for deserialization in the frontend without requiring the full framework.

use serde::{Deserialize, Serialize};

/// Sensor health status.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    /// Sensor is fully operational, all devices healthy.
    #[default]
    Healthy,
    /// Sensor is partially operational, some devices failing.
    Degraded,
    /// Sensor has critical issues, no devices responding.
    Unhealthy,
    /// Sensor is starting up.
    Starting,
    /// Sensor encountered a fatal error.
    Error,
    /// Sensor is gone: its liveliness token disappeared (clean shutdown or
    /// lease expiry after a crash). Set by the frontend, never published by
    /// sensors themselves.
    Offline,
}

impl std::fmt::Display for HealthStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HealthStatus::Healthy => write!(f, "healthy"),
            HealthStatus::Degraded => write!(f, "degraded"),
            HealthStatus::Unhealthy => write!(f, "unhealthy"),
            HealthStatus::Starting => write!(f, "starting"),
            HealthStatus::Error => write!(f, "error"),
            HealthStatus::Offline => write!(f, "offline"),
        }
    }
}

/// Device availability status.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum DeviceStatus {
    /// Device is responding normally.
    Online,
    /// Device is not responding.
    Offline,
    /// Device is responding but with errors.
    Degraded,
    /// Device status is unknown (never polled).
    #[default]
    Unknown,
}

impl std::fmt::Display for DeviceStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceStatus::Online => write!(f, "online"),
            DeviceStatus::Offline => write!(f, "offline"),
            DeviceStatus::Degraded => write!(f, "degraded"),
            DeviceStatus::Unknown => write!(f, "unknown"),
        }
    }
}

/// Health snapshot from a sensor.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HealthSnapshot {
    /// Sensor name.
    pub sensor: String,
    /// Overall health status.
    pub status: HealthStatus,
    /// Uptime in seconds.
    pub uptime_secs: u64,
    /// Total devices configured.
    pub devices_total: u64,
    /// Devices currently responding.
    pub devices_responding: u64,
    /// Devices currently failed.
    pub devices_failed: u64,
    /// Last poll duration in milliseconds.
    pub last_poll_duration_ms: u64,
    /// Errors in the last hour.
    pub errors_last_hour: u64,
    /// Total metrics published.
    pub metrics_published: u64,
    /// Hashed machine-id of the publishing host (identity envelope, #301).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    /// The `<source>` id of the publishing sensor instance (payload-only in
    /// v1 — the origin chunk scopes the health doc key). Optional for
    /// mixed-fleet/persisted payloads predating the host-scoped keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Self-measured resource telemetry (#811): the fields that let the
    /// platform notice its *own* growth. Absent on payloads from older
    /// sensors — absent always reads as *not measured*, never as zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_stats: Option<SelfStats>,
}

/// A sensor's self-measured resource usage (#811), collected on the health
/// tick from `/proc/self/*`, the publish counters, and any registered table
/// providers. Every field is optional and serde-defaulted: mixed-version
/// fleets are normal, and a missing field must read as *not asked*.
///
/// Motivating incident (2026-08-17): a sensor bundle grew 110→355 MB RSS on a
/// 1 GB VM, was OOM-killed, and reported `Healthy` throughout — by every
/// question the health doc knew how to ask, it was.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SelfStats {
    /// Resident set size, bytes (`/proc/self/status` `VmRSS`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rss_bytes: Option<u64>,
    /// Virtual size, bytes (`VmSize`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vsz_bytes: Option<u64>,
    /// CPU busy fraction since the previous health tick, percent of one core.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_percent: Option<f64>,
    /// What the sensor was told it may use (declared, not enforced — the
    /// enforcement ladder is #812). Absent = no budget declared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_bytes: Option<u64>,
    /// Publications through the baseline declared-publisher path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_total: Option<u64>,
    /// Payload bytes through the same path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_bytes_total: Option<u64>,
    /// Samples the sensor chose not to publish (shed, rate-limited) — fed by
    /// the sensor, counted only where wired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped_total: Option<u64>,
    /// Entries evicted from bounded tables — fed by the sensor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evicted_total: Option<u64>,
    /// Per-table occupancy, from providers the sensor registered. This is
    /// the field that turns "the sensor is big" into "the flow table is
    /// 280 MB of it".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tables: Vec<TableStats>,
    /// cgroup-v2 context, when the sensor runs in one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroup: Option<CgroupSelf>,
    /// The memory governor's shed-ladder state (#812). Absent when no ladder
    /// is armed (no budget declared or discovered) or on older sensors —
    /// absent reads as *no ladder*, never as "step 0".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ladder: Option<LadderState>,
}

/// The shed ladder's published state (#812): which rung is active and what
/// has been shed to stay inside the budget. A silently degraded sensor is a
/// lying sensor — every step is reported, every tick.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LadderState {
    /// 0 nominal, 1 evicting, 2 degraded (optional work stopped),
    /// 3 saturated (everything shed and still over budget — the loudest
    /// possible report; dying is not on the ladder).
    pub step: u8,
    /// When the current step was entered (epoch ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<i64>,
    /// Cumulative per-table evictions since the ladder last left step 0.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evicted: Vec<LadderEviction>,
    /// Degradables currently applied (by registered name).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub degraded: Vec<String>,
    /// Human account of the pressure and the response, e.g. "rss 31 MiB at
    /// 97% of 32 MiB budget; evicted 4096 entries (2.1 MiB) from dns_inventory".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// What the ladder evicted from one table (#812) — honest counts of what was
/// actually freed, not what was requested.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LadderEviction {
    pub table: String,
    pub entries: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

/// One bounded table's occupancy and capacity (#811), in entries and — where
/// the owner can say — bytes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TableStats {
    pub name: String,
    pub entries: u64,
    /// Estimated bytes held; absent when the owner cannot say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity_entries: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity_bytes: Option<u64>,
}

/// The sensor's own cgroup-v2 memory context (#811) — what the operator's
/// drop-in actually allows, read from `/sys/fs/cgroup<own path>`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CgroupSelf {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_current_bytes: Option<u64>,
    /// `memory.max`; `None` also when the literal is `max` (unlimited).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_max_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_high_bytes: Option<u64>,
    /// `memory.events` `oom_kill`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oom_kills: Option<u64>,
    /// `memory.events` `oom`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oom_events: Option<u64>,
}

/// Device liveness information.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DeviceLiveness {
    /// Device identifier.
    pub device: String,
    /// Current status.
    pub status: DeviceStatus,
    /// Last seen timestamp (millis since epoch).
    pub last_seen: i64,
    /// Consecutive failures count.
    pub consecutive_failures: u32,
    /// Last error message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// Error type classification.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ErrorType {
    /// Connection timeout.
    Timeout,
    /// Authentication failed.
    AuthFailed,
    /// Connection refused.
    ConnectionRefused,
    /// Connection reset.
    ConnectionReset,
    /// Parse/decode error.
    ParseError,
    /// Protocol error.
    ProtocolError,
    /// Configuration error.
    ConfigError,
    /// Other/unknown error.
    #[default]
    Other,
}

/// Error report from a sensor.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ErrorReport {
    /// Timestamp (millis since epoch).
    pub timestamp: i64,
    /// Device identifier (if applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// Error type classification.
    pub error_type: ErrorType,
    /// Error message.
    pub message: String,
    /// Whether the error is retryable.
    pub retryable: bool,
}

/// Sensor registration/discovery record, published by every sensor's runner on
/// the registration doc `state/<producer>/sensor` (identity envelope, #301).
/// Health and device counts live on `state/<producer>/health`, not here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SensorInfo {
    /// Sensor name (e.g., "sysinfo", "netlink").
    pub name: String,
    /// Sensor crate version.
    pub version: String,
    /// Producer name this sensor publishes under (e.g. "netlink").
    pub producer: String,
    /// Unified host/source id (the `<source>` telemetry key segment).
    pub source: String,
    /// Hashed machine-id (never the raw id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    /// Kernel boot id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fqdn: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ips: Vec<String>,
    /// Free-form sensor metadata (device counts, listener addresses, …) —
    /// carried on the registration doc since the legacy `@/status` document
    /// retired with the v1 cutover (the health doc absorbs the running flag).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub macs: Vec<String>,
    /// Unix epoch millis of the latest re-emission.
    pub last_updated: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_device_status_default() {
        assert_eq!(DeviceStatus::default(), DeviceStatus::Unknown);
    }

    #[test]
    fn test_device_status_display() {
        assert_eq!(format!("{}", DeviceStatus::Online), "online");
        assert_eq!(format!("{}", DeviceStatus::Offline), "offline");
        assert_eq!(format!("{}", DeviceStatus::Degraded), "degraded");
        assert_eq!(format!("{}", DeviceStatus::Unknown), "unknown");
    }

    #[test]
    fn test_health_snapshot_deserialize() {
        let json = r#"{
            "sensor": "snmp",
            "status": "healthy",
            "uptime_secs": 3600,
            "devices_total": 10,
            "devices_responding": 9,
            "devices_failed": 1,
            "last_poll_duration_ms": 150,
            "errors_last_hour": 5,
            "metrics_published": 1000
        }"#;

        let snapshot: HealthSnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(snapshot.sensor, "snmp");
        assert_eq!(snapshot.status, HealthStatus::Healthy);
        assert_eq!(snapshot.devices_total, 10);
        // An old-sensor payload (no self_stats key at all): absent, not zeroed
        // and not an error — mixed-version fleets are normal (#811).
        assert_eq!(snapshot.self_stats, None);
    }

    /// #811: the self-telemetry payload's optionality discipline.
    #[test]
    fn self_stats_roundtrip_and_absent_fields_stay_absent() {
        let stats = SelfStats {
            rss_bytes: Some(355 * 1024 * 1024),
            budget_bytes: Some(400 * 1024 * 1024),
            tables: vec![TableStats {
                name: "flow_inventory".into(),
                entries: 1_200_000,
                bytes: Some(280 * 1024 * 1024),
                capacity_entries: None,
                capacity_bytes: Some(16 * 1024 * 1024),
            }],
            ..Default::default()
        };
        let json = serde_json::to_value(&stats).unwrap();
        // Unmeasured fields are ABSENT on the wire, not null/zero.
        assert!(json.get("cpu_percent").is_none());
        assert!(json.get("dropped_total").is_none());
        assert!(json.get("cgroup").is_none());
        let back: SelfStats = serde_json::from_value(json).unwrap();
        assert_eq!(back, stats);
        // And a partial payload decodes with everything else defaulted.
        let partial: SelfStats = serde_json::from_str(r#"{"rss_bytes": 1024}"#).unwrap();
        assert_eq!(partial.rss_bytes, Some(1024));
        assert_eq!(partial.cpu_percent, None);
        assert!(partial.tables.is_empty());
        // #812: no ladder in the payload = no ladder, never "step 0".
        assert_eq!(partial.ladder, None);
    }

    /// #812: the ladder state rides `self_stats` additively and its own
    /// optional fields stay absent when unset.
    #[test]
    fn ladder_state_roundtrip_and_absence() {
        let stats = SelfStats {
            ladder: Some(LadderState {
                step: 2,
                since_ms: Some(1_700_000_000_000),
                evicted: vec![LadderEviction {
                    table: "tls_inventory".into(),
                    entries: 4096,
                    bytes: Some(2 * 1024 * 1024),
                }],
                degraded: vec!["anomaly_detectors".into()],
                reason: Some("rss 31 MiB at 97% of 32 MiB budget".into()),
            }),
            ..Default::default()
        };
        let json = serde_json::to_value(&stats).unwrap();
        let back: SelfStats = serde_json::from_value(json).unwrap();
        assert_eq!(back, stats);
        // A minimal ladder payload (old producer mid-incident, new fields
        // unknown) decodes with defaults.
        let thin: LadderState = serde_json::from_str(r#"{"step": 1}"#).unwrap();
        assert_eq!(thin.step, 1);
        assert!(thin.evicted.is_empty() && thin.degraded.is_empty());
        assert_eq!(thin.since_ms, None);
        // An armed-but-nominal ladder serializes without the empty vectors.
        let nominal = serde_json::to_value(LadderState::default()).unwrap();
        assert!(nominal.get("evicted").is_none());
        assert!(nominal.get("degraded").is_none());
        assert!(nominal.get("reason").is_none());
    }

    #[test]
    fn test_device_liveness_deserialize() {
        let json = r#"{
            "device": "router01",
            "status": "online",
            "last_seen": 1703500000000,
            "consecutive_failures": 0
        }"#;

        let liveness: DeviceLiveness = serde_json::from_str(json).unwrap();
        assert_eq!(liveness.device, "router01");
        assert_eq!(liveness.status, DeviceStatus::Online);
        assert!(liveness.last_error.is_none());
    }

    #[test]
    fn test_sensor_info_roundtrip() {
        let json = r#"{
            "name": "sysinfo",
            "version": "0.6.2",
            "producer": "sysinfo",
            "source": "host1",
            "host_id": "abcd",
            "ips": ["10.0.0.1"],
            "last_updated": 1703500000000
        }"#;

        let info: SensorInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.source, "host1");
        assert_eq!(info.host_id.as_deref(), Some("abcd"));
        // Absent optional fields default cleanly.
        assert!(info.macs.is_empty());
        assert_eq!(info.boot_id, None);
    }
}
