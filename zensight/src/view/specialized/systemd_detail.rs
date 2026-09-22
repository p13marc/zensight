//! The systemd view's on-demand vocabulary (#281): the topics it calls its
//! `@rpc/systemd/*` read procedures by (the calls go through
//! `Message::Call` and land in `DeviceDetailState::calls`, #1261), the
//! service-control gate a row is judged by, the write keys of the audited
//! action path, and the unit filters. Record types are the shared ones from
//! `zensight-common::query_detail`; the event record matches the sensor's
//! `events::EventRecord` JSON.
//!
//! Nothing stays a state of its own: the action machine is the generic
//! `DeviceDetailState::writes`, and the job counter the Units tab watches
//! is `DeviceDetailState::counters_seen` (#1261).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use zensight_common::action::{ActionCapability, Verb};
use zensight_common::query_detail::UnitRecord;

use crate::message::Message;

/// One control-plane timeline event (matches the sensor's `EventRecord` JSON).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemdEventRecord {
    pub ts_unix: u64,
    pub kind: String,
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub job_result: Option<String>,
}

/// Which systemd read procedure a panel calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemdDetailTopic {
    Units,
    Timers,
    Events,
    Cgroups,
    /// The service-control audit ring.
    Actions,
}

impl SystemdDetailTopic {
    /// The procedure this topic calls (matches the sensor's `query.rs`), and
    /// the name its answer is keyed by.
    pub fn procedure(&self) -> &'static str {
        match self {
            SystemdDetailTopic::Units => "units",
            SystemdDetailTopic::Timers => "timers",
            SystemdDetailTopic::Events => "events",
            SystemdDetailTopic::Cgroups => "cgroups",
            SystemdDetailTopic::Actions => "actions",
        }
    }

    /// The call for this topic on the selected device (#1261).
    pub fn call(&self) -> Message {
        Message::Call {
            procedure: self.procedure().to_string(),
            params: String::new(),
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            SystemdDetailTopic::Units => "Units",
            SystemdDetailTopic::Timers => "Timers",
            SystemdDetailTopic::Events => "Events",
            SystemdDetailTopic::Cgroups => "cgroups",
            SystemdDetailTopic::Actions => "Actions",
        }
    }
}

/// What the Units tab may offer for one unit, decided from the host's advertised
/// [`ActionCapability`]. Pure, so the whole table is testable without a bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionGate {
    /// The probe has not answered yet, or this host predates it. Controls render
    /// disabled rather than hidden: hiding would silently strip working buttons
    /// from an older sensor that does have actions on.
    Unknown,
    /// The host answered "service control is off here".
    Disabled,
    /// Actions are on, but this unit is outside `allow_units`.
    NotAllowed,
    /// Actions are on and this unit is in scope; the verbs are those the host
    /// advertised.
    Allowed(Vec<Verb>),
    /// An action on this unit is in flight — no re-arming until it resolves.
    Busy(Verb),
    /// A template (`getty@.service`): a pattern, not a unit. No host can start
    /// it — only an instance of it — so the row offers nothing to press.
    Template,
}

/// Whether `unit` is a template (`getty@.service`) rather than a real unit.
///
/// The stem ends with `@` only when the instance name is empty, so an actual
/// instance (`getty@tty1.service`) is correctly not a template.
pub fn is_template(unit: &str) -> bool {
    unit.rsplit_once('.')
        .is_some_and(|(stem, _)| stem.ends_with('@'))
}

/// The unit type the table shows until told otherwise.
pub const DEFAULT_UNIT_TYPE: &str = ".service";

/// Unit-type suffixes offered as filter chips, in the order they render.
pub const UNIT_TYPES: [&str; 5] = [".service", ".timer", ".socket", ".mount", ".target"];

/// The Units table's chip filters (#1261), read from the device's view
/// filters: `units/state` (`""`/unset = every state) and `units/type`
/// (unset = [`DEFAULT_UNIT_TYPE`], `""` = every type — a host lists hundreds
/// of units, and the operator reaching for this table is almost always
/// after a service).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnitFilters<'a> {
    pub state: Option<&'a str>,
    pub unit_type: Option<&'a str>,
}

impl<'a> UnitFilters<'a> {
    pub fn of(state: &'a crate::view::device::DeviceDetailState) -> Self {
        UnitFilters {
            state: state.filter_opt("units", "state").filter(|s| !s.is_empty()),
            unit_type: match state.filter_opt("units", "type") {
                None => Some(DEFAULT_UNIT_TYPE),
                Some("") => None,
                Some(t) => Some(t),
            },
        }
    }

