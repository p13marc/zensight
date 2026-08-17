//! Syslog sensor configuration.

use crate::filter::SyslogFilterConfig;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::Path;
use zensight_common::config::ZenohConfig;

/// Parse an IANA timezone name (e.g. `"Europe/Paris"`) into a [`chrono_tz::Tz`],
/// erroring with the offending name so a typo fails startup loudly (#545).
pub fn parse_tz(name: &str) -> anyhow::Result<chrono_tz::Tz> {
    name.parse::<chrono_tz::Tz>()
        .map_err(|_| anyhow::anyhow!("unknown IANA timezone: {name:?}"))
}

// Re-export LoggingConfig from the framework for compatibility
pub use zensight_sensor_core::LoggingConfig;

/// Complete syslog sensor configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyslogSensorConfig {
    /// Zenoh connection settings.
    pub zenoh: ZenohConfig,

    /// Serialization format for telemetry.
    #[serde(default)]
    pub serialization: zensight_common::serialization::Format,

    /// Syslog-specific settings.
    pub syslog: SyslogConfig,

    /// Logging configuration.
    #[serde(default)]
    pub logging: LoggingConfig,

    /// On-demand artifact channel (`@rpc/logs/artifact/*`) limits — report + snapshot.
    /// Every kind disabled by default.
    #[serde(default)]
    pub artifacts: zensight_sensor_core::ArtifactLimits,

    /// Forward-compat escape hatch (#547): when true, unknown config keys are
    /// warned about instead of rejected — for mixed-version fleets sharing one
    /// config. Default false: a typo'd key fails startup with a clear error.
    #[serde(default)]
    pub allow_unknown_fields: bool,
}

/// Syslog receiver configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyslogConfig {
    /// Override the agent-host source id (default: the local hostname).
    #[serde(default)]
    pub source: Option<String>,

    /// Listener configurations.
    #[serde(default)]
    pub listeners: Vec<ListenerConfig>,

    /// Hostname overrides for source identification.
    #[serde(default)]
    pub hostname_aliases: std::collections::HashMap<String, String>,

    /// Per-sender RFC 3164 timezone overrides (#545), keyed by sender IP — the
    /// same key space as `hostname_aliases`. Takes precedence over a listener's
    /// `timezone` for that sender, so a mixed fleet (some gear in UTC, some in
    /// local time) is stamped correctly. Values are IANA names
    /// (e.g. `"America/New_York"`).
    #[serde(default)]
    pub host_timezones: std::collections::HashMap<String, String>,

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
    /// `by_template/<id>/{count,errors}_total` series. Cheap + bounded, so
    /// it's on by default.
    #[serde(default)]
    pub templating: TemplatingConfig,

    /// Network-ingest robustness (#106): rate-limit + drop/parse-failure
    /// accounting for the UDP/TCP/Unix paths, bringing them to journald parity.
    /// Safe defaults (rate limit off, generous channel) so normal traffic is
    /// never dropped; emits `logs/ingest/*_total` counters and a sustained-loss
    /// health alert.
    #[serde(default)]
    pub ingest: IngestConfig,

    /// Multiline / stacktrace joining for the TCP/Unix stream paths (#107). On
    /// by default so LF-split Java/Python/Go tracebacks are stitched back into
    /// one record. journald is unaffected (already one record per entry).
    #[serde(default)]
    pub multiline: MultilineConfig,

    /// Capacity of the in-memory per-line event ring served on demand at
    /// `@rpc/logs/events` (#358). Per-line events no longer stream on the
    /// telemetry bus — consumers pull them from this ring. Default 10 000
    /// records (~3 MB), clamped to at least 100.
    #[serde(default = "default_events_ring_capacity")]
    pub events_ring_capacity: usize,

    /// Log sentinel (#543): declarative pattern→alert rules, also managed at
    /// runtime via `@rpc/logs/rules/set`. Empty by default; the shipped built-in
    /// known-event rules ride on `journald.detect_events`.
    #[serde(default)]
    pub sentinel: crate::sentinel::LogRulesConfig,

    /// Durable per-line history (#544): a disk-backed redb store behind the hot
    /// ring, so `@rpc/logs/events` can serve days back across restarts.
    /// Disabled by default (opt-in, like the other retention features).
    #[serde(default)]
    pub store: LogStoreConfig,

    /// File tailing sources (#549): tail `/var/log/*.log`-style files into the
    /// same intake pipeline. Empty by default (no file sources).
    #[serde(default)]
    pub files: FileTailingConfig,

    /// Observer evidence for remote senders (#552): publish `HostEvidence` for
    /// each device whose syslog this collector ingests, so they reach the
    /// correlator's entity catalog. On by default; no-op without network sources.
    #[serde(default)]
    pub evidence: EvidenceConfig,

    /// Log-bundle export artifact limits (#555). Producers own their limits (per
    /// the artifact framework), so the `logbundle` kind's policy lives here, not
    /// in the shared `artifacts` block. Disabled by default.
    #[serde(default)]
    pub logbundle: LogBundleLimits,
}

