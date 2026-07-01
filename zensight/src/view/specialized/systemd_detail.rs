//! On-demand detail fetches for the systemd specialized view (#281).
//!
//! Mirrors `netlink_detail`: each `@/query/*` topic has its own [`Fetch`] slot so
//! the UI can show idle/loading/ready/error independently. Record types are the
//! shared ones from `zensight-common::query_detail`; the event record matches the
//! sensor's `events::EventRecord` JSON.

use std::sync::Arc;

use serde::Deserialize;
use zensight_common::query_detail::{CgroupNode, TimerRecord, UnitRecord};

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
    pub fn key(&self) -> String {
        let topic = match self {
            SystemdDetailTopic::Units => "units",
            SystemdDetailTopic::Timers => "timers",
            SystemdDetailTopic::Events => "events",
            SystemdDetailTopic::Cgroups => "cgroups",
        };
        format!("zensight/systemd/@/query/{topic}")
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

/// Fetch + decode the first reply on `key` as a single `T` (for the cgroups tree,
/// which replies one object rather than an array).
pub async fn fetch_one<T: serde::de::DeserializeOwned>(
    session: Arc<zenoh::Session>,
    key: String,
) -> Option<T> {
    let replies = session.get(&key).await.ok()?;
    let reply = replies.recv_async().await.ok()?;
    let sample = reply.result().ok()?;
    serde_json::from_slice(&sample.payload().to_bytes()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_keys_and_labels() {
        assert_eq!(
            SystemdDetailTopic::Units.key(),
            "zensight/systemd/@/query/units"
        );
        assert_eq!(
            SystemdDetailTopic::Cgroups.key(),
            "zensight/systemd/@/query/cgroups"
        );
        assert_eq!(SystemdDetailTopic::Timers.label(), "Timers");
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