    /// Whether `unit` passes the chip filters (state + type). The filter-box
    /// text is applied separately by the table itself.
    pub fn admit(&self, unit: &UnitRecord) -> bool {
        let state_ok = self.state.is_none_or(|f| unit.active_state == f);
        let type_ok = self
            .unit_type
            .is_none_or(|suffix| unit.name.ends_with(suffix));
        state_ok && type_ok
    }
}

/// What the Units tab may offer for `unit`, from the host's advertised gate
/// (`@rpc/systemd/action/capability`, `None` until it answers) and the
/// action in flight.
///
/// The allowlist half delegates to [`zensight_common::action::allows`] — the
/// same function the sensor's gate calls — so this preview cannot promise a
/// button the host will refuse, nor grey out one it would have accepted.
pub fn action_gate(
    capability: Option<&ActionCapability>,
    inflight: Option<&(Verb, String)>,
    unit: &str,
) -> ActionGate {
    if let Some((verb, busy_unit)) = inflight
        && busy_unit == unit
    {
        return ActionGate::Busy(*verb);
    }
    // Independent of the host's gate: a template is not a startable unit
    // anywhere. The inventory lists them because they are worth finding.
    if is_template(unit) {
        return ActionGate::Template;
    }
    let Some(cap) = capability else {
        return ActionGate::Unknown;
    };
    if !cap.enabled {
        return ActionGate::Disabled;
    }
    if !zensight_common::action::allows(&cap.allow_units, unit) {
        return ActionGate::NotAllowed;
    }
    // Only the unit-scoped verbs belong in a row; daemon-reload is
    // manager-wide and lives in the tab header.
    let verbs: Vec<Verb> = cap
        .verbs
        .iter()
        .copied()
        .filter(|v| v.targets_unit() && cap.permits(*v))
        .collect();
    if verbs.is_empty() {
        ActionGate::NotAllowed
    } else {
        ActionGate::Allowed(verbs)
    }
}

/// Whether this host advertises manager-wide `daemon-reload`.
pub fn permits_daemon_reload(capability: Option<&ActionCapability>) -> bool {
    capability.is_some_and(|c| c.permits(Verb::DaemonReload))
}

/// The query deadline for an action on this host: the sensor blocks until
/// the job resolves, so our own timeout must clear its `job_timeout_secs` or
/// every slow restart reads as a failure. The grace covers the D-Bus enqueue
/// and the reply hop, so a sensor hitting *its* timeout still gets to answer
/// "issued, result unknown" — a strictly better outcome than us timing out.
pub fn action_timeout(capability: Option<&ActionCapability>) -> std::time::Duration {
    const GRACE_SECS: u64 = 5;
    let job = capability
        .map(|c| c.job_timeout_secs)
        .unwrap_or(30)
        .clamp(5, 120);
    std::time::Duration::from_secs(job + GRACE_SECS)
}

/// The service-control write key for one host: `…/v1/<origin>/@rpc/systemd/action/set`.
///
/// Deliberately takes `&str`, not `Option<&str>` like the read keys above: there
/// is no way to spell a wildcard action key, so a per-row start/stop/restart can
/// never widen into a fleet broadcast. A caller without an origin must refuse.
/// (Contrast `parallax_stream_set_key`, which does fall back to the fleet — a
/// media control is recoverable, `stop nginx.service` on every host is not.)
pub fn action_set_key(origin: &zenkey::RemoteOrigin) -> String {
    zensight_common::origin_rpc_key(origin, "systemd", "action/set")
}

/// The last-action-outcome read key for one host, origin-scoped for the same
/// reason as [`action_set_key`]: a fleet read returns whichever host replied
/// first, which is not necessarily the one we acted on.
pub fn action_read_key(origin: &zenkey::RemoteOrigin) -> String {
    zensight_common::origin_rpc_key(origin, "systemd", "action")
}

/// The service-control probe key. Answered by every 1.4+ sensor, enabled or not.
pub fn action_capability_key(origin: &zenkey::RemoteOrigin) -> String {
    zensight_common::origin_rpc_key(origin, "systemd", "action/capability")
}

/// The audit-timeline key: a bounded ring of recent action outcomes.
pub fn actions_history_key(origin: &zenkey::RemoteOrigin) -> String {
    zensight_common::origin_rpc_key(origin, "systemd", "actions")
}

