//! Service-control wire types (RFC 05 §3): the request, the outcome, and the
//! gate state a host advertises.
//!
//! These live here rather than in `zensight-sensor-systemd` because both sides
//! of the contract need them: the sensor enforces the gate, and the frontend
//! previews it before the operator clicks. A shared wire contract that lives in
//! one participant's crate is not shared — see the same argument, already paid
//! for once, in [`crate::rpc`].
//!
//! In particular [`allows`] is *the* allowlist rule. The sensor's gate and the
//! GUI's per-row button state both call it, so the preview cannot drift from
//! the decision it is previewing.

use serde::{Deserialize, Serialize};

/// A service-control verb.
///
/// The first four enqueue a systemd job and resolve via `JobRemoved`; the last
/// three take effect synchronously and have no job to track (see
/// [`Verb::enqueues_job`]). `daemon-reload` is manager-wide and carries no unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Verb {
    Start,
    Stop,
    Restart,
    Reload,
    Enable,
    Disable,
    DaemonReload,
}

impl Verb {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verb::Start => "start",
            Verb::Stop => "stop",
            Verb::Restart => "restart",
            Verb::Reload => "reload",
            Verb::Enable => "enable",
            Verb::Disable => "disable",
            Verb::DaemonReload => "daemon-reload",
        }
    }

    /// Whether this verb enqueues a systemd job whose completion arrives on
    /// `JobRemoved`. `EnableUnitFiles`/`DisableUnitFiles`/`Reload` return no job
    /// object path at all, so their outcome is known the moment the call returns.
    pub fn enqueues_job(&self) -> bool {
        matches!(
            self,
            Verb::Start | Verb::Stop | Verb::Restart | Verb::Reload
        )
    }

    /// Whether this verb acts on a named unit. Only `daemon-reload` does not —
    /// it is manager-wide, so the unit allowlist cannot gate it.
    pub fn targets_unit(&self) -> bool {
        !matches!(self, Verb::DaemonReload)
    }

    /// Whether this verb writes unit *files* (symlinks under `/etc` or `/run`)
    /// rather than only changing runtime state. These need the
    /// `org.freedesktop.systemd1.manage-unit-files` polkit action and persist
    /// across reboots, so they sit behind their own switch.
    pub fn writes_unit_files(&self) -> bool {
        matches!(self, Verb::Enable | Verb::Disable)
    }

    /// Every verb, for advertising in [`ActionCapability::verbs`].
    pub fn all() -> Vec<Verb> {
        vec![
            Verb::Start,
            Verb::Stop,
            Verb::Restart,
            Verb::Reload,
            Verb::Enable,
            Verb::Disable,
            Verb::DaemonReload,
        ]
    }
}

impl std::fmt::Display for Verb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An action request on `@rpc/systemd/action/set`.
///
/// `unit` is empty for [`Verb::DaemonReload`], which is manager-wide.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ServiceAction {
    pub verb: Verb,
    #[serde(default)]
    pub unit: String,
}

/// One symlink change reported by `EnableUnitFiles`/`DisableUnitFiles` (the
/// D-Bus `a(sss)` reply).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UnitFileChange {
    /// `symlink` or `unlink`.
    pub change: String,
    pub path: String,
    #[serde(default)]
    pub destination: String,
}

