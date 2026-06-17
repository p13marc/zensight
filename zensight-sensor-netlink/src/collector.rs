//! Netlink polling collector: reads kernel state via nlink and publishes
//! telemetry.

use std::sync::Arc;
use std::time::Duration;

use nlink::netlink::{
    Connection, Route, SockDiag, messages::NeighborMessage, messages::RouteMessage,
    neigh::State as NeighborState,
};
use nlink::sockdiag::{SocketFilter, SocketInfo, SocketState, TcpState};
use zensight_common::Format;
use zensight_sensor_core::{AdvancedPublisherConfig, AdvancedPublisherRegistry};

use crate::config::NetlinkConfig;
use crate::map::{self, IfaceSample, NeighborSummary, RouteSummary, SocketCounts};

const AF_INET: u8 = 2;
const AF_INET6: u8 = 10;

/// Polls netlink on an interval and publishes interface + socket telemetry.
pub struct Collector {
    host: String,
    config: NetlinkConfig,
    /// Cached advanced publishers so late-joining consumers get the current value
    /// of every metric on connect (via their AdvancedSubscriber `history()`),
    /// instead of waiting for the next poll.
    registry: AdvancedPublisherRegistry,
}

impl Collector {
    pub fn new(
        host: String,
        config: NetlinkConfig,
        session: Arc<zenoh::Session>,
        format: Format,
    ) -> Self {
        let registry = AdvancedPublisherRegistry::new(
            session,
            config.key_prefix.clone(),
            format,
            AdvancedPublisherConfig::default(),
        );
        Self {
            host,
            config,
            registry,
        }
    }

    pub async fn run(self) {
        let route = match Connection::<Route>::new() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "failed to open netlink route connection");
                return;
            }
        };
        let sockdiag = Connection::<SockDiag>::new().ok();
        if self.config.collect.sockets && sockdiag.is_none() {
            tracing::warn!("sockdiag unavailable; socket telemetry disabled");
        }

        let mut tick = tokio::time::interval(Duration::from_secs(self.config.poll_interval_secs));
        loop {
            tick.tick().await;
            if self.config.collect.interfaces {
                self.poll_interfaces(&route).await;
            }
            if self.config.collect.sockets
                && let Some(sd) = &sockdiag
            {
                self.poll_sockets(sd).await;
            }
            if self.config.collect.neighbors {
                self.poll_neighbors(&route).await;
            }
            if self.config.collect.routes {
                self.poll_routes(&route).await;
            }
        }
    }

    async fn poll_routes(&self, conn: &Connection<Route>) {
        let routes = match conn.get_routes().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "get_routes failed");
                return;
            }
        };
        let summary = aggregate_routes(&routes);
        for point in map::route_points(&self.host, &summary) {
            self.publish(&point).await;
        }
    }

    async fn poll_neighbors(&self, conn: &Connection<Route>) {
        let neighbors = match conn.get_neighbors().await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(error = %e, "get_neighbors failed");
                return;
            }
        };
        let summary = aggregate_neighbors(&neighbors);
        for point in map::neighbor_points(&self.host, &summary) {
            self.publish(&point).await;
        }
    }

    async fn poll_interfaces(&self, conn: &Connection<Route>) {
        let links = match conn.get_links().await {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, "get_links failed");
                return;
            }
        };
        for link in links {
            let name = link.name_or("?").to_string();
            if name == "?" || !self.config.interfaces.should_include(&name) {
                continue;
            }
            let stats = link.stats();
            let sample = IfaceSample {
                name: name.clone(),
                ifindex: link.ifindex(),
                up: link.is_up(),
                carrier: link.carrier(),
                mtu: link.mtu(),
                mac: link.mac_address(),
                oper_state: link.operstate().map(|o| format!("{o:?}").to_lowercase()),
                rx_bytes: stats.map(|s| s.rx_bytes).unwrap_or(0),
                tx_bytes: stats.map(|s| s.tx_bytes).unwrap_or(0),
                rx_packets: stats.map(|s| s.rx_packets).unwrap_or(0),
                tx_packets: stats.map(|s| s.tx_packets).unwrap_or(0),
                rx_errors: stats.map(|s| s.rx_errors).unwrap_or(0),
                tx_errors: stats.map(|s| s.tx_errors).unwrap_or(0),
                rx_dropped: stats.map(|s| s.rx_dropped).unwrap_or(0),
                tx_dropped: stats.map(|s| s.tx_dropped).unwrap_or(0),
                multicast: stats.map(|s| s.multicast).unwrap_or(0),
                collisions: stats.map(|s| s.collisions).unwrap_or(0),
            };
            for point in map::iface_points(&self.host, &sample) {
                self.publish(&point).await;
            }
        }
    }

    async fn poll_sockets(&self, conn: &Connection<SockDiag>) {
        let filter = SocketFilter::tcp().all_states().with_tcp_info().build();
        let socks = match conn.query(&filter).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "sockdiag query failed");
                return;
            }
        };
        let counts = aggregate_sockets(&socks);
        for point in map::socket_points(&self.host, &counts) {
            self.publish(&point).await;
        }
    }

    async fn publish(&self, point: &zensight_common::TelemetryPoint) {
        // Key = <prefix>/<source>/<metric>, published via a cached AdvancedPublisher.
        let suffix = format!("{}/{}", point.source, point.metric);
        if let Err(e) = self.registry.publish(&suffix, point).await {
            tracing::warn!(error = %e, "publish failed");
        }
    }
}