/// Extract the systemd unit name from a cgroup path (#313) — the
/// `process.cgroup == unit.control_group` join, reduced to a clickable name:
/// `/system.slice/redis.service` → `redis.service`. Only leaf `.service` /
/// `.scope` segments resolve (slices are aggregates, not pivotable units).
pub fn unit_from_cgroup(cgroup: &str) -> Option<String> {
    let leaf = cgroup.rsplit('/').next()?.trim();
    (leaf.ends_with(".service") || leaf.ends_with(".scope")).then(|| leaf.to_string())
}

/// Fetch + decode the first reply on `key` as a single `T` (for the cgroups tree,
/// which replies one object rather than an array).
pub async fn fetch_one<T: serde::de::DeserializeOwned>(
    session: Arc<zenoh::Session>,
    key: String,
) -> Option<T> {
    let replies = session.get(&key).await.ok()?;
    let reply = replies.recv_async().await.ok()?;
    let sample = reply.result().ok()?;
    zensight_common::decode_with_encoding(sample.encoding(), &sample.payload().to_bytes()).ok()
}

#[cfg(test)]
mod tests {
    // Fixtures build state stepwise (`let mut s = State::default(); s.field = ..`),
    // which reads more clearly here than a struct literal naming every field —
    // the same call the integration tests make.
    #![allow(clippy::field_reassign_with_default)]

    use super::*;

    /// A parsed origin for the drill-down key tests (#485).
    fn test_origin() -> zenkey::RemoteOrigin {
        zenkey::RemoteOrigin::parse("h-3fa9c2d41b7e").expect("valid test origin")
    }

    #[test]
    fn topics_name_the_sensor_s_procedures() {
        use SystemdDetailTopic as T;
        for (topic, procedure) in [
            (T::Units, "units"),
            (T::Timers, "timers"),
            (T::Events, "events"),
            (T::Cgroups, "cgroups"),
            (T::Actions, "actions"),
        ] {
            assert_eq!(topic.procedure(), procedure);
            assert!(matches!(
                topic.call(),
                Message::Call { procedure: p, params } if p == procedure && params.is_empty()
            ));
        }
        assert_eq!(SystemdDetailTopic::Timers.label(), "Timers");
    }

    /// Service control is addressed to exactly one host. A wildcard here would
    /// restart the unit on every host running the sensor; the key builders take
    /// a concrete origin so that cannot be spelled.
    #[test]
    fn action_keys_are_origin_scoped() {
        assert_eq!(
            action_set_key(&test_origin()),
            "v1/h-3fa9c2d41b7e/@rpc/systemd/action/set"
        );
        assert_eq!(
            action_read_key(&test_origin()),
            "v1/h-3fa9c2d41b7e/@rpc/systemd/action"
        );
        assert_eq!(
            action_capability_key(&test_origin()),
            "v1/h-3fa9c2d41b7e/@rpc/systemd/action/capability"
        );
        assert_eq!(
            actions_history_key(&test_origin()),
            "v1/h-3fa9c2d41b7e/@rpc/systemd/actions"
        );
        for k in [
            action_set_key(&test_origin()),
            action_read_key(&test_origin()),
        ] {
            assert!(!k.contains('*'), "{k} must not be a fleet selector");
        }
    }

    fn unit(name: &str, active: &str) -> UnitRecord {
        UnitRecord {
            name: name.to_string(),
            description: format!("{name} desc"),
            load_state: "loaded".to_string(),
            active_state: active.to_string(),
            sub_state: "running".to_string(),
            job: None,
            unit_file_state: None,
        }
    }

    fn cap(enabled: bool, allow: &[&str]) -> ActionCapability {
        ActionCapability {
            enabled,
            allow_units: allow.iter().map(|s| s.to_string()).collect(),
            job_timeout_secs: 30,
            verbs: Verb::all(),
            unit_files: true,
            daemon_reload: true,
            reason: None,
        }
    }

    fn device() -> crate::view::device::DeviceDetailState {
        crate::view::device::DeviceDetailState::new(crate::message::DeviceId::fixture(
            "systemd", "server01",
        ))
    }

    #[test]
    fn the_table_shows_services_until_told_otherwise() {
        let st = device();
        let filters = UnitFilters::of(&st);
        assert!(filters.admit(&unit("nginx.service", "active")));
        assert!(!filters.admit(&unit("logrotate.timer", "active")));
    }

    #[test]
    fn chip_filters_compose() {
        let mut st = device();
        st.filters.insert("units/state".into(), "failed".into());
        let filters = UnitFilters::of(&st);
        assert!(filters.admit(&unit("nginx.service", "failed")));
        assert!(!filters.admit(&unit("nginx.service", "active")), "state");
        assert!(!filters.admit(&unit("x.timer", "failed")), "type");
        // Clearing the type chip widens to every unit type; clearing the
        // state chip is the same spelling.
        st.filters.insert("units/type".into(), String::new());
        assert!(UnitFilters::of(&st).admit(&unit("x.timer", "failed")));
        st.filters.insert("units/state".into(), String::new());
        assert!(UnitFilters::of(&st).admit(&unit("x.timer", "active")));
    }