/// `logbundle` artifact limits (#555) — a filtered log export over `@blob`.
/// Disabled by default (64 MiB / 1M lines / 30 s cooldown / 600 s TTL).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogBundleLimits {
    /// Whether the sensor serves log-bundle export requests at all.
    #[serde(default)]
    pub enabled: bool,
    /// Hard cap on the compressed bundle size; production stops + flags
    /// truncation past this.
    #[serde(default = "default_logbundle_max_bytes")]
    pub max_bytes: u64,
    /// Hard cap on lines included; production stops + flags truncation past this.
    #[serde(default = "default_logbundle_max_lines")]
    pub max_lines: u64,
    /// Minimum gap between successive exports, seconds.
    #[serde(default = "default_logbundle_cooldown")]
    pub cooldown_secs: u64,
    /// How long a produced bundle stays available before the reaper drops it.
    #[serde(default = "default_logbundle_ttl")]
    pub ttl_secs: u64,
    /// Blob transfer chunk size (clamped to 256 KiB–1 MiB).
    #[serde(default = "default_logbundle_chunk_size")]
    pub chunk_size: u32,
}

impl Default for LogBundleLimits {
    fn default() -> Self {
        Self {
            enabled: false,
            max_bytes: default_logbundle_max_bytes(),
            max_lines: default_logbundle_max_lines(),
            cooldown_secs: default_logbundle_cooldown(),
            ttl_secs: default_logbundle_ttl(),
            chunk_size: default_logbundle_chunk_size(),
        }
    }
}

impl LogBundleLimits {
    /// The shared bounds view used by the artifact channel.
    pub fn common(&self) -> zensight_common::CommonArtifactLimits {
        zensight_common::CommonArtifactLimits {
            enabled: self.enabled,
            max_bytes: self.max_bytes,
            cooldown_secs: self.cooldown_secs,
            ttl_secs: self.ttl_secs,
            chunk_size: self.chunk_size,
        }
    }
}

fn default_logbundle_max_bytes() -> u64 {
    64 * 1024 * 1024
}
fn default_logbundle_max_lines() -> u64 {
    1_000_000
}
fn default_logbundle_cooldown() -> u64 {
    30
}
fn default_logbundle_ttl() -> u64 {
    600
}
fn default_logbundle_chunk_size() -> u32 {
    512 * 1024
}

/// Observer-evidence configuration (#552).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceConfig {
    /// Publish observer evidence for remote senders. Default true.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Re-publish + prune cadence (seconds). Default 300.
    #[serde(default = "default_evidence_refresh_secs")]
    pub refresh_secs: u64,

    /// Drop a sender not heard from in this many seconds. Default 21600 (6h).
    #[serde(default = "default_evidence_expire_secs")]
    pub expire_secs: u64,

    /// Cardinality cap on tracked senders (bounds the key space against spoofed
    /// sources). Default 4096.
    #[serde(default = "default_evidence_max_senders")]
    pub max_senders: usize,

    /// Opt-in reverse-DNS (PTR) FQDN enrichment. Off by default; when on,
    /// lookups run only in the publish tick (never on the intake path) and are
    /// cached per IP.
    #[serde(default)]
    pub reverse_dns: bool,
}

impl Default for EvidenceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            refresh_secs: default_evidence_refresh_secs(),
            expire_secs: default_evidence_expire_secs(),
            max_senders: default_evidence_max_senders(),
            reverse_dns: false,
        }
    }
}

fn default_evidence_refresh_secs() -> u64 {
    300
}
fn default_evidence_expire_secs() -> u64 {
    21_600
}
fn default_evidence_max_senders() -> usize {
    4096
}

