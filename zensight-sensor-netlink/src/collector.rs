//! Netlink polling collector: reads kernel state via nlink and publishes
//! telemetry.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;

use nlink::netlink::diagnostics::{Bottleneck, DiagnosticReport, Diagnostics, Severity};
use nlink::netlink::{
    Connection, Netfilter, Route, SockDiag, messages::NeighborMessage, messages::RouteMessage,
    neigh::State as NeighborState,
    netfilter::{ConntrackEntry, IpProtocol},
};
use nlink::sockdiag::{SocketFilter, SocketInfo, SocketState, TcpState};
use zensight_common::Format;
use zensight_sensor_core::{AdvancedPublisherConfig, AdvancedPublisherRegistry};

use crate::config::{CollectConfig, NetlinkConfig};
use crate::map::{
    self, ConntrackSummary, DiagnosticsSummary, IfaceSample, NeighborSummary, RouteSummary,
    SocketCounts,
};

const AF_INET: u8 = 2;
const AF_INET6: u8 = 10;

/// A shared cache of the latest numeric value of every published metric, keyed
/// by metric path (e.g. `sockets/tcp/established`). The collector writes it as it
/// publishes; the sentinel reads it to evaluate `metric-threshold` expectations
/// without re-deriving the data (P4 generic threshold / GUI rule-promotion).
#[derive(Clone, Default)]
pub struct MetricCache {
    inner: Arc<RwLock<std::collections::HashMap<String, f64>>>,
}

impl MetricCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the latest numeric value for `metric`. No-op for non-numeric points.
    pub async fn update(&self, metric: &str, value: &zensight_common::TelemetryValue) {
        use zensight_common::TelemetryValue as V;
        let n = match value {
            V::Counter(c) => *c as f64,
            V::Gauge(g) => *g,
            V::Boolean(b) => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            }
            _ => return,
        };
        self.inner.write().await.insert(metric.to_string(), n);
    }

    /// Latest value for `metric`, if seen.
    pub async fn get(&self, metric: &str) -> Option<f64> {
        self.inner.read().await.get(metric).copied()
    }
}

/// Hot-swappable collector toggles, shared between the poll loop and the runtime
/// `collection` command channel (same pattern as the sentinel's `SentinelHandle`).
/// A `set`/`replace` takes effect on the next poll tick — no restart (P4).
#[derive(Clone)]
pub struct CollectHandle {
    inner: Arc<RwLock<CollectConfig>>,
}

impl CollectHandle {
    pub fn new(cfg: CollectConfig) -> Self {
        Self {
            inner: Arc::new(RwLock::new(cfg)),
        }
    }

    /// Current toggles (cloned snapshot).
    pub async fn snapshot(&self) -> CollectConfig {
        self.inner.read().await.clone()
    }

    /// Replace the full toggle set.
    pub async fn replace(&self, cfg: CollectConfig) {
        *self.inner.write().await = cfg;
    }

    /// Toggle one collector by name. Returns `false` for an unknown name.
    pub async fn set(&self, name: &str, enabled: bool) -> bool {
        let mut g = self.inner.write().await;
        match name {
            "interfaces" => g.interfaces = enabled,
            "sockets" => g.sockets = enabled,
            "neighbors" => g.neighbors = enabled,
            "routes" => g.routes = enabled,
            "diagnostics" => g.diagnostics = enabled,
            "conntrack" => g.conntrack = enabled,
            _ => return false,
        }
        true
    }
}

