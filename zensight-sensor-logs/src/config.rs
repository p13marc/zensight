//! Syslog sensor configuration.

use crate::filter::SyslogFilterConfig;
use serde::{Deserialize, Serialize};
use std::path::Path;
use zensight_common::config::ZenohConfig;

// Re-export LoggingConfig from the framework for compatibility
pub use zensight_sensor_core::LoggingConfig;

/// Complete syslog sensor configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyslogSensorConfig {
    /// Zenoh connection settings.
    pub zenoh: ZenohConfig,

    /// Syslog-specific settings.
    pub syslog: SyslogConfig,

    /// Logging configuration.
    #[serde(default)]
    pub logging: LoggingConfig,
}

/// Syslog receiver configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyslogConfig {
    /// Key expression prefix for publishing.
    #[serde(default = "default_key_prefix")]
    pub key_prefix: String,

    /// Listener configurations.
    #[serde(default)]
    pub listeners: Vec<ListenerConfig>,

    /// Hostname overrides for source identification.
    #[serde(default)]
    pub hostname_aliases: std::collections::HashMap<String, String>,

    /// Whether to include raw message in labels.
    #[serde(default)]
    pub include_raw_message: bool,

    /// Message filtering configuration.
    #[serde(default)]
    pub filter: SyslogFilterConfig,

    /// Enable dynamic filter commands via Zenoh.
    #[serde(default)]
    pub enable_dynamic_filters: bool,

    /// systemd-journald ingestion (#57). Reads the local journal directly via
    /// libsystemd (no `journalctl` subprocess) and feeds the same pipeline as
    /// the network listeners. `None` (the default) leaves journald disabled.
    #[serde(default)]
    pub journald: Option<JournaldConfig>,

    /// Emit derived rollup telemetry (#63): per-severity + per-unit (top-N) log
    /// rates, error/warning rollups, units-in-failure, and journald throughput —
    /// cheap aggregates on a tick, alongside the per-message points. Default on.
    #[serde(default = "default_true")]
    pub derived: bool,

    /// Interval (seconds) between derived-telemetry emissions. Default 10.
    #[serde(default = "default_derived_interval_secs")]
    pub derived_interval_secs: u64,

    /// Cardinality cap for per-unit rollups: at most this many distinct units
    /// are tracked as their own series; the rest aggregate into an `other`
    /// bucket (never an unbounded label space). Default 10.
    #[serde(default = "default_top_units")]
    pub top_units: usize,

    /// Per-unit error-budget / SLO burn-rate alerting (#105). Layered on top of
    /// the derived per-unit `messages_total`/`errors_total` rollups: emits
    /// `error_ratio` + `burn_rate` gauges and, when enabled, raises a
    /// `log-error-budget` alert on sustained multi-window burn. Disabled by
    /// default so it never surprises existing deployments.
    #[serde(default)]
    pub error_budget: ErrorBudgetConfig,

    /// Drain-style streaming log-template mining (#102). Masks variables and
    /// clusters each line into a stable template; attaches `template_id` /
    /// `template` labels to the per-line points and emits bounded
    /// `logs/by_template/<id>/{count,errors}_total` series. Cheap + bounded, so
    /// it's on by default.
    #[serde(default)]
    pub templating: TemplatingConfig,

    /// Novelty / "what's new" detection (#103). Layered on top of the template
    /// miner: after a warm-up window, a never-before-seen template shape raises a
    /// `log-novelty` anomaly alert, and (optionally) a known template whose rate
    /// jumps N× over its EWMA baseline raises a `log-rate-spike` alert. Requires
    /// `templating.enabled`. Disabled by default (raises alerts → opt-in).
    #[serde(default)]
    pub novelty: NoveltyConfig,

    /// Network-ingest robustness (#106): rate-limit + drop/parse-failure
    /// accounting for the UDP/TCP/Unix paths, bringing them to journald parity.
    /// Safe defaults (rate limit off, generous channel) so normal traffic is
    /// never dropped; emits `logs/ingest/*_total` counters and a sustained-loss
    /// health alert.
    #[serde(default)]
    pub ingest: IngestConfig,
}

