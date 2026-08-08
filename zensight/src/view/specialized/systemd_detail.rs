//! On-demand detail fetches for the systemd specialized view (#281).
//!
//! Mirrors `netlink_detail`: each `@rpc/systemd/*` topic has its own [`Fetch`] slot so
//! the UI can show idle/loading/ready/error independently. Record types are the
//! shared ones from `zensight-common::query_detail`; the event record matches the
//! sensor's `events::EventRecord` JSON.

use std::sync::Arc;

use serde::Deserialize;
use zensight_common::query_detail::{CgroupNode, TimerRecord, UnitDetail, UnitRecord};

use super::fetch::Fetch;

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
}

/// Fetched systemd detail, each channel with its own loading/error state.
#[derive(Debug, Clone, Default)]
pub struct SystemdDetailState {
    pub units: Fetch<Vec<UnitRecord>>,
    pub timers: Fetch<Vec<TimerRecord>>,
    pub events: Fetch<Vec<SystemdEventRecord>>,
    pub cgroups: Fetch<Option<CgroupNode>>,
    /// Units table: active-state filter (`None` = all).
    pub unit_state_filter: Option<String>,
    /// Armed (verb, unit) awaiting inline confirmation in the Units tab (#283).
    /// `Some` swaps that unit's action buttons for a confirm/cancel pair.
    pub pending_action: Option<(String, String)>,
    /// The unit whose identity drill-down panel is open (#313).
    pub selected_unit: Option<String>,
    /// The drill-down's `@rpc/systemd/unit?name=` reply (#313): control_group,
    /// MainPID + start_time, invocation_id — the cross-view join keys.
    pub unit_detail: Fetch<UnitDetail>,
}

impl SystemdDetailState {
    /// Mark a topic's fetch as in flight.
    pub fn loading(&mut self, topic: SystemdDetailTopic) {
        match topic {
            SystemdDetailTopic::Units => self.units = Fetch::Loading,
            SystemdDetailTopic::Timers => self.timers = Fetch::Loading,
            SystemdDetailTopic::Events => self.events = Fetch::Loading,
            SystemdDetailTopic::Cgroups => self.cgroups = Fetch::Loading,
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
            Err(e) => match topic {
                SystemdDetailTopic::Units => self.units = Fetch::Error(e),
                SystemdDetailTopic::Timers => self.timers = Fetch::Error(e),
                SystemdDetailTopic::Events => self.events = Fetch::Error(e),
                SystemdDetailTopic::Cgroups => self.cgroups = Fetch::Error(e),
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
        assert!(!action_set_key("h-3fa9c2d41b7e").contains('*'));
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
