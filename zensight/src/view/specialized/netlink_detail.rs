//! The netlink view's on-demand vocabulary: the record types its
//! `@rpc/netlink/*` procedures reply with (principle P2 — nothing is
//! streamed; the GUI pulls detail only when a user drills in), the topics it
//! calls them by, and the socket explorer's client-side filter. The calls
//! themselves go through `Message::Call` and land in
//! `DeviceDetailState::calls` (#1261); the view reads them back as
//! `Answer<Vec<Record>>`.
//!
//! The fetch+decode core ([`fetch_records`]) is independent of Iced so it can be
//! integration-tested against a real in-process Zenoh queryable; the netring
//! detail and the app's cross-sensor joins still use it.

use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use zensight_common::SocketRecord;

use crate::message::Message;

// The sensor defines these record types locally (it owns only its own crate); we
// mirror their JSON shape here so the GUI can decode the addresses/events/tc/
// xfrm/nft query channels (#109). Field names/types must match
// `zensight-sensor-netlink/src/{map,events}.rs` exactly.

/// One configured IP address (`@rpc/netlink/addresses`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AddressRecord {
    pub family: u8,
    pub ip: Option<String>,
    pub prefix_len: u8,
    pub scope: String,
    pub label: Option<String>,
    pub ifindex: u32,
}

/// One row of the recent control-plane events ring (`@rpc/netlink/events`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventRecord {
    pub ts_unix: u64,
    pub family: String,
    pub action: String,
    pub ifindex: Option<u32>,
    pub detail: String,
}

/// One default-route transition (`@rpc/netlink/route_changes`, #111). Mirrors the
/// sensor's `RouteChangeRecord` JSON shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteChangeRecord {
    pub ts_unix: u64,
    pub family: String,
    /// `"added"` / `"changed"` / `"withdrawn"`.
    pub action: String,
    pub gateway: Option<String>,
    pub prev_gateway: Option<String>,
}

/// One TC qdisc/class entry (`@rpc/netlink/tc`). `node` is `qdisc` or `class`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TcRecord {
    pub iface: String,
    pub node: String,
    pub kind: Option<String>,
    pub handle: String,
    pub parent: String,
    pub bytes: u64,
    pub packets: u64,
    pub drops: u64,
    pub overlimits: u64,
    pub requeues: u64,
    pub backlog_bytes: u64,
    pub backlog_pkts: u64,
}

/// One IPsec Security Association (`@rpc/netlink/xfrm`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct XfrmSaRecord {
    pub src: Option<String>,
    pub dst: Option<String>,
    pub spi: u32,
    pub proto: String,
    pub mode: String,
    pub reqid: u32,
    pub bytes: u64,
    pub packets: u64,
}

/// One nftables rule (`@rpc/netlink/nft`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NftRuleRecord {
    pub family: String,
    pub table: String,
    pub chain: String,
    pub handle: u64,
    pub comment: Option<String>,
    /// Decoded per-rule firewall counter (#115). `serde(default)` keeps older
    /// sensors (no counter fields) decodable.
    #[serde(default)]
    pub packets: u64,
    #[serde(default)]
    pub bytes: u64,
}

/// The eBPF query channels' reply types (`@rpc/netlink/{retransmits,connections}`,
/// #269/#114). These used to be hand-written mirrors of types that lived in the
/// sensor crate; they are shared now, under the names the registry declares.
pub use zensight_common::query_detail::{ConnectionRecord, RetransmitRecord};

/// Which detail table to call for — one read procedure of the netlink
/// slice each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetlinkDetailTopic {
    Sockets,
    Routes,
    Neighbors,
    Addresses,
    Events,
    RouteChanges,
    Tc,
    Xfrm,
    Nft,
    /// eBPF top-retransmit peers (#269), served only on eBPF-enabled hosts.
    Retransmits,
    /// eBPF tcplife connection records (#269).
    Connections,
}

