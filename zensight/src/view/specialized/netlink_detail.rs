//! On-demand netlink detail client: fetches the full route/neighbor/socket
//! tables from the sensor's `@/query/*` channels (principle P2 — nothing is
//! streamed; the GUI pulls detail only when a user drills in).
//!
//! The fetch+decode core ([`fetch_records`]) is independent of Iced so it can be
//! integration-tested against a real in-process Zenoh queryable.

use std::sync::Arc;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use zensight_common::{NeighborRecord, RouteRecord, SocketRecord};

use std::collections::HashMap;

use crate::view::components::TableState;
use crate::view::specialized::fetch::Fetch;

// The sensor defines these record types locally (it owns only its own crate); we
// mirror their JSON shape here so the GUI can decode the addresses/events/tc/
// xfrm/nft query channels (#109). Field names/types must match
// `zensight-sensor-netlink/src/{map,events}.rs` exactly.

/// One configured IP address (`@/query/addresses`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AddressRecord {
    pub family: u8,
    pub ip: Option<String>,
    pub prefix_len: u8,
    pub scope: String,
    pub label: Option<String>,
    pub ifindex: u32,
}

/// One row of the recent control-plane events ring (`@/query/events`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct EventRecord {
    pub ts_unix: u64,
    pub family: String,
    pub action: String,
    pub ifindex: Option<u32>,
    pub detail: String,
}

/// One default-route transition (`@/query/route_changes`, #111). Mirrors the
/// sensor's `RouteChangeRecord` JSON shape.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RouteChangeRecord {
    pub ts_unix: u64,
    pub family: String,
    /// `"added"` / `"changed"` / `"withdrawn"`.
    pub action: String,
    pub gateway: Option<String>,
    pub prev_gateway: Option<String>,
}

/// One TC qdisc/class entry (`@/query/tc`). `node` is `qdisc` or `class`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
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

/// One IPsec Security Association (`@/query/xfrm`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
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

/// One nftables rule (`@/query/nft`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
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

/// One top-retransmit peer from the eBPF module (`@/query/retransmits`, #269).
/// Mirrors the sensor's `RetransRecord`; only served on eBPF-enabled hosts.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RetransRecord {
    pub peer: String,
    pub family: u8,
    pub count: u64,
}

/// One tcplife connection-lifecycle record from the eBPF module
/// (`@/query/connections`, #269). Mirrors the sensor's `ConnView`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ConnRecord {
    pub pid: u32,
    pub comm: String,
    pub family: u8,
    pub local: String,
    pub lport: u16,
    pub remote: String,
    pub rport: u16,
    pub duration_ms: u64,
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    pub segs_out: u32,
    pub segs_in: u32,
    pub retrans: u32,
}