/// Network-ingest robustness configuration (#106).
///
/// Mirrors the journald loss-accounting controls for the network paths. By
/// default the rate limiter is **off** (`max_eps: None`) so nothing is shed in
/// normal use; under a configured budget or a full telemetry channel, drops are
/// counted (`logs/ingest/dropped_total`) and a sustained-loss `ErrorReport` is
/// raised rather than silently dropping logs.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct IngestConfig {
    /// Optional global rate limit (parsed messages/sec across all network
    /// listeners). Beyond the budget the limiter keeps 1-in-`sample_ratio` and
    /// counts the rest as dropped. `None` (the default) = unlimited.
    #[serde(default)]
    pub max_eps: Option<u64>,

    /// When rate-limited, keep 1 of every N over-budget messages. Default 100;
    /// clamped to ≥1.
    #[serde(default = "default_sample_ratio")]
    pub sample_ratio: u64,

    /// Behavior when the bounded telemetry channel is full. `drop_newest` (the
    /// default) sheds the incoming message and counts it (bounded memory);
    /// `block` applies backpressure to the listener instead.
    #[serde(default)]
    pub overflow: OverflowPolicy,

    /// Emit an `ErrorReport` once the dropped fraction over a window exceeds this
    /// (0.0..=1.0) — "not silently dropping your logs". Default 0.01 (1%).
    #[serde(default = "default_drop_alert_ratio")]
    pub drop_alert_ratio: f64,
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            max_eps: None,
            sample_ratio: default_sample_ratio(),
            overflow: OverflowPolicy::default(),
            drop_alert_ratio: default_drop_alert_ratio(),
        }
    }
}

/// TCP/Unix stream framing mode (RFC 6587, #106).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Framing {
    /// Auto-detect per frame: a leading digit ⇒ octet-counting (`MSG-LEN SP
    /// MSG`), otherwise LF-delimited. The safe default — handles both legacy
    /// LF senders and RFC 6587 octet-counted senders on the same listener.
    #[default]
    Auto,
    /// Always non-transparent (LF-delimited) framing.
    Lf,
    /// Always RFC 6587 octet-counted framing.
    Octet,
}

fn default_derived_interval_secs() -> u64 {
    10
}
fn default_top_units() -> usize {
    10
}

/// Per-unit error-budget / SLO configuration (#105).
///
/// SLO math (see also `derived::BudgetParams`): per derived window a unit's
/// error ratio is `errors / messages`; it *burns budget* when that ratio
/// exceeds `target_ratio * burn_rate` with at least `min_messages` of volume.
/// An alert fires only after `burn_windows` consecutive burning windows and
/// auto-resolves the first window the unit is back within budget.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ErrorBudgetConfig {
    /// Master switch for *alerting*. When false the `error_ratio`/`burn_rate`
    /// gauges are still emitted (cheap, bounded) but no alert is ever raised.
    #[serde(default)]
    pub enabled: bool,

    /// Tolerated per-window error fraction — the SLO target (0.0..=1.0).
    /// Default 0.05 (5%).
    #[serde(default = "default_target_ratio")]
    pub target_ratio: f64,

    /// Burn threshold multiplier: fire when the window error ratio exceeds
    /// `target_ratio * burn_rate`. Default 2.0.
    #[serde(default = "default_burn_rate")]
    pub burn_rate: f64,

    /// Consecutive over-budget windows required before an alert fires (the
    /// multi-window anti-flap guard). Default 3.
    #[serde(default = "default_burn_windows")]
    pub burn_windows: u32,

    /// Minimum messages in a window before the ratio is trusted, so a near-idle
    /// unit can't trip a 100% ratio off a single line. Default 20.
    #[serde(default = "default_min_messages")]
    pub min_messages: u64,
}

fn default_target_ratio() -> f64 {
    0.05
}
fn default_burn_rate() -> f64 {
    2.0
}
fn default_burn_windows() -> u32 {
    3
}
fn default_min_messages() -> u64 {
    20
}

impl Default for ErrorBudgetConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            target_ratio: default_target_ratio(),
            burn_rate: default_burn_rate(),
            burn_windows: default_burn_windows(),
            min_messages: default_min_messages(),
        }
    }
}

