use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::error::{Error, Result};
use crate::serialization::Format;

/// Common Zenoh connection configuration.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ZenohConfig {
    /// Zenoh mode: "client", "peer", or "router".
    #[serde(default = "default_mode")]
    pub mode: String,

    /// Endpoints to connect to (for client mode).
    #[serde(default)]
    pub connect: Vec<String>,

    /// Endpoints to listen on (for peer/router mode).
    #[serde(default)]
    pub listen: Vec<String>,
}

fn default_mode() -> String {
    "peer".to_string()
}

impl Default for ZenohConfig {
    fn default() -> Self {
        Self {
            mode: default_mode(),
            connect: Vec::new(),
            listen: Vec::new(),
        }
    }
}

impl ZenohConfig {
    /// Apply `ZENSIGHT_ZENOH_{MODE,CONNECT,LISTEN}` environment overrides.
    ///
    /// `CONNECT`/`LISTEN` are comma-separated endpoint lists. Unset variables
    /// leave the field untouched. This lets a launcher (e.g. `just run`) pin
    /// explicit local endpoints so the GUI and sensors connect reliably without
    /// depending on multicast peer discovery (which is unreliable on hosts with
    /// a VPN or multiple interfaces, e.g. tailscale/docker).
    pub fn with_env_overrides(self) -> Self {
        self.with_overrides_from(|k| std::env::var(k).ok())
    }

    /// Testable core of [`with_env_overrides`]: `get` resolves a variable name.
    fn with_overrides_from(mut self, get: impl Fn(&str) -> Option<String>) -> Self {
        if let Some(mode) = get("ZENSIGHT_ZENOH_MODE") {
            self.mode = mode;
        }
        let parse = |v: String| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect::<Vec<_>>()
        };
        if let Some(c) = get("ZENSIGHT_ZENOH_CONNECT") {
            self.connect = parse(c);
        }
        if let Some(l) = get("ZENSIGHT_ZENOH_LISTEN") {
            self.listen = parse(l);
        }
        self
    }
}

#[cfg(test)]
mod zenoh_env_tests {
    use super::*;
    use std::collections::HashMap;

    fn over(base: ZenohConfig, vars: &[(&str, &str)]) -> ZenohConfig {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        base.with_overrides_from(|k| map.get(k).cloned())
    }

    #[test]
    fn no_vars_leaves_config_unchanged() {
        let base = ZenohConfig {
            mode: "peer".into(),
            connect: vec!["tcp/a:1".into()],
            listen: vec![],
        };
        assert_eq!(over(base.clone(), &[]), base);
    }

    #[test]
    fn connect_and_listen_override_parse_csv() {
        let out = over(
            ZenohConfig::default(),
            &[
                ("ZENSIGHT_ZENOH_CONNECT", "tcp/127.0.0.1:7447, tcp/h:2 ,"),
                ("ZENSIGHT_ZENOH_LISTEN", "tcp/0.0.0.0:7448"),
                ("ZENSIGHT_ZENOH_MODE", "client"),
            ],
        );
        assert_eq!(out.mode, "client");
        assert_eq!(out.connect, vec!["tcp/127.0.0.1:7447", "tcp/h:2"]); // trimmed, empties dropped
        assert_eq!(out.listen, vec!["tcp/0.0.0.0:7448"]);
    }
}

/// Link profile for telemetry consumers on constrained links (#364).
///
/// `Constrained` bundles the low-bandwidth behaviors documented in
/// `docs/ZENOH-EFFICIENCY.md` §R6: scoped subscriptions instead of the
/// `zensight/**` firehose and no AdvancedSubscriber `history()`/`recovery()`
/// (late-join back-fill comes from the consumer's local store instead of a
/// reconnect history burst). Serialization already defaults to CBOR
/// ([`Format::default`]) regardless of profile.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkProfile {
    /// Full-fidelity subscription: history, recovery, late-publisher detection.
    #[default]
    Standard,
    /// Low-bandwidth links: plain subscribers, no history/recovery burst.
    Constrained,
}