/// Which detail table to fetch.
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
    /// The queryable key for this topic (matches the sensor's `query.rs`).
    pub fn key(&self) -> String {
        let topic = match self {
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
        };
        format!("zensight/netlink/@/query/{topic}")
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

/// A decoded detail table.
#[derive(Debug, Clone)]
pub enum NetlinkDetailData {
    Sockets(Vec<SocketRecord>),
    Routes(Vec<RouteRecord>),
    Neighbors(Vec<NeighborRecord>),
    Addresses(Vec<AddressRecord>),
    Events(Vec<EventRecord>),
    RouteChanges(Vec<RouteChangeRecord>),
    Tc(Vec<TcRecord>),
    Xfrm(Vec<XfrmSaRecord>),
    Nft(Vec<NftRuleRecord>),
    Retransmits(Vec<RetransRecord>),
    Connections(Vec<ConnRecord>),
}

/// Identifies a sortable/filterable netlink detail table so the shared sort/
/// filter/load-more messages can address one table without a message per table
/// (#244, reused across the Routing/QoS/Firewall tabs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NetlinkTable {
    Routes,
    Neighbors,
    Addresses,
    RouteChanges,
    Tc,
    Xfrm,
    Nft,
    Events,
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

/// Fetched detail tables for the selected host (each fetched on demand, each with
/// its own loading/error state).
#[derive(Debug, Clone, Default)]
pub struct NetlinkDetailState {
    pub sockets: Fetch<Vec<SocketRecord>>,
    pub routes: Fetch<Vec<RouteRecord>>,
    pub neighbors: Fetch<Vec<NeighborRecord>>,
    pub addresses: Fetch<Vec<AddressRecord>>,
    pub events: Fetch<Vec<EventRecord>>,
    pub route_changes: Fetch<Vec<RouteChangeRecord>>,
    pub tc: Fetch<Vec<TcRecord>>,
    pub xfrm: Fetch<Vec<XfrmSaRecord>>,
    pub nft: Fetch<Vec<NftRuleRecord>>,
    /// eBPF top-retransmit peers (#269); populated only on eBPF-enabled hosts.
    pub retransmits: Fetch<Vec<RetransRecord>>,
    /// eBPF tcplife connection records (#269).
    pub connections: Fetch<Vec<ConnRecord>>,
    /// Socket explorer (#112): active TCP-state filter (`None` = all states).
    pub socket_state_filter: Option<String>,
    /// Socket explorer: port substring filter (matches local or remote port).
    pub socket_port_filter: String,
    /// Socket explorer: active sort order.
    pub socket_sort: SocketSort,
    /// Socket explorer pagination (#261): only `limit` used here (row cap + load
    /// more), replacing the old silent `.take(200)` cutoff. Default = 200 rows.
    pub sockets_table: TableState,
    /// Per-detail-table sort/filter/pagination state, addressed by
    /// [`NetlinkTable`] (#244).
    pub tables: HashMap<NetlinkTable, TableState>,
}

impl NetlinkDetailState {
    /// Read a detail table's interaction state (a shared default when untouched).
    pub fn table(&self, which: NetlinkTable) -> &TableState {
        use std::sync::OnceLock;
        static DEFAULT: OnceLock<TableState> = OnceLock::new();
        self.tables
            .get(&which)
            .unwrap_or_else(|| DEFAULT.get_or_init(TableState::default))
    }

    /// Mutable table state, created lazily on first interaction.
    pub fn table_mut(&mut self, which: NetlinkTable) -> &mut TableState {
        self.tables.entry(which).or_default()
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

impl NetlinkDetailState {
    /// Mark a topic's fetch as in flight.
    pub fn loading(&mut self, topic: NetlinkDetailTopic) {
        match topic {
            NetlinkDetailTopic::Sockets => self.sockets = Fetch::Loading,
            NetlinkDetailTopic::Routes => self.routes = Fetch::Loading,
            NetlinkDetailTopic::Neighbors => self.neighbors = Fetch::Loading,
            NetlinkDetailTopic::Addresses => self.addresses = Fetch::Loading,
            NetlinkDetailTopic::Events => self.events = Fetch::Loading,
            NetlinkDetailTopic::RouteChanges => self.route_changes = Fetch::Loading,
            NetlinkDetailTopic::Tc => self.tc = Fetch::Loading,
            NetlinkDetailTopic::Xfrm => self.xfrm = Fetch::Loading,
            NetlinkDetailTopic::Nft => self.nft = Fetch::Loading,
            NetlinkDetailTopic::Retransmits => self.retransmits = Fetch::Loading,
            NetlinkDetailTopic::Connections => self.connections = Fetch::Loading,
        }
    }

    /// Store a topic's fetch outcome (success data or an error message).
    pub fn apply(&mut self, topic: NetlinkDetailTopic, result: Result<NetlinkDetailData, String>) {
        match result {
            Ok(NetlinkDetailData::Sockets(v)) => self.sockets = Fetch::Ready(v),
            Ok(NetlinkDetailData::Routes(v)) => self.routes = Fetch::Ready(v),
            Ok(NetlinkDetailData::Neighbors(v)) => self.neighbors = Fetch::Ready(v),
            Ok(NetlinkDetailData::Addresses(v)) => self.addresses = Fetch::Ready(v),
            Ok(NetlinkDetailData::Events(mut v)) => {
                // Timelines render newest-first (#265).
                v.sort_by_key(|r| std::cmp::Reverse(r.ts_unix));
                self.events = Fetch::Ready(v);
            }
            Ok(NetlinkDetailData::RouteChanges(mut v)) => {
                v.sort_by_key(|r| std::cmp::Reverse(r.ts_unix));
                self.route_changes = Fetch::Ready(v);
            }
            Ok(NetlinkDetailData::Tc(v)) => self.tc = Fetch::Ready(v),
            Ok(NetlinkDetailData::Xfrm(v)) => self.xfrm = Fetch::Ready(v),
            Ok(NetlinkDetailData::Nft(v)) => self.nft = Fetch::Ready(v),
            Ok(NetlinkDetailData::Retransmits(mut v)) => {
                // Worst peers first.
                v.sort_by_key(|r| std::cmp::Reverse(r.count));
                self.retransmits = Fetch::Ready(v);
            }
            Ok(NetlinkDetailData::Connections(mut v)) => {
                // Longest-lived first.
                v.sort_by_key(|r| std::cmp::Reverse(r.duration_ms));
                self.connections = Fetch::Ready(v);
            }
            Err(e) => match topic {
                NetlinkDetailTopic::Sockets => self.sockets = Fetch::Error(e),
                NetlinkDetailTopic::Routes => self.routes = Fetch::Error(e),
                NetlinkDetailTopic::Neighbors => self.neighbors = Fetch::Error(e),
                NetlinkDetailTopic::Addresses => self.addresses = Fetch::Error(e),
                NetlinkDetailTopic::Events => self.events = Fetch::Error(e),
                NetlinkDetailTopic::RouteChanges => self.route_changes = Fetch::Error(e),
                NetlinkDetailTopic::Tc => self.tc = Fetch::Error(e),
                NetlinkDetailTopic::Xfrm => self.xfrm = Fetch::Error(e),
                NetlinkDetailTopic::Nft => self.nft = Fetch::Error(e),
                NetlinkDetailTopic::Retransmits => self.retransmits = Fetch::Error(e),
                NetlinkDetailTopic::Connections => self.connections = Fetch::Error(e),
            },
        }
    }
}

/// Fetch + decode the first reply on `key` into `Vec<T>`. Returns `None` if no
/// sensor replied or the payload didn't decode. Iced-independent (testable).
pub async fn fetch_records<T: DeserializeOwned>(
    session: Arc<zenoh::Session>,
    key: String,
) -> Option<Vec<T>> {
    let replies = session.get(&key).await.ok()?;
    let reply = replies.recv_async().await.ok()?;
    let sample = reply.result().ok()?;
    serde_json::from_slice(&sample.payload().to_bytes()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_keys_match_sensor() {
        assert_eq!(
            NetlinkDetailTopic::Sockets.key(),
            "zensight/netlink/@/query/sockets"
        );
        assert_eq!(
            NetlinkDetailTopic::Routes.key(),
            "zensight/netlink/@/query/routes"
        );
        assert_eq!(
            NetlinkDetailTopic::Neighbors.key(),
            "zensight/netlink/@/query/neighbors"
        );
        // The 5 previously-dead channels now reachable (#109).
        assert_eq!(
            NetlinkDetailTopic::Addresses.key(),
            "zensight/netlink/@/query/addresses"
        );
        assert_eq!(
            NetlinkDetailTopic::Events.key(),
            "zensight/netlink/@/query/events"
        );
        // Default-route flap history (#111).
        assert_eq!(
            NetlinkDetailTopic::RouteChanges.key(),
            "zensight/netlink/@/query/route_changes"
        );
        assert_eq!(NetlinkDetailTopic::Tc.key(), "zensight/netlink/@/query/tc");
        assert_eq!(
            NetlinkDetailTopic::Xfrm.key(),
            "zensight/netlink/@/query/xfrm"
        );
        assert_eq!(
            NetlinkDetailTopic::Nft.key(),
            "zensight/netlink/@/query/nft"
        );
    }

    #[test]
    fn apply_stores_new_topics() {
        let mut s = NetlinkDetailState::default();
        s.loading(NetlinkDetailTopic::Tc);
        assert!(s.tc.is_loading());
        s.apply(
            NetlinkDetailTopic::Tc,
            Ok(NetlinkDetailData::Tc(vec![TcRecord {
                iface: "eth0".into(),
                node: "qdisc".into(),
                kind: Some("fq_codel".into()),
                handle: "0:".into(),
                parent: "root".into(),
                bytes: 1,
                packets: 1,
                drops: 0,
                overlimits: 0,
                requeues: 0,
                backlog_bytes: 0,
                backlog_pkts: 0,
            }])),
        );
        assert_eq!(s.tc.ready().map(|v| v.len()), Some(1));
        s.apply(NetlinkDetailTopic::Nft, Err("no sensor".into()));
        assert_eq!(s.nft.error(), Some("no sensor"));
    }

    #[test]
    fn apply_stores_each_topic() {
        let mut s = NetlinkDetailState::default();
        s.loading(NetlinkDetailTopic::Routes);
        assert!(s.routes.is_loading());
        s.apply(
            NetlinkDetailTopic::Routes,
            Ok(NetlinkDetailData::Routes(vec![RouteRecord {
                family: 4,
                dst: "default".into(),
                gateway: Some("10.0.0.1".into()),
                oif: Some(2),
                priority: Some(100),
                protocol: "dhcp".into(),
                scope: "universe".into(),
                table: 254,
            }])),
        );
        assert_eq!(s.routes.ready().map(|v| v.len()), Some(1));
        assert!(matches!(s.sockets, Fetch::Idle));
        // An error on a topic is recorded as such.
        s.apply(NetlinkDetailTopic::Sockets, Err("no sensor".into()));
        assert_eq!(s.sockets.error(), Some("no sensor"));
    }

    /// End-to-end: `fetch_records` against a real in-process Zenoh queryable
    /// replying with the same JSON shape the sensor produces. Proves the actual
    /// get + decode path (the part the Iced simulator can't exercise).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fetch_records_decodes_live_queryable() {
        let key = "zensight/netlink/@/query/sockets";
        let session = Arc::new(zenoh::open(zenoh::Config::default()).await.unwrap());

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
            snd_cwnd: 10,
            snd_buf: 16384,
            rcv_buf: 32768,
            delivery_rate: 0,
            pacing_rate: 0,
            bytes_retrans: 0,
            total_retrans: 0,
            rcv_rtt_us: 0,
            lost: 0,
            reord_seen: 0,
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
            snd_cwnd: 0,
            snd_buf: 0,
            rcv_buf: 0,
            delivery_rate: 0,
            pacing_rate: 0,
            bytes_retrans: 0,
            total_retrans: 0,
            rcv_rtt_us: 0,
            lost: 0,
            reord_seen: 0,
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