/// File-tailing configuration (#549).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileTailingConfig {
    /// Sources to tail. Empty = file tailing disabled.
    #[serde(default)]
    pub sources: Vec<FileSourceConfig>,

    /// How often (seconds) to re-expand globs and pick up newly-created files.
    /// Default 15.
    #[serde(default = "default_files_rescan_secs")]
    pub rescan_secs: u64,

    /// How often (ms) to poll tracked files for new bytes. Default 500.
    #[serde(default = "default_files_poll_ms")]
    pub poll_ms: u64,

    /// Path of the offsets state file (atomic JSON, same scheme as the journald
    /// cursor). `None` resolves the `$STATE_DIRECTORY` / XDG state location.
    #[serde(default)]
    pub offsets_path: Option<std::path::PathBuf>,

    /// Hard cap on one joined line's bytes; a longer line is truncated. Default
    /// 1 MiB.
    #[serde(default = "default_files_max_line_bytes")]
    pub max_line_bytes: usize,
}

impl Default for FileTailingConfig {
    fn default() -> Self {
        Self {
            sources: Vec::new(),
            rescan_secs: default_files_rescan_secs(),
            poll_ms: default_files_poll_ms(),
            offsets_path: None,
            max_line_bytes: default_files_max_line_bytes(),
        }
    }
}

fn default_files_rescan_secs() -> u64 {
    15
}
fn default_files_poll_ms() -> u64 {
    500
}
fn default_files_max_line_bytes() -> usize {
    1024 * 1024
}

/// One file-tailing source: a set of globs + how to interpret their lines.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileSourceConfig {
    /// Glob patterns to tail (e.g. `["/var/log/app/*.log"]`).
    pub paths: Vec<String>,

    /// Static labels attached to every line from this source (as `sd.file.*`).
    #[serde(default)]
    pub labels: std::collections::HashMap<String, String>,

    /// Attribute lines to this unit (flows like the journald `unit` field, so
    /// per-unit rollups/SLO apply).
    #[serde(default)]
    pub unit: Option<String>,

    /// Attribute lines to this app / program name.
    #[serde(default)]
    pub app: Option<String>,

    /// Line format: `plain` (whole line is the message) or `syslog` (run the
    /// RFC 3164/5424 parser, e.g. files that contain `<PRI>` lines). Default
    /// `plain`.
    #[serde(default)]
    pub format: FileFormat,

    /// Default severity slug for `plain` lines (`emerg`..`debug`). Default
    /// `info`.
    #[serde(default)]
    pub severity: Option<String>,

    /// Optional regex whose first capture group (or a `severity` named group)
    /// extracts a level word (`ERROR`/`WARN`/…) per line, overriding `severity`.
    #[serde(default)]
    pub severity_regex: Option<String>,

    /// Join multi-line records (stack traces) per file. On by default.
    #[serde(default = "default_true")]
    pub multiline: bool,
}

/// How to interpret a tailed file's lines (#549).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileFormat {
    /// The whole line is the message; severity/app/unit come from config.
    #[default]
    Plain,
    /// Run the syslog parser over each line (`<PRI>…`), falling back to plain.
    Syslog,
}

/// Durable log store configuration (#544).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogStoreConfig {
    /// Persist retained lines to disk. Off by default.
    #[serde(default)]
    pub enabled: bool,

    /// Store file path. `None` resolves the systemd `STATE_DIRECTORY` / XDG
    /// state location (same scheme as the journald cursor).
    #[serde(default)]
    pub path: Option<std::path::PathBuf>,

    /// Retain history at most this many days (pruned by age). Default 7.
    #[serde(default = "default_store_max_age_days")]
    pub max_age_days: u64,

    /// Hard cap on stored records regardless of age (pruned by size). Default
    /// 2 000 000 (~a few hundred MB).
    #[serde(default = "default_store_max_records")]
    pub max_records: usize,

    /// Flush a batch once this many records are queued. Default 500.
    #[serde(default = "default_store_batch_size")]
    pub batch_size: usize,

    /// Flush at least this often even if the batch isn't full (seconds).
    /// Default 2.
    #[serde(default = "default_store_flush_secs")]
    pub flush_interval_secs: u64,

    /// Prune + health-report cadence (seconds). Default 300.
    #[serde(default = "default_store_prune_secs")]
    pub prune_interval_secs: u64,

    /// Bound on the writer channel; when full, records are dropped and counted
    /// (`store/write_drops_total`) rather than back-pressuring intake. Default
    /// 100 000.
    #[serde(default = "default_store_queue_capacity")]
    pub queue_capacity: usize,

    /// redb page-cache budget in bytes. redb's own default is 1 GiB (#625) —
    /// on the small hosts this sensor targets that reads as a slow multi-day
    /// RSS climb toward OOM as the database grows. Default 64 MiB.
    #[serde(default = "default_store_cache_bytes")]
    pub cache_bytes: usize,
}