impl LinkProfile {
    /// All profiles, for settings pickers.
    pub const ALL: &'static [LinkProfile] = &[LinkProfile::Standard, LinkProfile::Constrained];

    /// Stable lowercase name (matches the serde representation).
    pub fn as_str(&self) -> &'static str {
        match self {
            LinkProfile::Standard => "standard",
            LinkProfile::Constrained => "constrained",
        }
    }
}

impl std::fmt::Display for LinkProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Log output format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Human-readable text format (default).
    #[default]
    Text,
    /// Structured JSON format.
    Json,
}

/// Common logging configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Log level: "trace", "debug", "info", "warn", "error".
    #[serde(default = "default_log_level")]
    pub level: String,

    /// Log output format: "text" or "json".
    #[serde(default)]
    pub format: LogFormat,
}

fn default_log_level() -> String {
    "info".to_string()
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            format: LogFormat::default(),
        }
    }
}

/// Host-identity envelope options (`identity.*` in a sensor config, #311).
///
/// Only the *extras* are configurable — the core identity (hashed machine-id,
/// boot id, hostname, IPs, MACs) is always detected when a sensor enables
/// `with_identity`, because it never leaves the local file system / interface
/// table. The cloud probe is different: it makes network requests (link-local
/// IMDS endpoints), so it is **opt-in** and off by default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IdentityConfig {
    /// Probe the cloud instance-metadata service (AWS/GCP/Azure, 169.254.169.254)
    /// once at startup and attach the result to self-report evidence.
    /// Default **false** — never make network requests unless asked.
    #[serde(default)]
    pub cloud_metadata: bool,

    /// Per-request timeout for the IMDS probe, milliseconds. The endpoints are
    /// link-local so anything slow means "not on that cloud"; keep this short.
    #[serde(default = "default_cloud_timeout_ms")]
    pub cloud_timeout_ms: u64,
}

fn default_cloud_timeout_ms() -> u64 {
    1_000
}

impl Default for IdentityConfig {
    fn default() -> Self {
        Self {
            cloud_metadata: false,
            cloud_timeout_ms: default_cloud_timeout_ms(),
        }
    }
}

/// Base configuration shared by all sensors.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BaseConfig {
    /// Zenoh connection settings.
    #[serde(default)]
    pub zenoh: ZenohConfig,

    /// Serialization format for telemetry.
    #[serde(default)]
    pub serialization: Format,

    /// Logging configuration.
    #[serde(default)]
    pub logging: LoggingConfig,
}

// The `default_*` fns below back both the shared `CommonArtifactLimits` defaults
// and the per-kind `Artifact{Report,Snapshot}Limits` serde defaults.

fn default_report_max_bytes() -> u64 {
    64 * 1024 * 1024
}
fn default_report_cooldown() -> u64 {
    30
}
fn default_report_ttl() -> u64 {
    600
}
fn default_report_chunk_size() -> u32 {
    512 * 1024
}

/// One allowlisted directory a sensor will package into a Tier-2 snapshot.
///
/// The operator requests a snapshot by `name` (never an arbitrary path), so this
/// list is the authorization boundary for directory transfer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotDir {
    /// Logical name the operator picks (e.g. `"etc"`, `"pcaps"`).
    pub name: String,
    /// Absolute path on the sensor host this name resolves to.
    pub path: String,
}

fn default_snapshot_max_bytes() -> u64 {
    256 * 1024 * 1024
}
fn default_snapshot_max_files() -> u64 {
    50_000
}
fn default_snapshot_chunk_size() -> u32 {
    256 * 1024
}

/// The bounds every artifact producer shares (the unified-channel view). Producers
/// hold one of these; the channel enforces `cooldown_secs` + `ttl_secs` and
/// advertises `max_bytes`, while `enabled` gates whether the kind is served.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommonArtifactLimits {
    /// Whether this artifact kind is served at all.
    pub enabled: bool,
    /// Hard cap on the produced artifact size, bytes.
    pub max_bytes: u64,
    /// Minimum gap between successive productions, seconds (rate-limit).
    pub cooldown_secs: u64,
    /// How long a produced artifact stays available before the reaper drops it.
    pub ttl_secs: u64,
    /// Chunk size for the transfer (clamped by `zenoh-blob` to 256 KiB–1 MiB).
    pub chunk_size: u32,
}

