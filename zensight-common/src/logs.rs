//! The log sentinel's wire types (#543, moved here in #849): the pattern→alert
//! rule vocabulary a fleet authors and the logs sensor evaluates.
//!
//! Here for the same reason [`crate::hostspec`]'s, [`crate::systemd`]'s and
//! [`crate::netlink`]'s are: they are WIRE CONTRACTS with three consumers —
//! the sensor, the GUI, and the `@desired` fleet author (#816) — and RFC 08
//! §7's schema gate requires a real schemars-generated schema for a
//! state-class payload, which a sensor-crate type can never provide
//! (`zensight-common` cannot depend on a sensor, so `describe` could only
//! carry a stub; #815's gate refused that, correctly).
//!
//! Compilation and matching stay in the sensor. A [`LogRule`]'s `regex` is a
//! **string** here and a compiled `Regex` there: the wire carries the pattern,
//! the sensor decides whether it compiles, and `zensight-common` does not grow
//! a `regex` dependency to hold a field it never executes.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::alert::AlertSeverity;

fn default_true() -> bool {
    true
}
fn default_eval_interval() -> u64 {
    10
}
/// The wire default for [`LogRule::for_secs`].
///
/// Public because a consumer building a rule by hand — the GUI's rule form,
/// a test — must land on the same value serde gives an absent field. Two
/// spellings of one default is how a rule authored in a form comes to behave
/// differently from the identical rule authored in a file.
pub fn default_for_secs() -> u64 {
    300
}

/// The full sentinel ruleset — seeded from config, hot-swapped at runtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LogRulesConfig {
    /// How often (seconds) expired alerts are reconciled / windows pruned.
    #[serde(default = "default_eval_interval")]
    pub eval_interval_secs: u64,
    /// Include the shipped built-in known-event rules (coredump/OOM/unit-failed).
    /// On by default so upgrading keeps the known-events working unchanged.
    #[serde(default = "default_true")]
    pub include_builtins: bool,
    /// Include the built-in **kernel pattern** rules (#824): EXT4-fs error,
    /// md/RAID disk failure, block-device I/O error — the handful of lines
    /// that mean a machine is dying. **Off by default** (the quiet-alerts
    /// stance: silence unless asked), and pattern-based, so unlike
    /// `include_builtins` they work on any source, not just journald.
    #[serde(default)]
    pub include_kernel_builtins: bool,
    /// Operator-declared rules.
    #[serde(default)]
    pub rules: Vec<LogRule>,
}

impl Default for LogRulesConfig {
    fn default() -> Self {
        Self {
            eval_interval_secs: default_eval_interval(),
            include_builtins: true,
            include_kernel_builtins: false,
            rules: Vec::new(),
        }
    }
}

/// One declarative rule: match criteria → an alert.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LogRule {
    /// Stable id — the alert `rule` namespace and the hit-counter key. Must be
    /// unique; later duplicates are dropped at compile with a warning.
    pub id: String,
    /// Human description (optional; surfaced in the read RPC).
    #[serde(default)]
    pub description: Option<String>,
    /// Match criteria (all present fields must hold — AND).
    #[serde(default, rename = "match")]
    pub matcher: LogMatch,
    /// Optional `count >= N within window` threshold to avoid single-line noise.
    #[serde(default)]
    pub threshold: Option<Threshold>,
    /// Alert severity when the rule fires.
    #[serde(default)]
    pub severity: AlertSeverity,
    /// Summary template. `{message}`, `{unit}`, `{app}`, `{host}`, `{count}`,
    /// `{severity}` and regex capture groups `{1}`..`{9}` / `{name}` are
    /// substituted. Defaults to `"<id>: <truncated message>"`.
    #[serde(default)]
    pub summary: Option<String>,
    /// Journald / structured-data fields to lift into the alert labels (e.g.
    /// `coredump_exe`), on top of the always-included `unit`/`app`.
    #[serde(default)]
    pub labels_from: Vec<String>,
    /// Auto-resolve TTL: the alert clears this long after its last match
    /// (the "quiet period"). Defaults to 300s.
    ///
    /// **This is already the recovery window** (#932), which is why this
    /// sensor gained no `recover_after_secs` while netlink, hostspec and
    /// systemd did. A log rule has no "currently violated" state to debounce —
    /// a line either matched or it did not — so `for_secs` here means "must
    /// stay quiet this long", implemented in this module's own `active` map
    /// with an expiry sweep, and `observe` is called with `Some(Duration::ZERO)`
    /// precisely because the reporter's debounce is meaningless for it.
    ///
    /// A second hold stacked on top would be two timers meaning the same
    /// thing, with the alert clearing after the sum of them.
    #[serde(default = "default_for_secs")]
    pub for_secs: u64,
    /// Cap on *fires* per window (#824): at most `max_fires` alert
    /// publications within `per_secs`, further fires suppressed (and counted)
    /// until the window frees. Distinct from `threshold`, which delays the
    /// first fire; this bounds how often a flapping rule can page. `None` =
    /// no cap.
    #[serde(default)]
    pub rate_limit: Option<RateLimit>,
}

/// A `max_fires per per_secs` cap on alert publications for one rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RateLimit {
    pub max_fires: u64,
    pub per_secs: u64,
}

/// Match criteria for a [`LogRule`]. An empty matcher matches everything.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LogMatch {
    /// Regex tested against the message text (unanchored).
    #[serde(default)]
    pub pattern: Option<String>,
    /// Match lines at least this severe: syslog severity number `<=` this
    /// (0=emerg … 7=debug, lower is worse). `Some(4)` = warning-and-worse.
    #[serde(default)]
    pub min_severity: Option<u8>,
    /// Exact facility slug (e.g. `auth`).
    #[serde(default)]
    pub facility: Option<String>,
    /// Exact `_SYSTEMD_UNIT` (journald `unit` structured field).
    #[serde(default)]
    pub unit: Option<String>,
    /// Exact app / program name (syslog tag).
    #[serde(default)]
    pub app: Option<String>,
    /// Exact mined `template_id` (requires templating on).
    #[serde(default)]
    pub template_id: Option<String>,
    /// Exact journald `MESSAGE_ID` (32-char hex, case-insensitive).
    #[serde(default)]
    pub message_id: Option<String>,
}

/// A `count >= N within window` threshold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Threshold {
    pub count: u64,
    pub within_secs: u64,
}
