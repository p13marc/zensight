//! The netring view's on-demand vocabulary: the topics it calls its
//! `@rpc/netring/*` read procedures by (the calls go through `Message::Call`
//! and land in `DeviceDetailState::calls`, #1261 — principle P2, pulled only
//! when a user drills into a netring host, never streamed). The flow↔process
//! join is two of those calls to netlink, keyed by flow
//! (`specialized::attribution`); nothing of the view's is held elsewhere.
//!
//! The `*_key` builders and `fetch_*` helpers remain for the fleet-wide
//! joins (topology, the Security drill-down) that fetch with the `*` origin
//! and land elsewhere than a device.

use std::sync::Arc;

use zensight_common::{
    AssetRecord, CaptureRecord, DnsRecord, ElephantRecord, EncryptedDnsRecord, FlowRecord,
    HttpHostRecord, Ja4hRecord, MatrixRecord, QuicRecord, SshRecord, TalkerRecord, TlsRecord,
};

use crate::message::Message;

/// Which netring read procedure a panel calls — and the name its answer
/// and its table's UI state are keyed by (#1261).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NetringTopic {
    Flows,
    Elephants,
    Talkers,
    Matrix,
    Dns,
    EncryptedDns,
    Http,
    Tls,
    Quic,
    Ssh,
    Assets,
    Ja4h,
    Captures,
}

impl NetringTopic {
    /// The procedure this topic calls (matches the sensor's `query.rs`).
    pub fn procedure(&self) -> &'static str {
        match self {
            NetringTopic::Flows => "flows",
            NetringTopic::Elephants => "elephant_flows",
            NetringTopic::Talkers => "talkers",
            NetringTopic::Matrix => "matrix",
            NetringTopic::Dns => "dns",
            NetringTopic::EncryptedDns => "encrypted_dns",
            NetringTopic::Http => "http",
            NetringTopic::Tls => "tls",
            NetringTopic::Quic => "quic",
            NetringTopic::Ssh => "ssh",
            NetringTopic::Assets => "assets",
            NetringTopic::Ja4h => "ja4h",
            NetringTopic::Captures => "captures",
        }
    }

    /// The call's params: the top-N channels (talkers/matrix/dns/http) ask
    /// for `top=50`; the rest reply with their whole ring or inventory.
    pub fn params(&self) -> String {
        match self {
            NetringTopic::Talkers
            | NetringTopic::Matrix
            | NetringTopic::Dns
            | NetringTopic::Http => {
                format!("top={TOP_N}")
            }
            _ => String::new(),
        }
    }

    /// The call for this topic on the selected device (#1261).
    pub fn call(&self) -> Message {
        Message::Call(crate::call::Request::new(self.procedure(), self.params()))
    }
}

/// How many rows the top-N query channels (talkers/dns/http) ask the sensor for.
const TOP_N: usize = 50;

/// One netring @rpc key: `Some(origin)` targets that host's concrete
/// procedure key (device drill-down — RFC 05 §2); `None` selects the whole
/// fleet (`*` origin — inventory/topology joins, fetched with target `All`).
fn rpc_key(origin: Option<&zenkey::RemoteOrigin>, topic: &str) -> String {
    match origin {
        Some(o) => zensight_common::origin_rpc_key(o, "netring", topic),
        None => zensight_common::fleet_rpc_key("netring", topic),
    }
}

/// The flow-detail queryable key (matches the netring sensor's `query.rs`).
pub fn flows_key(origin: Option<&zenkey::RemoteOrigin>) -> String {
    rpc_key(origin, "flows")
}

/// The TLS-inventory queryable key.
pub fn tls_key(origin: Option<&zenkey::RemoteOrigin>) -> String {
    rpc_key(origin, "tls")
}

/// The QUIC SNI/ALPN inventory queryable key.
pub fn quic_key(origin: Option<&zenkey::RemoteOrigin>) -> String {
    rpc_key(origin, "quic")
}

/// The SSH/HASSH inventory queryable key.
pub fn ssh_key(origin: Option<&zenkey::RemoteOrigin>) -> String {
    rpc_key(origin, "ssh")
}

/// The JA4H HTTP-fingerprint inventory queryable key (#124).
pub fn ja4h_key(origin: Option<&zenkey::RemoteOrigin>) -> String {
    rpc_key(origin, "ja4h")
}

/// The passive asset-inventory queryable key.
pub fn assets_key(origin: Option<&zenkey::RemoteOrigin>) -> String {
    rpc_key(origin, "assets")
}

/// The per-destination top-talker histogram key (`?top=N`).
pub fn talkers_key(origin: Option<&zenkey::RemoteOrigin>) -> String {
    format!("{}?top={TOP_N}", rpc_key(origin, "talkers"))
}

/// The recent-elephant-flow ring key.
pub fn elephant_key(origin: Option<&zenkey::RemoteOrigin>) -> String {
    rpc_key(origin, "elephant_flows")
}