/// Drain-style log-template mining configuration (#102).
///
/// Defaults follow the logpai/Drain3 conventions (`depth=4`, `sim=0.4`) and are
/// bounded so a noisy stream can't blow up cardinality or memory: at most
/// `max_clusters` templates are mined, and only `top_templates` (+ an `other`
/// bucket) are emitted as `logs/by_template/*` series.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TemplatingConfig {
    /// Master switch. On by default (cheap + bounded).
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Fixed parse-tree depth: token layers descended below the length layer.
    /// Default 4.
    #[serde(default = "default_templating_depth")]
    pub depth: usize,

    /// Similarity threshold (fraction of matching non-wildcard tokens) to join
    /// an existing cluster. Default 0.4.
    #[serde(default = "default_sim_threshold")]
    pub sim_threshold: f64,

    /// Max distinct literal children per tree node before new tokens fold into
    /// the wildcard branch. Default 100.
    #[serde(default = "default_max_children")]
    pub max_children: usize,

    /// Hard cap on retained clusters (bounds memory). Default 1000.
    #[serde(default = "default_max_clusters")]
    pub max_clusters: usize,

    /// Cardinality cap for the emitted per-template series: at most this many
    /// distinct templates get their own series; the rest fold into `other`.
    /// Default 50.
    #[serde(default = "default_top_templates")]
    pub top_templates: usize,
}

fn default_templating_depth() -> usize {
    4
}
fn default_sim_threshold() -> f64 {
    0.4
}
fn default_max_children() -> usize {
    100
}
fn default_max_clusters() -> usize {
    1000
}
fn default_top_templates() -> usize {
    50
}

impl Default for TemplatingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            depth: default_templating_depth(),
            sim_threshold: default_sim_threshold(),
            max_children: default_max_children(),
            max_clusters: default_max_clusters(),
            top_templates: default_top_templates(),
        }
    }
}

/// Novelty / rate-spike detection configuration (#103).
///
/// Builds on the template miner: maintains a bounded seen-templates set and,
/// after `warm_up_secs`, raises a `log-novelty` anomaly for a never-before-seen
/// template shape and (when `rate_spike_multiplier > 1`) a `log-rate-spike`
/// anomaly for a known template whose window rate jumps over its EWMA baseline.
/// Disabled by default — it raises alerts, so it's strictly opt-in.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct NoveltyConfig {
    /// Master switch. Off by default. Requires `templating.enabled`.
    #[serde(default)]
    pub enabled: bool,

    /// Startup warm-up: templates first seen within this window are folded into
    /// the baseline (never flagged), so a cold start isn't all "novel".
    /// Rate-spikes are likewise suppressed until it elapses. Default 300s.
    #[serde(default = "default_warm_up_secs")]
    pub warm_up_secs: u64,

    /// How long a fired novelty point-event stays firing before auto-resolving
    /// (it never re-fires; dedup is by `template_id`). Default 300s.
    #[serde(default = "default_novelty_dedup_secs")]
    pub novelty_dedup_secs: u64,

    /// Rate-spike multiplier: a known template fires when its window rate exceeds
    /// this many times its EWMA baseline. `<= 1.0` disables spike detection.
    /// Default 5.0.
    #[serde(default = "default_rate_spike_multiplier")]
    pub rate_spike_multiplier: f64,

    /// Absolute floor on a window's count before a spike can fire, so a jump from
    /// a handful of lines can't alert. Default 10.
    #[serde(default = "default_min_spike_count")]
    pub min_spike_count: f64,

    /// EWMA smoothing factor (0.0..=1.0) for the per-template baseline rate.
    /// Default 0.3.
    #[serde(default = "default_ewma_alpha")]
    pub ewma_alpha: f64,

    /// Hard cap on the seen-set size (bounds memory). Beyond the cap new shapes
    /// are conservatively treated as known (no alert, not tracked). Default 2000.
    #[serde(default = "default_max_novelty_templates")]
    pub max_templates: usize,
}

fn default_warm_up_secs() -> u64 {
    300
}
fn default_novelty_dedup_secs() -> u64 {
    300
}
fn default_rate_spike_multiplier() -> f64 {
    5.0
}
fn default_min_spike_count() -> f64 {
    10.0
}
fn default_ewma_alpha() -> f64 {
    0.3
}
fn default_max_novelty_templates() -> usize {
    2000
}

impl Default for NoveltyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            warm_up_secs: default_warm_up_secs(),
            novelty_dedup_secs: default_novelty_dedup_secs(),
            rate_spike_multiplier: default_rate_spike_multiplier(),
            min_spike_count: default_min_spike_count(),
            ewma_alpha: default_ewma_alpha(),
            max_templates: default_max_novelty_templates(),
        }
    }
}

/// systemd-journald source configuration.
///
/// Minimal by design: `{ "enabled": true }` tails the local system journal with
/// sane defaults. Cursor resume (#58) and server-side matching (#59) extend this.
#[derive(Debug, Clone, Serialize, Deserialize)]
// Fields beyond `enabled` are read only by the (feature-gated) journald reader.
#[cfg_attr(not(feature = "journald"), allow(dead_code))]
pub struct JournaldConfig {
    /// Master switch. When false the reader is not started.
    #[serde(default)]
    pub enabled: bool,

