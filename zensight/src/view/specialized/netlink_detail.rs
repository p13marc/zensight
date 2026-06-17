//! On-demand netlink detail client: fetches the full route/neighbor/socket
//! tables from the sensor's `@/query/*` channels (principle P2 — nothing is
//! streamed; the GUI pulls detail only when a user drills in).
//!
//! The fetch+decode core ([`fetch_records`]) is independent of Iced so it can be
//! integration-tested against a real in-process Zenoh queryable.

use std::sync::Arc;

use serde::de::DeserializeOwned;
use zensight_common::{NeighborRecord, RouteRecord, SocketRecord};

/// Which detail table to fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetlinkDetailTopic {
    Sockets,
    Routes,
    Neighbors,
}

impl NetlinkDetailTopic {
    /// The queryable key for this topic (matches the sensor's `query.rs`).
    pub fn key(&self) -> String {
        let topic = match self {
            NetlinkDetailTopic::Sockets => "sockets",
            NetlinkDetailTopic::Routes => "routes",
            NetlinkDetailTopic::Neighbors => "neighbors",
        };
        format!("zensight/netlink/@/query/{topic}")
    }

    pub fn label(&self) -> &'static str {
        match self {
            NetlinkDetailTopic::Sockets => "Sockets",
            NetlinkDetailTopic::Routes => "Routes",
            NetlinkDetailTopic::Neighbors => "Neighbors",
        }
    }
}

/// A decoded detail table.
#[derive(Debug, Clone)]
pub enum NetlinkDetailData {
    Sockets(Vec<SocketRecord>),
    Routes(Vec<RouteRecord>),
    Neighbors(Vec<NeighborRecord>),
}

/// Fetched detail tables for the selected host (each populated on demand).
#[derive(Debug, Clone, Default)]
pub struct NetlinkDetailState {
    pub sockets: Option<Vec<SocketRecord>>,
    pub routes: Option<Vec<RouteRecord>>,
    pub neighbors: Option<Vec<NeighborRecord>>,
}

impl NetlinkDetailState {
    /// Store a freshly-fetched table.
    pub fn apply(&mut self, data: NetlinkDetailData) {
        match data {
            NetlinkDetailData::Sockets(v) => self.sockets = Some(v),
            NetlinkDetailData::Routes(v) => self.routes = Some(v),
            NetlinkDetailData::Neighbors(v) => self.neighbors = Some(v),
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
    }

    #[test]
    fn apply_stores_each_topic() {
        let mut s = NetlinkDetailState::default();
        s.apply(NetlinkDetailData::Routes(vec![RouteRecord {
            family: 4,
            dst: "default".into(),
            gateway: Some("10.0.0.1".into()),
            oif: Some(2),
            priority: Some(100),
            protocol: "dhcp".into(),
            scope: "universe".into(),
            table: 254,
        }]));
        assert_eq!(s.routes.as_ref().unwrap().len(), 1);
        assert!(s.sockets.is_none());
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
        }];
        let payload = serde_json::to_vec(&records).unwrap();

        // Serve the queryable in the background.
        let qsession = session.clone();
        let qkey = key.to_string();
        let queryable = qsession.declare_queryable(&qkey).await.unwrap();
        tokio::spawn(async move {
            while let Ok(query) = queryable.recv_async().await {
                let _ = query
                    .reply(query.key_expr().clone(), payload.clone())
                    .await;
            }
        });

        // Fetch + decode through the production helper.
        let got: Option<Vec<SocketRecord>> =
            fetch_records(session.clone(), key.to_string()).await;
        let got = got.expect("decoded socket records");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].local, "10.0.0.1:5555");
        assert_eq!(got[0].rtt_us, 1234);

        session.close().await.unwrap();
    }
}