impl Default for LogStoreConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: None,
            max_age_days: default_store_max_age_days(),
            max_records: default_store_max_records(),
            batch_size: default_store_batch_size(),
            flush_interval_secs: default_store_flush_secs(),
            prune_interval_secs: default_store_prune_secs(),
            queue_capacity: default_store_queue_capacity(),
            cache_bytes: default_store_cache_bytes(),
        }
    }
}

fn default_store_cache_bytes() -> usize {
    64 * 1024 * 1024
}

fn default_store_max_age_days() -> u64 {
    7
}
fn default_store_max_records() -> usize {
    2_000_000
}
fn default_store_batch_size() -> usize {
    500
}
fn default_store_flush_secs() -> u64 {
    2
}
fn default_store_prune_secs() -> u64 {
    300
}
fn default_store_queue_capacity() -> usize {
    100_000
}

/// Multiline / stacktrace joining configuration (#107, C6).
///
/// Applies to the stream (TCP/Unix) listeners only. Continuation lines (indented
/// stack frames, `Caused by:`, `...`, `Traceback …`) are folded into the
/// preceding record; the record is emitted when the next real syslog line
/// (`<PRI>…`) arrives or `flush_timeout_ms` elapses with no new frame.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct MultilineConfig {
    /// Master switch. On by default — this is the fix for shattered tracebacks.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Idle flush: emit a buffered record this many ms after the last frame when
    /// no continuation/next-record has arrived. Bounds the added latency on the
    /// final line of a burst. Default 200ms.
    #[serde(default = "default_multiline_flush_ms")]
    pub flush_timeout_ms: u64,

    /// Hard cap on lines folded into one record (a runaway continuation stream
    /// is flushed and restarted at the cap). Default 500.
    #[serde(default = "default_multiline_max_lines")]
    pub max_lines: usize,

    /// Hard cap on bytes in one joined record. Default 65536.
    #[serde(default = "default_multiline_max_bytes")]
    pub max_bytes: usize,
}

fn default_multiline_flush_ms() -> u64 {
    200
}
fn default_multiline_max_lines() -> usize {
    500
}
fn default_multiline_max_bytes() -> usize {
    65536
}

impl Default for MultilineConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            flush_timeout_ms: default_multiline_flush_ms(),
            max_lines: default_multiline_max_lines(),
            max_bytes: default_multiline_max_bytes(),
        }
    }
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

    /// Capacity of the central intake channel between the listeners and the
    /// processing loop (#546). Under a burst larger than this, `drop_newest`
    /// sheds before the (optional) rate limiter engages; raise it to trade
    /// memory for burst absorption (each slot holds one parsed message).
    /// Default 1000, clamped to ≥1.
    #[serde(default = "default_channel_capacity")]
    pub channel_capacity: usize,

    /// Collapse consecutive identical `(source, message)` lines into one record
    /// carrying a `repeat_count` label (#546) — syslog's classic "last message
    /// repeated N times", done at the receiver so a screaming line doesn't
    /// exhaust the ring, rate budget, and attention one copy at a time.
    /// **Disabled by default** to preserve exact streams; the collapsed count
    /// still feeds the rollup counters so totals stay honest.
    #[serde(default)]
    pub collapse_repeats: bool,

    /// Idle gap (ms) that closes a run of identical lines when `collapse_repeats`
    /// is on (#546): a run is emitted once no matching line has arrived for this
    /// long, or a different line arrives. Also the max added latency for a line
    /// with no follow-up. Default 1000ms, clamped to ≥1.
    #[serde(default = "default_collapse_window_ms")]
    pub collapse_window_ms: u64,
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            max_eps: None,
            sample_ratio: default_sample_ratio(),
            overflow: OverflowPolicy::default(),
            drop_alert_ratio: default_drop_alert_ratio(),
            channel_capacity: default_channel_capacity(),
            collapse_repeats: false,
            collapse_window_ms: default_collapse_window_ms(),
        }
    }
}

