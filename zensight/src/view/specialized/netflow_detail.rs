//! On-demand NetFlow record client: fetches the bounded recent-flow ring from
//! the sensor's `@rpc/netflow/flows` procedure (#469).
//!
//! This procedure is what keyspace-v2 put in place of per-flow-pair telemetry
//! keys — a flow is an event with unbounded cardinality, not a metric, so it is
//! pulled as a record and never published as a key. It shipped with the cutover
//! and had no caller until now; the netflow view meanwhile reconstructed
//! "flows" from telemetry *labels* that the sensor stopped emitting, which is
//! why every row read `0.0.0.0:0 -> 0.0.0.0:0`.

use std::sync::Arc;

use zensight_common::NetflowRecord;

use crate::view::specialized::fetch::Fetch;

/// How many flow records to ask the sensor's ring for.
const FLOWS_MAX: usize = 200;

/// The recent-flow ring key (`?max=N`). `Some(origin)` targets the drilled-in
/// exporter's host; `None` selects the fleet.
pub fn flows_key(origin: Option<&zenkey::RemoteOrigin>) -> String {
    let key = match origin {
        Some(o) => zensight_common::origin_rpc_key(o, "netflow", "flows"),
        None => zensight_common::fleet_rpc_key("netflow", "flows"),
    };
    format!("{key}?max={FLOWS_MAX}")
}

/// On-demand flow detail fetched for the selected netflow exporter.
#[derive(Debug, Clone, Default)]
pub struct NetflowDetailState {
    pub flows: Fetch<Vec<NetflowRecord>>,
    pub table: crate::view::components::TableState,
}

impl NetflowDetailState {
    pub fn loading(&mut self) {
        self.flows = Fetch::Loading;
    }

    pub fn apply(&mut self, result: Result<Vec<NetflowRecord>, String>) {
        self.flows = Fetch::from_result(result);
    }
}

/// Fetch + decode the recent-flow ring.
pub async fn fetch_flows(
    session: Arc<zenoh::Session>,
    origin: Option<zenkey::RemoteOrigin>,
) -> Option<Vec<NetflowRecord>> {
    match origin {
        Some(o) => super::netlink_detail::fetch_records(session, flows_key(Some(&o))).await,
        None => super::netlink_detail::fetch_records_all(session, flows_key(None)).await,
    }
}

#[cfg(test)]
mod tests {
    /// A parsed origin for the drill-down key tests (#485): the builders take
    /// a `RemoteOrigin` now, so a test cannot hand them a string that would
    /// never have routed.
    fn test_origin() -> zenkey::RemoteOrigin {
        zenkey::RemoteOrigin::parse("h-3fa9c2d41b7e").expect("valid test origin")
    }

    use super::*;

    #[test]
    fn flows_key_is_origin_scoped_or_fleet() {
        assert_eq!(
            flows_key(Some(&test_origin())),
            "v1/h-3fa9c2d41b7e/@rpc/netflow/flows?max=200"
        );
        assert_eq!(flows_key(None), "v1/*/@rpc/netflow/flows?max=200");
    }
}
