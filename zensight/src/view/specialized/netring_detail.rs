//! On-demand netring flow-detail client: fetches the recent-flow ring from the
//! sensor's `@/query/flows` channel (principle P2 — pulled only when a user
//! drills into a netring host, never streamed).
//!
//! Reuses the Iced-independent [`fetch_records`](super::netlink_detail::fetch_records)
//! so the fetch+decode path is shared and already integration-tested.

use std::sync::Arc;

use zensight_common::{AssetRecord, FlowRecord, TlsRecord};

use crate::view::specialized::fetch::Fetch;

/// The flow-detail queryable key (matches the netring sensor's `query.rs`).
pub fn flows_key() -> String {
    "zensight/netring/@/query/flows".to_string()
}

/// The TLS-inventory queryable key.
pub fn tls_key() -> String {
    "zensight/netring/@/query/tls".to_string()
}

/// The passive asset-inventory queryable key.
pub fn assets_key() -> String {
    "zensight/netring/@/query/assets".to_string()
}

/// On-demand detail fetched for the selected netring host.
#[derive(Debug, Clone, Default)]
pub struct NetringDetailState {
    pub flows: Fetch<Vec<FlowRecord>>,
    pub tls: Fetch<Vec<TlsRecord>>,
    pub assets: Fetch<Vec<AssetRecord>>,
}

impl NetringDetailState {
    /// Mark a flow fetch as in flight (called when the request is sent).
    pub fn loading(&mut self) {
        self.flows = Fetch::Loading;
    }

    /// Store the flow fetch outcome (success or failure).
    pub fn apply(&mut self, result: Result<Vec<FlowRecord>, String>) {
        self.flows = Fetch::from_result(result);
    }

    /// Mark a TLS-inventory fetch as in flight.
    pub fn loading_tls(&mut self) {
        self.tls = Fetch::Loading;
    }

    /// Store the TLS-inventory fetch outcome.
    pub fn apply_tls(&mut self, result: Result<Vec<TlsRecord>, String>) {
        self.tls = Fetch::from_result(result);
    }

    /// Mark an asset-inventory fetch as in flight.
    pub fn loading_assets(&mut self) {
        self.assets = Fetch::Loading;
    }

    /// Store the asset-inventory fetch outcome.
    pub fn apply_assets(&mut self, result: Result<Vec<AssetRecord>, String>) {
        self.assets = Fetch::from_result(result);
    }
}

/// Fetch + decode the recent-flow ring. Thin wrapper over the shared helper.
pub async fn fetch_flows(session: Arc<zenoh::Session>) -> Option<Vec<FlowRecord>> {
    super::netlink_detail::fetch_records(session, flows_key()).await
}

/// Fetch + decode the TLS asset inventory.
pub async fn fetch_tls(session: Arc<zenoh::Session>) -> Option<Vec<TlsRecord>> {
    super::netlink_detail::fetch_records(session, tls_key()).await
}

/// Fetch + decode the passive asset inventory.
pub async fn fetch_assets(session: Arc<zenoh::Session>) -> Option<Vec<AssetRecord>> {
    super::netlink_detail::fetch_records(session, assets_key()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_matches_sensor() {
        assert_eq!(flows_key(), "zensight/netring/@/query/flows");
        assert_eq!(tls_key(), "zensight/netring/@/query/tls");
        assert_eq!(assets_key(), "zensight/netring/@/query/assets");
    }

    #[test]
    fn apply_stores_assets() {
        let mut s = NetringDetailState::default();
        assert!(s.assets.ready().is_none());
        s.loading_assets();
        assert!(s.assets.is_loading());
        s.apply_assets(Ok(vec![AssetRecord {
            mac: "aa:bb:cc:dd:ee:ff".into(),
            ipv4: vec!["10.0.0.5".into()],
            ipv6: vec![],
            hostname: Some("switch01".into()),
            vendor: None,
            platform: Some("cisco WS-C2960X".into()),
            capabilities: vec!["switch".into()],
            seen_via: vec!["lldp".into()],
            last_seen: 1_700_000_000_000,
        }]));
        assert_eq!(s.assets.ready().map(|v| v.len()), Some(1));
        s.apply_assets(Err("no sensor".into()));
        assert_eq!(s.assets.error(), Some("no sensor"));
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