/// Polls netlink on an interval and publishes interface + socket telemetry.
pub struct Collector {
    host: String,
    config: NetlinkConfig,
    /// Live collector toggles (runtime-reconfigurable via the command channel).
    collect: CollectHandle,
    /// Latest numeric value of every published metric (read by the sentinel's
    /// metric-threshold expectations).
    metric_cache: MetricCache,
    /// Cached advanced publishers so late-joining consumers get the current value
    /// of every metric on connect (via their AdvancedSubscriber `history()`),
    /// instead of waiting for the next poll.
    registry: AdvancedPublisherRegistry,
    /// Sensor health, updated each poll (poll latency, metrics published, host
    /// liveness) so the frontend's Sensors view shows real activity.
    health: Arc<zensight_sensor_core::SensorHealth>,
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
        let collect = CollectHandle::new(config.collect.clone());
        let health = Arc::new(zensight_sensor_core::SensorHealth::new("netlink"));
        Self {
            host,
            config,
            collect,
            metric_cache: MetricCache::new(),
            registry,
            health,
        }
    }

    /// Use the runner's shared health tracker (so updates reach the published
    /// `@/health` snapshot). Without this the collector updates a local tracker.
    pub fn with_health(mut self, health: Arc<zensight_sensor_core::SensorHealth>) -> Self {
        self.health = health;
        self
    }

    /// A clonable handle to this collector's live toggles, for the command channel.
    pub fn collect_handle(&self) -> CollectHandle {
        self.collect.clone()
    }

    /// A clonable handle to the latest-metric cache, for the sentinel's
    /// metric-threshold expectations.
    pub fn metric_cache(&self) -> MetricCache {
        self.metric_cache.clone()
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
        if sockdiag.is_none() {
            tracing::warn!("sockdiag unavailable; socket telemetry disabled");
        }

        // The diagnostics scanner takes ownership of its own Route connection.
        // Opened unconditionally (cheap, unprivileged) so the `diagnostics`
        // toggle can be flipped on at runtime without a restart.
        let diagnostics = match Connection::<Route>::new() {
            Ok(c) => Some(Diagnostics::new(c)),
            Err(e) => {
                tracing::warn!(error = %e, "diagnostics connection failed; disabled");
                None
            }
        };

        // Conntrack needs CAP_NET_ADMIN; open it lazily so the sensor still runs
        // unprivileged (conntrack telemetry just stays absent).
        let conntrack = Connection::<Netfilter>::new().ok();
        if self.config.collect.conntrack && conntrack.is_none() {
            tracing::warn!("conntrack unavailable (needs CAP_NET_ADMIN); disabled");
        }
        // nf_conntrack_max is a one-shot read from procfs (capacity rarely changes).
        let conntrack_max = read_conntrack_max();

        // This sensor monitors one host (itself).
        self.health.set_devices_total(1);

        let mut tick = tokio::time::interval(Duration::from_secs(self.config.poll_interval_secs));
        loop {
            tick.tick().await;
            // Re-read live toggles each tick (runtime-reconfigurable, P4).
            let collect = self.collect.snapshot().await;
            let started = std::time::Instant::now();
            if collect.interfaces {
                self.poll_interfaces(&route).await;
            }
            if collect.sockets
                && let Some(sd) = &sockdiag
            {
                self.poll_sockets(sd).await;
            }
            if collect.neighbors {
                self.poll_neighbors(&route).await;
            }
            if collect.routes {
                self.poll_routes(&route).await;
            }
            if collect.diagnostics
                && let Some(diag) = &diagnostics
            {
                self.poll_diagnostics(diag).await;
            }
            if collect.conntrack
                && let Some(nf) = &conntrack
            {
                self.poll_conntrack(nf, conntrack_max).await;
            }
            // Record this poll's latency + that the host responded, for the
            // Sensors view.
            self.health
                .record_poll_duration(started.elapsed().as_millis() as u64);
            self.health.record_device_success(&self.host);
        }
    }

    async fn poll_conntrack(&self, conn: &Connection<Netfilter>, max: Option<u64>) {
        let v4 = conn.get_conntrack().await.unwrap_or_default();
        let v6 = conn.get_conntrack_v6().await.unwrap_or_default();
        if v4.is_empty() && v6.is_empty() {
            // Either the table is empty or we lack permission; either way nothing
            // to publish (avoids emitting a misleading all-zero summary).
            return;
        }
        let mut summary = aggregate_conntrack(&v4);
        let v6_summary = aggregate_conntrack(&v6);
        summary.total += v6_summary.total;
        summary.tcp += v6_summary.tcp;
        summary.udp += v6_summary.udp;
        summary.icmp += v6_summary.icmp;
        summary.other += v6_summary.other;
        summary.max = max;
        for point in map::conntrack_points(&self.host, &summary) {
            self.publish(&point).await;
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
        self.health.record_metrics_published(1);
        // Tap the latest numeric value for the sentinel's metric-threshold checks.
        self.metric_cache.update(&point.metric, &point.value).await;
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

/// Aggregate conntrack entries by protocol (does not set `max`). Pure;
/// unit-tested via [`map::ConntrackSummary`] shape.
pub fn aggregate_conntrack(entries: &[ConntrackEntry]) -> ConntrackSummary {
    let mut c = ConntrackSummary::default();
    for e in entries {
        c.total += 1;
        match e.proto {
            IpProtocol::Tcp => c.tcp += 1,
            IpProtocol::Udp => c.udp += 1,
            IpProtocol::Icmp | IpProtocol::Icmpv6 => c.icmp += 1,
            _ => c.other += 1,
        }
    }
    c
}

/// Read `nf_conntrack_max` from procfs (the table capacity). `None` if the file
/// is absent or unreadable (e.g. conntrack module not loaded).
fn read_conntrack_max() -> Option<u64> {
    std::fs::read_to_string("/proc/sys/net/netfilter/nf_conntrack_max")
        .ok()?
        .trim()
        .parse()
        .ok()
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