fn default_channel_capacity() -> usize {
    1000
}

fn default_collapse_window_ms() -> u64 {
    1000
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

/// Default `@rpc/logs/events` ring capacity (#358): 10 000 records ≈ 3 MB.
fn default_events_ring_capacity() -> usize {
    10_000
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
/// bucket) are emitted as `by_template/*` series.
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

    /// Drop the bundle's own journal output (#625): entries whose
    /// `_SYSTEMD_UNIT` or `SYSLOG_IDENTIFIER` starts with `zensight-sensor`
    /// are discarded in the reader, before rate limiting and decode. On by
    /// default — a logs sensor tailing a journal that contains its own
    /// stdout is a feedback loop (every line it logs becomes traffic it
    /// publishes, which produces more lines). libsystemd matches are
    /// include-only, so this cannot be expressed server-side.
    #[serde(default = "default_true")]
    pub exclude_self: bool,

    /// Client-side exclusion of additional exact `_SYSTEMD_UNIT` names —
    /// the negative match libsystemd doesn't have. Checked in the reader
    /// before rate limiting; dropped entries are counted
    /// (`journald/self_excluded_total`). Empty by default.
    #[serde(default)]
    pub exclude_units: Vec<String>,

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
    /// stable `MESSAGE_ID` and raise alerts on `state/logs/alert/*` (#61). On by default.
    #[serde(default = "default_true")]
    pub detect_events: bool,

    /// **Deprecated (#543):** superseded by the log sentinel. The known-events
    /// are now built-in sentinel rules with their own `for_secs`; still parsed
    /// so existing configs load, but ignored. Kept only for compatibility.
    #[serde(default = "default_event_dedup_secs")]
    pub event_dedup_secs: u64,

    /// **Deprecated (#543):** override a known-event's severity by adding a
    /// sentinel rule with the same `id` (`coredump`/`unit-failed`/`oomd-kill`/
    /// `kernel-oom`). Still parsed so existing configs load, but ignored; a
    /// startup warning fires when non-empty.
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
            exclude_self: true,
            exclude_units: Vec::new(),
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

impl SyslogConfig {
    /// The agent host's unified source id: the `source` override, else the hostname.
    pub fn resolved_source(&self) -> String {
        self.source.clone().unwrap_or_else(|| {
            hostname::get()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "unknown".to_string())
        })
    }
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

    /// IANA timezone (e.g. `"Europe/Paris"`) that RFC 3164 senders on this
    /// listener express their yearless, zoneless timestamps in (#545). Applies
    /// DST via the tz database. `None` (the default) means UTC. Overridden
    /// per-sender by `syslog.host_timezones`. Has no effect on RFC 5424, which
    /// carries its own explicit offset.
    #[serde(default)]
    pub timezone: Option<String>,

    /// TLS settings (#550). Required when `protocol` is `tls` (RFC 5425); a TLS
    /// listener always uses octet-counting framing regardless of `framing`.
    #[serde(default)]
    pub tls: Option<TlsListenerConfig>,
}

/// TLS listener settings (#550, RFC 5425). Key material is referenced by **path
/// only** (never inline PEM); paths accept `${ENV}` / `file:` secret indirection
/// (#538).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsListenerConfig {
    /// Server certificate chain (PEM).
    pub cert_file: String,
    /// Server private key (PEM: PKCS#8 / PKCS#1 / SEC1).
    pub key_file: String,
    /// Require + verify client certificates against this CA bundle (PEM) —
    /// mutual TLS. `None` (default) accepts any client (server-auth only).
    #[serde(default)]
    pub client_ca_file: Option<String>,
    /// Minimum TLS version: `"1.3"` (default) or `"1.2"`.
    #[serde(default = "default_tls_min_version")]
    pub min_version: String,
}

fn default_tls_min_version() -> String {
    "1.3".to_string()
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

pub(crate) fn default_true() -> bool {
    true
}

/// Listener protocol type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ListenerProtocol {
    Udp,
    Tcp,
    Unix,
    /// TLS over TCP (RFC 5425, #550) — octet-counting framing only.
    Tls,
}

