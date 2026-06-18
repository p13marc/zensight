//! On-demand netring flow-detail client: fetches the recent-flow ring from the
//! sensor's `@/query/flows` channel (principle P2 — pulled only when a user
//! drills into a netring host, never streamed).
//!
//! Reuses the Iced-independent [`fetch_records`](super::netlink_detail::fetch_records)
//! so the fetch+decode path is shared and already integration-tested.

use std::sync::Arc;

use zensight_common::FlowRecord;

use crate::view::specialized::fetch::Fetch;

/// The flow-detail queryable key (matches the netring sensor's `query.rs`).
pub fn flows_key() -> String {
    "zensight/netring/@/query/flows".to_string()
}

/// Recent flow records fetched on demand for the selected netring host.
#[derive(Debug, Clone, Default)]
pub struct NetringDetailState {
    pub flows: Fetch<Vec<FlowRecord>>,
}

impl NetringDetailState {
    /// Mark a fetch as in flight (called when the request is sent).
    pub fn loading(&mut self) {
        self.flows = Fetch::Loading;
    }

    /// Store the fetch outcome (success or failure).
    pub fn apply(&mut self, result: Result<Vec<FlowRecord>, String>) {
        self.flows = Fetch::from_result(result);
    }
}

/// Fetch + decode the recent-flow ring. Thin wrapper over the shared helper.
pub async fn fetch_flows(session: Arc<zenoh::Session>) -> Option<Vec<FlowRecord>> {
    super::netlink_detail::fetch_records(session, flows_key()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_matches_sensor() {
        assert_eq!(flows_key(), "zensight/netring/@/query/flows");
    }

    #[test]
    fn apply_stores_flows() {
        let mut s = NetringDetailState::default();
        assert!(s.flows.ready().is_none());
        s.loading();
        assert!(s.flows.is_loading());
        s.apply(Ok(vec![FlowRecord {
            src: "10.0.0.1:5555".into(),
            dst: "1.1.1.1:443".into(),
            proto: "tcp".into(),
            bytes: 694,
            packets: 10,
            duration_ms: 100,
            reason: "fin".into(),
        }]));
        assert_eq!(s.flows.ready().map(|v| v.len()), Some(1));
        s.apply(Err("no sensor".into()));
        assert_eq!(s.flows.error(), Some("no sensor"));
    }
}
