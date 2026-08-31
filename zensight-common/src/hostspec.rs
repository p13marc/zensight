//! Hostspec's wire types (#821, moved here in #816): the assertion
//! vocabulary (`ExpectationsConfig` and its seven expectation kinds) and the
//! evaluation reply (`HostspecEvaluation`).
//!
//! They live in zensight-common — like `DiscoveryReport` or `InterfaceTable`
//! — because they are WIRE CONTRACTS with three consumers: the sensor
//! (evaluates them), the GUI (authors them), and the `@desired` fleet author
//! (#816 publishes them per host, and RFC 08 §7's schema gate requires a
//! real schemars-generated schema for every state-class payload — which a
//! sensor-crate type can never provide). Checking logic (validation, the
//! checkers) stays in the sensor: these are data.

use std::collections::HashSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::alert::AlertSeverity;

fn default_eval_interval() -> u64 {
    60
}
fn default_severity() -> AlertSeverity {
    AlertSeverity::Warning
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MountExpectation {
    pub name: String,
    /// The mount point, absolute.
    pub path: String,
    /// Assert `path` is a bind of this source path (same filesystem, root
    /// computed through the containing mount — btrfs subvolumes included).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_bind_of: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fstype: Option<String>,
    /// Each listed option must be present (mount or superblock options).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    #[serde(default = "default_severity")]
    pub severity: AlertSeverity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub for_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FileExpectation {
    pub name: String,
    pub path: String,
    /// `false` asserts the opposite of `absent`: this file may exist but its
    /// other clauses are only checked when it does. Default: must exist.
    #[serde(default = "default_true")]
    pub exists: bool,
    /// Maximum mtime age, seconds ("the nightly backup is newer than 26h").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newer_than_secs: Option<u64>,
    /// Fire when the size drifts more than this percentage from the latched
    /// baseline — the last size that PASSED. The baseline advances only on a
    /// pass, so a halved backup stays firing instead of self-resolving one
    /// sweep later when the halved size becomes "previous". In-memory: a
    /// restart reseeds the baseline on first observation (documented).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_within_pct_of_previous: Option<f64>,
    #[serde(default = "default_severity")]
    pub severity: AlertSeverity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub for_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ListeningExpectation {
    pub name: String,
    pub port: u16,
    /// Exact bound address to require (or forbid). `None` = any listener on
    /// the port. `0.0.0.0` and `::` are DISTINCT wildcards — forbid both if
    /// you mean "not world-reachable" on a dual-stack host (the shipped
    /// example shows the pair).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<String>,
    /// `true`: fire when a matching listener EXISTS (the bound-to-0.0.0.0
    /// case); `false`: fire when none does.
    #[serde(default)]
    pub forbid: bool,
    #[serde(default = "default_severity")]
    pub severity: AlertSeverity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub for_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SymlinkExpectation {
    pub name: String,
    pub path: String,
    /// The literal `readlink` target — never canonicalized: the assertion is
    /// about what the link SAYS, not what it currently resolves to.
    pub target: String,
    #[serde(default = "default_severity")]
    pub severity: AlertSeverity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub for_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AbsentExpectation {
    pub name: String,
    pub path: String,
    #[serde(default = "default_severity")]
    pub severity: AlertSeverity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub for_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ContentExpectation {
    pub name: String,
    pub path: String,
    /// Literal substrings; each must be present.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contains: Vec<String>,
    /// Regexes; each must match somewhere.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matches: Vec<String>,
    #[serde(default = "default_severity")]
    pub severity: AlertSeverity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub for_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PermsExpectation {
    pub name: String,
    pub path: String,
    /// Octal permission bits, exact ("0600"). `lstat` needs no read
    /// permission, so this works on secrets the sensor cannot open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Owner by name (`/etc/passwd`) or numeric uid. NSS/LDAP-resolved hosts
    /// should use numeric.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default = "default_severity")]
    pub severity: AlertSeverity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub for_secs: Option<u64>,
}

/// The whole hot-swappable assertion set. Every field defaults, so a partial
/// `expectations/set` payload is legal and means "empty for the kinds you
/// omitted" — the systemd convention.
///
/// `Default` is hand-written to agree with the serde defaults: the derived
/// impl would zero `eval_interval_secs`, making an absent `expectations`
/// block fail its own validation — caught by the minimal-config test.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ExpectationsConfig {
    #[serde(default = "default_eval_interval")]
    pub eval_interval_secs: u64,
    /// Set-wide alert debounce; `0` = fire on the first failing sweep (see
    /// the module doc for why that is the right default here).
    #[serde(default)]
    pub default_for_secs: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<MountExpectation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileExpectation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub listening: Vec<ListeningExpectation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub symlinks: Vec<SymlinkExpectation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub absent: Vec<AbsentExpectation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<ContentExpectation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub perms: Vec<PermsExpectation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AssertionStatus {
    Pass,
    Fail,
    /// Could not check — and that is never a pass (module doc).
    Unreadable,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AssertionResult {
    /// `<kind>:<name>` — the alert rule this assertion fires under.
    pub rule: String,
    pub kind: String,
    pub name: String,
    pub status: AssertionStatus,
    /// First violation summary when not passing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub severity: AlertSeverity,
}

/// The `@rpc/hostspec/spec` reply: per-assertion outcome plus when it was
/// computed. `evaluated_at_ms == 0` is the honest "not yet evaluated" —
/// never a fabricated pass.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct HostspecEvaluation {
    pub evaluated_at_ms: i64,
    pub eval_interval_secs: u64,
    pub assertions: Vec<AssertionResult>,
}

impl Default for ExpectationsConfig {
    fn default() -> Self {
        ExpectationsConfig {
            eval_interval_secs: default_eval_interval(),
            default_for_secs: 0,
            mounts: Vec::new(),
            files: Vec::new(),
            listening: Vec::new(),
            symlinks: Vec::new(),
            absent: Vec::new(),
            content: Vec::new(),
            perms: Vec::new(),
        }
    }
}

/// `(kind, name, severity, for_secs)` for every expectation, in a stable
/// order — the sweep and the `spec` reply both walk this.
macro_rules! for_each_kind {
    ($cfg:expr, $f:expr) => {{
        let f = $f;
        for e in &$cfg.mounts {
            f("mount", &e.name);
        }
        for e in &$cfg.files {
            f("file", &e.name);
        }
        for e in &$cfg.listening {
            f("listening", &e.name);
        }
        for e in &$cfg.symlinks {
            f("symlink", &e.name);
        }
        for e in &$cfg.absent {
            f("absent", &e.name);
        }
        for e in &$cfg.content {
            f("content", &e.name);
        }
        for e in &$cfg.perms {
            f("perms", &e.name);
        }
    }};
}

impl ExpectationsConfig {
    pub fn is_empty(&self) -> bool {
        self.mounts.is_empty()
            && self.files.is_empty()
            && self.listening.is_empty()
            && self.symlinks.is_empty()
            && self.absent.is_empty()
            && self.content.is_empty()
            && self.perms.is_empty()
    }

    /// Every rule slug the current set can produce (`<kind>:<name>`), for the
    /// seen-rules GC.
    pub fn rule_slugs(&self) -> HashSet<String> {
        let out = std::cell::RefCell::new(HashSet::new());
        for_each_kind!(self, |kind: &str, name: &str| {
            out.borrow_mut().insert(format!("{kind}:{name}"));
        });
        out.into_inner()
    }
}

pub fn parse_mode(s: &str) -> Option<u32> {
    let v = u32::from_str_radix(s, 8).ok()?;
    (v <= 0o7777).then_some(v)
}