impl std::fmt::Display for ListenerProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Udp => write!(f, "udp"),
            Self::Tcp => write!(f, "tcp"),
            Self::Unix => write!(f, "unix"),
            Self::Tls => write!(f, "tls"),
        }
    }
}

impl SyslogSensorConfig {
    /// Load configuration from a JSON5 file.
    pub fn load_from_file(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path.as_ref())?;
        Self::parse_strict(&content)
    }

    /// Parse config, rejecting unknown keys (#547) unless
    /// `allow_unknown_fields` is set. Unknown keys are collected by full path
    /// (including nested/external types) via `serde_ignored`, so a typo like
    /// `novlety` fails loudly naming the key instead of silently taking the
    /// default. With the escape hatch on, unknown keys are logged as warnings.
    pub fn parse_strict(content: &str) -> anyhow::Result<Self> {
        // json5 → Value → serde_ignored, so one parse yields both the typed
        // config and the set of ignored (unknown) key paths.
        let value: serde_json::Value = json5::from_str(content)?;
        let mut unknown: Vec<String> = Vec::new();
        let config: Self = serde_ignored::deserialize(value, |path| {
            unknown.push(path.to_string());
        })?;

        if !unknown.is_empty() {
            let list = unknown.join(", ");
            if config.allow_unknown_fields {
                tracing::warn!(
                    unknown_keys = %list,
                    "config has unknown keys (allow_unknown_fields is set — ignoring)"
                );
            } else {
                anyhow::bail!(
                    "unknown config key(s): {list}. Fix the typo, or set \
                     allow_unknown_fields: true to ignore (mixed-version fleets)."
                );
            }
        }

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
                ListenerProtocol::Udp | ListenerProtocol::Tcp | ListenerProtocol::Tls => {
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

            // A TLS listener needs cert/key material and a known min version.
            if listener.protocol == ListenerProtocol::Tls {
                let Some(tls) = &listener.tls else {
                    anyhow::bail!("Listener {i} is `tls` but has no `tls` cert/key config");
                };
                if tls.cert_file.is_empty() || tls.key_file.is_empty() {
                    anyhow::bail!("Listener {i} tls: cert_file and key_file are required");
                }
                if !matches!(tls.min_version.as_str(), "1.2" | "1.3") {
                    anyhow::bail!(
                        "Listener {i} tls: min_version must be \"1.2\" or \"1.3\", got {:?}",
                        tls.min_version
                    );
                }
            }

            // Fail fast on a bad IANA timezone rather than silently falling
            // back to UTC at runtime (#545).
            if let Some(tz) = &listener.timezone {
                parse_tz(tz).with_context(|| format!("listener {i} timezone"))?;
            }
        }

        for (host, tz) in &self.syslog.host_timezones {
            parse_tz(tz).with_context(|| format!("host_timezones[{host}]"))?;
        }

        Ok(())
    }

    /// One-glance startup summary (#547): what sources are active and which
    /// analytics are on/off, so a misconfiguration (a source that never came up,
    /// an analytic silently left at its default) is visible in the log the moment
    /// the sensor starts — not inferred later from missing telemetry.
    pub fn startup_summary(&self) -> String {
        let s = &self.syslog;

        // Sources.
        let mut sources: Vec<String> = s
            .listeners
            .iter()
            .map(|l| format!("{:?}:{}", l.protocol, l.bind))
            .collect();
        if let Some(j) = &s.journald
            && j.enabled
        {
            sources.push(format!(
                "journald({:?}, {} unit filter(s))",
                j.scope,
                j.units.len()
            ));
        }
        let sources = if sources.is_empty() {
            "<none>".to_string()
        } else {
            sources.join(", ")
        };

        // Analytics toggles — name each so an off one reads as a deliberate off,
        // not an oversight.
        let on_off = |b: bool| if b { "on" } else { "off" };
        let analytics = format!(
            "derived={}, error_budget={}, templating={}, \
             journald.detect_events={}, dynamic_filters={}, multiline={}",
            on_off(s.derived),
            on_off(s.error_budget.enabled),
            on_off(s.templating.enabled),
            on_off(
                s.journald
                    .as_ref()
                    .is_some_and(|j| j.enabled && j.detect_events)
            ),
            on_off(s.enable_dynamic_filters),
            on_off(s.multiline.enabled),
        );

        let ingest = match s.ingest.max_eps {
            Some(eps) => format!("rate_limit={eps}/s overflow={:?}", s.ingest.overflow),
            None => format!("rate_limit=off overflow={:?}", s.ingest.overflow),
        };

        format!(
            "sources: {sources} | analytics: {analytics} | ingest: {ingest} | \
             events_ring={}",
            s.events_ring_capacity
        )
    }
}