    /// Which journal to open.
    #[serde(default)]
    pub scope: JournaldScope,

    /// Open a specific journald log namespace instead of the default journal.
    #[serde(default)]
    pub namespace: Option<String>,

    /// Where to begin reading on startup (#58). Defaults to resuming from the
    /// persisted cursor (first run behaves like `tail`).
    #[serde(default)]
    pub start_from: StartFrom,

    /// Lookback window for `start_from: "since"`, e.g. `"15m"`, `"1h"`, `"2d"`.
    #[serde(default)]
    pub since: Option<String>,

    /// Path of the cursor state file. `None` picks a sensible default
    /// (`$STATE_DIRECTORY/journald.cursor` under systemd, else an XDG state dir).
    #[serde(default)]
    pub cursor_file: Option<std::path::PathBuf>,

    /// What to do when `start_from: "cursor"` but the saved cursor is gone
    /// (rotated out): start from the tail, or from `since`.
    #[serde(default)]
    pub on_missing_cursor: MissingCursor,

    /// Server-side filter: only these systemd units (`_SYSTEMD_UNIT`), OR'd.
    /// Empty = all units. Applied in the journal itself (#59), so filtered
    /// entries are never decoded or transported.
    #[serde(default)]
    pub units: Vec<String>,

    /// Server-side filter: minimum priority 0..=7 (3 = err). Expands to a
    /// `PRIORITY=0..min` OR-group (libsystemd has no `<=` match). `None` = all.
    #[serde(default)]
    pub min_priority: Option<u8>,

    /// Server-side filter: only these transports (`_TRANSPORT`, e.g. `kernel`,
    /// `journal`, `stdout`, `syslog`), OR'd. Empty = all.
    #[serde(default)]
    pub transports: Vec<String>,

    /// Server-side filter: raw `FIELD=value` matches, AND'd with the above
    /// (same-field entries OR per libsystemd semantics). Escape hatch for
    /// arbitrary journald fields.
    #[serde(default, rename = "match")]
    pub match_fields: std::collections::HashMap<String, String>,

    /// Extra raw journald field names (e.g. `_SELINUX_CONTEXT`) to copy verbatim
    /// into labels, on top of the standard set (unit, pid, comm, boot_id, …).
    #[serde(default)]
    pub extra_fields: Vec<String>,

    /// Include developer/code-location fields (CODE_FILE/CODE_LINE/CODE_FUNC,
    /// ERRNO). Off by default to keep label cardinality bounded.
    #[serde(default)]
    pub include_dev_fields: bool,

    /// Detect well-known systemd events (coredump, unit-failed, OOM) by their
    /// stable `MESSAGE_ID` and raise alerts on `@/alerts/*` (#61). On by default.
    #[serde(default = "default_true")]
    pub detect_events: bool,

    /// Coalesce repeats of the same `(event, unit)` within this many seconds,
    /// and auto-resolve a fired event alert after the window passes (#61).
    #[serde(default = "default_event_dedup_secs")]
    pub event_dedup_secs: u64,

    /// Per-`MESSAGE_ID` severity overrides (`info` | `warning` | `critical`),
    /// keyed by the 32-char hex id. Empty = use the built-in defaults.
    #[serde(default)]
    pub event_severity: std::collections::HashMap<String, String>,

    /// Behavior when the bounded telemetry channel is full under a log storm
    /// (#62). `block` applies backpressure to the journal read (safe, may lag);
    /// `drop_newest` keeps memory bounded and counts what it sheds. Default
    /// `drop_newest`.
    #[serde(default)]
    pub overflow: OverflowPolicy,

    /// Optional global rate limit (entries/sec, #62). Beyond the budget the
    /// reader samples 1-in-`sample_ratio` and counts the rest as sampled-out,
    /// so a single screaming unit can't drown the bus. `None` = unlimited.
    #[serde(default)]
    pub max_eps: Option<u64>,

    /// When rate-limited, keep 1 of every N over-budget entries (the rest are
    /// counted as sampled-out). Default 100; clamped to ≥1.
    #[serde(default = "default_sample_ratio")]
    pub sample_ratio: u64,

    /// Emit an `ErrorReport` once the dropped+sampled fraction over a window
    /// exceeds this (0.0..=1.0) — "not silently dropping your logs". Default
    /// 0.01 (1%).
    #[serde(default = "default_drop_alert_ratio")]
    pub drop_alert_ratio: f64,
}

