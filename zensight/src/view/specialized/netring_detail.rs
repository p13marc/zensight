//! On-demand netring flow-detail client: fetches the recent-flow ring from the
//! sensor's `@/query/flows` channel (principle P2 — pulled only when a user
//! drills into a netring host, never streamed).
//!
//! Reuses the Iced-independent [`fetch_records`](super::netlink_detail::fetch_records)
//! so the fetch+decode path is shared and already integration-tested.

use std::sync::Arc;

use zensight_common::{
    AssetRecord, DnsRecord, ElephantRecord, FlowRecord, HttpHostRecord, Ja4hRecord, MatrixRecord,
    QuicRecord, SshRecord, TalkerRecord, TlsRecord,
};

use crate::view::specialized::fetch::Fetch;

/// How many rows the top-N query channels (talkers/dns/http) ask the sensor for.
const TOP_N: usize = 50;

/// The flow-detail queryable key (matches the netring sensor's `query.rs`).
pub fn flows_key() -> String {
    "zensight/netring/@/query/flows".to_string()
}

/// The TLS-inventory queryable key.
pub fn tls_key() -> String {
    "zensight/netring/@/query/tls".to_string()
}

/// The QUIC SNI/ALPN inventory queryable key.
pub fn quic_key() -> String {
    "zensight/netring/@/query/quic".to_string()
}

/// The SSH/HASSH inventory queryable key.
pub fn ssh_key() -> String {
    "zensight/netring/@/query/ssh".to_string()
}

/// The JA4H HTTP-fingerprint inventory queryable key (#124).
pub fn ja4h_key() -> String {
    "zensight/netring/@/query/ja4h".to_string()
}

/// The passive asset-inventory queryable key.
pub fn assets_key() -> String {
    "zensight/netring/@/query/assets".to_string()
}

/// The per-destination top-talker histogram key (`?top=N`).
pub fn talkers_key() -> String {
    format!("zensight/netring/@/query/talkers?top={TOP_N}")
}

/// The recent-elephant-flow ring key.
pub fn elephant_key() -> String {
    "zensight/netring/@/query/elephant_flows".to_string()
}

/// The `(src,dst)` traffic-matrix / service-map key (`?top=N`) (#122).
pub fn matrix_key() -> String {
    format!("zensight/netring/@/query/matrix?top={TOP_N}")
}

/// The per-SLD DNS detail key (`?top=N`).
pub fn dns_key() -> String {
    format!("zensight/netring/@/query/dns?top={TOP_N}")
}

/// The per-host HTTP detail key (`?top=N`).
pub fn http_key() -> String {
    format!("zensight/netring/@/query/http?top={TOP_N}")
}

/// On-demand detail fetched for the selected netring host.
#[derive(Debug, Clone, Default)]
pub struct NetringDetailState {
    pub flows: Fetch<Vec<FlowRecord>>,
    pub tls: Fetch<Vec<TlsRecord>>,
    pub quic: Fetch<Vec<QuicRecord>>,
    pub ssh: Fetch<Vec<SshRecord>>,
    pub assets: Fetch<Vec<AssetRecord>>,
    pub talkers: Fetch<Vec<TalkerRecord>>,
    pub matrix: Fetch<Vec<MatrixRecord>>,
    pub elephants: Fetch<Vec<ElephantRecord>>,
    pub dns: Fetch<Vec<DnsRecord>>,
    pub http: Fetch<Vec<HttpHostRecord>>,
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

    /// Mark a QUIC-inventory fetch as in flight.
    pub fn loading_quic(&mut self) {
        self.quic = Fetch::Loading;
    }

    /// Store the QUIC-inventory fetch outcome.
    pub fn apply_quic(&mut self, result: Result<Vec<QuicRecord>, String>) {
        self.quic = Fetch::from_result(result);
    }

    /// Mark an SSH-inventory fetch as in flight.
    pub fn loading_ssh(&mut self) {
        self.ssh = Fetch::Loading;
    }

    /// Store the SSH-inventory fetch outcome.
    pub fn apply_ssh(&mut self, result: Result<Vec<SshRecord>, String>) {
        self.ssh = Fetch::from_result(result);
    }