/// The `(src,dst)` traffic-matrix / service-map key (`?top=N`) (#122).
pub fn matrix_key(origin: Option<&zenkey::RemoteOrigin>) -> String {
    format!("{}?top={TOP_N}", rpc_key(origin, "matrix"))
}

/// The capture-to-disk file-index key (#327).
pub fn captures_key(origin: Option<&zenkey::RemoteOrigin>) -> String {
    rpc_key(origin, "captures")
}

/// The per-SLD DNS detail key (`?top=N`).
pub fn dns_key(origin: Option<&zenkey::RemoteOrigin>) -> String {
    format!("{}?top={TOP_N}", rpc_key(origin, "dns"))
}

/// The passive DoT/DoQ/DoH destination inventory key (#326). No `?top=` —
/// the sensor replies with the whole inventory, which is small by construction
/// (one row per transport × SNI × resolver class).
pub fn encrypted_dns_key(origin: Option<&zenkey::RemoteOrigin>) -> String {
    rpc_key(origin, "encrypted_dns")
}

/// The per-host HTTP detail key (`?top=N`).
pub fn http_key(origin: Option<&zenkey::RemoteOrigin>) -> String {
    format!("{}?top={TOP_N}", rpc_key(origin, "http"))
}

/// Fetch + decode the recent-flow ring. Thin wrapper over the shared helper.
pub async fn fetch_flows(
    session: Arc<zenoh::Session>,
    origin: Option<zenkey::RemoteOrigin>,
) -> Option<Vec<FlowRecord>> {
    match origin {
        Some(o) => super::netlink_detail::fetch_records(session, flows_key(Some(&o))).await,
        None => super::netlink_detail::fetch_records_all(session, flows_key(None)).await,
    }
}

/// Fetch + decode the TLS asset inventory.
pub async fn fetch_tls(
    session: Arc<zenoh::Session>,
    origin: Option<zenkey::RemoteOrigin>,
) -> Option<Vec<TlsRecord>> {
    match origin {
        Some(o) => super::netlink_detail::fetch_records(session, tls_key(Some(&o))).await,
        None => super::netlink_detail::fetch_records_all(session, tls_key(None)).await,
    }
}

/// Fetch + decode the QUIC SNI/ALPN inventory.
pub async fn fetch_quic(
    session: Arc<zenoh::Session>,
    origin: Option<zenkey::RemoteOrigin>,
) -> Option<Vec<QuicRecord>> {
    match origin {
        Some(o) => super::netlink_detail::fetch_records(session, quic_key(Some(&o))).await,
        None => super::netlink_detail::fetch_records_all(session, quic_key(None)).await,
    }
}

/// Fetch + decode the SSH/HASSH inventory.
pub async fn fetch_ssh(
    session: Arc<zenoh::Session>,
    origin: Option<zenkey::RemoteOrigin>,
) -> Option<Vec<SshRecord>> {
    match origin {
        Some(o) => super::netlink_detail::fetch_records(session, ssh_key(Some(&o))).await,
        None => super::netlink_detail::fetch_records_all(session, ssh_key(None)).await,
    }
}

/// Fetch + decode the JA4H HTTP-fingerprint inventory (#124).
pub async fn fetch_ja4h(
    session: Arc<zenoh::Session>,
    origin: Option<zenkey::RemoteOrigin>,
) -> Option<Vec<Ja4hRecord>> {
    match origin {
        Some(o) => super::netlink_detail::fetch_records(session, ja4h_key(Some(&o))).await,
        None => super::netlink_detail::fetch_records_all(session, ja4h_key(None)).await,
    }
}

/// Fetch + decode the passive asset inventory.
pub async fn fetch_assets(
    session: Arc<zenoh::Session>,
    origin: Option<zenkey::RemoteOrigin>,
) -> Option<Vec<AssetRecord>> {
    match origin {
        Some(o) => super::netlink_detail::fetch_records(session, assets_key(Some(&o))).await,
        None => super::netlink_detail::fetch_records_all(session, assets_key(None)).await,
    }
}

/// Fetch + decode the per-destination top-talker histogram.
pub async fn fetch_talkers(
    session: Arc<zenoh::Session>,
    origin: Option<zenkey::RemoteOrigin>,
) -> Option<Vec<TalkerRecord>> {
    match origin {
        Some(o) => super::netlink_detail::fetch_records(session, talkers_key(Some(&o))).await,
        None => super::netlink_detail::fetch_records_all(session, talkers_key(None)).await,
    }
}

/// Fetch + decode the recent-elephant-flow ring.
pub async fn fetch_elephants(
    session: Arc<zenoh::Session>,
    origin: Option<zenkey::RemoteOrigin>,
) -> Option<Vec<ElephantRecord>> {
    match origin {
        Some(o) => super::netlink_detail::fetch_records(session, elephant_key(Some(&o))).await,
        None => super::netlink_detail::fetch_records_all(session, elephant_key(None)).await,
    }
}