fn default_event_dedup_secs() -> u64 {
    30
}
fn default_sample_ratio() -> u64 {
    100
}
fn default_drop_alert_ratio() -> f64 {
    0.01
}

/// Telemetry-channel overflow policy under load (#62).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverflowPolicy {
    /// Apply backpressure to the journal read — never lose an entry, but the
    /// reader may lag behind a sustained storm.
    Block,
    /// Drop the incoming entry when the channel is full (bounded memory),
    /// counting each drop. The default — a logs sensor should shed under a
    /// storm rather than block or OOM.
    #[default]
    DropNewest,
}

impl Default for JournaldConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            scope: JournaldScope::default(),
            namespace: None,
            start_from: StartFrom::default(),
            since: None,
            cursor_file: None,
            on_missing_cursor: MissingCursor::default(),
            units: Vec::new(),
            min_priority: None,
            transports: Vec::new(),
            match_fields: std::collections::HashMap::new(),
            extra_fields: Vec::new(),
            include_dev_fields: false,
            detect_events: true,
            event_dedup_secs: default_event_dedup_secs(),
            event_severity: std::collections::HashMap::new(),
            overflow: OverflowPolicy::default(),
            max_eps: None,
            sample_ratio: default_sample_ratio(),
            drop_alert_ratio: default_drop_alert_ratio(),
        }
    }
}

/// Where the journald reader begins on startup (#58).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartFrom {
    /// Resume from the persisted cursor; first run behaves like `tail`.
    #[default]
    Cursor,
    /// Only entries newer than startup.
    Tail,
    /// Replay the entire journal from the beginning (can be large).
    Head,
    /// Only entries from the current boot.
    Boot,
    /// Entries within the `since` lookback window.
    Since,
}

/// Fallback when a saved cursor can no longer be resolved (#58).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissingCursor {
    /// Start from the tail (only new entries).
    #[default]
    Tail,
    /// Start from the `since` lookback window.
    Since,
}

/// Which systemd journal to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournaldScope {
    /// System services and the kernel (default; needs journal-read access).
    #[default]
    System,
    /// The invoking user's journal (always readable unprivileged).
    User,
    /// Only local journal files (exclude remote/uploaded journals).
    LocalOnly,
    /// Only volatile runtime journals (`/run`), not persisted ones.
    RuntimeOnly,
}

fn default_key_prefix() -> String {
    "zensight/logs".to_string()
}

/// Individual listener configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListenerConfig {
    /// Protocol: "udp", "tcp", or "unix".
    pub protocol: ListenerProtocol,

    /// Bind address.
    /// - For UDP/TCP: "0.0.0.0:514"
    /// - For Unix: "/var/run/syslog.sock"
    pub bind: String,

    /// Maximum message size in bytes (UDP only).
    #[serde(default = "default_max_message_size")]
    pub max_message_size: usize,

    /// TCP/Unix: maximum concurrent connections.
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,

    /// TCP/Unix: connection timeout in seconds.
    #[serde(default = "default_connection_timeout_secs")]
    pub connection_timeout_secs: u64,

    /// Unix socket: file permissions (octal, e.g., 0o666 = 438).
    #[serde(default = "default_socket_mode")]
    pub socket_mode: u32,

    /// Unix socket: remove existing socket file before binding.
    #[serde(default = "default_true")]
    pub remove_existing_socket: bool,

    /// TCP/Unix: stream framing mode (RFC 6587, #106). Ignored for UDP (a
    /// datagram is always exactly one frame). Default `auto`.
    #[serde(default)]
    pub framing: Framing,
}

fn default_max_message_size() -> usize {
    65535
}

fn default_max_connections() -> usize {
    1000
}

fn default_connection_timeout_secs() -> u64 {
    300
}

fn default_socket_mode() -> u32 {
    0o666
}

fn default_true() -> bool {
    true
}

/// Listener protocol type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ListenerProtocol {
    Udp,
    Tcp,
    Unix,
}

impl std::fmt::Display for ListenerProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Udp => write!(f, "udp"),
            Self::Tcp => write!(f, "tcp"),
            Self::Unix => write!(f, "unix"),
        }
    }
}