/// The outcome of one action, replied by `@rpc/systemd/action/set` and held as
/// the most recent entry behind `@rpc/systemd/action`.
///
/// Note there is deliberately no `Default`: an all-empty `ActionStatus` is
/// indistinguishable from a rejection (`accepted: false`, no unit), which is
/// exactly the ambiguity that made "nothing has run yet" unreadable. The sensor
/// holds an `Option` and replies `null` instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ActionStatus {
    pub unit: String,
    pub verb: Verb,
    /// Whether the request cleared every gate and was issued.
    pub accepted: bool,
    /// For job verbs, the `JobRemoved` result (`done`/`failed`/`timeout`/…) once
    /// the job completed, or `None` if the bounded wait elapsed first — unknown,
    /// never assumed successful. For jobless verbs, `applied`.
    #[serde(default)]
    pub result: Option<String>,
    /// Rejection or error reason when `accepted` is false or the call errored.
    #[serde(default)]
    pub error: Option<String>,
    /// Symlinks written or removed by `enable`/`disable`.
    #[serde(default)]
    pub changes: Vec<UnitFileChange>,
    /// Set after `enable`/`disable`: systemd will not observe the change until a
    /// `daemon-reload`. ZenSight never chains one implicitly — it is a separate
    /// gate — so this is surfaced to the operator instead.
    #[serde(default)]
    pub needs_daemon_reload: bool,
    pub ts_unix: i64,
}

/// The service-control gate as one host actually configured it.
///
/// Served **whether or not actions are enabled**, so that "disabled" is an
/// answer rather than a silence. Silence cannot be told apart from an offline
/// host, an older build, or a sensor busy running someone else's 30-second job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ActionCapability {
    /// `actions.enabled`. When false, no write procedure is declared at all.
    pub enabled: bool,
    /// The `actions.allow_units` globs, verbatim, so a caller can preview the
    /// gate per unit with [`allows`] before offering a control.
    #[serde(default)]
    pub allow_units: Vec<String>,
    /// The sensor's bounded `JobRemoved` wait. A caller's own deadline must
    /// exceed this, or every slow job looks like a failure.
    pub job_timeout_secs: u64,
    /// Verbs this build and configuration will accept.
    #[serde(default)]
    pub verbs: Vec<Verb>,
    /// `actions.allow_unit_files` — whether `enable`/`disable` are permitted.
    #[serde(default)]
    pub unit_files: bool,
    /// `actions.allow_daemon_reload` — whether `daemon-reload` is permitted.
    #[serde(default)]
    pub daemon_reload: bool,
}

impl ActionCapability {
    /// A read-only host: the reply a sensor serves when `actions.enabled` is false.
    pub fn disabled(job_timeout_secs: u64) -> Self {
        Self {
            enabled: false,
            allow_units: Vec::new(),
            job_timeout_secs,
            verbs: Vec::new(),
            unit_files: false,
            daemon_reload: false,
        }
    }

    /// Whether `verb` is permitted here at all, ignoring the unit allowlist.
    pub fn permits(&self, verb: Verb) -> bool {
        self.enabled
            && self.verbs.contains(&verb)
            && (!verb.writes_unit_files() || self.unit_files)
            && (verb != Verb::DaemonReload || self.daemon_reload)
    }
}

