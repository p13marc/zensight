//! On-demand detail fetches for the systemd specialized view (#281).
//!
//! Mirrors `netlink_detail`: each `@rpc/systemd/*` topic has its own [`Fetch`] slot so
//! the UI can show idle/loading/ready/error independently. Record types are the
//! shared ones from `zensight-common::query_detail`; the event record matches the
//! sensor's `events::EventRecord` JSON.

use std::sync::Arc;

use serde::Deserialize;
use zensight_common::action::{ActionCapability, Verb};
use zensight_common::query_detail::{CgroupNode, TimerRecord, UnitDetail, UnitRecord};

use super::fetch::Fetch;
use crate::view::components::TableState;

/// One control-plane timeline event (matches the sensor's `EventRecord` JSON).
#[derive(Debug, Clone, Deserialize)]
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

/// Which systemd detail channel to fetch.
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
    /// The queryable key for this topic (matches the sensor's `query.rs`).
    /// `Some(origin)` targets the drilled-in host's concrete key; `None`
    /// selects the fleet.
    pub fn key(&self, origin: Option<&str>) -> String {
        let topic = match self {
            SystemdDetailTopic::Units => "units",
            SystemdDetailTopic::Timers => "timers",
            SystemdDetailTopic::Events => "events",
            SystemdDetailTopic::Cgroups => "cgroups",
            SystemdDetailTopic::Actions => "actions",
        };
        match origin {
            Some(o) => zensight_common::origin_rpc_key(o, "systemd", topic),
            None => zensight_common::fleet_rpc_key("systemd", topic),
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

/// A decoded systemd detail payload.
#[derive(Debug, Clone)]
pub enum SystemdDetailData {
    Units(Vec<UnitRecord>),
    Timers(Vec<TimerRecord>),
    Events(Vec<SystemdEventRecord>),
    /// The cgroups query replies a single tree node (or `null`).
    Cgroups(Option<CgroupNode>),
    Actions(Vec<zensight_common::action::ActionStatus>),
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
}

/// The unit type the table shows until told otherwise.
pub const DEFAULT_UNIT_TYPE: &str = ".service";

/// Unit-type suffixes offered as filter chips, in the order they render.
pub const UNIT_TYPES: [&str; 5] = [".service", ".timer", ".socket", ".mount", ".target"];

/// Fetched systemd detail, each channel with its own loading/error state.
#[derive(Debug, Clone)]
pub struct SystemdDetailState {
    pub units: Fetch<Vec<UnitRecord>>,
    pub timers: Fetch<Vec<TimerRecord>>,
    pub events: Fetch<Vec<SystemdEventRecord>>,
    pub cgroups: Fetch<Option<CgroupNode>>,
    /// The service-control audit ring (#283).
    pub actions: Fetch<Vec<zensight_common::action::ActionStatus>>,
    /// Units table: sort column, filter-box text, row cap.
    pub units_table: TableState,
    /// Units table: active-state filter (`None` = all).
    pub unit_state_filter: Option<String>,
    /// Units table: unit-type suffix filter (`.service`, `.timer`, …;
    /// `None` = all). Defaults to `.service` — "restart a service" is the job
    /// this table mostly exists for, and a host has hundreds of other units.
    pub unit_type_filter: Option<String>,
    /// This host's advertised service-control gate (#283).
    pub capability: Fetch<ActionCapability>,
    /// An action issued and not yet resolved. The write blocks until the job
    /// completes, so without this a second click would queue a second job.
    pub action_inflight: Option<(Verb, String)>,
    /// Armed (verb, unit) awaiting inline confirmation in the Units tab (#283).
    /// `Some` swaps that unit's action buttons for a confirm/cancel pair.
    pub pending_action: Option<(Verb, String)>,
    /// The unit whose identity drill-down panel is open (#313).
    pub selected_unit: Option<String>,
    /// The drill-down's `@rpc/systemd/unit?name=` reply (#313): control_group,
    /// MainPID + start_time, invocation_id — the cross-view join keys.
    pub unit_detail: Fetch<UnitDetail>,
    /// The selected unit's on-disk definition, fetched on demand (opt-in per
    /// host). Reset whenever the selected unit changes.
    pub unit_file: Fetch<zensight_common::query_detail::UnitFile>,
    /// Last seen `events/job_removed_total`. A change means some unit's state
    /// moved on the host — including from outside ZenSight — so the open table
    /// is stale and should re-pull. `None` until first sight, so arriving at a
    /// host does not itself trigger a refresh.
    pub job_events_seen: Option<f64>,
}

impl Default for SystemdDetailState {
    fn default() -> Self {
        Self {
            units: Fetch::default(),
            timers: Fetch::default(),
            events: Fetch::default(),
            cgroups: Fetch::default(),
            actions: Fetch::default(),
            units_table: TableState::default(),
            unit_state_filter: None,
            // Not `None`: a host lists hundreds of units, and the operator
            // reaching for this table is almost always after a service.
            unit_type_filter: Some(DEFAULT_UNIT_TYPE.to_string()),
            capability: Fetch::default(),
            action_inflight: None,
            pending_action: None,
            selected_unit: None,
            unit_detail: Fetch::default(),
            unit_file: Fetch::default(),
            job_events_seen: None,
        }
    }
}

impl SystemdDetailState {
    /// Whether `unit` passes the chip filters (state + type). The filter-box
    /// text is applied separately by the table itself.
    pub fn chips_admit(&self, unit: &UnitRecord) -> bool {
        let state_ok = self
            .unit_state_filter
            .as_deref()
            .is_none_or(|f| unit.active_state == f);
        let type_ok = self
            .unit_type_filter
            .as_deref()
            .is_none_or(|suffix| unit.name.ends_with(suffix));
        state_ok && type_ok
    }

    /// What the Units tab may offer for `unit`.
    ///
    /// The allowlist half delegates to [`zensight_common::action::allows`] — the
    /// same function the sensor's gate calls — so this preview cannot promise a
    /// button the host will refuse, nor grey out one it would have accepted.
    pub fn action_gate(&self, unit: &str) -> ActionGate {
        if let Some((verb, busy_unit)) = &self.action_inflight
            && busy_unit == unit
        {
            return ActionGate::Busy(*verb);
        }
        let Some(cap) = self.capability.ready() else {
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
    pub fn permits_daemon_reload(&self) -> bool {
        self.capability
            .ready()
            .is_some_and(|c| c.permits(Verb::DaemonReload))
    }

    /// The query deadline for an action on this host: the sensor blocks until
    /// the job resolves, so our own timeout must clear its `job_timeout_secs` or
    /// every slow restart reads as a failure. The grace covers the D-Bus enqueue
    /// and the reply hop, so a sensor hitting *its* timeout still gets to answer
    /// "issued, result unknown" — a strictly better outcome than us timing out.
    pub fn action_timeout(&self) -> std::time::Duration {
        const GRACE_SECS: u64 = 5;
        let job = self
            .capability
            .ready()
            .map(|c| c.job_timeout_secs)
            .unwrap_or(30)
            .clamp(5, 120);
        std::time::Duration::from_secs(job + GRACE_SECS)
    }

    /// Mark a topic's fetch as in flight.
    pub fn loading(&mut self, topic: SystemdDetailTopic) {
        match topic {
            SystemdDetailTopic::Units => self.units = Fetch::Loading,
            SystemdDetailTopic::Timers => self.timers = Fetch::Loading,
            SystemdDetailTopic::Events => self.events = Fetch::Loading,
            SystemdDetailTopic::Cgroups => self.cgroups = Fetch::Loading,
            SystemdDetailTopic::Actions => self.actions = Fetch::Loading,
        }
    }

    /// Store a topic's fetch outcome.
    pub fn apply(&mut self, topic: SystemdDetailTopic, result: Result<SystemdDetailData, String>) {
        match result {
            Ok(SystemdDetailData::Units(v)) => self.units = Fetch::Ready(v),
            Ok(SystemdDetailData::Timers(v)) => self.timers = Fetch::Ready(v),
            Ok(SystemdDetailData::Events(mut v)) => {
                // Timelines render newest-first.
                v.sort_by_key(|r| std::cmp::Reverse(r.ts_unix));
                self.events = Fetch::Ready(v);
            }
            Ok(SystemdDetailData::Cgroups(v)) => self.cgroups = Fetch::Ready(v),
            Ok(SystemdDetailData::Actions(mut v)) => {
                v.sort_by_key(|a| std::cmp::Reverse(a.ts_unix));
                self.actions = Fetch::Ready(v);
            }
            Err(e) => match topic {
                SystemdDetailTopic::Units => self.units = Fetch::Error(e),
                SystemdDetailTopic::Timers => self.timers = Fetch::Error(e),
                SystemdDetailTopic::Events => self.events = Fetch::Error(e),
                SystemdDetailTopic::Cgroups => self.cgroups = Fetch::Error(e),
                SystemdDetailTopic::Actions => self.actions = Fetch::Error(e),
            },
        }
    }
}

/// The service-control write key for one host: `…/v1/<origin>/@rpc/systemd/action/set`.
///
/// Deliberately takes `&str`, not `Option<&str>` like the read keys above: there
/// is no way to spell a wildcard action key, so a per-row start/stop/restart can
/// never widen into a fleet broadcast. A caller without an origin must refuse.
/// (Contrast `parallax_stream_set_key`, which does fall back to the fleet — a
/// media control is recoverable, `stop nginx.service` on every host is not.)
pub fn action_set_key(origin: &str) -> String {
    zensight_common::origin_rpc_key(origin, "systemd", "action/set")
}

/// The last-action-outcome read key for one host, origin-scoped for the same
/// reason as [`action_set_key`]: a fleet read returns whichever host replied
/// first, which is not necessarily the one we acted on.
pub fn action_read_key(origin: &str) -> String {
    zensight_common::origin_rpc_key(origin, "systemd", "action")
}

/// The service-control probe key. Answered by every 1.4+ sensor, enabled or not.
pub fn action_capability_key(origin: &str) -> String {
    zensight_common::origin_rpc_key(origin, "systemd", "action/capability")
}

/// The audit-timeline key: a bounded ring of recent action outcomes.
pub fn actions_history_key(origin: &str) -> String {
    zensight_common::origin_rpc_key(origin, "systemd", "actions")
}

/// The unit-file read key, matching the sensor's `unit/file?name=` queryable.
pub fn unit_file_key(origin: Option<&str>, unit: &str) -> String {
    let key = match origin {
        Some(o) => zensight_common::origin_rpc_key(o, "systemd", "unit/file"),
        None => zensight_common::fleet_rpc_key("systemd", "unit/file"),
    };
    format!("{key}?name={unit}")
}

/// Why an action produced no `ActionStatus`. GUI-only — not a wire type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionFailure {
    /// The sensor refused: the `error/gated` reply-error it sent back.
    Refused {
        error: String,
        message: String,
    },
    /// Replies closed well before our deadline — nobody serves the key, so
    /// actions are off or the sensor is offline.
    NotServed,
    /// Our deadline elapsed. The job was accepted and may still be running; this
    /// is emphatically not a failure, and must not be reported as one.
    StillRunning {
        waited_secs: u64,
    },
    Transport(String),
}

/// The single-unit detail key (#313), matching the sensor's
/// `@rpc/systemd/unit?name=<u>` queryable.
pub fn unit_detail_key(origin: Option<&str>, unit: &str) -> String {
    let key = match origin {
        Some(o) => zensight_common::origin_rpc_key(o, "systemd", "unit"),
        None => zensight_common::fleet_rpc_key("systemd", "unit"),
    };
    format!("{key}?name={unit}")
}

/// Fetch + decode one unit's detail for the drill-down panel (#313).
pub async fn fetch_unit_detail(
    session: Arc<zenoh::Session>,
    origin: Option<String>,
    unit: String,
) -> Option<UnitDetail> {
    fetch_one(session, unit_detail_key(origin.as_deref(), &unit)).await
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
    zensight_common::decode_auto(&sample.payload().to_bytes()).ok()
}

#[cfg(test)]
mod tests {
    // Fixtures build state stepwise (`let mut s = State::default(); s.field = ..`),
    // which reads more clearly here than a struct literal naming every field —
    // the same call the integration tests make.
    #![allow(clippy::field_reassign_with_default)]

    use super::*;

    #[test]
    fn topic_keys_and_labels() {
        assert_eq!(
            SystemdDetailTopic::Units.key(None),
            "v1/*/@rpc/systemd/units"
        );
        assert_eq!(
            SystemdDetailTopic::Cgroups.key(None),
            "v1/*/@rpc/systemd/cgroups"
        );
        assert_eq!(SystemdDetailTopic::Timers.label(), "Timers");
        // Single-unit detail key (#313) matches the sensor's queryable selector.
        assert_eq!(
            unit_detail_key(None, "sshd.service"),
            "v1/*/@rpc/systemd/unit?name=sshd.service"
        );
    }

    /// Service control is addressed to exactly one host. A wildcard here would
    /// restart the unit on every host running the sensor; the key builders take
    /// a concrete origin so that cannot be spelled.
    #[test]
    fn action_keys_are_origin_scoped() {
        assert_eq!(
            action_set_key("h-3fa9c2d41b7e"),
            "v1/h-3fa9c2d41b7e/@rpc/systemd/action/set"
        );
        assert_eq!(
            action_read_key("h-3fa9c2d41b7e"),
            "v1/h-3fa9c2d41b7e/@rpc/systemd/action"
        );
        assert_eq!(
            action_capability_key("h-3fa9c2d41b7e"),
            "v1/h-3fa9c2d41b7e/@rpc/systemd/action/capability"
        );
        assert_eq!(
            actions_history_key("h-3fa9c2d41b7e"),
            "v1/h-3fa9c2d41b7e/@rpc/systemd/actions"
        );
        for k in [
            action_set_key("h-3fa9c2d41b7e"),
            action_read_key("h-3fa9c2d41b7e"),
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
        }
    }

    #[test]
    fn the_table_shows_services_until_told_otherwise() {
        let st = SystemdDetailState::default();
        assert!(st.chips_admit(&unit("nginx.service", "active")));
        assert!(!st.chips_admit(&unit("logrotate.timer", "active")));
    }

    #[test]
    fn chip_filters_compose() {
        let mut st = SystemdDetailState::default();
        st.unit_state_filter = Some("failed".to_string());
        assert!(st.chips_admit(&unit("nginx.service", "failed")));
        assert!(!st.chips_admit(&unit("nginx.service", "active")), "state");
        assert!(!st.chips_admit(&unit("x.timer", "failed")), "type");
        // Clearing the type chip widens to every unit type.
        st.unit_type_filter = None;
        assert!(st.chips_admit(&unit("x.timer", "failed")));
    }

    #[test]
    fn gate_is_unknown_until_the_probe_answers() {
        let st = SystemdDetailState::default();
        assert_eq!(st.action_gate("nginx.service"), ActionGate::Unknown);
    }

    #[test]
    fn gate_reports_a_read_only_host() {
        let mut st = SystemdDetailState::default();
        st.capability = Fetch::Ready(cap(false, &[]));
        assert_eq!(st.action_gate("nginx.service"), ActionGate::Disabled);
        assert!(!st.permits_daemon_reload());
    }

    /// The preview must agree with the sensor's gate, which is why both call
    /// `zensight_common::action::allows`.
    #[test]
    fn gate_follows_the_allowlist() {
        let mut st = SystemdDetailState::default();
        st.capability = Fetch::Ready(cap(true, &["app-*.service"]));
        assert_eq!(st.action_gate("nginx.service"), ActionGate::NotAllowed);
        match st.action_gate("app-web.service") {
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

    #[test]
    fn gate_blocks_re_arming_while_an_action_is_in_flight() {
        let mut st = SystemdDetailState::default();
        st.capability = Fetch::Ready(cap(true, &["*"]));
        st.action_inflight = Some((Verb::Restart, "nginx.service".to_string()));
        assert_eq!(
            st.action_gate("nginx.service"),
            ActionGate::Busy(Verb::Restart)
        );
        // Only that unit is busy.
        assert!(matches!(
            st.action_gate("sshd.service"),
            ActionGate::Allowed(_)
        ));
    }

    /// Our deadline must exceed the sensor's, or a slow-but-successful restart
    /// reads as a failure.
    #[test]
    fn action_timeout_clears_the_sensors_job_wait() {
        let mut st = SystemdDetailState::default();
        assert_eq!(st.action_timeout().as_secs(), 35, "unprobed default");

        let mut c = cap(true, &["*"]);
        c.job_timeout_secs = 90;
        st.capability = Fetch::Ready(c.clone());
        assert!(st.action_timeout().as_secs() > 90);

        // A nonsense advertised timeout cannot hang the UI forever.
        c.job_timeout_secs = 100_000;
        st.capability = Fetch::Ready(c);
        assert_eq!(st.action_timeout().as_secs(), 125);
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
    fn apply_sorts_events_newest_first() {
        let mut st = SystemdDetailState::default();
        st.apply(
            SystemdDetailTopic::Events,
            Ok(SystemdDetailData::Events(vec![
                SystemdEventRecord {
                    ts_unix: 100,
                    kind: "job_removed".into(),
                    unit: Some("a.service".into()),
                    from: None,
                    to: None,
                    job_result: Some("done".into()),
                },
                SystemdEventRecord {
                    ts_unix: 200,
                    kind: "unit_new".into(),
                    unit: Some("b.service".into()),
                    from: None,
                    to: None,
                    job_result: None,
                },
            ])),
        );
        let events = st.events.ready().unwrap();
        assert_eq!(events[0].ts_unix, 200); // newest first
    }

    #[test]
    fn apply_error_sets_error_state() {
        let mut st = SystemdDetailState::default();
        st.apply(SystemdDetailTopic::Units, Err("boom".into()));
        assert_eq!(st.units.error(), Some("boom"));
    }

    #[test]
    fn event_record_json_roundtrip() {
        let json = r#"{"ts_unix":1700,"kind":"job_removed","unit":"x.service","from":"active","to":"failed","job_result":"failed"}"#;
        let r: SystemdEventRecord = serde_json::from_str(json).unwrap();
        assert_eq!(r.kind, "job_removed");
        assert_eq!(r.to.as_deref(), Some("failed"));
    }
}