impl zensight_sensor_core::SensorConfig for SyslogSensorConfig {
    fn zenoh(&self) -> &ZenohConfig {
        &self.zenoh
    }

    fn logging(&self) -> &LoggingConfig {
        &self.logging
    }

    fn producer(&self) -> &str {
        "logs"
    }

    fn artifact_limits(&self) -> zensight_sensor_core::ArtifactLimits {
        self.artifacts.clone()
    }

    fn validate(&self) -> zensight_sensor_core::Result<()> {
        self.validate_config()
            .map_err(|e| zensight_sensor_core::SensorError::config(e.to_string()))
    }
}

impl Default for SyslogConfig {
    fn default() -> Self {
        Self {
            source: None,
            listeners: vec![ListenerConfig {
                protocol: ListenerProtocol::Udp,
                bind: "0.0.0.0:514".to_string(),
                max_message_size: default_max_message_size(),
                max_connections: default_max_connections(),
                connection_timeout_secs: default_connection_timeout_secs(),
                socket_mode: default_socket_mode(),
                remove_existing_socket: default_true(),
                framing: Framing::default(),
                timezone: None,
                tls: None,
            }],
            hostname_aliases: std::collections::HashMap::new(),
            host_timezones: std::collections::HashMap::new(),
            include_raw_message: false,
            filter: SyslogFilterConfig::default(),
            enable_dynamic_filters: false,
            journald: None,
            derived: true,
            derived_interval_secs: default_derived_interval_secs(),
            top_units: default_top_units(),
            error_budget: ErrorBudgetConfig::default(),
            templating: TemplatingConfig::default(),
            ingest: IngestConfig::default(),
            multiline: MultilineConfig::default(),
            events_ring_capacity: default_events_ring_capacity(),
            sentinel: crate::sentinel::LogRulesConfig::default(),
            store: LogStoreConfig::default(),
            files: FileTailingConfig::default(),
            evidence: EvidenceConfig::default(),
            logbundle: LogBundleLimits::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped example config must physically spell out the opt-in analytics.
    ///
    /// An absent block parses clean and takes the Rust default — that's correct
    /// (absent = default). The old hazard was a *typo* (`novlety:`) making an
    /// operator believe an analytic was on while it silently stayed at its
    /// default; #547's strict loader now rejects that (see
    /// `unknown_key_is_rejected`). Asserting the parsed value here would be
    /// vacuous (both ship `false` and default `false`), so walk the raw tree and
    /// prove the key is really present for `gen-configs.sh` to sed on.
    #[test]
    fn shipped_config_spells_out_the_opt_in_analytics() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../configs/logs.json5");
        let text = std::fs::read_to_string(path).expect("configs/logs.json5");

        // Typed parse: the file is valid and every key is well-typed.
        let cfg = SyslogSensorConfig::load_from_file(path).expect("configs/logs.json5");
        assert!(!cfg.syslog.error_budget.enabled);
        // The base is optional and empty by default — the shipped example
        // deliberately leaves it unset (bus-root deployment).
        assert_eq!(cfg.zenoh.namespace, "");
        // the template miner is on by default (cheap + bounded).
        assert!(cfg.syslog.templating.enabled);

        let raw: serde_json::Value = json5::from_str(&text).expect("json5");
        let at = |path: &str| -> serde_json::Value {
            let mut cur = &raw;
            for seg in path.split('.') {
                cur = cur
                    .get(seg)
                    .unwrap_or_else(|| panic!("configs/logs.json5 is missing `{path}`"));
            }
            cur.clone()
        };

        assert_eq!(at("syslog.error_budget.enabled"), false);
    }

    const MINIMAL: &str = r#"{
        zenoh: { mode: "peer" },
        syslog: { listeners: [ { protocol: "udp", bind: "0.0.0.0:514" } ] }
    }"#;

