//! Netlink polling collector: reads kernel state via nlink and publishes
//! telemetry.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Notify, RwLock};
use tokio_stream::StreamExt;

use nlink::netlink::diagnostics::{Bottleneck, DiagnosticReport, Diagnostics, Severity};
use nlink::netlink::genl::ethtool::Duplex;
use nlink::netlink::{
    Connection, Ethtool, Netfilter, Nftables, Route, RtnetlinkGroup, SockDiag, Wireguard, Xfrm,
    genl::wireguard::WgPeer,
    messages::NeighborMessage,
    messages::RouteMessage,
    messages::TcMessage,
    neigh::State as NeighborState,
    netfilter::{ConntrackEntry, IpProtocol},
    nftables::types::Family as NftFamily,
    types::addr::Scope,
    xfrm::{SecurityAssociation, XfrmMode},
};

use crate::events::EventState;
use crate::map::WgPeerView;
use nlink::sockdiag::{SocketFilter, SocketInfo, SocketState, TcpState};
use zensight_common::Format;
use zensight_sensor_core::{AdvancedPublisherConfig, AdvancedPublisherRegistry};

use crate::config::{CollectConfig, NetlinkConfig};
use crate::map::{
    self, AddrEntry, AddressSummary, ConntrackSummary, DiagnosticsSummary, DuplexKind,
    EthtoolSample, IfaceSample, NeighborSummary, NftSummary, NftTableSample, RouteSummary,
    SocketCounts, TcQdiscSample, XfrmSaEntry, XfrmSummary,
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
            "events" => g.events = enabled,
            "ethtool" => g.ethtool = enabled,
            "addresses" => g.addresses = enabled,
            "tc" => g.tc = enabled,
            "xfrm" => g.xfrm = enabled,
            "nftables" => g.nftables = enabled,
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
    /// Real-time RTNETLINK event counters + recent-events ring (issue #8).
    /// Shared with the `@/query/events` channel.
    event_state: EventState,
    /// Nudged whenever a sentinel-relevant event arrives so the sentinel
    /// re-evaluates instantly (~0s) instead of at its next sweep tick (#8).
    sentinel_wake: Arc<Notify>,
    /// Warn-once latch for the XFRM SA dump (EPERM where the host gates it):
    /// avoids a WARN every poll tick for an expected recurring failure (P05 §4).
    warned_xfrm: std::sync::atomic::AtomicBool,
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
        let event_state = EventState::new(config.events.ring_capacity);
        Self {
            host,
            config,
            collect,
            metric_cache: MetricCache::new(),
            registry,
            health,
            event_state,
            sentinel_wake: Arc::new(Notify::new()),
            warned_xfrm: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// A clonable handle to the real-time event state (counters + recent ring),
    /// for the `@/query/events` channel.
    pub fn event_state(&self) -> EventState {
        self.event_state.clone()
    }

    /// A clonable handle to the sentinel-wake signal: the event task nudges this
    /// on a relevant transition so the sentinel re-evaluates immediately (#8).
    pub fn sentinel_wake(&self) -> Arc<Notify> {
        self.sentinel_wake.clone()
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

        // nftables (#14): listing rules typically needs CAP_NET_ADMIN. Open it
        // lazily so the sensor still runs unprivileged (nft telemetry stays absent).
        let nftables = Connection::<Nftables>::new().ok();
        if self.config.collect.nftables && nftables.is_none() {
            tracing::warn!("nftables unavailable (needs CAP_NET_ADMIN); disabled");
        }

        // Conntrack needs CAP_NET_ADMIN; open it lazily so the sensor still runs
        // unprivileged (conntrack telemetry just stays absent).
        let conntrack = Connection::<Netfilter>::new().ok();
        if self.config.collect.conntrack && conntrack.is_none() {
            tracing::warn!("conntrack unavailable (needs CAP_NET_ADMIN); disabled");
        }
        // nf_conntrack_max is a one-shot read from procfs (capacity rarely changes).
        let conntrack_max = read_conntrack_max();

        // ethtool genl handle (link speed/duplex, rings, pause, offloads, #9).
        // Read is unprivileged; absence (no genl family) just disables it.
        let ethtool = match Connection::<Ethtool>::new_async().await {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(error = %e, "ethtool connection failed; ethtool telemetry disabled");
                None
            }
        };

        // XFRM/IPsec handle (#13). Read is unprivileged; absence (no xfrm) just
        // disables it. Opened unconditionally so the toggle can flip at runtime.
        let xfrm = match Connection::<Xfrm>::new() {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(error = %e, "xfrm connection failed; IPsec telemetry disabled");
                None
            }
        };

        // Real-time RTNETLINK events (#8): a *dedicated* Route connection holds the
        // events stream's request lock for its lifetime, so the poll loop's
        // connection stays free for dumps. Subscribe + consume in a background task
        // that folds events into `event_state` and nudges the sentinel.
        if self.config.collect.events {
            match Connection::<Route>::new() {
                Ok(ev_conn) => {
                    if let Err(e) = ev_conn.subscribe(&[
                        RtnetlinkGroup::Link,
                        RtnetlinkGroup::Ipv4Addr,
                        RtnetlinkGroup::Ipv6Addr,
                        RtnetlinkGroup::Ipv4Route,
                        RtnetlinkGroup::Ipv6Route,
                        RtnetlinkGroup::Neigh,
                    ]) {
                        tracing::warn!(error = %e, "event subscribe failed; events disabled");
                    } else {
                        let state = self.event_state.clone();
                        let wake = self.sentinel_wake.clone();
                        tokio::spawn(async move {
                            run_event_stream(ev_conn, state, wake).await;
                        });
                        tracing::info!("real-time RTNETLINK event stream active");
                    }
                }
                Err(e) => tracing::warn!(error = %e, "event connection failed; events disabled"),
            }
        }

        // WireGuard genl handle (needs the wireguard module; full peer data needs
        // CAP_NET_ADMIN). Only opened when interfaces are configured.
        let wireguard = if self.config.wireguard.interfaces.is_empty() {
            None
        } else {
            match Connection::<Wireguard>::new_async().await {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::warn!(error = %e, "wireguard connection failed; disabled");
                    None
                }
            }
        };

        // This sensor monitors one host (itself).
        self.health.set_devices_total(1);

        let mut tick = tokio::time::interval(Duration::from_secs(self.config.poll_interval_secs));
        loop {
            tick.tick().await;
            // Re-read live toggles each tick (runtime-reconfigurable, P4).
            let collect = self.collect.snapshot().await;
            let started = std::time::Instant::now();
            // Track the first error of the tick so the host's health reflects a
            // failed poll (errors_last_hour + degraded/error status) and the GUI
            // gets an error report.
            let mut tick_error: Option<String> = None;
            if collect.interfaces
                && let Err(e) = self.poll_interfaces(&route).await
            {
                tick_error.get_or_insert(e);
            }
            if collect.sockets
                && let Some(sd) = &sockdiag
                && let Err(e) = self.poll_sockets(sd).await
            {
                tick_error.get_or_insert(e);
            }
            if collect.neighbors {
                self.poll_neighbors(&route).await;
            }
            if collect.routes {
                self.poll_routes(&route).await;
            }
            if collect.addresses {
                self.poll_addresses(&route).await;
            }
            if collect.ethtool
                && let Some(et) = &ethtool
            {
                self.poll_ethtool(&route, et).await;
            }
            if collect.tc {
                self.poll_tc(&route).await;
            }
            if collect.xfrm
                && let Some(x) = &xfrm
            {
                self.poll_xfrm(x).await;
            }
            if collect.nftables
                && let Some(nft) = &nftables
            {
                self.poll_nftables(nft).await;
            }
            if collect.events {
                // Publish the cumulative event counters each tick (the per-event
                // reaction happens live in the event task, off this loop).
                for point in self.event_state.counter_points(&self.host) {
                    self.publish(&point).await;
                }
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
            if let Some(wg) = &wireguard {
                self.poll_wireguard(wg).await;
            }
            self.health
                .record_poll_duration(started.elapsed().as_millis() as u64);
            match tick_error {
                Some(err) => {
                    self.health.record_device_failure(&self.host, &err);
                    let report = zensight_sensor_core::ErrorReport::new(
                        zensight_sensor_core::ErrorType::ProtocolError,
                        err,
                    )
                    .with_device(self.host.clone());
                    if let Err(e) = self.health.publish_error(&report).await {
                        tracing::warn!(error = %e, "failed to publish error report");
                    }
                }
                None => self.health.record_device_success(&self.host),
            }
        }
    }

    async fn poll_wireguard(&self, conn: &Connection<Wireguard>) {
        let stale = self.config.wireguard.stale_after_secs;
        for iface in &self.config.wireguard.interfaces {
            let device = match conn.get_device(iface).await {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(error = %e, iface = %iface, "wireguard get_device failed");
                    continue;
                }
            };
            let views: Vec<WgPeerView> = device.peers.iter().map(wg_peer_view).collect();
            for point in map::wireguard_points(&self.host, iface, &views, stale) {
                self.publish(&point).await;
            }
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

    async fn poll_interfaces(&self, conn: &Connection<Route>) -> Result<(), String> {
        let links = conn
            .get_links()
            .await
            .map_err(|e| format!("get_links failed: {e}"))?;
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
        Ok(())
    }

    /// Poll the IP address inventory (#10): stream a low-cardinality summary
    /// (per-family + global counts); per-address detail is served on demand via
    /// `@/query/addresses`. Graceful on failure (logs, emits nothing).
    async fn poll_addresses(&self, conn: &Connection<Route>) {
        let addrs = match conn.get_addresses().await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(error = %e, "get_addresses failed");
                return;
            }
        };
        let entries: Vec<AddrEntry> = addrs
            .iter()
            .map(|a| AddrEntry {
                family: a.family(),
                global: a.scope() == Scope::Universe,
            })
            .collect();
        let summary = aggregate_addresses(&entries);
        for point in map::address_points(&self.host, &summary) {
            self.publish(&point).await;
        }
    }

    /// Poll ethtool per interface (#9): negotiated speed/duplex/autoneg, ring
    /// sizes, pause/flow-control, and a curated offload-feature set. Each family
    /// is best-effort — a NIC/driver that does not expose one leaves it absent
    /// (no misleading zeros). lo and filtered-out interfaces are skipped.
    async fn poll_ethtool(&self, route: &Connection<Route>, et: &Connection<Ethtool>) {
        let links = match route.get_links().await {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, "ethtool: get_links failed");
                return;
            }
        };
        for link in links {
            let name = link.name_or("?").to_string();
            if name == "?" || name == "lo" || !self.config.interfaces.should_include(&name) {
                continue;
            }
            let sample = self.ethtool_sample(et, &name).await;
            for point in map::ethtool_points(&self.host, &sample) {
                self.publish(&point).await;
            }
        }
    }

    /// Build an [`EthtoolSample`] for one interface, querying each ethtool family
    /// independently so a driver that lacks one still yields the others.
    async fn ethtool_sample(&self, et: &Connection<Ethtool>, iface: &str) -> EthtoolSample {
        let mut s = EthtoolSample {
            iface: iface.to_string(),
            ..Default::default()
        };
        if let Ok(ls) = et.get_link_state(iface).await {
            s.carrier = Some(ls.link);
        }
        if let Ok(m) = et.get_link_modes(iface).await {
            s.speed_mbps = m.speed.filter(|&v| v != 0 && v != u32::MAX);
            s.duplex = m.duplex.map(duplex_kind);
            s.autoneg = Some(m.autoneg);
        }
        if let Ok(r) = et.get_rings(iface).await {
            s.rx_ring = r.rx;
            s.tx_ring = r.tx;
            s.rx_ring_max = r.rx_max;
            s.tx_ring_max = r.tx_max;
        }
        if let Ok(p) = et.get_pause(iface).await {
            s.pause_rx = p.rx;
            s.pause_tx = p.tx;
            s.pause_autoneg = p.autoneg;
            if let Some(stats) = p.stats {
                s.pause_rx_frames = stats.rx_frames;
                s.pause_tx_frames = stats.tx_frames;
            }
        }
        if let Ok(f) = et.get_features(iface).await {
            // Curated, bounded set of the offloads operators care about (P2).
            for feat in CURATED_FEATURES {
                if f.is_changeable(feat) || f.is_active(feat) {
                    s.features.push((feat.to_string(), f.is_active(feat)));
                }
            }
        }
        s
    }

    /// Poll XFRM/IPsec SA + policy health (#13): a low-cardinality summary (SA
    /// counts by mode/proto + policy total). Per-SA detail is served on demand via
    /// `@/query/xfrm`. Graceful on failure / no-IPsec.
    async fn poll_xfrm(&self, conn: &Connection<Xfrm>) {
        let sas = match conn.get_security_associations().await {
            Ok(s) => s,
            Err(e) => {
                // Warn-once: on hosts that gate the SA dump this fails every tick.
                if !self
                    .warned_xfrm
                    .swap(true, std::sync::atomic::Ordering::Relaxed)
                {
                    tracing::warn!(error = %e, "get_security_associations failed; XFRM disabled (warn-once)");
                }
                return;
            }
        };
        let policy_total = conn
            .get_security_policies()
            .await
            .map(|p| p.len() as u64)
            .unwrap_or(0);
        if sas.is_empty() && policy_total == 0 {
            // No IPsec configured — emit nothing (avoid misleading all-zero SA).
            return;
        }
        let entries: Vec<XfrmSaEntry> = sas.iter().map(xfrm_sa_entry).collect();
        let summary = aggregate_xfrm(&entries, policy_total);
        for point in map::xfrm_points(&self.host, &summary) {
            self.publish(&point).await;
        }
    }

    /// Poll nftables (#14): per-table chain/rule counts + host totals — firewall
    /// ruleset shape / policy-drift visibility. Full inventory served on demand
    /// via `@/query/nft`. Graceful on failure / empty ruleset.
    async fn poll_nftables(&self, conn: &Connection<Nftables>) {
        let tables = match conn.list_tables().await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, "nftables list_tables failed");
                return;
            }
        };
        if tables.is_empty() {
            return;
        }
        let chains = conn.list_chains().await.unwrap_or_default();
        let mut summary = NftSummary {
            tables_total: tables.len() as u64,
            ..Default::default()
        };
        for t in &tables {
            let family = nft_family_label(t.family);
            let chain_count = chains
                .iter()
                .filter(|c| c.table == t.name && c.family == t.family)
                .count() as u64;
            let rule_count = conn
                .list_rules(&t.name, t.family)
                .await
                .map(|r| r.len() as u64)
                .unwrap_or(0);
            summary.chains_total += chain_count;
            summary.rules_total += rule_count;
            summary.tables.push(NftTableSample {
                family: family.to_string(),
                table: t.name.clone(),
                chains: chain_count,
                rules: rule_count,
            });
        }
        for point in map::nft_points(&self.host, &summary) {
            self.publish(&point).await;
        }
    }

    /// Poll TC/QoS qdisc stats (#12): per-(iface,qdisc) drops/overlimits/backlog.
    /// Bounded by the TC hierarchy (one series set per qdisc). Interfaces are
    /// resolved to names via a single `get_links` map; filtered/`lo` are skipped.
    /// Full tree is served on demand via `@/query/tc`.
    async fn poll_tc(&self, conn: &Connection<Route>) {
        let qdiscs = match conn.get_qdiscs().await {
            Ok(q) => q,
            Err(e) => {
                tracing::warn!(error = %e, "get_qdiscs failed");
                return;
            }
        };
        if qdiscs.is_empty() {
            return;
        }
        let names = self.ifindex_names(conn).await;
        for q in &qdiscs {
            let Some(iface) = names.get(&q.ifindex()) else {
                continue;
            };
            if iface == "lo" || !self.config.interfaces.should_include(iface) {
                continue;
            }
            let sample = tc_qdisc_sample(q, iface);
            for point in map::tc_points(&self.host, &sample) {
                self.publish(&point).await;
            }
        }
    }

    /// Build an `ifindex -> name` map from a single `get_links` dump.
    async fn ifindex_names(
        &self,
        conn: &Connection<Route>,
    ) -> std::collections::HashMap<u32, String> {
        match conn.get_links().await {
            Ok(links) => links
                .into_iter()
                .filter_map(|l| {
                    let name = l.name_or("?").to_string();
                    (name != "?").then_some((l.ifindex(), name))
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "tc: get_links (for ifindex map) failed");
                std::collections::HashMap::new()
            }
        }
    }

    async fn poll_sockets(&self, conn: &Connection<SockDiag>) -> Result<(), String> {
        // Request mem + congestion extensions (#11) on top of tcp_info so the
        // aggregate carries per-algorithm counts and buffer totals.
        let filter = SocketFilter::tcp()
            .all_states()
            .with_tcp_info()
            .with_mem_info()
            .with_congestion()
            .build();
        let socks = conn
            .query(&filter)
            .await
            .map_err(|e| format!("sockdiag query failed: {e}"))?;
        let counts = aggregate_sockets(&socks);
        for point in map::socket_points(&self.host, &counts) {
            self.publish(&point).await;
        }
        Ok(())
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
        let established = matches!(
            inet.state,
            SocketState::Tcp(TcpState::Established) | SocketState::Established
        );
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
        // Congestion algorithm — count only established sockets (a listener has no
        // negotiated algorithm) so the by_cong breakdown matches `established`.
        if established && let Some(algo) = &inet.congestion {
            *c.by_cong.entry(algo.clone()).or_insert(0) += 1;
        }
        if let Some(mem) = &inet.mem_info {
            c.snd_buf_total += mem.sndbuf as u64;
            c.rcv_buf_total += mem.rcvbuf as u64;
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

/// Curated, bounded set of ethtool offload features streamed as booleans (#9).
/// Cardinality discipline (P2): a fixed handful rather than every kernel flag.
const CURATED_FEATURES: &[&str] = &[
    "tx-checksumming",
    "rx-checksumming",
    "tcp-segmentation-offload",
    "generic-segmentation-offload",
    "generic-receive-offload",
    "scatter-gather",
];

/// Map nlink's `Duplex` into the nlink-free [`DuplexKind`] used by `map.rs`.
fn duplex_kind(d: Duplex) -> DuplexKind {
    match d {
        Duplex::Half => DuplexKind::Half,
        Duplex::Full => DuplexKind::Full,
        // `Duplex` is #[non_exhaustive] upstream; treat anything else as unknown.
        _ => DuplexKind::Unknown,
    }
}

/// Aggregate per-address entries into an [`AddressSummary`] (#10). Pure;
/// unit-tested. `family` is the `AF_*` byte (`AF_INET`/`AF_INET6`).
pub fn aggregate_addresses(entries: &[AddrEntry]) -> AddressSummary {
    let mut a = AddressSummary::default();
    for e in entries {
        a.total += 1;
        if e.family == AF_INET {
            a.ipv4_count += 1;
        } else if e.family == AF_INET6 {
            a.ipv6_count += 1;
        }
        if e.global {
            a.global_count += 1;
        }
    }
    a
}

/// Stable lowercase label for an nftables address family (#14).
fn nft_family_label(f: NftFamily) -> &'static str {
    match f {
        NftFamily::Ip => "ip",
        NftFamily::Ip6 => "ip6",
        NftFamily::Inet => "inet",
        NftFamily::Arp => "arp",
        NftFamily::Bridge => "bridge",
        NftFamily::Netdev => "netdev",
        // `Family` is #[non_exhaustive] upstream.
        _ => "other",
    }
}

/// Decode an XFRM [`SecurityAssociation`] into the pure [`XfrmSaEntry`] (#13).
fn xfrm_sa_entry(sa: &SecurityAssociation) -> XfrmSaEntry {
    let mode = match sa.mode {
        XfrmMode::Transport => "transport",
        XfrmMode::Tunnel => "tunnel",
        XfrmMode::Beet => "beet",
        // `XfrmMode` is #[non_exhaustive] upstream (also covers `Other`).
        _ => "other",
    };
    XfrmSaEntry {
        mode: mode.to_string(),
        proto: ipsec_proto_label(&sa.protocol).to_string(),
    }
}

/// Stable lowercase label for an IPsec protocol (`#[non_exhaustive]` upstream).
fn ipsec_proto_label(p: &nlink::netlink::xfrm::IpsecProtocol) -> &'static str {
    use nlink::netlink::xfrm::IpsecProtocol;
    match p {
        IpsecProtocol::Esp => "esp",
        IpsecProtocol::Ah => "ah",
        IpsecProtocol::Comp => "comp",
        _ => "other",
    }
}