impl SyslogSensorConfig {
    /// Load configuration from a JSON5 file.
    pub fn load_from_file(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path.as_ref())?;
        let config: Self = json5::from_str(&content)?;
        config.validate_config()?;
        Ok(config)
    }

    /// Validate the configuration.
    pub fn validate_config(&self) -> anyhow::Result<()> {
        // A source is required: at least one network listener OR journald.
        let journald_enabled = self.syslog.journald.as_ref().is_some_and(|j| j.enabled);
        if self.syslog.listeners.is_empty() && !journald_enabled {
            anyhow::bail!("No source configured: add at least one listener or enable journald");
        }

        for (i, listener) in self.syslog.listeners.iter().enumerate() {
            if listener.bind.is_empty() {
                anyhow::bail!("Listener {} has empty bind address", i);
            }

            match listener.protocol {
                ListenerProtocol::Udp | ListenerProtocol::Tcp => {
                    // Validate bind address format for network protocols
                    if !listener.bind.contains(':') {
                        anyhow::bail!(
                            "Listener {} bind address must include port (e.g., '0.0.0.0:514')",
                            i
                        );
                    }
                }
                ListenerProtocol::Unix => {
                    // Unix socket path should be absolute or relative path
                    // Just check it's not empty (already done above)
                }
            }
        }

        Ok(())
    }
}

impl zensight_sensor_core::SensorConfig for SyslogSensorConfig {
    fn zenoh(&self) -> &ZenohConfig {
        &self.zenoh
    }

    fn logging(&self) -> &LoggingConfig {
        &self.logging
    }

    fn key_prefix(&self) -> &str {
        &self.syslog.key_prefix
    }

    fn validate(&self) -> zensight_sensor_core::Result<()> {
        self.validate_config()
            .map_err(|e| zensight_sensor_core::SensorError::config(e.to_string()))
    }
}

impl Default for SyslogConfig {
    fn default() -> Self {
        Self {
            key_prefix: default_key_prefix(),
            listeners: vec![ListenerConfig {
                protocol: ListenerProtocol::Udp,
                bind: "0.0.0.0:514".to_string(),
                max_message_size: default_max_message_size(),
                max_connections: default_max_connections(),
                connection_timeout_secs: default_connection_timeout_secs(),
                socket_mode: default_socket_mode(),
                remove_existing_socket: default_true(),
                framing: Framing::default(),
            }],
            hostname_aliases: std::collections::HashMap::new(),
            include_raw_message: false,
            filter: SyslogFilterConfig::default(),
            enable_dynamic_filters: false,
            journald: None,
            derived: true,
            derived_interval_secs: default_derived_interval_secs(),
            top_units: default_top_units(),
            error_budget: ErrorBudgetConfig::default(),
            templating: TemplatingConfig::default(),
            novelty: NoveltyConfig::default(),
            ingest: IngestConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_config() {
        let json = r#"{
            zenoh: { mode: "peer" },
            syslog: {
                listeners: [
                    { protocol: "udp", bind: "0.0.0.0:514" }
                ]
            }
        }"#;

        let config: SyslogSensorConfig = json5::from_str(json).unwrap();
        assert_eq!(config.syslog.key_prefix, "zensight/logs");
        assert_eq!(config.syslog.listeners.len(), 1);
        assert_eq!(config.syslog.listeners[0].protocol, ListenerProtocol::Udp);
    }

    #[test]
    fn test_parse_full_config() {
        let json = r#"{
            zenoh: {
                mode: "client",
                connect: ["tcp/localhost:7447"]
            },
            syslog: {
                key_prefix: "custom/syslog",
                listeners: [
                    { protocol: "udp", bind: "0.0.0.0:514", max_message_size: 8192 },
                    { protocol: "tcp", bind: "0.0.0.0:514", max_connections: 500 }
                ],
                hostname_aliases: {
                    "192.168.1.1": "router01"
                },
                include_raw_message: true
            },
            logging: {
                level: "debug"
            }
        }"#;

        let config: SyslogSensorConfig = json5::from_str(json).unwrap();
        assert_eq!(config.syslog.key_prefix, "custom/syslog");
        assert_eq!(config.syslog.listeners.len(), 2);
        assert_eq!(config.syslog.listeners[0].max_message_size, 8192);
        assert_eq!(config.syslog.listeners[1].max_connections, 500);
        assert_eq!(
            config.syslog.hostname_aliases.get("192.168.1.1"),
            Some(&"router01".to_string())
        );
        assert!(config.syslog.include_raw_message);
        assert_eq!(config.logging.level, "debug");
    }

    #[test]
    fn test_parse_unix_socket_config() {
        let json = r#"{
            zenoh: { mode: "peer" },
            syslog: {
                listeners: [
                    {
                        protocol: "unix",
                        bind: "/var/run/syslog.sock",
                        socket_mode: 438,
                        remove_existing_socket: true
                    }
                ]
            }
        }"#;