/// `artifacts.report` — limits/policy for on-demand debug bundles (Tier-1).
///
/// Disabled by default (64 MiB / 512 KiB chunks / 30 s cooldown / 600 s TTL).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactReportLimits {
    /// Whether the sensor serves debug-report requests at all.
    #[serde(default)]
    pub enabled: bool,
    /// Hard cap on the generated bundle size; generation fails past this.
    #[serde(default = "default_report_max_bytes")]
    pub max_bytes: u64,
    /// Minimum gap between successive generations, seconds.
    #[serde(default = "default_report_cooldown")]
    pub cooldown_secs: u64,
    /// How long a generated bundle stays available.
    #[serde(default = "default_report_ttl")]
    pub ttl_secs: u64,
    /// Chunk size used by the blob transfer (clamped to 256 KiB–1 MiB).
    #[serde(default = "default_report_chunk_size")]
    pub chunk_size: u32,
    /// Extra config field-name patterns to redact (case-insensitive substring).
    #[serde(default)]
    pub redact_extra: Vec<String>,
}

impl Default for ArtifactReportLimits {
    fn default() -> Self {
        ArtifactReportLimits {
            enabled: false,
            max_bytes: default_report_max_bytes(),
            cooldown_secs: default_report_cooldown(),
            ttl_secs: default_report_ttl(),
            chunk_size: default_report_chunk_size(),
            redact_extra: Vec::new(),
        }
    }
}

impl ArtifactReportLimits {
    /// The shared bounds view used by the artifact channel.
    pub fn common(&self) -> CommonArtifactLimits {
        CommonArtifactLimits {
            enabled: self.enabled,
            max_bytes: self.max_bytes,
            cooldown_secs: self.cooldown_secs,
            ttl_secs: self.ttl_secs,
            chunk_size: self.chunk_size,
        }
    }
}

/// `artifacts.snapshot` — limits/policy for on-demand directory snapshots (Tier-2).
///
/// Disabled by default (256 MiB / 50k files / 256 KiB FastCDC chunks). `dirs` is
/// the directory allowlist (the authorization boundary — requests name a dir, never
/// a raw path).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactSnapshotLimits {
    /// Whether the sensor serves directory-snapshot requests at all.
    #[serde(default)]
    pub enabled: bool,
    /// The allowlist of named directories that may be snapshotted.
    #[serde(default)]
    pub dirs: Vec<SnapshotDir>,
    /// Hard cap on the total (uncompressed) snapshot size; build fails past this.
    #[serde(default = "default_snapshot_max_bytes")]
    pub max_bytes: u64,
    /// Hard cap on the number of files in a snapshot; build fails past this.
    #[serde(default = "default_snapshot_max_files")]
    pub max_files: u64,
    /// Minimum gap between successive snapshot builds, seconds.
    #[serde(default = "default_report_cooldown")]
    pub cooldown_secs: u64,
    /// How long a built snapshot stays available.
    #[serde(default = "default_report_ttl")]
    pub ttl_secs: u64,
    /// Average chunk size for the content-defined (FastCDC) chunker, bytes.
    #[serde(default = "default_snapshot_chunk_size")]
    pub chunk_size: u32,
}

impl Default for ArtifactSnapshotLimits {
    fn default() -> Self {
        ArtifactSnapshotLimits {
            enabled: false,
            dirs: Vec::new(),
            max_bytes: default_snapshot_max_bytes(),
            max_files: default_snapshot_max_files(),
            cooldown_secs: default_report_cooldown(),
            ttl_secs: default_report_ttl(),
            chunk_size: default_snapshot_chunk_size(),
        }
    }
}