/// Aggregate XFRM SA entries + a policy count into an [`XfrmSummary`] (#13).
/// Pure; unit-tested. (The pinned nlink SA carries no liveness "state" field, so
/// we group by mode/proto — the dimensions the typed SA actually exposes.)
pub fn aggregate_xfrm(entries: &[XfrmSaEntry], policy_total: u64) -> XfrmSummary {
    let mut x = XfrmSummary {
        policy_total,
        ..Default::default()
    };
    for e in entries {
        x.sa_total += 1;
        *x.sa_by_mode.entry(e.mode.clone()).or_insert(0) += 1;
        *x.sa_by_proto.entry(e.proto.clone()).or_insert(0) += 1;
    }
    x
}

/// Decode a TC qdisc [`TcMessage`] into the pure [`TcQdiscSample`] (#12).
/// `backlog()` is the byte backlog; `qlen()` is the queued-packet count.
fn tc_qdisc_sample(q: &TcMessage, iface: &str) -> TcQdiscSample {
    TcQdiscSample {
        iface: iface.to_string(),
        kind: q.kind().unwrap_or("unknown").to_string(),
        handle: q.handle_str(),
        bytes: q.bytes(),
        packets: q.packets(),
        drops: q.drops() as u64,
        overlimits: q.overlimits() as u64,
        requeues: q.requeues() as u64,
        backlog_bytes: q.backlog() as u64,
        backlog_pkts: q.qlen() as u64,
    }
}