    #[test]
    fn gate_is_unknown_until_the_probe_answers() {
        assert_eq!(
            action_gate(None, None, "nginx.service"),
            ActionGate::Unknown
        );
    }

    #[test]
    fn gate_reports_a_read_only_host() {
        let c = cap(false, &[]);
        assert_eq!(
            action_gate(Some(&c), None, "nginx.service"),
            ActionGate::Disabled
        );
        assert!(!permits_daemon_reload(Some(&c)));
    }

    /// The preview must agree with the sensor's gate, which is why both call
    /// `zensight_common::action::allows`.
    #[test]
    fn gate_follows_the_allowlist() {
        let c = cap(true, &["app-*.service"]);
        assert_eq!(
            action_gate(Some(&c), None, "nginx.service"),
            ActionGate::NotAllowed
        );
        match action_gate(Some(&c), None, "app-web.service") {
            ActionGate::Allowed(verbs) => {
                assert!(verbs.contains(&Verb::Restart));
                assert!(
                    !verbs.contains(&Verb::DaemonReload),
                    "manager-wide verbs do not belong in a row"
                );
            }
            other => panic!("expected Allowed, got {other:?}"),
        }
    }

    /// The inventory lists templates so they can be found, but nothing can act
    /// on one — not even a host that allowlists it.
    #[test]
    fn gate_offers_nothing_on_a_template() {
        let c = cap(true, &["*"]);
        assert_eq!(
            action_gate(Some(&c), None, "getty@.service"),
            ActionGate::Template
        );
        // An instance of that template is a real unit and stays actionable.
        assert!(matches!(
            action_gate(Some(&c), None, "getty@tty1.service"),
            ActionGate::Allowed(_)
        ));
    }

    #[test]
    fn template_detection_needs_an_empty_instance_name() {
        assert!(is_template("getty@.service"));
        assert!(is_template("sshd@.socket"));
        assert!(!is_template("getty@tty1.service"));
        assert!(!is_template("nginx.service"));
        assert!(!is_template("weird@"));
    }

    #[test]
    fn gate_blocks_re_arming_while_an_action_is_in_flight() {
        let c = cap(true, &["*"]);
        let busy = (Verb::Restart, "nginx.service".to_string());
        assert_eq!(
            action_gate(Some(&c), Some(&busy), "nginx.service"),
            ActionGate::Busy(Verb::Restart)
        );
        // Only that unit is busy.
        assert!(matches!(
            action_gate(Some(&c), Some(&busy), "sshd.service"),
            ActionGate::Allowed(_)
        ));
    }

    /// Our deadline must exceed the sensor's, or a slow-but-successful restart
    /// reads as a failure.
    #[test]
    fn action_timeout_clears_the_sensors_job_wait() {
        assert_eq!(action_timeout(None).as_secs(), 35, "unprobed default");

        let mut c = cap(true, &["*"]);
        c.job_timeout_secs = 90;
        assert!(action_timeout(Some(&c)).as_secs() > 90);

        // A nonsense advertised timeout cannot hang the UI forever.
        c.job_timeout_secs = 100_000;
        assert_eq!(action_timeout(Some(&c)).as_secs(), 125);
    }

    #[test]
    fn unit_from_cgroup_resolves_leaf_units_only() {
        assert_eq!(
            unit_from_cgroup("/system.slice/redis.service").as_deref(),
            Some("redis.service")
        );
        assert_eq!(
            unit_from_cgroup("/user.slice/user-1000.slice/session-2.scope").as_deref(),
            Some("session-2.scope")
        );
        // Slices and non-unit paths are not pivotable.
        assert_eq!(unit_from_cgroup("/system.slice"), None);
        assert_eq!(unit_from_cgroup(""), None);
        assert_eq!(unit_from_cgroup("/sys/fs/cgroup"), None);
    }

    #[test]
    fn event_record_json_roundtrip() {
        let json = r#"{"ts_unix":1700,"kind":"job_removed","unit":"x.service","from":"active","to":"failed","job_result":"failed"}"#;
        let r: SystemdEventRecord = serde_json::from_str(json).unwrap();
        assert_eq!(r.kind, "job_removed");
        assert_eq!(r.to.as_deref(), Some("failed"));
    }
}