impl NetlinkDetailTopic {
    /// The procedure this topic calls (matches the sensor's `query.rs`), and
    /// the name its answer and its table's UI state are keyed by.
    pub fn procedure(&self) -> &'static str {
        match self {
            NetlinkDetailTopic::Sockets => "sockets",
            NetlinkDetailTopic::Routes => "routes",
            NetlinkDetailTopic::Neighbors => "neighbors",
            NetlinkDetailTopic::Addresses => "addresses",
            NetlinkDetailTopic::Events => "events",
            NetlinkDetailTopic::RouteChanges => "route_changes",
            NetlinkDetailTopic::Tc => "tc",
            NetlinkDetailTopic::Xfrm => "xfrm",
            NetlinkDetailTopic::Nft => "nft",
            NetlinkDetailTopic::Retransmits => "retransmits",
            NetlinkDetailTopic::Connections => "connections",
        }
    }

    /// The call for this topic on the selected device (#1261).
    pub fn call(&self) -> Message {
        Message::Call(crate::call::Request::new(self.procedure(), String::new()))
    }

    pub fn label(&self) -> &'static str {
        match self {
            NetlinkDetailTopic::Sockets => "Sockets",
            NetlinkDetailTopic::Routes => "Routes",
            NetlinkDetailTopic::Neighbors => "Neighbors",
            NetlinkDetailTopic::Addresses => "Addresses",
            NetlinkDetailTopic::Events => "Events",
            NetlinkDetailTopic::RouteChanges => "Route flaps",
            NetlinkDetailTopic::Tc => "TC",
            NetlinkDetailTopic::Xfrm => "XFRM",
            NetlinkDetailTopic::Nft => "NFT",
            NetlinkDetailTopic::Retransmits => "Retransmits",
            NetlinkDetailTopic::Connections => "Connections",
        }
    }
}

/// Sort order for the socket explorer (#112). `Default` keeps the sensor's order;
/// the others surface the worst flows first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SocketSort {
    #[default]
    Default,
    /// Highest smoothed RTT first.
    Rtt,
    /// Highest retransmit count first.
    Retrans,
}

impl SocketSort {
    /// The value the `sockets/sort` filter holds (#1261).
    pub fn token(&self) -> &'static str {
        match self {
            SocketSort::Default => "",
            SocketSort::Rtt => "rtt",
            SocketSort::Retrans => "retrans",
        }
    }

    pub fn from_token(token: &str) -> Self {
        match token {
            "rtt" => SocketSort::Rtt,
            "retrans" => SocketSort::Retrans,
            _ => SocketSort::Default,
        }
    }
}

/// The port component of an `addr:port` endpoint (after the last colon), so IPv6
/// literals like `[::1]:443` still yield `443`. Empty when there is no port.
fn port_of(addr: &str) -> &str {
    addr.rsplit_once(':').map(|(_, p)| p).unwrap_or("")
}

/// Apply the active state/port filter and sort order to a socket record slice
/// (#112). Pure and borrow-returning so the explorer logic is testable without a
/// live session. State matches case-insensitively; the port filter is a substring
/// match against either endpoint's port.
pub fn filter_sort_sockets<'a>(
    socks: &'a [SocketRecord],
    state_filter: Option<&str>,
    port_filter: &str,
    sort: SocketSort,
) -> Vec<&'a SocketRecord> {
    let port = port_filter.trim();
    let mut out: Vec<&SocketRecord> = socks
        .iter()
        .filter(|s| state_filter.is_none_or(|st| s.state.eq_ignore_ascii_case(st)))
        .filter(|s| {
            port.is_empty() || port_of(&s.local).contains(port) || port_of(&s.remote).contains(port)
        })
        .collect();
    match sort {
        SocketSort::Default => {}
        SocketSort::Rtt => out.sort_by_key(|s| std::cmp::Reverse(s.rtt_us)),
        SocketSort::Retrans => out.sort_by_key(|s| std::cmp::Reverse(s.retrans)),
    }
    out
}

/// The sockets key narrowed to one endpoint IP (#309), for the flow↔process
/// join. Matches the sensor's `SocketSelector` `ip=` parameter.
pub fn sockets_match_key(ip: &str) -> String {
    format!(
        "{}?ip={ip}",
        zensight_common::fleet_rpc_key("netlink", "sockets")
    )
}

