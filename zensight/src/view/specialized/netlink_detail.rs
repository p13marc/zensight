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
}

/// Which detail table to fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetlinkDetailTopic {
    Sockets,
    Routes,
    Neighbors,
    Addresses,
    Events,
    Tc,
    Xfrm,
    Nft,
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
            NetlinkDetailTopic::Tc => "tc",
            NetlinkDetailTopic::Xfrm => "xfrm",
            NetlinkDetailTopic::Nft => "nft",
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
            NetlinkDetailTopic::Tc => "TC",
            NetlinkDetailTopic::Xfrm => "XFRM",
            NetlinkDetailTopic::Nft => "NFT",
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
    Tc(Vec<TcRecord>),
    Xfrm(Vec<XfrmSaRecord>),
    Nft(Vec<NftRuleRecord>),
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
    pub tc: Fetch<Vec<TcRecord>>,
    pub xfrm: Fetch<Vec<XfrmSaRecord>>,
    pub nft: Fetch<Vec<NftRuleRecord>>,
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
            NetlinkDetailTopic::Tc => self.tc = Fetch::Loading,
            NetlinkDetailTopic::Xfrm => self.xfrm = Fetch::Loading,
            NetlinkDetailTopic::Nft => self.nft = Fetch::Loading,
        }
    }

    /// Store a topic's fetch outcome (success data or an error message).
    pub fn apply(&mut self, topic: NetlinkDetailTopic, result: Result<NetlinkDetailData, String>) {
        match result {
            Ok(NetlinkDetailData::Sockets(v)) => self.sockets = Fetch::Ready(v),
            Ok(NetlinkDetailData::Routes(v)) => self.routes = Fetch::Ready(v),
            Ok(NetlinkDetailData::Neighbors(v)) => self.neighbors = Fetch::Ready(v),
            Ok(NetlinkDetailData::Addresses(v)) => self.addresses = Fetch::Ready(v),
            Ok(NetlinkDetailData::Events(v)) => self.events = Fetch::Ready(v),
            Ok(NetlinkDetailData::Tc(v)) => self.tc = Fetch::Ready(v),
            Ok(NetlinkDetailData::Xfrm(v)) => self.xfrm = Fetch::Ready(v),
            Ok(NetlinkDetailData::Nft(v)) => self.nft = Fetch::Ready(v),
            Err(e) => match topic {
                NetlinkDetailTopic::Sockets => self.sockets = Fetch::Error(e),
                NetlinkDetailTopic::Routes => self.routes = Fetch::Error(e),
                NetlinkDetailTopic::Neighbors => self.neighbors = Fetch::Error(e),
                NetlinkDetailTopic::Addresses => self.addresses = Fetch::Error(e),
                NetlinkDetailTopic::Events => self.events = Fetch::Error(e),
                NetlinkDetailTopic::Tc => self.tc = Fetch::Error(e),
                NetlinkDetailTopic::Xfrm => self.xfrm = Fetch::Error(e),
                NetlinkDetailTopic::Nft => self.nft = Fetch::Error(e),
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
        assert_eq!(NetlinkDetailTopic::Tc.key(), "zensight/netlink/@/query/tc");
        assert_eq!(
            NetlinkDetailTopic::Xfrm.key(),
            "zensight/netlink/@/query/xfrm"
        );
        assert_eq!(NetlinkDetailTopic::Nft.key(), "zensight/netlink/@/query/nft");
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
}