/// Fetch + decode the `(src,dst)` traffic matrix / service map (#122).
pub async fn fetch_matrix(
    session: Arc<zenoh::Session>,
    origin: Option<zenkey::RemoteOrigin>,
) -> Option<Vec<MatrixRecord>> {
    match origin {
        Some(o) => super::netlink_detail::fetch_records(session, matrix_key(Some(&o))).await,
        None => super::netlink_detail::fetch_records_all(session, matrix_key(None)).await,
    }
}

/// Fetch + decode the per-SLD DNS detail (top SLDs / NXDOMAIN).
pub async fn fetch_dns(
    session: Arc<zenoh::Session>,
    origin: Option<zenkey::RemoteOrigin>,
) -> Option<Vec<DnsRecord>> {
    match origin {
        Some(o) => super::netlink_detail::fetch_records(session, dns_key(Some(&o))).await,
        None => super::netlink_detail::fetch_records_all(session, dns_key(None)).await,
    }
}

/// Fetch + decode the passive encrypted-DNS destination inventory (#326).
pub async fn fetch_encrypted_dns(
    session: Arc<zenoh::Session>,
    origin: Option<zenkey::RemoteOrigin>,
) -> Option<Vec<EncryptedDnsRecord>> {
    match origin {
        Some(o) => super::netlink_detail::fetch_records(session, encrypted_dns_key(Some(&o))).await,
        None => super::netlink_detail::fetch_records_all(session, encrypted_dns_key(None)).await,
    }
}

/// Fetch + decode the per-host HTTP detail (top hosts / errors).
pub async fn fetch_http(
    session: Arc<zenoh::Session>,
    origin: Option<zenkey::RemoteOrigin>,
) -> Option<Vec<HttpHostRecord>> {
    match origin {
        Some(o) => super::netlink_detail::fetch_records(session, http_key(Some(&o))).await,
        None => super::netlink_detail::fetch_records_all(session, http_key(None)).await,
    }
}

/// Fetch + decode the capture-to-disk file index (#327).
pub async fn fetch_captures(
    session: Arc<zenoh::Session>,
    origin: Option<zenkey::RemoteOrigin>,
) -> Option<Vec<CaptureRecord>> {
    match origin {
        Some(o) => super::netlink_detail::fetch_records(session, captures_key(Some(&o))).await,
        None => super::netlink_detail::fetch_records_all(session, captures_key(None)).await,
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
    fn key_matches_sensor() {
        assert_eq!(flows_key(None), "v1/*/@rpc/netring/flows");
        assert_eq!(quic_key(None), "v1/*/@rpc/netring/quic");
        assert_eq!(ssh_key(None), "v1/*/@rpc/netring/ssh");
        assert_eq!(ja4h_key(None), "v1/*/@rpc/netring/ja4h");
        assert_eq!(tls_key(None), "v1/*/@rpc/netring/tls");
        assert_eq!(assets_key(None), "v1/*/@rpc/netring/assets");
        // The 4 previously-orphaned channels now reachable (#45).
        assert_eq!(talkers_key(None), "v1/*/@rpc/netring/talkers?top=50");
        assert_eq!(elephant_key(None), "v1/*/@rpc/netring/elephant_flows");
        assert_eq!(dns_key(None), "v1/*/@rpc/netring/dns?top=50");
        assert_eq!(http_key(None), "v1/*/@rpc/netring/http?top=50");
        // Traffic-matrix / service-map channel (#122).
        assert_eq!(matrix_key(None), "v1/*/@rpc/netring/matrix?top=50");
        // Capture-to-disk index channel (#327).
        assert_eq!(captures_key(None), "v1/*/@rpc/netring/captures");
        // The device drill-down targets one host's concrete procedure key.
        assert_eq!(
            flows_key(Some(&test_origin())),
            "v1/h-3fa9c2d41b7e/@rpc/netring/flows"
        );
    }

    #[test]
    fn topics_name_the_sensor_s_procedures_and_params() {
        use NetringTopic as T;
        for (topic, procedure, params) in [
            (T::Flows, "flows", ""),
            (T::Elephants, "elephant_flows", ""),
            (T::Talkers, "talkers", "top=50"),
            (T::Matrix, "matrix", "top=50"),
            (T::Dns, "dns", "top=50"),
            (T::EncryptedDns, "encrypted_dns", ""),
            (T::Http, "http", "top=50"),
            (T::Tls, "tls", ""),
            (T::Quic, "quic", ""),
            (T::Ssh, "ssh", ""),
            (T::Assets, "assets", ""),
            (T::Ja4h, "ja4h", ""),
            (T::Captures, "captures", ""),
        ] {
            assert_eq!(topic.procedure(), procedure);
            assert_eq!(topic.params(), params);
            assert!(matches!(
                topic.call(),
                Message::Call(r) if r.procedure == procedure && r.params == params
            ));
        }
    }
}