/// Fetch + decode **all** replies on `key`, concatenated (#309). The shared
/// netlink query keys are answered by every netlink sensor on the mesh — the
/// flow↔process join needs every host's (already `?ip=`-narrowed) rows, not
/// just whichever host replied first. `None` when no sensor replied at all.
pub async fn fetch_records_all<T: DeserializeOwned>(
    session: Arc<zenoh::Session>,
    key: String,
) -> Option<Vec<T>> {
    // Fleet fan-in: target All so BestMatching can never short-circuit the
    // multi-host consolidation (RFC 05 §2.1).
    let replies = match session
        .get(&key)
        .target(zenoh::query::QueryTarget::All)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(key = %key, error = %e, "query get failed");
            return None;
        }
    };
    let mut out: Vec<T> = Vec::new();
    let mut any = false;
    while let Ok(reply) = replies.recv_async().await {
        let Ok(sample) = reply.result() else { continue };
        match zensight_common::decode_with_encoding::<Vec<T>>(
            sample.encoding(),
            &sample.payload().to_bytes(),
        ) {
            Ok(mut records) => {
                out.append(&mut records);
                any = true;
            }
            Err(e) => tracing::warn!(key = %key, error = %e, "query: reply decode failed"),
        }
    }
    any.then_some(out)
}