    /// Mark an asset-inventory fetch as in flight.
    pub fn loading_assets(&mut self) {
        self.assets = Fetch::Loading;
    }

    /// Store the asset-inventory fetch outcome.
    pub fn apply_assets(&mut self, result: Result<Vec<AssetRecord>, String>) {
        self.assets = Fetch::from_result(result);
    }

    /// Mark a top-talker fetch as in flight.
    pub fn loading_talkers(&mut self) {
        self.talkers = Fetch::Loading;
    }

    /// Store the top-talker fetch outcome.
    pub fn apply_talkers(&mut self, result: Result<Vec<TalkerRecord>, String>) {
        self.talkers = Fetch::from_result(result);
    }

    /// Mark a traffic-matrix fetch as in flight (#122).
    pub fn loading_matrix(&mut self) {
        self.matrix = Fetch::Loading;
    }

    /// Store the traffic-matrix fetch outcome (#122).
    pub fn apply_matrix(&mut self, result: Result<Vec<MatrixRecord>, String>) {
        self.matrix = Fetch::from_result(result);
    }

    /// Mark an elephant-flow fetch as in flight.
    pub fn loading_elephants(&mut self) {
        self.elephants = Fetch::Loading;
    }

    /// Store the elephant-flow fetch outcome.
    pub fn apply_elephants(&mut self, result: Result<Vec<ElephantRecord>, String>) {
        self.elephants = Fetch::from_result(result);
    }

    /// Mark a DNS-detail fetch as in flight.
    pub fn loading_dns(&mut self) {
        self.dns = Fetch::Loading;
    }

    /// Store the DNS-detail fetch outcome.
    pub fn apply_dns(&mut self, result: Result<Vec<DnsRecord>, String>) {
        self.dns = Fetch::from_result(result);
    }

    /// Mark an HTTP-detail fetch as in flight.
    pub fn loading_http(&mut self) {
        self.http = Fetch::Loading;
    }