/// Aggregate a set of sockdiag results into [`SocketCounts`]. Pure; unit-tested.
pub fn aggregate_sockets(socks: &[SocketInfo]) -> SocketCounts {
    let mut c = SocketCounts::default();
    for s in socks {
        let SocketInfo::Inet(inet) = s else { continue };
        match inet.state {
            SocketState::Tcp(TcpState::Established) | SocketState::Established => {
                c.established += 1
            }
            SocketState::Tcp(TcpState::Listen) | SocketState::Listen => c.listen += 1,
            SocketState::Tcp(TcpState::TimeWait) => c.time_wait += 1,
            SocketState::Tcp(TcpState::SynSent) => c.syn_sent += 1,
            SocketState::Tcp(TcpState::CloseWait) => c.close_wait += 1,
            _ => {}
        }
        if let Some(ti) = &inet.tcp_info {
            c.retransmits_total += ti.retrans as u64;
            c.max_rtt_us = c.max_rtt_us.max(ti.rtt as u64);
        }
    }
    c
}

/// Aggregate route messages into a [`RouteSummary`].
pub fn aggregate_routes(routes: &[RouteMessage]) -> RouteSummary {
    let mut r = RouteSummary::default();
    for rt in routes {
        r.total += 1;
        let v6 = rt.family() == AF_INET6;
        if rt.family() == AF_INET {
            r.ipv4_count += 1;
        } else if v6 {
            r.ipv6_count += 1;
        }
        if rt.is_default() {
            if v6 {
                r.default_v6_present = true;
            } else {
                r.default_v4_present = true;
                if r.default_v4_gw.is_none() {
                    r.default_v4_gw = rt.gateway().map(|g| g.to_string());
                }
            }
        }
    }
    r
}

/// Aggregate neighbor messages into a [`NeighborSummary`] (counts by state).
pub fn aggregate_neighbors(neighbors: &[NeighborMessage]) -> NeighborSummary {
    let mut n = NeighborSummary::default();
    for nb in neighbors {
        n.total += 1;
        match nb.state() {
            NeighborState::Reachable => n.reachable += 1,
            NeighborState::Stale => n.stale += 1,
            NeighborState::Failed => n.failed += 1,
            NeighborState::Incomplete => n.incomplete += 1,
            NeighborState::Permanent => n.permanent += 1,
            _ => n.other += 1,
        }
    }
    n
}