/// Consume the real-time RTNETLINK event stream (#8): fold each event into the
/// shared [`EventState`] (counters + recent ring) and, for sentinel-relevant
/// transitions, nudge `wake` so the sentinel re-evaluates instantly.
///
/// `events()` borrows the connection for the stream's lifetime and holds its
/// request lock — hence the dedicated connection. If the socket errors the task
/// logs and exits; the periodic poll loop keeps publishing counters meanwhile.
async fn run_event_stream(conn: Connection<Route>, state: EventState, wake: Arc<Notify>) {
    let mut events = conn.events().await;
    while let Some(item) = events.next().await {
        match item {
            Ok(ev) => {
                state.observe(&ev);
                if crate::events::is_sentinel_relevant(&ev) {
                    wake.notify_one();
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "event stream error; stopping event task");
                break;
            }
        }
    }
    tracing::info!("RTNETLINK event stream ended");
}

/// Decompose an nlink [`WgPeer`] into the pure [`WgPeerView`] (computes the
/// handshake age relative to now and a short, stable peer id from the pubkey).
fn wg_peer_view(peer: &WgPeer) -> WgPeerView {
    // Short id: first 8 chars of the base64 public key (bounded-cardinality
    // label that still distinguishes peers).
    let b64 = base64_encode(&peer.public_key);
    let id: String = b64.chars().take(8).collect();
    let handshake_age_s = peer.last_handshake.and_then(|t| {
        std::time::SystemTime::now()
            .duration_since(t)
            .ok()
            .map(|d| d.as_secs())
    });
    WgPeerView {
        id,
        endpoint: peer.endpoint.map(|e| e.to_string()),
        handshake_age_s,
        rx_bytes: peer.rx_bytes,
        tx_bytes: peer.tx_bytes,
    }
}

/// Minimal standard-base64 of a 32-byte key (no external dep), for a short peer
/// id label.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6 & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 63) as usize] as char);
        }
    }
    out
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
    use super::{aggregate_addresses, percentile};
    use crate::map::AddrEntry;

    const AF_INET: u8 = 2;
    const AF_INET6: u8 = 10;

    #[test]
    fn aggregate_addresses_counts_family_and_global() {
        let e = |family, global| AddrEntry { family, global };
        let entries = [
            e(AF_INET, true),   // global v4
            e(AF_INET, false),  // non-global v4
            e(AF_INET6, true),  // global v6
            e(AF_INET6, false), // link-local v6
            e(99, true),        // unknown family: counted in total only
        ];
        let a = aggregate_addresses(&entries);
        assert_eq!(a.total, 5);
        assert_eq!(a.ipv4_count, 2);
        assert_eq!(a.ipv6_count, 2);
        assert_eq!(a.global_count, 3);
        // Empty input yields an all-zero summary.
        assert_eq!(aggregate_addresses(&[]), super::AddressSummary::default());
    }

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