        let config: SyslogSensorConfig = json5::from_str(json).unwrap();
        assert_eq!(config.syslog.listeners.len(), 1);
        assert_eq!(config.syslog.listeners[0].protocol, ListenerProtocol::Unix);
        assert_eq!(config.syslog.listeners[0].bind, "/var/run/syslog.sock");
        assert_eq!(config.syslog.listeners[0].socket_mode, 438); // 0o666
        assert!(config.syslog.listeners[0].remove_existing_socket);
        assert!(config.validate_config().is_ok());
    }

    #[test]
    fn test_parse_filter_config() {
        let json = r#"{
            zenoh: { mode: "peer" },
            syslog: {
                listeners: [
                    { protocol: "udp", bind: "0.0.0.0:514" }
                ],
                filter: {
                    min_severity: 4,
                    exclude_facilities: ["local7"],
                    exclude_app_patterns: [
                        { pattern: "systemd-*", pattern_type: "glob" }
                    ]
                },
                enable_dynamic_filters: true
            }
        }"#;

        let config: SyslogSensorConfig = json5::from_str(json).unwrap();
        assert_eq!(config.syslog.filter.min_severity, Some(4));
        assert_eq!(config.syslog.filter.exclude_facilities, vec!["local7"]);
        assert_eq!(config.syslog.filter.exclude_app_patterns.len(), 1);
        assert!(config.syslog.enable_dynamic_filters);
    }

    #[test]
    fn test_validate_empty_listeners() {
        let json = r#"{
            zenoh: { mode: "peer" },
            syslog: {
                listeners: []
            }
        }"#;

        let config: SyslogSensorConfig = json5::from_str(json).unwrap();
        assert!(config.validate_config().is_err());
    }

    #[test]
    fn test_validate_missing_port() {
        let json = r#"{
            zenoh: { mode: "peer" },
            syslog: {
                listeners: [
                    { protocol: "udp", bind: "0.0.0.0" }
                ]
            }
        }"#;

        let config: SyslogSensorConfig = json5::from_str(json).unwrap();
        assert!(config.validate_config().is_err());
    }

    #[test]
    fn test_error_budget_defaults_off() {
        let json = r#"{
            zenoh: { mode: "peer" },
            syslog: { listeners: [ { protocol: "udp", bind: "0.0.0.0:514" } ] }
        }"#;
        let config: SyslogSensorConfig = json5::from_str(json).unwrap();
        let eb = config.syslog.error_budget;
        assert!(!eb.enabled);
        assert_eq!(eb.target_ratio, 0.05);
        assert_eq!(eb.burn_rate, 2.0);
        assert_eq!(eb.burn_windows, 3);
        assert_eq!(eb.min_messages, 20);
    }

    #[test]
    fn test_error_budget_parsed() {
        let json = r#"{
            zenoh: { mode: "peer" },
            syslog: {
                listeners: [ { protocol: "udp", bind: "0.0.0.0:514" } ],
                error_budget: {
                    enabled: true,
                    target_ratio: 0.02,
                    burn_rate: 5.0,
                    burn_windows: 4,
                    min_messages: 50
                }
            }
        }"#;
        let config: SyslogSensorConfig = json5::from_str(json).unwrap();
        let eb = config.syslog.error_budget;
        assert!(eb.enabled);
        assert_eq!(eb.target_ratio, 0.02);
        assert_eq!(eb.burn_rate, 5.0);
        assert_eq!(eb.burn_windows, 4);
        assert_eq!(eb.min_messages, 50);
    }

    #[test]
    fn test_templating_defaults_on() {
        let json = r#"{
            zenoh: { mode: "peer" },
            syslog: { listeners: [ { protocol: "udp", bind: "0.0.0.0:514" } ] }
        }"#;
        let config: SyslogSensorConfig = json5::from_str(json).unwrap();
        let t = config.syslog.templating;
        assert!(t.enabled);
        assert_eq!(t.depth, 4);
        assert_eq!(t.sim_threshold, 0.4);
        assert_eq!(t.max_children, 100);
        assert_eq!(t.max_clusters, 1000);
        assert_eq!(t.top_templates, 50);
    }

    #[test]
    fn test_templating_parsed() {
        let json = r#"{
            zenoh: { mode: "peer" },
            syslog: {
                listeners: [ { protocol: "udp", bind: "0.0.0.0:514" } ],
                templating: {
                    enabled: false,
                    depth: 6,
                    sim_threshold: 0.6,
                    max_children: 50,
                    max_clusters: 200,
                    top_templates: 25
                }
            }
        }"#;
        let config: SyslogSensorConfig = json5::from_str(json).unwrap();
        let t = config.syslog.templating;
        assert!(!t.enabled);
        assert_eq!(t.depth, 6);
        assert_eq!(t.sim_threshold, 0.6);
        assert_eq!(t.max_children, 50);
        assert_eq!(t.max_clusters, 200);
        assert_eq!(t.top_templates, 25);
    }

    #[test]
    fn test_novelty_defaults_off() {
        let json = r#"{
            zenoh: { mode: "peer" },
            syslog: { listeners: [ { protocol: "udp", bind: "0.0.0.0:514" } ] }
        }"#;
        let config: SyslogSensorConfig = json5::from_str(json).unwrap();
        let n = config.syslog.novelty;
        assert!(!n.enabled);
        assert_eq!(n.warm_up_secs, 300);
        assert_eq!(n.novelty_dedup_secs, 300);
        assert_eq!(n.rate_spike_multiplier, 5.0);
        assert_eq!(n.min_spike_count, 10.0);
        assert_eq!(n.ewma_alpha, 0.3);
        assert_eq!(n.max_templates, 2000);
    }

    #[test]
    fn test_novelty_parsed() {
        let json = r#"{
            zenoh: { mode: "peer" },
            syslog: {
                listeners: [ { protocol: "udp", bind: "0.0.0.0:514" } ],
                novelty: {
                    enabled: true,
                    warm_up_secs: 60,
                    novelty_dedup_secs: 120,
                    rate_spike_multiplier: 8.0,
                    min_spike_count: 25.0,
                    ewma_alpha: 0.5,
                    max_templates: 500
                }
            }
        }"#;
        let config: SyslogSensorConfig = json5::from_str(json).unwrap();
        let n = config.syslog.novelty;
        assert!(n.enabled);
        assert_eq!(n.warm_up_secs, 60);
        assert_eq!(n.novelty_dedup_secs, 120);
        assert_eq!(n.rate_spike_multiplier, 8.0);
        assert_eq!(n.min_spike_count, 25.0);
        assert_eq!(n.ewma_alpha, 0.5);
        assert_eq!(n.max_templates, 500);
    }

    #[test]
    fn test_ingest_defaults_safe() {
        let json = r#"{
            zenoh: { mode: "peer" },
            syslog: { listeners: [ { protocol: "udp", bind: "0.0.0.0:514" } ] }
        }"#;
        let config: SyslogSensorConfig = json5::from_str(json).unwrap();
        let ing = config.syslog.ingest;
        // Rate limit off by default → nothing shed in normal use.
        assert_eq!(ing.max_eps, None);
        assert_eq!(ing.sample_ratio, 100);
        assert_eq!(ing.overflow, OverflowPolicy::DropNewest);
        assert_eq!(ing.drop_alert_ratio, 0.01);
        // Listener framing defaults to auto-detect.
        assert_eq!(config.syslog.listeners[0].framing, Framing::Auto);
    }

    #[test]
    fn test_ingest_and_framing_parsed() {
        let json = r#"{
            zenoh: { mode: "peer" },
            syslog: {
                listeners: [
                    { protocol: "tcp", bind: "0.0.0.0:514", framing: "octet" }
                ],
                ingest: {
                    max_eps: 5000,
                    sample_ratio: 10,
                    overflow: "block",
                    drop_alert_ratio: 0.05
                }
            }
        }"#;
        let config: SyslogSensorConfig = json5::from_str(json).unwrap();
        let ing = config.syslog.ingest;
        assert_eq!(ing.max_eps, Some(5000));
        assert_eq!(ing.sample_ratio, 10);
        assert_eq!(ing.overflow, OverflowPolicy::Block);
        assert_eq!(ing.drop_alert_ratio, 0.05);
        assert_eq!(config.syslog.listeners[0].framing, Framing::Octet);
    }

    #[test]
    fn test_validate_unix_no_port_required() {
        let json = r#"{
            zenoh: { mode: "peer" },
            syslog: {
                listeners: [
                    { protocol: "unix", bind: "/tmp/syslog.sock" }
                ]
            }
        }"#;

        let config: SyslogSensorConfig = json5::from_str(json).unwrap();
        assert!(config.validate_config().is_ok());
    }
}