/// Fetch + decode every reply on `key`, keeping the fullest one, into `Vec<T>`.
/// Returns `None` if no sensor replied or nothing decoded. Iced-independent
/// (testable).
///
/// One origin-scoped procedure key names ONE producer instance (RFC 05 §2.1),
/// so this is a single-answer read — but nothing on the wire enforces that.
/// Two processes minting the same host origin (a stray instance, or two hosts
/// cloned from one machine-id) both declare the same `@rpc` key and both
/// answer. Taking the *first* reply then makes every detail panel flap: the
/// live sensor's rows on one fetch, the idle twin's empty ring on the next.
/// So: target All, consolidation off, keep the reply that carries the most
/// records, and say out loud that the deployment has a duplicate.
///
/// Each failure path logs a `warn` naming the key, so a silently-empty detail
/// table (netring/netlink on-demand tabs) is diagnosable from the log instead of
/// just rendering blank.
pub async fn fetch_records<T: DeserializeOwned>(
    session: Arc<zenoh::Session>,
    key: String,
) -> Option<Vec<T>> {
    let replies = match session
        .get(&key)
        .target(zenoh::query::QueryTarget::All)
        .consolidation(zenoh::query::ConsolidationMode::None)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(key = %key, error = %e, "query get failed");
            return None;
        }
    };
    let mut best: Option<Vec<T>> = None;
    let mut answered = 0usize;
    while let Ok(reply) = replies.recv_async().await {
        let sample = match reply.result() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(key = %key, error = ?e, "query reply was an error");
                continue;
            }
        };
        match zensight_common::decode_with_encoding::<Vec<T>>(
            sample.encoding(),
            &sample.payload().to_bytes(),
        ) {
            Ok(records) => {
                answered += 1;
                if best.as_ref().is_none_or(|b| records.len() > b.len()) {
                    best = Some(records);
                }
            }
            Err(e) => tracing::warn!(
                key = %key, error = %e, bytes = sample.payload().len(),
                "query reply failed to decode"
            ),
        }
    }
    if answered > 1 {
        tracing::warn!(
            key = %key, answered,
            "more than one producer answered a single-origin procedure — \
             duplicate sensor instances share this origin (one host id, two \
             processes); showing the fullest reply"
        );
    }
    if answered == 0 {
        tracing::warn!(key = %key, "query: no sensor replied");
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_name_the_sensor_s_procedures() {
        use NetlinkDetailTopic as T;
        for (topic, procedure) in [
            (T::Sockets, "sockets"),
            (T::Routes, "routes"),
            (T::Neighbors, "neighbors"),
            (T::Addresses, "addresses"),
            (T::Events, "events"),
            (T::RouteChanges, "route_changes"),
            (T::Tc, "tc"),
            (T::Xfrm, "xfrm"),
            (T::Nft, "nft"),
            (T::Retransmits, "retransmits"),
            (T::Connections, "connections"),
        ] {
            assert_eq!(topic.procedure(), procedure);
            assert!(matches!(
                topic.call(),
                Message::Call(r) if r.procedure == procedure && r.params.is_empty()
            ));
        }
        // The endpoint-narrowed sockets key (#309) matches the sensor's
        // SocketSelector `ip=` parameter.
        assert_eq!(
            sockets_match_key("10.0.0.5"),
            "v1/*/@rpc/netlink/sockets?ip=10.0.0.5"
        );
        for sort in [SocketSort::Default, SocketSort::Rtt, SocketSort::Retrans] {
            assert_eq!(SocketSort::from_token(sort.token()), sort);
        }
    }

    /// End-to-end: `fetch_records` against a real in-process Zenoh queryable
    /// replying with the same JSON shape the sensor produces. Proves the actual
    /// get + decode path (the part the Iced simulator can't exercise).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fetch_records_decodes_live_queryable() {
        let key = "v1/*/@rpc/netlink/sockets";
        // Scouting off: with the default config this session joins any real
        // ZenSight mesh on the host, and a live netlink sensor's queryable
        // answers with real sockets instead of the single mock record.
        let mut config = zenoh::Config::default();
        config
            .insert_json5("scouting/multicast/enabled", "false")
            .unwrap();
        config
            .insert_json5("scouting/gossip/enabled", "false")
            .unwrap();
        let session = Arc::new(zenoh::open(config).await.unwrap());

        let records = vec![SocketRecord {
            local: "10.0.0.1:5555".into(),
            remote: "1.1.1.1:443".into(),
            state: "established".into(),
            uid: 1000,
            recv_q: 0,
            send_q: 0,
            rtt_us: 1234,
            retrans: 0,
            inode: 9999,
            congestion: Some("cubic".into()),
            bbr_bw_bps: None,
            cc_min_rtt_us: None,
            snd_cwnd: 10,
            snd_buf: 16384,
            rcv_buf: 32768,
            delivery_rate: 0,
            pacing_rate: 0,
            bytes_retrans: 0,
            bytes_acked: 0,
            bytes_received: 0,
            bytes_sent: 0,
            total_retrans: 0,
            rcv_rtt_us: 0,
            lost: 0,
            reord_seen: 0,
            cookie: 42,
            cgroup_id: Some(7),
            cgroup: Some("system.slice/sshd.service".into()),
            pid: Some(4321),
            process: Some("sshd".into()),
            proc_start_time: Some(987654),
        }];
        let payload = serde_json::to_vec(&records).unwrap();

        // Serve the queryable in the background.
        let qsession = session.clone();
        let qkey = key.to_string();
        let queryable = qsession.declare_queryable(&qkey).await.unwrap();
        tokio::spawn(async move {
            while let Ok(query) = queryable.recv_async().await {
                let _ = query.reply(query.key_expr().clone(), payload.clone()).await;
            }
        });

        // Fetch + decode through the production helper.
        let got: Option<Vec<SocketRecord>> = fetch_records(session.clone(), key.to_string()).await;
        let got = got.expect("decoded socket records");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].local, "10.0.0.1:5555");
        assert_eq!(got[0].rtt_us, 1234);
        // Socket→process attribution fields survive the wire round-trip (#304).
        assert_eq!(got[0].cookie, 42);
        assert_eq!(got[0].pid, Some(4321));
        assert_eq!(got[0].process.as_deref(), Some("sshd"));
        assert_eq!(got[0].proc_start_time, Some(987654));
        assert_eq!(got[0].cgroup.as_deref(), Some("system.slice/sshd.service"));

        session.close().await.unwrap();
    }

    /// Regression: an origin-scoped procedure answered by TWO producers — a
    /// stray second sensor instance minting the same host origin, or two hosts
    /// cloned from one machine-id. First-reply-wins made every detail panel
    /// flap (rows on one fetch, the idle twin's empty ring on the next); the
    /// fetch must be deterministic and keep the fullest reply.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fetch_records_survives_a_duplicate_origin_instance() {
        let key = "v1/h-decafbad0001/@rpc/netlink/sockets";
        let mut config = zenoh::Config::default();
        config
            .insert_json5("scouting/multicast/enabled", "false")
            .unwrap();
        config
            .insert_json5("scouting/gossip/enabled", "false")
            .unwrap();
        let session = Arc::new(zenoh::open(config).await.unwrap());

        // The live instance: one socket. The twin: an empty ring.
        let full =
            serde_json::to_vec(&vec![sock("10.0.0.1:22", "1.1.1.1:443", "listen", 1, 0)]).unwrap();
        let empty = serde_json::to_vec(&Vec::<SocketRecord>::new()).unwrap();
        for payload in [full, empty] {
            let q = session.declare_queryable(key).await.unwrap();
            tokio::spawn(async move {
                while let Ok(query) = q.recv_async().await {
                    let _ = query.reply(query.key_expr().clone(), payload.clone()).await;
                }
            });
        }

        // Every fetch sees the same thing — no flapping between the two.
        for _ in 0..10 {
            let got: Option<Vec<SocketRecord>> =
                fetch_records(session.clone(), key.to_string()).await;
            assert_eq!(
                got.as_ref().map(|v| v.len()),
                Some(1),
                "the fullest reply wins on every fetch, not whichever answered first"
            );
        }

        session.close().await.unwrap();
    }

    /// A socket record with the given endpoints/state/rtt/retrans; other fields
    /// defaulted — enough to exercise the explorer's filter/sort (#112).
    fn sock(local: &str, remote: &str, state: &str, rtt_us: u32, retrans: u32) -> SocketRecord {
        SocketRecord {
            local: local.into(),
            remote: remote.into(),
            state: state.into(),
            uid: 0,
            recv_q: 0,
            send_q: 0,
            rtt_us,
            retrans,
            inode: 0,
            congestion: None,
            bbr_bw_bps: None,
            cc_min_rtt_us: None,
            snd_cwnd: 0,
            snd_buf: 0,
            rcv_buf: 0,
            delivery_rate: 0,
            pacing_rate: 0,
            bytes_retrans: 0,
            bytes_acked: 0,
            bytes_received: 0,
            bytes_sent: 0,
            total_retrans: 0,
            rcv_rtt_us: 0,
            lost: 0,
            reord_seen: 0,
            cookie: 0,
            cgroup_id: None,
            cgroup: None,
            pid: None,
            process: None,
            proc_start_time: None,
        }
    }

    /// #112: the state filter is case-insensitive and the port filter matches
    /// either endpoint's port; an empty/`None` filter passes everything through.
    #[test]
    fn socket_filter_by_state_and_port() {
        let socks = vec![
            sock("10.0.0.1:5555", "1.1.1.1:443", "established", 100, 0),
            sock("10.0.0.1:22", "2.2.2.2:51000", "listen", 50, 0),
            sock("10.0.0.1:8080", "3.3.3.3:443", "time_wait", 70, 0),
        ];

        // No filters → all rows, original order.
        let all = filter_sort_sockets(&socks, None, "", SocketSort::Default);
        assert_eq!(all.len(), 3);

        // State filter (case-insensitive).
        let est = filter_sort_sockets(&socks, Some("ESTABLISHED"), "", SocketSort::Default);
        assert_eq!(est.len(), 1);
        assert_eq!(est[0].local, "10.0.0.1:5555");

        // Port filter matches remote :443 on two rows (not the IP octets).
        let p443 = filter_sort_sockets(&socks, None, "443", SocketSort::Default);
        assert_eq!(p443.len(), 2);

        // Port filter on a local port.
        let p22 = filter_sort_sockets(&socks, None, "22", SocketSort::Default);
        assert_eq!(p22.len(), 1);
        assert_eq!(p22[0].state, "listen");
    }

    /// #112: sorting surfaces the worst flows first (highest RTT / retrans),
    /// leaving filtering composable with sort.
    #[test]
    fn socket_sort_worst_first() {
        let socks = vec![
            sock("a:1", "b:2", "established", 100, 3),
            sock("c:1", "d:2", "established", 900, 0),
            sock("e:1", "f:2", "established", 50, 9),
        ];

        let by_rtt = filter_sort_sockets(&socks, None, "", SocketSort::Rtt);
        assert_eq!(by_rtt[0].rtt_us, 900);
        assert_eq!(by_rtt[2].rtt_us, 50);

        let by_retx = filter_sort_sockets(&socks, None, "", SocketSort::Retrans);
        assert_eq!(by_retx[0].retrans, 9);
        assert_eq!(by_retx[2].retrans, 0);
    }
}