    /// Store the HTTP-detail fetch outcome.
    pub fn apply_http(&mut self, result: Result<Vec<HttpHostRecord>, String>) {
        self.http = Fetch::from_result(result);
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

/// Fetch + decode the QUIC SNI/ALPN inventory.
pub async fn fetch_quic(session: Arc<zenoh::Session>) -> Option<Vec<QuicRecord>> {
    super::netlink_detail::fetch_records(session, quic_key()).await
}

/// Fetch + decode the SSH/HASSH inventory.
pub async fn fetch_ssh(session: Arc<zenoh::Session>) -> Option<Vec<SshRecord>> {
    super::netlink_detail::fetch_records(session, ssh_key()).await
}

/// Fetch + decode the JA4H HTTP-fingerprint inventory (#124).
pub async fn fetch_ja4h(session: Arc<zenoh::Session>) -> Option<Vec<Ja4hRecord>> {
    super::netlink_detail::fetch_records(session, ja4h_key()).await
}

/// Fetch + decode the passive asset inventory.
pub async fn fetch_assets(session: Arc<zenoh::Session>) -> Option<Vec<AssetRecord>> {
    super::netlink_detail::fetch_records(session, assets_key()).await
}

/// Fetch + decode the per-destination top-talker histogram.
pub async fn fetch_talkers(session: Arc<zenoh::Session>) -> Option<Vec<TalkerRecord>> {
    super::netlink_detail::fetch_records(session, talkers_key()).await
}

/// Fetch + decode the recent-elephant-flow ring.
pub async fn fetch_elephants(session: Arc<zenoh::Session>) -> Option<Vec<ElephantRecord>> {
    super::netlink_detail::fetch_records(session, elephant_key()).await
}

/// Fetch + decode the `(src,dst)` traffic matrix / service map (#122).
pub async fn fetch_matrix(session: Arc<zenoh::Session>) -> Option<Vec<MatrixRecord>> {
    super::netlink_detail::fetch_records(session, matrix_key()).await
}

/// Fetch + decode the per-SLD DNS detail (top SLDs / NXDOMAIN).
pub async fn fetch_dns(session: Arc<zenoh::Session>) -> Option<Vec<DnsRecord>> {
    super::netlink_detail::fetch_records(session, dns_key()).await
}

/// Fetch + decode the per-host HTTP detail (top hosts / errors).
pub async fn fetch_http(session: Arc<zenoh::Session>) -> Option<Vec<HttpHostRecord>> {
    super::netlink_detail::fetch_records(session, http_key()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_matches_sensor() {
        assert_eq!(flows_key(), "zensight/netring/@/query/flows");
        assert_eq!(quic_key(), "zensight/netring/@/query/quic");
        assert_eq!(ssh_key(), "zensight/netring/@/query/ssh");
        assert_eq!(ja4h_key(), "zensight/netring/@/query/ja4h");
        assert_eq!(tls_key(), "zensight/netring/@/query/tls");
        assert_eq!(assets_key(), "zensight/netring/@/query/assets");
        // The 4 previously-orphaned channels now reachable (#45).
        assert_eq!(talkers_key(), "zensight/netring/@/query/talkers?top=50");
        assert_eq!(elephant_key(), "zensight/netring/@/query/elephant_flows");
        assert_eq!(dns_key(), "zensight/netring/@/query/dns?top=50");
        assert_eq!(http_key(), "zensight/netring/@/query/http?top=50");
        // Traffic-matrix / service-map channel (#122).
        assert_eq!(matrix_key(), "zensight/netring/@/query/matrix?top=50");
    }

    #[test]
    fn apply_stores_traffic_matrix() {
        let mut s = NetringDetailState::default();
        s.loading_matrix();
        assert!(s.matrix.is_loading());
        s.apply_matrix(Ok(vec![MatrixRecord {
            src: "10.0.0.1".into(),
            dst: "8.8.8.8".into(),
            bytes: 5000,
            packets: 40,
            flows: 6,
        }]));
        assert_eq!(s.matrix.ready().map(|v| v.len()), Some(1));
        s.apply_matrix(Err("no sensor".into()));
        assert_eq!(s.matrix.error(), Some("no sensor"));
    }

    #[test]
    fn apply_stores_dns_http_talkers_elephants() {
        let mut s = NetringDetailState::default();
        s.loading_dns();
        assert!(s.dns.is_loading());
        s.apply_dns(Ok(vec![DnsRecord {
            domain: "example".into(),
            queries: 10,
            nxdomain: 2,
        }]));
        assert_eq!(s.dns.ready().map(|v| v.len()), Some(1));
        s.apply_http(Ok(vec![HttpHostRecord {
            host: "api.example.com".into(),
            requests: 30,
            errors: 1,
        }]));
        assert_eq!(s.http.ready().map(|v| v.len()), Some(1));
        s.apply_talkers(Ok(vec![TalkerRecord {
            dst: "1.1.1.1".into(),
            bytes: 1000,
            packets: 10,
            flows: 2,
        }]));
        assert_eq!(s.talkers.ready().map(|v| v.len()), Some(1));
        s.apply_elephants(Err("no sensor".into()));
        assert_eq!(s.elephants.error(), Some("no sensor"));
    }

    #[test]
    fn apply_stores_quic_and_ssh() {
        let mut s = NetringDetailState::default();
        s.loading_quic();
        assert!(s.quic.is_loading());
        s.apply_quic(Ok(vec![QuicRecord {
            sni: Some("example.com".into()),
            alpn: vec!["h3".into()],
            version: "v1".into(),
            count: 4,
        }]));
        assert_eq!(s.quic.ready().map(|v| v.len()), Some(1));

        s.apply_ssh(Ok(vec![SshRecord {
            hassh: "deadbeef".into(),
            role: "client".into(),
            banner: Some("SSH-2.0-OpenSSH_9.6".into()),
            count: 2,
        }]));
        assert_eq!(s.ssh.ready().map(|v| v.len()), Some(1));
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
            community_id: Some("1:abc".into()),
        }]));
        assert_eq!(s.flows.ready().map(|v| v.len()), Some(1));
        s.apply(Err("no sensor".into()));
        assert_eq!(s.flows.error(), Some("no sensor"));
    }
}