/// Does `unit` clear the `allow_units` globs?
///
/// **The** allowlist rule, shared by the sensor's gate and the frontend's
/// preview. An empty pattern list allows nothing (a sensor with actions on and
/// no allowlist rejects every request), and an empty unit name never matches.
/// Glob semantics are the `glob` crate's: `*`, `?`, `[…]`. Invalid patterns are
/// skipped rather than treated as literals — a pattern that cannot compile must
/// not silently widen or narrow the gate.
pub fn allows(patterns: &[String], unit: &str) -> bool {
    if unit.is_empty() {
        return false;
    }
    patterns
        .iter()
        .filter_map(|p| glob::Pattern::new(p).ok())
        .any(|p| p.matches(unit))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pats(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn allowlist_matches_globs() {
        let a = pats(&["nginx.service", "app-*.service"]);
        assert!(allows(&a, "nginx.service"));
        assert!(allows(&a, "app-web.service"));
        assert!(!allows(&a, "sshd.service"));
        assert!(!allows(&a, ""));
    }

    #[test]
    fn empty_allowlist_rejects_everything() {
        assert!(!allows(&[], "nginx.service"));
    }

    #[test]
    fn invalid_pattern_is_skipped_not_treated_as_a_literal() {
        let a = pats(&["[unclosed", "nginx.service"]);
        assert!(allows(&a, "nginx.service"));
        assert!(!allows(&a, "[unclosed"));
    }

    #[test]
    fn service_action_json_shape() {
        let cmd: ServiceAction =
            serde_json::from_str(r#"{"verb":"restart","unit":"nginx.service"}"#).unwrap();
        assert_eq!(cmd.verb, Verb::Restart);
        assert_eq!(cmd.unit, "nginx.service");
    }

    #[test]
    fn all_verbs_roundtrip_on_the_wire() {
        for (s, v) in [
            ("start", Verb::Start),
            ("stop", Verb::Stop),
            ("restart", Verb::Restart),
            ("reload", Verb::Reload),
            ("enable", Verb::Enable),
            ("disable", Verb::Disable),
            ("daemon-reload", Verb::DaemonReload),
        ] {
            let cmd: ServiceAction =
                serde_json::from_str(&format!(r#"{{"verb":"{s}","unit":"x.service"}}"#)).unwrap();
            assert_eq!(cmd.verb, v);
            assert_eq!(v.as_str(), s);
            // Typing `verb` must not have changed how it serializes.
            assert_eq!(serde_json::to_value(v).unwrap(), serde_json::json!(s));
        }
    }

    #[test]
    fn daemon_reload_carries_no_unit() {
        let cmd: ServiceAction = serde_json::from_str(r#"{"verb":"daemon-reload"}"#).unwrap();
        assert_eq!(cmd.verb, Verb::DaemonReload);
        assert!(cmd.unit.is_empty());
        assert!(!cmd.verb.targets_unit());
    }

    #[test]
    fn verb_classes() {
        assert!(Verb::Restart.enqueues_job());
        assert!(!Verb::Enable.enqueues_job());
        assert!(!Verb::DaemonReload.enqueues_job());
        assert!(Verb::Enable.writes_unit_files());
        assert!(!Verb::Start.writes_unit_files());
    }

    /// A v1.3 sensor's payload predates `changes`/`needs_daemon_reload`; it must
    /// still decode.
    #[test]
    fn action_status_decodes_a_pre_1_4_payload() {
        let st: ActionStatus = serde_json::from_str(
            r#"{"unit":"nginx.service","verb":"restart","accepted":true,"result":"done","ts_unix":1700}"#,
        )
        .unwrap();
        assert!(st.accepted);
        assert_eq!(st.result.as_deref(), Some("done"));
        assert!(st.changes.is_empty());
        assert!(!st.needs_daemon_reload);
    }

    #[test]
    fn capability_tolerates_a_missing_verbs_field() {
        let cap: ActionCapability =
            serde_json::from_str(r#"{"enabled":true,"job_timeout_secs":30}"#).unwrap();
        assert!(cap.enabled);
        assert!(cap.verbs.is_empty());
        assert!(!cap.unit_files);
    }

    #[test]
    fn permits_honours_the_per_class_switches() {
        let mut cap = ActionCapability {
            enabled: true,
            allow_units: pats(&["*"]),
            job_timeout_secs: 30,
            verbs: Verb::all(),
            unit_files: false,
            daemon_reload: false,
        };
        assert!(cap.permits(Verb::Restart));
        assert!(!cap.permits(Verb::Enable), "unit_files is off");
        assert!(!cap.permits(Verb::DaemonReload), "daemon_reload is off");
        cap.unit_files = true;
        cap.daemon_reload = true;
        assert!(cap.permits(Verb::Enable));
        assert!(cap.permits(Verb::DaemonReload));
        // The master switch overrides every per-class switch.
        cap.enabled = false;
        assert!(!cap.permits(Verb::Restart));
        assert!(!cap.permits(Verb::Enable));
    }

    #[test]
    fn disabled_capability_advertises_nothing() {
        let cap = ActionCapability::disabled(30);
        assert!(!cap.enabled);
        assert!(cap.verbs.is_empty());
        assert!(cap.allow_units.is_empty());
        for v in Verb::all() {
            assert!(!cap.permits(v));
        }
    }
}