    /// A typo'd top-level key (the classic `novlety` silent-default trap) is
    /// rejected with an error that names the offending key (#547).
    #[test]
    fn unknown_key_is_rejected() {
        let bad = MINIMAL.replace(r#"syslog: {"#, r#"novlety: true, syslog: {"#);
        let err = SyslogSensorConfig::parse_strict(&bad).expect_err("typo must fail");
        let msg = err.to_string();
        assert!(msg.contains("unknown config key"), "got: {msg}");
        assert!(
            msg.contains("novlety"),
            "error must name the key, got: {msg}"
        );
    }

    /// A misspelled *nested* key is caught too — serde_ignored reports the full
    /// path, so a typo inside an analytics block can't hide.
    #[test]
    fn unknown_nested_key_is_rejected() {
        let bad = MINIMAL.replace(
            r#"listeners:"#,
            r#"error_budget: { enabld: true }, listeners:"#,
        );
        let err = SyslogSensorConfig::parse_strict(&bad).expect_err("nested typo must fail");
        assert!(
            err.to_string().contains("enabld"),
            "error must name the nested key, got: {err}"
        );
    }

    /// The escape hatch downgrades rejection to a warning for mixed-version
    /// fleets: with `allow_unknown_fields: true` the unknown key is tolerated.
    #[test]
    fn allow_unknown_fields_tolerates_extras() {
        let ok = MINIMAL.replace(
            r#"syslog: {"#,
            r#"allow_unknown_fields: true, future_knob: 42, syslog: {"#,
        );
        let cfg = SyslogSensorConfig::parse_strict(&ok).expect("escape hatch must allow extras");
        assert!(cfg.allow_unknown_fields);
    }

    /// Every config shipped in-repo must survive the strict loader (#547): the
    /// strictness is worthless if our own examples trip it. Guards against a
    /// future edit adding a key the schema doesn't know.
    #[test]
    fn shipped_configs_load_strict() {
        for rel in [
            "/../configs/logs.json5",
            "/../configs/syslog.json5",
            "/../docker/configs/syslog.json5",
        ] {
            let path = format!("{}{rel}", env!("CARGO_MANIFEST_DIR"));
            SyslogSensorConfig::load_from_file(&path)
                .unwrap_or_else(|e| panic!("{path} must load strict: {e}"));
        }
    }

    /// A bad IANA timezone fails validation loudly at load (#545) rather than
    /// silently falling back to UTC at runtime.
    #[test]
    fn bad_timezone_fails_validation() {
        let bad = MINIMAL.replace(
            r#"bind: "0.0.0.0:514""#,
            r#"bind: "0.0.0.0:514", timezone: "Mars/Olympus""#,
        );
        let err = SyslogSensorConfig::parse_strict(&bad).expect_err("bad tz must fail");
        // `{:#}` walks the anyhow context chain (context + source).
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Mars/Olympus"),
            "error must name the bad zone, got: {msg}"
        );
    }

    #[test]
    fn good_timezone_validates() {
        let ok = MINIMAL.replace(
            r#"bind: "0.0.0.0:514""#,
            r#"bind: "0.0.0.0:514", timezone: "Europe/Paris""#,
        );
        let cfg = SyslogSensorConfig::parse_strict(&ok).expect("valid tz");
        assert_eq!(
            cfg.syslog.listeners[0].timezone.as_deref(),
            Some("Europe/Paris")
        );
    }

    /// A `tls` listener without a `tls` block fails validation (#550).
    #[test]
    fn tls_listener_requires_cert_config() {
        let bad = r#"{
            zenoh: { mode: "peer" },
            syslog: { listeners: [ { protocol: "tls", bind: "0.0.0.0:6514" } ] }
        }"#;
        let err = SyslogSensorConfig::parse_strict(bad).expect_err("tls without certs must fail");
        assert!(format!("{err:#}").contains("tls"), "got: {err}");
    }

    #[test]
    fn tls_listener_with_certs_validates() {
        let ok = r#"{
            zenoh: { mode: "peer" },
            syslog: { listeners: [ {
                protocol: "tls", bind: "0.0.0.0:6514",
                tls: { cert_file: "/c.crt", key_file: "/k.key", min_version: "1.2" }
            } ] }
        }"#;
        let cfg = SyslogSensorConfig::parse_strict(ok).expect("valid tls listener");
        assert_eq!(cfg.syslog.listeners[0].protocol, ListenerProtocol::Tls);
    }

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