impl ArtifactSnapshotLimits {
    /// The shared bounds view used by the artifact channel.
    pub fn common(&self) -> CommonArtifactLimits {
        CommonArtifactLimits {
            enabled: self.enabled,
            max_bytes: self.max_bytes,
            cooldown_secs: self.cooldown_secs,
            ttl_secs: self.ttl_secs,
            chunk_size: self.chunk_size,
        }
    }

    /// Resolve a requested logical `name` to its configured absolute path, if
    /// allowlisted.
    pub fn resolve(&self, name: &str) -> Option<&str> {
        self.dirs
            .iter()
            .find(|d| d.name == name)
            .map(|d| d.path.as_str())
    }

    /// The logical names of every allowlisted directory.
    pub fn dir_names(&self) -> Vec<String> {
        self.dirs.iter().map(|d| d.name.clone()).collect()
    }
}

/// The `artifacts` config section: one limits block per artifact kind a sensor
/// can serve. Every kind is disabled by default. Capture limits live in the
/// netring sensor's own config (producers own their limits; the channel only
/// sees each producer's [`CommonArtifactLimits`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArtifactLimits {
    /// Tier-1 debug-bundle limits.
    #[serde(default)]
    pub report: ArtifactReportLimits,
    /// Tier-2 directory-snapshot limits.
    #[serde(default)]
    pub snapshot: ArtifactSnapshotLimits,
}

/// Load a configuration file in JSON5 format.
pub fn load_config<T: for<'de> Deserialize<'de>>(path: impl AsRef<Path>) -> Result<T> {
    let path = path.as_ref();
    let content = std::fs::read_to_string(path).map_err(|e| {
        Error::Config(format!(
            "Failed to read config file '{}': {}",
            path.display(),
            e
        ))
    })?;

    json5::from_str(&content).map_err(|e| {
        Error::Config(format!(
            "Failed to parse config file '{}': {}",
            path.display(),
            e
        ))
    })
}

/// Load a configuration from a JSON5 string.
pub fn parse_config<T: for<'de> Deserialize<'de>>(content: &str) -> Result<T> {
    json5::from_str(content).map_err(|e| Error::Config(format!("Failed to parse config: {}", e)))
}

#[cfg(test)]
mod link_profile_tests {
    use super::*;

    #[test]
    fn default_is_standard() {
        assert_eq!(LinkProfile::default(), LinkProfile::Standard);
    }

    #[test]
    fn serde_lowercase_round_trip() {
        for profile in LinkProfile::ALL {
            let json = serde_json::to_string(profile).unwrap();
            assert_eq!(json, format!("\"{}\"", profile.as_str()));
            let back: LinkProfile = serde_json::from_str(&json).unwrap();
            assert_eq!(back, *profile);
        }
        let constrained: LinkProfile = json5::from_str("\"constrained\"").unwrap();
        assert_eq!(constrained, LinkProfile::Constrained);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_base_config() {
        let json5 = r#"
        {
            zenoh: {
                mode: "client",
                connect: ["tcp/localhost:7447"],
            },
            serialization: "cbor",
            logging: {
                level: "debug",
            },
        }
        "#;

        let config: BaseConfig = parse_config(json5).unwrap();

        assert_eq!(config.zenoh.mode, "client");
        assert_eq!(config.zenoh.connect, vec!["tcp/localhost:7447"]);
        assert_eq!(config.serialization, Format::Cbor);
        assert_eq!(config.logging.level, "debug");
    }

    #[test]
    fn test_default_config() {
        let json5 = "{}";
        let config: BaseConfig = parse_config(json5).unwrap();

        assert_eq!(config.zenoh.mode, "peer");
        assert!(config.zenoh.connect.is_empty());
        assert_eq!(config.serialization, Format::Cbor);
        assert_eq!(config.logging.level, "info");
        assert_eq!(config.logging.format, LogFormat::Text);
    }

    #[test]
    fn test_json_logging_format() {
        let json5 = r#"
        {
            logging: {
                level: "debug",
                format: "json",
            },
        }
        "#;

        let config: BaseConfig = parse_config(json5).unwrap();

        assert_eq!(config.logging.level, "debug");
        assert_eq!(config.logging.format, LogFormat::Json);
    }
}
