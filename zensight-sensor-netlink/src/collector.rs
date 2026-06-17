//! Netlink polling collector: reads kernel state via nlink and publishes
//! telemetry.

use std::sync::Arc;
use std::time::Duration;

use nlink::netlink::diagnostics::{Bottleneck, DiagnosticReport, Diagnostics, Severity};
use nlink::netlink::{
    Connection, Route, SockDiag, messages::NeighborMessage, messages::RouteMessage,
    neigh::State as NeighborState,
};
use nlink::sockdiag::{SocketFilter, SocketInfo, SocketState, TcpState};
use zensight_common::Format;
use zensight_sensor_core::{AdvancedPublisherConfig, AdvancedPublisherRegistry};

use crate::config::NetlinkConfig;
use crate::map::{
    self, DiagnosticsSummary, IfaceSample, NeighborSummary, RouteSummary, SocketCounts,
};

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

        // The diagnostics scanner takes ownership of its own Route connection.
        let diagnostics = if self.config.collect.diagnostics {
            match Connection::<Route>::new() {
                Ok(c) => Some(Diagnostics::new(c)),
                Err(e) => {
                    tracing::warn!(error = %e, "diagnostics connection failed; disabled");
                    None
                }
            }
        } else {
            None
        };

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
            if let Some(diag) = &diagnostics {
                self.poll_diagnostics(diag).await;
            }
        }
    }

    async fn poll_diagnostics(&self, diag: &Diagnostics) {
        let report = match diag.scan().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "diagnostics scan failed");
                return;
            }
        };
        let bottleneck = diag.find_bottleneck().await.ok().flatten();
        let summary = aggregate_diagnostics(&report, bottleneck.as_ref());
        for point in map::diagnostics_points(&self.host, &summary) {
            self.publish(&point).await;
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
    let mut rtts: Vec<u64> = Vec::new();
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
            if ti.rtt > 0 {
                rtts.push(ti.rtt as u64);
            }
        }
    }
    c.rtt_p50_us = percentile(&mut rtts, 50);
    c.rtt_p95_us = percentile(&mut rtts, 95);
    c
}

/// Nearest-rank percentile of a sample set (sorts in place). 0 if empty.
fn percentile(values: &mut [u64], p: u8) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let rank = ((p as usize * values.len()).div_ceil(100)).max(1);
    values[rank - 1]
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

/// Aggregate a diagnostics report + worst bottleneck into a [`DiagnosticsSummary`].
/// `Bottleneck::score()` is nlink's documented 0..=1 severity formula.
pub fn aggregate_diagnostics(
    report: &DiagnosticReport,
    bottleneck: Option<&Bottleneck>,
) -> DiagnosticsSummary {
    let mut d = DiagnosticsSummary::default();
    for issue in &report.issues {
        match issue.severity {
            Severity::Info => d.issues_info += 1,
            Severity::Warning => d.issues_warning += 1,
            Severity::Error => d.issues_error += 1,
            Severity::Critical => d.issues_critical += 1,
            // `Severity` is #[non_exhaustive]; ignore unknown future variants.
            _ => {}
        }
    }
    if let Some(b) = bottleneck {
        d.bottleneck_score = b.score();
        d.bottleneck_location = Some(b.location.clone());
        d.bottleneck_type = Some(b.bottleneck_type.to_string());
        d.bottleneck_recommendation = Some(b.recommendation.clone());
        d.bottleneck_drop_rate = b.drop_rate;
    }
    d
}

#[cfg(test)]
mod tests {
    use super::percentile;

    #[test]
    fn percentile_nearest_rank() {
        assert_eq!(percentile(&mut [], 50), 0);
        assert_eq!(percentile(&mut [10], 50), 10);
        // p50 of 1..=10 (nearest-rank) = 5th value = 5; p95 = 10th = 10.
        let mut v: Vec<u64> = (1..=10).collect();
        assert_eq!(percentile(&mut v, 50), 5);
        assert_eq!(percentile(&mut v, 95), 10);
        assert_eq!(percentile(&mut v, 100), 10);
    }
}
