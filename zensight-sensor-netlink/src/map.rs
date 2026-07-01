//! Pure mapping from observed kernel state to [`TelemetryPoint`]s.
//!
//! The collector reads nlink and fills these plain sample structs; the mapping
//! to telemetry is kept here, free of any kernel/nlink dependency, so it is
//! unit-testable without privileges or a live netlink socket.

use std::collections::HashMap;

use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

/// A snapshot of one network interface.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IfaceSample {
    pub name: String,
    pub ifindex: u32,
    pub up: bool,
    pub carrier: Option<bool>,
    pub mtu: Option<u32>,
    pub mac: Option<String>,
    pub oper_state: Option<String>,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_packets: u64,
    pub tx_packets: u64,
    pub rx_errors: u64,
    pub tx_errors: u64,
    pub rx_dropped: u64,
    pub tx_dropped: u64,
    pub multicast: u64,
    pub collisions: u64,
}

/// Summary of the routing table.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RouteSummary {
    pub ipv4_count: u64,
    pub ipv6_count: u64,
    pub total: u64,
    pub default_v4_present: bool,
    pub default_v6_present: bool,
    /// The IPv4 default gateway, if any (label/text).
    pub default_v4_gw: Option<String>,
}

/// A decomposed WireGuard peer (built from nlink's `WgPeer`, kept pure so the
/// mapping is unit-testable without a live WG device).
#[derive(Debug, Clone, PartialEq)]
pub struct WgPeerView {
    /// Short peer identifier (e.g. first chars of the base64 public key).
    pub id: String,
    pub endpoint: Option<String>,
    /// Seconds since the last successful handshake; `None` if it never happened.
    pub handshake_age_s: Option<u64>,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    /// Peer's AllowedIPs from a matching `wg-quick` config (#268), for a readable
    /// GUI label. `None` when no config covers this peer.
    pub allowed_ips: Option<String>,
}

/// Build telemetry for one WireGuard interface's peers. Metric paths are
/// `wireguard/<iface>/<peer>/<stat>` plus `wireguard/<iface>/peers`. A peer is
/// `up` when its last handshake is within `stale_after_s`.
pub fn wireguard_points(
    host: &str,
    iface: &str,
    peers: &[WgPeerView],
    stale_after_s: u64,
) -> Vec<TelemetryPoint> {
    let mut out = vec![point(
        host,
        format!("wireguard/{iface}/peers"),
        TelemetryValue::Gauge(peers.len() as f64),
    )];
    for p in peers {
        let pfx = format!("wireguard/{iface}/{}", p.id);
        let mut labels = std::collections::HashMap::new();
        if let Some(ep) = &p.endpoint {
            labels.insert("endpoint".to_string(), ep.clone());
        }
        // wg-quick enrichment (#268): AllowedIPs as a readable peer label.
        if let Some(aips) = &p.allowed_ips {
            labels.insert("allowed_ips".to_string(), aips.clone());
        }
        out.push(
            point(
                host,
                format!("{pfx}/rx_bytes"),
                TelemetryValue::Counter(p.rx_bytes),
            )
            .with_labels(labels.clone()),
        );
        out.push(point(
            host,
            format!("{pfx}/tx_bytes"),
            TelemetryValue::Counter(p.tx_bytes),
        ));
        // Handshake age (large sentinel when never handshaked) + up/down.
        let age = p.handshake_age_s.unwrap_or(u64::MAX);
        let up = p
            .handshake_age_s
            .map(|a| a <= stale_after_s)
            .unwrap_or(false);
        if let Some(a) = p.handshake_age_s {
            out.push(point(
                host,
                format!("{pfx}/last_handshake_age_s"),
                TelemetryValue::Gauge(a as f64),
            ));
        }
        let _ = age;
        out.push(point(
            host,
            format!("{pfx}/up"),
            TelemetryValue::Boolean(up),
        ));
    }
    out
}

/// Aggregate of the netfilter connection-tracking table (NAT/flow-table health).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConntrackSummary {
    pub total: u64,
    pub tcp: u64,
    pub udp: u64,
    pub icmp: u64,
    pub other: u64,
    /// `nf_conntrack_max` (table capacity), if readable.
    pub max: Option<u64>,
}

/// Aggregate ARP/NDP neighbor counts by reachability state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NeighborSummary {
    pub reachable: u64,
    pub stale: u64,
    pub failed: u64,
    pub incomplete: u64,
    pub permanent: u64,
    pub other: u64,
    pub total: u64,
}

/// Summary of nlink's built-in network diagnostics scan: issue counts by
/// severity plus the single worst bottleneck (if any). The per-issue detail is
/// intentionally collapsed to counts here (cardinality discipline, P2).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DiagnosticsSummary {
    pub issues_info: u64,
    pub issues_warning: u64,
    pub issues_error: u64,
    pub issues_critical: u64,
    /// Normalized 0..=1 severity of the worst bottleneck (0 if none).
    pub bottleneck_score: f64,
    /// Worst-bottleneck descriptors (labels on the bottleneck point), if any.
    pub bottleneck_location: Option<String>,
    pub bottleneck_type: Option<String>,
    pub bottleneck_recommendation: Option<String>,
    pub bottleneck_drop_rate: f64,
}

impl DiagnosticsSummary {
    pub fn issues_total(&self) -> u64 {
        self.issues_info + self.issues_warning + self.issues_error + self.issues_critical
    }
}

/// Aggregate TCP socket counts (from sockdiag).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SocketCounts {
    pub established: u64,
    pub listen: u64,
    pub time_wait: u64,
    pub syn_sent: u64,
    pub close_wait: u64,
    pub retransmits_total: u64,
    pub max_rtt_us: u64,
    /// RTT percentiles across established sockets (microseconds).
    pub rtt_p50_us: u64,
    pub rtt_p95_us: u64,
    /// Delivery-rate percentiles across established sockets (bytes/sec, #108) —
    /// "are flows actually delivering" vs just "how many are established".
    pub delivery_rate_p50: u64,
    pub delivery_rate_p95: u64,
    /// Pacing-rate percentiles across established sockets (bytes/sec, #108).
    /// Sockets reporting the kernel's `~0` "unlimited" sentinel are excluded.
    pub pacing_rate_p50: u64,
    pub pacing_rate_p95: u64,
    /// Receiver-side RTT estimate percentiles (microseconds, #108).
    pub rcv_rtt_p50_us: u64,
    pub rcv_rtt_p95_us: u64,
    /// Sum of bytes retransmitted across sockets (#108, monotonic per-socket).
    pub bytes_retrans_total: u64,
    /// Sum of segment retransmits across sockets (#108) — distinct from the
    /// legacy `retransmits_total` (current `retrans`), this is lifetime `total_retrans`.
    pub total_retrans_total: u64,
    /// Sum of reordering events observed across sockets (`reord_seen`, #108).
    pub reordered_total: u64,
    /// Sum of currently-lost (unacked, presumed-lost) segments across sockets (#108).
    pub lost_total: u64,
    /// Established-socket count by TCP congestion-control algorithm (#11). Bounded
    /// cardinality — there are only a handful of algorithms on any host.
    pub by_cong: HashMap<String, u64>,
    /// Sum of socket send/receive buffer bytes across sockets (#11):
    /// bufferbloat / memory-pressure signal.
    pub snd_buf_total: u64,
    pub rcv_buf_total: u64,
}

fn point(host: &str, metric: impl Into<String>, value: TelemetryValue) -> TelemetryPoint {
    TelemetryPoint::new(host, Protocol::Netlink, metric, value)
}

/// Build telemetry points for one interface. Metric paths are
/// `iface/<name>/<stat>`.
pub fn iface_points(host: &str, s: &IfaceSample) -> Vec<TelemetryPoint> {
    let pfx = format!("iface/{}", s.name);
    let ifindex = s.ifindex.to_string();
    let counter = |metric: String, v: u64| {
        point(host, metric, TelemetryValue::Counter(v)).with_label("ifindex", ifindex.clone())
    };

    let mut out = vec![
        counter(format!("{pfx}/rx_bytes"), s.rx_bytes),
        counter(format!("{pfx}/tx_bytes"), s.tx_bytes),
        counter(format!("{pfx}/rx_packets"), s.rx_packets),
        counter(format!("{pfx}/tx_packets"), s.tx_packets),
        counter(format!("{pfx}/rx_errors"), s.rx_errors),
        counter(format!("{pfx}/tx_errors"), s.tx_errors),
        counter(format!("{pfx}/rx_dropped"), s.rx_dropped),
        counter(format!("{pfx}/tx_dropped"), s.tx_dropped),
        counter(format!("{pfx}/multicast"), s.multicast),
        counter(format!("{pfx}/collisions"), s.collisions),
        point(
            host,
            format!("{pfx}/oper_state"),
            TelemetryValue::Text(
                s.oper_state
                    .clone()
                    .unwrap_or_else(|| if s.up { "up".into() } else { "down".into() }),
            ),
        )
        .with_label("ifindex", ifindex.clone()),
        point(host, format!("{pfx}/up"), TelemetryValue::Boolean(s.up))
            .with_label("ifindex", ifindex.clone()),
    ];

    if let Some(carrier) = s.carrier {
        out.push(
            point(
                host,
                format!("{pfx}/carrier"),
                TelemetryValue::Boolean(carrier),
            )
            .with_label("ifindex", ifindex.clone()),
        );
    }
    if let Some(mtu) = s.mtu {
        out.push(
            point(
                host,
                format!("{pfx}/mtu"),
                TelemetryValue::Gauge(mtu as f64),
            )
            .with_label("ifindex", ifindex.clone()),
        );
    }
    if let Some(mac) = &s.mac {
        let mut labels = HashMap::new();
        labels.insert("ifindex".to_string(), ifindex.clone());
        labels.insert("mac".to_string(), mac.clone());
        out.push(
            point(
                host,
                format!("{pfx}/info"),
                TelemetryValue::Text(mac.clone()),
            )
            .with_labels(labels),
        );
    }
    out
}

/// Build telemetry points for the TCP socket aggregates. Metric paths are
/// `sockets/tcp/<stat>`, plus `sockets/tcp/by_cong/<algo>` and
/// `sockets/tcp/mem/{snd,rcv}_buf_total` when mem/congestion info is available.
pub fn socket_points(host: &str, c: &SocketCounts) -> Vec<TelemetryPoint> {
    let mut out = vec![
        point(
            host,
            "sockets/tcp/established",
            TelemetryValue::Gauge(c.established as f64),
        ),
        point(
            host,
            "sockets/tcp/listen",
            TelemetryValue::Gauge(c.listen as f64),
        ),
        point(
            host,
            "sockets/tcp/time_wait",
            TelemetryValue::Gauge(c.time_wait as f64),
        ),
        point(
            host,
            "sockets/tcp/syn_sent",
            TelemetryValue::Gauge(c.syn_sent as f64),
        ),
        point(
            host,
            "sockets/tcp/close_wait",
            TelemetryValue::Gauge(c.close_wait as f64),
        ),
        point(
            host,
            "sockets/tcp/retransmits_total",
            TelemetryValue::Counter(c.retransmits_total),
        ),
        point(
            host,
            "sockets/tcp/max_rtt_us",
            TelemetryValue::Gauge(c.max_rtt_us as f64),
        ),
        point(
            host,
            "sockets/tcp/rtt_p50_us",
            TelemetryValue::Gauge(c.rtt_p50_us as f64),
        ),
        point(
            host,
            "sockets/tcp/rtt_p95_us",
            TelemetryValue::Gauge(c.rtt_p95_us as f64),
        ),
        // Delivery-health counters (#108) — always emitted (monotonic-ish sums).
        point(
            host,
            "sockets/tcp/bytes_retrans_total",
            TelemetryValue::Counter(c.bytes_retrans_total),
        ),
        point(
            host,
            "sockets/tcp/total_retrans_total",
            TelemetryValue::Counter(c.total_retrans_total),
        ),
        point(
            host,
            "sockets/tcp/reordered_total",
            TelemetryValue::Counter(c.reordered_total),
        ),
        point(
            host,
            "sockets/tcp/lost_total",
            TelemetryValue::Gauge(c.lost_total as f64),
        ),
    ];
    // Delivery/pacing/rcv-rtt percentiles (#108): only meaningful with established
    // sockets carrying tcp_info — omit when 0 so a quiet host doesn't clobber the
    // cached gauge with a misleading zero (mirrors the buffer-totals policy).
    if c.delivery_rate_p50 > 0 || c.delivery_rate_p95 > 0 {
        out.push(point(
            host,
            "sockets/tcp/delivery_rate_p50",
            TelemetryValue::Gauge(c.delivery_rate_p50 as f64),
        ));
        out.push(point(
            host,
            "sockets/tcp/delivery_rate_p95",
            TelemetryValue::Gauge(c.delivery_rate_p95 as f64),
        ));
    }
    if c.pacing_rate_p50 > 0 || c.pacing_rate_p95 > 0 {
        out.push(point(
            host,
            "sockets/tcp/pacing_rate_p50",
            TelemetryValue::Gauge(c.pacing_rate_p50 as f64),
        ));
        out.push(point(
            host,
            "sockets/tcp/pacing_rate_p95",
            TelemetryValue::Gauge(c.pacing_rate_p95 as f64),
        ));
    }
    if c.rcv_rtt_p50_us > 0 || c.rcv_rtt_p95_us > 0 {
        out.push(point(
            host,
            "sockets/tcp/rcv_rtt_p50_us",
            TelemetryValue::Gauge(c.rcv_rtt_p50_us as f64),
        ));
        out.push(point(
            host,
            "sockets/tcp/rcv_rtt_p95_us",
            TelemetryValue::Gauge(c.rcv_rtt_p95_us as f64),
        ));
    }
    // Buffer totals (only meaningful when mem info was requested; both 0 → omit).
    if c.snd_buf_total > 0 || c.rcv_buf_total > 0 {
        out.push(point(
            host,
            "sockets/tcp/mem/snd_buf_total",
            TelemetryValue::Gauge(c.snd_buf_total as f64),
        ));
        out.push(point(
            host,
            "sockets/tcp/mem/rcv_buf_total",
            TelemetryValue::Gauge(c.rcv_buf_total as f64),
        ));
    }
    // Established-socket count per congestion-control algorithm (bounded set).
    for (algo, n) in &c.by_cong {
        out.push(point(
            host,
            format!("sockets/tcp/by_cong/{algo}"),
            TelemetryValue::Gauge(*n as f64),
        ));
    }
    out
}

/// Build telemetry points for the routing-table summary.
pub fn route_points(host: &str, r: &RouteSummary) -> Vec<TelemetryPoint> {
    let mut out = vec![
        point(
            host,
            "routes/ipv4_count",
            TelemetryValue::Gauge(r.ipv4_count as f64),
        ),
        point(
            host,
            "routes/ipv6_count",
            TelemetryValue::Gauge(r.ipv6_count as f64),
        ),
        point(host, "routes/total", TelemetryValue::Gauge(r.total as f64)),
        point(
            host,
            "routes/default_v4_present",
            TelemetryValue::Boolean(r.default_v4_present),
        ),
        point(
            host,
            "routes/default_v6_present",
            TelemetryValue::Boolean(r.default_v6_present),
        ),
    ];
    if let Some(gw) = &r.default_v4_gw {
        out.push(
            point(
                host,
                "routes/default_v4_gw",
                TelemetryValue::Text(gw.clone()),
            )
            .with_label("gateway", gw.clone()),
        );
    }
    out
}

/// Build telemetry points for the neighbor (ARP/NDP) summary. Metric paths are
/// `neighbors/by_state/<state>` plus `neighbors/total`.
pub fn neighbor_points(host: &str, n: &NeighborSummary) -> Vec<TelemetryPoint> {
    let g = |metric: &str, v: u64| point(host, metric, TelemetryValue::Gauge(v as f64));
    vec![
        g("neighbors/by_state/reachable", n.reachable),
        g("neighbors/by_state/stale", n.stale),
        g("neighbors/by_state/failed", n.failed),
        g("neighbors/by_state/incomplete", n.incomplete),
        g("neighbors/by_state/permanent", n.permanent),
        g("neighbors/by_state/other", n.other),
        g("neighbors/total", n.total),
    ]
}

/// Build telemetry points for the diagnostics summary. Metric paths are
/// `diagnostics/issues/<severity>`, `diagnostics/issues/total`,
/// `diagnostics/bottleneck_score`, and (when a bottleneck exists)
/// `diagnostics/bottleneck` (Text = type, with location/recommendation labels).
pub fn diagnostics_points(host: &str, d: &DiagnosticsSummary) -> Vec<TelemetryPoint> {
    let g = |metric: &str, v: u64| point(host, metric, TelemetryValue::Gauge(v as f64));
    let mut out = vec![
        g("diagnostics/issues/info", d.issues_info),
        g("diagnostics/issues/warning", d.issues_warning),
        g("diagnostics/issues/error", d.issues_error),
        g("diagnostics/issues/critical", d.issues_critical),
        g("diagnostics/issues/total", d.issues_total()),
        point(
            host,
            "diagnostics/bottleneck_score",
            TelemetryValue::Gauge(d.bottleneck_score),
        ),
    ];
    if let Some(kind) = &d.bottleneck_type {
        let mut labels = HashMap::new();
        if let Some(loc) = &d.bottleneck_location {
            labels.insert("location".to_string(), loc.clone());
        }
        if let Some(rec) = &d.bottleneck_recommendation {
            labels.insert("recommendation".to_string(), rec.clone());
        }
        labels.insert(
            "drop_rate".to_string(),
            format!("{}", d.bottleneck_drop_rate),
        );
        out.push(
            point(
                host,
                "diagnostics/bottleneck",
                TelemetryValue::Text(kind.clone()),
            )
            .with_labels(labels),
        );
    }
    out
}

// ---------------------------------------------------------------------------
// On-demand detail (principle P2): served via the query channel
// (`@/query/{routes,neighbors,sockets}`), never streamed onto the telemetry bus.
// The record DTOs live in `zensight-common` (shared with the GUI decoder);
// `query.rs` builds them from live nlink dumps. The `SocketSelector` below is
// sensor-side filtering logic, kept here and unit-tested.
// ---------------------------------------------------------------------------

pub use zensight_common::{NeighborRecord, RouteRecord, SocketRecord};

/// Selector parameters for the sockets query (`?state=&port=`). Both optional;
/// absent means "no filter on that field".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SocketSelector {
    /// Match `SocketRecord::state` case-insensitively (e.g. `"established"`).
    pub state: Option<String>,
    /// Match local OR remote port.
    pub port: Option<u16>,
}

impl SocketSelector {
    /// Parse a Zenoh selector parameter string (`"state=established&port=22"`).
    /// Unknown keys and unparseable ports are ignored (best-effort filter).
    pub fn parse(params: &str) -> Self {
        let mut sel = SocketSelector::default();
        for pair in params.split('&').filter(|s| !s.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            match k.trim() {
                "state" if !v.is_empty() => sel.state = Some(v.trim().to_lowercase()),
                "port" => sel.port = v.trim().parse().ok(),
                _ => {}
            }
        }
        sel
    }

    /// Does `rec` pass this selector? Port matches either endpoint.
    pub fn matches(&self, rec: &SocketRecord) -> bool {
        if let Some(state) = &self.state
            && !rec.state.eq_ignore_ascii_case(state)
        {
            return false;
        }
        if let Some(port) = self.port {
            let suffix = format!(":{port}");
            if !rec.local.ends_with(&suffix) && !rec.remote.ends_with(&suffix) {
                return false;
            }
        }
        true
    }
}

/// Build telemetry points for the conntrack summary. Metric paths are
/// `conntrack/entries`, `conntrack/by_proto/<proto>`, `conntrack/max`,
/// `conntrack/utilization`.
pub fn conntrack_points(host: &str, c: &ConntrackSummary) -> Vec<TelemetryPoint> {
    let g = |metric: &str, v: u64| point(host, metric, TelemetryValue::Gauge(v as f64));
    let mut out = vec![
        g("conntrack/entries", c.total),
        g("conntrack/by_proto/tcp", c.tcp),
        g("conntrack/by_proto/udp", c.udp),
        g("conntrack/by_proto/icmp", c.icmp),
        g("conntrack/by_proto/other", c.other),
    ];
    if let Some(max) = c.max {
        out.push(g("conntrack/max", max));
        // Utilization in [0,1]; the classic outage predictor when near 1.
        let util = if max > 0 {
            c.total as f64 / max as f64
        } else {
            0.0
        };
        out.push(point(
            host,
            "conntrack/utilization",
            TelemetryValue::Gauge(util),
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// ethtool (issue #9): link speed/duplex/autoneg, ring sizes, offloads, pause.
// ---------------------------------------------------------------------------

/// Negotiated duplex mode (mirrors nlink's `Duplex`, decoupled so `map.rs`
/// carries no nlink dependency).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuplexKind {
    Half,
    Full,
    Unknown,
}

impl DuplexKind {
    /// Lowercase wire label.
    pub fn label(self) -> &'static str {
        match self {
            DuplexKind::Half => "half",
            DuplexKind::Full => "full",
            DuplexKind::Unknown => "unknown",
        }
    }
}

/// A snapshot of one interface's ethtool view (all fields optional: a NIC/driver
/// that does not expose a family simply leaves it `None` — no misleading zeros).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EthtoolSample {
    pub iface: String,
    /// Physical carrier detected (link state).
    pub carrier: Option<bool>,
    /// Negotiated speed in Mb/s.
    pub speed_mbps: Option<u32>,
    pub duplex: Option<DuplexKind>,
    pub autoneg: Option<bool>,
    /// Current/maximum RX & TX ring sizes (undersized ring = drop risk).
    pub rx_ring: Option<u32>,
    pub tx_ring: Option<u32>,
    pub rx_ring_max: Option<u32>,
    pub tx_ring_max: Option<u32>,
    /// Pause/flow-control settings + frame counters.
    pub pause_rx: Option<bool>,
    pub pause_tx: Option<bool>,
    pub pause_autoneg: Option<bool>,
    pub pause_rx_frames: Option<u64>,
    pub pause_tx_frames: Option<u64>,
    /// Curated offload features `(short_name, active)` (bounded cardinality).
    pub features: Vec<(String, bool)>,
    /// Forward Error Correction (nlink 0.23 `get_fec`): configured mode names
    /// (e.g. `RS`, `BASER`, `None`) joined for display, and whether FEC mode is
    /// auto-negotiated. High-speed links silently corrupting under marginal
    /// optics show up here before they show up as drops.
    pub fec_modes: Option<String>,
    pub fec_auto: Option<bool>,
    /// Energy-Efficient Ethernet (nlink 0.23 `get_eee`): administratively
    /// enabled vs currently active. EEE is a common culprit for added latency /
    /// micro-stalls on NICs that negotiate it unexpectedly.
    pub eee_enabled: Option<bool>,
    pub eee_active: Option<bool>,
}

/// Build telemetry points for one interface's ethtool view. Metric paths are
/// `ethtool/<iface>/...`. Absent fields are omitted (graceful degradation).
pub fn ethtool_points(host: &str, s: &EthtoolSample) -> Vec<TelemetryPoint> {
    let pfx = format!("ethtool/{}", s.iface);
    let mut out = Vec::new();
    if let Some(carrier) = s.carrier {
        out.push(point(
            host,
            format!("{pfx}/carrier"),
            TelemetryValue::Boolean(carrier),
        ));
    }
    if let Some(speed) = s.speed_mbps {
        out.push(point(
            host,
            format!("{pfx}/speed_mbps"),
            TelemetryValue::Gauge(speed as f64),
        ));
    }
    if let Some(duplex) = s.duplex {
        out.push(point(
            host,
            format!("{pfx}/duplex"),
            TelemetryValue::Text(duplex.label().to_string()),
        ));
        // A numeric/boolean companion so a generic metric-threshold expectation
        // can flag half-duplex without parsing text.
        out.push(point(
            host,
            format!("{pfx}/full_duplex"),
            TelemetryValue::Boolean(duplex == DuplexKind::Full),
        ));
    }
    if let Some(autoneg) = s.autoneg {
        out.push(point(
            host,
            format!("{pfx}/autoneg"),
            TelemetryValue::Boolean(autoneg),
        ));
    }
    let gauge = |out: &mut Vec<TelemetryPoint>, name: &str, v: Option<u32>| {
        if let Some(v) = v {
            out.push(point(
                host,
                format!("{pfx}/{name}"),
                TelemetryValue::Gauge(v as f64),
            ));
        }
    };
    gauge(&mut out, "rings/rx", s.rx_ring);
    gauge(&mut out, "rings/tx", s.tx_ring);
    gauge(&mut out, "rings/rx_max", s.rx_ring_max);
    gauge(&mut out, "rings/tx_max", s.tx_ring_max);
    if let Some(v) = s.pause_rx {
        out.push(point(
            host,
            format!("{pfx}/pause/rx"),
            TelemetryValue::Boolean(v),
        ));
    }
    if let Some(v) = s.pause_tx {
        out.push(point(
            host,
            format!("{pfx}/pause/tx"),
            TelemetryValue::Boolean(v),
        ));
    }
    if let Some(v) = s.pause_autoneg {
        out.push(point(
            host,
            format!("{pfx}/pause/autoneg"),
            TelemetryValue::Boolean(v),
        ));
    }
    if let Some(v) = s.pause_rx_frames {
        out.push(point(
            host,
            format!("{pfx}/pause/rx_frames"),
            TelemetryValue::Counter(v),
        ));
    }
    if let Some(v) = s.pause_tx_frames {
        out.push(point(
            host,
            format!("{pfx}/pause/tx_frames"),
            TelemetryValue::Counter(v),
        ));
    }
    for (name, active) in &s.features {
        out.push(point(
            host,
            format!("{pfx}/features/{name}"),
            TelemetryValue::Boolean(*active),
        ));
    }
    // FEC (nlink 0.23): configured mode(s) as text + an auto-negotiation flag.
    if let Some(modes) = &s.fec_modes {
        out.push(point(
            host,
            format!("{pfx}/fec/modes"),
            TelemetryValue::Text(modes.clone()),
        ));
    }
    if let Some(v) = s.fec_auto {
        out.push(point(
            host,
            format!("{pfx}/fec/auto"),
            TelemetryValue::Boolean(v),
        ));
    }
    // EEE (nlink 0.23): admin-enabled vs link-active power saving.
    if let Some(v) = s.eee_enabled {
        out.push(point(
            host,
            format!("{pfx}/eee/enabled"),
            TelemetryValue::Boolean(v),
        ));
    }
    if let Some(v) = s.eee_active {
        out.push(point(
            host,
            format!("{pfx}/eee/active"),
            TelemetryValue::Boolean(v),
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Address inventory (issue #10): low-cardinality summary streamed; per-address
// detail served via `@/query/addresses`.
// ---------------------------------------------------------------------------

/// One decoded address entry (nlink-free), the pure input to
/// [`crate::collector::aggregate_addresses`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddrEntry {
    /// `AF_*` family byte (`AF_INET` = 2, `AF_INET6` = 10).
    pub family: u8,
    /// Global (universe) scope — "actually reachable" beyond this host.
    pub global: bool,
}

/// Host-level IP address inventory summary.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AddressSummary {
    pub ipv4_count: u64,
    pub ipv6_count: u64,
    /// Addresses with global (universe) scope — "actually reachable".
    pub global_count: u64,
    pub total: u64,
}

/// Build telemetry points for the address inventory summary. Metric paths are
/// `addresses/{ipv4_count,ipv6_count,global_count,total}`.
pub fn address_points(host: &str, a: &AddressSummary) -> Vec<TelemetryPoint> {
    let g = |metric: &str, v: u64| point(host, metric, TelemetryValue::Gauge(v as f64));
    vec![
        g("addresses/ipv4_count", a.ipv4_count),
        g("addresses/ipv6_count", a.ipv6_count),
        g("addresses/global_count", a.global_count),
        g("addresses/total", a.total),
    ]
}

/// One configured IP address (served via `@/query/addresses`). Defined locally
/// (this sensor owns only its own crate); the GUI mirrors this JSON shape.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AddressRecord {
    /// IP family: 4 or 6.
    pub family: u8,
    pub ip: Option<String>,
    pub prefix_len: u8,
    /// Scope label: `global`/`site`/`link`/`host`/`nowhere`.
    pub scope: String,
    pub label: Option<String>,
    pub ifindex: u32,
}

// ---------------------------------------------------------------------------
// TC / QoS qdisc stats (issue #12): per-(iface,qdisc) aggregates streamed,
// bounded by the TC hierarchy; full tree served via `@/query/tc`.
// ---------------------------------------------------------------------------

/// A decoded TC qdisc snapshot (nlink-free), the pure input to [`tc_points`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TcQdiscSample {
    /// Interface name (label); the metric path uses it.
    pub iface: String,
    /// Qdisc kind, e.g. `fq_codel`, `htb`, `pfifo_fast`, `noqueue`.
    pub kind: String,
    /// Handle string, e.g. `8001:` (label only — bounded; kept off the path).
    pub handle: String,
    pub bytes: u64,
    pub packets: u64,
    pub drops: u64,
    pub overlimits: u64,
    pub requeues: u64,
    /// Backlog still queued, in bytes and packets (egress-congestion signal).
    pub backlog_bytes: u64,
    pub backlog_pkts: u64,
}

/// AQM classification of a qdisc `kind` (pure string match, #110). Reports whether
/// the qdisc does *active queue management* (the bufferbloat-relevant question):
///
/// * `aqm`     — fq_codel / cake / fq_pie / codel / pie: actively bounds latency
///   under load (AQM and/or fair-queueing). The healthy egress default.
/// * `fifo`    — pfifo_fast / pfifo / bfifo: a dumb drop-tail FIFO, bufferbloat-prone.
/// * `noqueue` — the kernel `noqueue` pseudo-qdisc (virtual / loopback-style links).
/// * `none`    — any other kind (e.g. htb, tbf, mq, prio): no AQM of its own. A
///   loaded link landing here is itself a finding (no AQM under load).
pub fn aqm_class(kind: &str) -> &'static str {
    match kind {
        "fq_codel" | "cake" | "fq_pie" | "codel" | "pie" => "aqm",
        "pfifo_fast" | "pfifo" | "bfifo" => "fifo",
        "noqueue" => "noqueue",
        _ => "none",
    }
}

/// Bufferbloat / qdisc health score in `0.0..=1.0` (1 = healthy), from a single
/// qdisc sample (#110).
///
/// The TC kernel stats are cumulative counters, and this pure `map` layer holds no
/// cross-poll state (the collector caches no prior TC sample), so the score uses
/// *instantaneous cumulative ratios* from one sample rather than per-poll rates.
/// This is a deliberate, documented proxy: it keeps the function pure and unit
/// testable, and the cumulative ratios are still defensible health signals (a qdisc
/// that has dropped 5% of everything it ever saw is unhealthy regardless of when).
/// It blends three penalties, each normalized to `0.0..=1.0` (0 = fine, 1 = worst):
///
/// * **drop penalty** (weight 0.5) — the dominant bufferbloat signal. Cumulative
///   drop fraction `drops / (packets + drops)`; a `DROP_FULL` (5%) drop fraction
///   saturates it. An idle qdisc (no packets, no drops) scores 0 here.
/// * **backlog penalty** (weight 0.3) — sustained queue depth, the classic
///   latency-under-load symptom. `backlog_pkts` normalized against
///   `BACKLOG_FULL_PKTS` (1000 packets queued == worst).
/// * **overlimit penalty** (weight 0.2) — shaping pressure. `overlimits / packets`
///   normalized against `OVERLIMIT_FULL` (10%). Overlimits are expected for shapers,
///   so this term is weighted lightest.
///
/// The weights sum to 1.0, so the blended penalty stays in `0.0..=1.0`;
/// `health_score = (1 - penalty)`, clamped.
pub fn tc_health_score(s: &TcQdiscSample) -> f64 {
    // Saturation thresholds: a term reaching its threshold yields full penalty.
    const DROP_FULL: f64 = 0.05; // 5% lifetime drop fraction == worst
    const BACKLOG_FULL_PKTS: f64 = 1000.0; // packets queued == worst
    const OVERLIMIT_FULL: f64 = 0.10; // 10% of packets over the shaper limit
    // Weights (sum to 1.0 so the blended penalty stays in 0.0..=1.0).
    const W_DROP: f64 = 0.5;
    const W_BACKLOG: f64 = 0.3;
    const W_OVERLIMIT: f64 = 0.2;

    let packets = s.packets as f64;
    let drops = s.drops as f64;
    let seen = packets + drops;
    let drop_frac = if seen > 0.0 { drops / seen } else { 0.0 };
    let drop_penalty = (drop_frac / DROP_FULL).clamp(0.0, 1.0);

    let backlog_penalty = (s.backlog_pkts as f64 / BACKLOG_FULL_PKTS).clamp(0.0, 1.0);

    let overlimit_ratio = if packets > 0.0 {
        s.overlimits as f64 / packets
    } else {
        0.0
    };
    let overlimit_penalty = (overlimit_ratio / OVERLIMIT_FULL).clamp(0.0, 1.0);

    let penalty =
        W_DROP * drop_penalty + W_BACKLOG * backlog_penalty + W_OVERLIMIT * overlimit_penalty;
    (1.0 - penalty).clamp(0.0, 1.0)
}

/// Build telemetry points for one qdisc (#12, #110). Metric paths are
/// `tc/<iface>/<kind>/<stat>`. Drops/overlimits/requeues are counters (monotonic
/// kernel stats); backlog is an instantaneous gauge. Additionally emits the derived
/// `tc/<iface>/<kind>/health_score` (Gauge 0..=1, 1 = healthy, see
/// [`tc_health_score`]) and `tc/<iface>/aqm_class` (Text, see [`aqm_class`]).
pub fn tc_points(host: &str, s: &TcQdiscSample) -> Vec<TelemetryPoint> {
    let pfx = format!("tc/{}/{}", s.iface, s.kind);
    let label = |p: TelemetryPoint| p.with_label("handle", s.handle.clone());
    vec![
        label(point(
            host,
            format!("{pfx}/drops"),
            TelemetryValue::Counter(s.drops),
        )),
        label(point(
            host,
            format!("{pfx}/overlimits"),
            TelemetryValue::Counter(s.overlimits),
        )),
        label(point(
            host,
            format!("{pfx}/requeues"),
            TelemetryValue::Counter(s.requeues),
        )),
        label(point(
            host,
            format!("{pfx}/bytes"),
            TelemetryValue::Counter(s.bytes),
        )),
        label(point(
            host,
            format!("{pfx}/packets"),
            TelemetryValue::Counter(s.packets),
        )),
        label(point(
            host,
            format!("{pfx}/backlog_bytes"),
            TelemetryValue::Gauge(s.backlog_bytes as f64),
        )),
        label(point(
            host,
            format!("{pfx}/backlog_pkts"),
            TelemetryValue::Gauge(s.backlog_pkts as f64),
        )),
        // Derived bufferbloat health score (#110): 0..=1, 1 = healthy.
        label(point(
            host,
            format!("{pfx}/health_score"),
            TelemetryValue::Gauge(tc_health_score(s)),
        )),
        // AQM classification of the qdisc kind (#110). Path omits `<kind>` (per the
        // issue); the `kind` label disambiguates multiple qdiscs on one iface.
        label(point(
            host,
            format!("tc/{}/aqm_class", s.iface),
            TelemetryValue::Text(aqm_class(&s.kind).to_string()),
        ))
        .with_label("kind", s.kind.clone()),
    ]
}

/// One TC qdisc/class entry (served via `@/query/tc`). The GUI mirrors this JSON
/// shape. `node` is `qdisc` or `class`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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

// ---------------------------------------------------------------------------
// XFRM / IPsec SA health (issue #13): low-cardinality summary streamed; per-SA
// detail served via `@/query/xfrm`.
// ---------------------------------------------------------------------------

/// One decoded XFRM Security Association fact (nlink-free), the pure input to
/// [`crate::collector::aggregate_xfrm`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XfrmSaEntry {
    /// `tunnel`/`transport`/`beet`/`other`.
    pub mode: String,
    /// `esp`/`ah`/`comp`/`other`.
    pub proto: String,
}

/// Host-level IPsec/XFRM summary (SA counts by mode/proto + policy total).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct XfrmSummary {
    pub sa_total: u64,
    /// SA counts keyed by mode (bounded: tunnel/transport/beet/other).
    pub sa_by_mode: HashMap<String, u64>,
    /// SA counts keyed by IPsec protocol (bounded: esp/ah/comp/other).
    pub sa_by_proto: HashMap<String, u64>,
    pub policy_total: u64,
}

/// Build telemetry points for the XFRM/IPsec summary (#13). Metric paths are
/// `xfrm/sa/total`, `xfrm/sa/by_mode/<mode>`, `xfrm/sa/by_proto/<proto>`,
/// `xfrm/policy/total`.
pub fn xfrm_points(host: &str, x: &XfrmSummary) -> Vec<TelemetryPoint> {
    let g = |metric: String, v: u64| point(host, metric, TelemetryValue::Gauge(v as f64));
    let mut out = vec![
        g("xfrm/sa/total".into(), x.sa_total),
        g("xfrm/policy/total".into(), x.policy_total),
    ];
    for (mode, n) in &x.sa_by_mode {
        out.push(g(format!("xfrm/sa/by_mode/{mode}"), *n));
    }
    for (proto, n) in &x.sa_by_proto {
        out.push(g(format!("xfrm/sa/by_proto/{proto}"), *n));
    }
    out
}

/// One IPsec Security Association (served via `@/query/xfrm`). GUI mirrors shape.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

// ---------------------------------------------------------------------------
// nftables rule counters (issues #14, #115): per-table chain/rule counts plus the
// per-rule packet/byte counters that are the real value of nft telemetry (firewall
// hit-rate). Full table/chain/rule inventory served via `@/query/nft`.
//
// The kernel exposes per-rule counters only as a `counter` *expression* inside the
// raw `NFTA_RULE_EXPRESSIONS` blob (nlink's `RuleInfo::expression_bytes`), so #115
// decodes that blob — see [`decode_nft_counter`] — rather than reporting counts
// only.
// ---------------------------------------------------------------------------

// netlink TLV / nftables UAPI constants used by the counter decoder (#115). These
// are stable kernel ABI values; defined locally so the decode is self-contained.
const NLA_TYPE_MASK: u16 = 0x3fff; // strip NLA_F_NESTED (0x8000) | NLA_F_NET_BYTEORDER (0x4000)
const NFTA_LIST_ELEM: u16 = 1;
const NFTA_EXPR_NAME: u16 = 1;
const NFTA_EXPR_DATA: u16 = 2;
const NFTA_COUNTER_BYTES: u16 = 1;
const NFTA_COUNTER_PACKETS: u16 = 2;

/// A decoded per-rule firewall counter (#115): cumulative packets/bytes matched.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NftCounter {
    pub packets: u64,
    pub bytes: u64,
}

/// Iterate netlink TLV attributes in `buf`, yielding `(type & NLA_TYPE_MASK,
/// payload)`. Header is host byte order (the kernel emits native-endian nla_len /
/// nla_type); the payload is unpadded and the cursor advances on the 4-byte
/// `NLA_ALIGNTO` boundary. Stops on any malformed/truncated attribute.
fn nl_attrs(buf: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
    let mut rest = buf;
    std::iter::from_fn(move || {
        if rest.len() < 4 {
            return None;
        }
        let len = u16::from_ne_bytes([rest[0], rest[1]]) as usize;
        let typ = u16::from_ne_bytes([rest[2], rest[3]]);
        if len < 4 || len > rest.len() {
            return None;
        }
        let payload = &rest[4..len];
        let advance = (len + 3) & !3; // align to NLA_ALIGNTO
        rest = if advance >= rest.len() {
            &[]
        } else {
            &rest[advance..]
        };
        Some((typ & NLA_TYPE_MASK, payload))
    })
}

/// Read a NUL-terminated netlink string attribute payload.
fn nl_attr_str(payload: &[u8]) -> &str {
    let end = payload
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(payload.len());
    std::str::from_utf8(&payload[..end]).unwrap_or("")
}

/// Decode the `counter` expression from a rule's raw `NFTA_RULE_EXPRESSIONS`
/// payload (#115). `expression_bytes` is a list of `NFTA_LIST_ELEM` attributes,
/// one per expression; a counter expression carries `NFTA_COUNTER_{BYTES,PACKETS}`
/// as big-endian `u64`s inside its `NFTA_EXPR_DATA`. Returns the summed counter
/// (rules normally have at most one), or `None` if the rule has no counter
/// statement — distinguishing "no counter" from "counter reading zero".
pub fn decode_nft_counter(expression_bytes: &[u8]) -> Option<NftCounter> {
    let mut found = false;
    let mut acc = NftCounter::default();
    for (etype, elem) in nl_attrs(expression_bytes) {
        if etype != NFTA_LIST_ELEM {
            continue;
        }
        let mut name = "";
        let mut data: &[u8] = &[];
        for (atype, payload) in nl_attrs(elem) {
            match atype {
                NFTA_EXPR_NAME => name = nl_attr_str(payload),
                NFTA_EXPR_DATA => data = payload,
                _ => {}
            }
        }
        if name == "counter" {
            for (ctype, payload) in nl_attrs(data) {
                if payload.len() >= 8 {
                    let v = u64::from_be_bytes(payload[..8].try_into().unwrap());
                    match ctype {
                        NFTA_COUNTER_BYTES => acc.bytes = acc.bytes.saturating_add(v),
                        NFTA_COUNTER_PACKETS => acc.packets = acc.packets.saturating_add(v),
                        _ => {}
                    }
                }
            }
            found = true;
        }
    }
    found.then_some(acc)
}

/// One nftables table's shape + traffic (nlink-free), pure input to [`nft_points`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NftTableSample {
    pub family: String,
    pub table: String,
    pub chains: u64,
    pub rules: u64,
    /// Summed packets across this table's counter rules (#115).
    pub packets: u64,
    /// Summed bytes across this table's counter rules (#115).
    pub bytes: u64,
}

/// Host-level nftables summary across all tables.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NftSummary {
    pub tables: Vec<NftTableSample>,
    pub tables_total: u64,
    pub chains_total: u64,
    pub rules_total: u64,
    /// Host-wide firewall hit counters across all counter rules (#115).
    pub packets_total: u64,
    pub bytes_total: u64,
}

/// Build telemetry points for the nftables summary (#14, #115). Counts are gauges
/// (`nft/{tables,chains,rules}_total`, per-table `nft/<family>/<table>/{chains,
/// rules}`); the decoded firewall traffic is monotonic counters
/// (`nft/{packets,bytes}_total`, per-table `.../{packets,bytes}`) so exporters and
/// charts derive a hit-rate.
pub fn nft_points(host: &str, s: &NftSummary) -> Vec<TelemetryPoint> {
    let g = |metric: String, v: u64| point(host, metric, TelemetryValue::Gauge(v as f64));
    let c = |metric: String, v: u64| point(host, metric, TelemetryValue::Counter(v));
    let mut out = vec![
        g("nft/tables_total".into(), s.tables_total),
        g("nft/chains_total".into(), s.chains_total),
        g("nft/rules_total".into(), s.rules_total),
        c("nft/packets_total".into(), s.packets_total),
        c("nft/bytes_total".into(), s.bytes_total),
    ];
    for t in &s.tables {
        let pfx = format!("nft/{}/{}", t.family, t.table);
        out.push(g(format!("{pfx}/chains"), t.chains));
        out.push(g(format!("{pfx}/rules"), t.rules));
        out.push(c(format!("{pfx}/packets"), t.packets));
        out.push(c(format!("{pfx}/bytes"), t.bytes));
    }
    out
}

/// One nftables rule (served via `@/query/nft`). GUI mirrors this JSON shape.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NftRuleRecord {
    pub family: String,
    pub table: String,
    pub chain: String,
    pub handle: u64,
    pub comment: Option<String>,
    /// Per-rule firewall counter (#115); `0` when the rule has no `counter`
    /// statement. `#[serde(default)]` keeps older sensors wire-compatible.
    #[serde(default)]
    pub packets: u64,
    #[serde(default)]
    pub bytes: u64,
}

// =============================================================================
// eBPF module (#114) — pure mapping for connlat / retransmits / tcplife.
//
// Feature-independent so the percentile/top-K/record math unit-tests on stable
// with no kernel. The `ebpf` userspace (ebpf.rs) reads the BPF maps/ring buffer
// and calls these to build telemetry + query replies.
// =============================================================================

use std::net::{Ipv4Addr, Ipv6Addr};

use zensight_sensor_netlink_ebpf_common::{ConnRecord, RetransKey};

/// AF_INET6 raw family byte (matches the kernel-side constant).
const AF_INET6: u8 = 10;

/// Format raw address bytes by family into an IP string (`family` is the raw
/// `AF_*` byte: 10 = IPv6, anything else treated as IPv4 over the first 4 bytes).
fn fmt_addr(family: u8, addr: &[u8; 16]) -> String {
    if family == AF_INET6 {
        Ipv6Addr::from(*addr).to_string()
    } else {
        Ipv4Addr::new(addr[0], addr[1], addr[2], addr[3]).to_string()
    }
}

/// Friendly `4`/`6` from a raw `AF_*` family byte (0 if neither).
fn fam_digit(family: u8) -> u8 {
    match family {
        2 => 4,
        10 => 6,
        _ => 0,
    }
}

/// Decode a null-padded `comm[16]` into a `String`.
fn comm_str(comm: &[u8; 16]) -> String {
    let end = comm.iter().position(|&b| b == 0).unwrap_or(comm.len());
    String::from_utf8_lossy(&comm[..end]).into_owned()
}

/// Nearest-rank p50/p95 (µs) over a log2 connect-latency histogram. Returns the
/// representative bucket upper bound; `(0, 0)` for an empty histogram.
pub fn connlat_percentiles(hist: &[u64]) -> (u64, u64) {
    let total: u64 = hist.iter().sum();
    if total == 0 {
        return (0, 0);
    }
    let pct = |q: f64| -> u64 {
        let rank = ((q * total as f64).ceil() as u64).clamp(1, total);
        let mut cum = 0u64;
        for (i, &c) in hist.iter().enumerate() {
            cum += c;
            if cum >= rank {
                return if i == 0 { 1 } else { 1u64 << i };
            }
        }
        if hist.is_empty() {
            0
        } else {
            1u64 << (hist.len() - 1)
        }
    };
    (pct(0.50), pct(0.95))
}

/// Connect-latency gauges, routed through the normal publish path so the
/// sentinel's metric-threshold expectations can watch them. Both are omitted
/// when zero (matching the rtt/delivery-rate "omit-on-zero" policy).
pub fn connlat_points(host: &str, p50_us: u64, p95_us: u64) -> Vec<TelemetryPoint> {
    let mut out = Vec::new();
    if p50_us > 0 || p95_us > 0 {
        out.push(point(
            host,
            "sockets/tcp/connlat_us_p50",
            TelemetryValue::Gauge(p50_us as f64),
        ));
        out.push(point(
            host,
            "sockets/tcp/connlat_us_p95",
            TelemetryValue::Gauge(p95_us as f64),
        ));
    }
    out
}

/// A per-peer retransmit count (`@/query/retransmits`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RetransRecord {
    pub peer: String,
    pub family: u8,
    pub count: u64,
}

/// Bounded top-K peers by retransmit count (count desc, stable peer tiebreak).
pub fn top_k_retransmits(snapshot: &[(RetransKey, u64)], k: usize) -> Vec<RetransRecord> {
    let mut recs: Vec<RetransRecord> = snapshot
        .iter()
        .map(|(key, count)| RetransRecord {
            peer: fmt_addr(key.family, &key.addr),
            family: fam_digit(key.family),
            count: *count,
        })
        .collect();
    recs.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.peer.cmp(&b.peer)));
    recs.truncate(k);
    recs
}

/// A completed-connection record (tcplife, `@/query/connections`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ConnView {
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

impl ConnView {
    /// Build a wire record from a kernel `ConnRecord` (pure, testable).
    pub fn from_record(r: &ConnRecord) -> Self {
        Self {
            pid: r.pid,
            comm: comm_str(&r.comm),
            family: fam_digit(r.family),
            local: fmt_addr(r.family, &r.saddr),
            lport: r.sport,
            remote: fmt_addr(r.family, &r.daddr),
            rport: r.dport,
            duration_ms: r.duration_ns / 1_000_000,
            tx_bytes: r.tx_bytes,
            rx_bytes: r.rx_bytes,
            segs_out: r.segs_out,
            segs_in: r.segs_in,
            retrans: r.retrans,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> IfaceSample {
        IfaceSample {
            name: "eth0".into(),
            ifindex: 2,
            up: true,
            carrier: Some(true),
            mtu: Some(1500),
            mac: Some("aa:bb:cc:dd:ee:ff".into()),
            oper_state: Some("up".into()),
            rx_bytes: 1000,
            tx_bytes: 2000,
            rx_packets: 10,
            tx_packets: 20,
            rx_errors: 1,
            tx_errors: 0,
            rx_dropped: 3,
            tx_dropped: 0,
            multicast: 7,
            collisions: 0,
        }
    }

    #[test]
    fn route_points_shape() {
        let r = RouteSummary {
            ipv4_count: 5,
            ipv6_count: 2,
            total: 7,
            default_v4_present: true,
            default_v6_present: false,
            default_v4_gw: Some("10.0.0.1".into()),
        };
        let pts = route_points("h", &r);
        let find = |m: &str| pts.iter().find(|p| p.metric == m).unwrap();
        assert_eq!(find("routes/ipv4_count").value, TelemetryValue::Gauge(5.0));
        assert_eq!(
            find("routes/default_v4_present").value,
            TelemetryValue::Boolean(true)
        );
        assert_eq!(
            find("routes/default_v4_gw").value,
            TelemetryValue::Text("10.0.0.1".into())
        );
    }

    #[test]
    fn wireguard_points_shape() {
        let peers = vec![
            WgPeerView {
                id: "AbCd1234".into(),
                endpoint: Some("203.0.113.5:51820".into()),
                handshake_age_s: Some(30),
                rx_bytes: 1000,
                tx_bytes: 2000,
                allowed_ips: Some("10.8.0.2/32".into()),
            },
            WgPeerView {
                id: "Zz99".into(),
                endpoint: None,
                handshake_age_s: None, // never handshaked → down
                rx_bytes: 0,
                tx_bytes: 0,
                allowed_ips: None,
            },
        ];
        let pts = wireguard_points("h", "wg0", &peers, 180);
        let find = |m: &str| pts.iter().find(|p| p.metric == m).unwrap();
        assert_eq!(
            find("wireguard/wg0/peers").value,
            TelemetryValue::Gauge(2.0)
        );
        assert_eq!(
            find("wireguard/wg0/AbCd1234/rx_bytes").value,
            TelemetryValue::Counter(1000)
        );
        assert_eq!(
            find("wireguard/wg0/AbCd1234/up").value,
            TelemetryValue::Boolean(true)
        );
        assert_eq!(
            find("wireguard/wg0/Zz99/up").value,
            TelemetryValue::Boolean(false)
        );
        // The never-handshaked peer has no age point.
        assert!(
            pts.iter()
                .all(|p| p.metric != "wireguard/wg0/Zz99/last_handshake_age_s")
        );
        // wg-quick AllowedIPs enrichment (#268) rides on the peer's rx_bytes point.
        assert_eq!(
            find("wireguard/wg0/AbCd1234/rx_bytes")
                .labels
                .get("allowed_ips")
                .map(String::as_str),
            Some("10.8.0.2/32")
        );
    }

    #[test]
    fn conntrack_points_shape() {
        let c = ConntrackSummary {
            total: 1500,
            tcp: 1000,
            udp: 400,
            icmp: 50,
            other: 50,
            max: Some(2000),
        };
        let pts = conntrack_points("h", &c);
        let find = |m: &str| pts.iter().find(|p| p.metric == m).unwrap();
        assert_eq!(
            find("conntrack/entries").value,
            TelemetryValue::Gauge(1500.0)
        );
        assert_eq!(
            find("conntrack/by_proto/tcp").value,
            TelemetryValue::Gauge(1000.0)
        );
        assert_eq!(find("conntrack/max").value, TelemetryValue::Gauge(2000.0));
        assert_eq!(
            find("conntrack/utilization").value,
            TelemetryValue::Gauge(0.75)
        );
        // No max → no max/utilization points.
        let c2 = ConntrackSummary {
            total: 10,
            max: None,
            ..Default::default()
        };
        let pts2 = conntrack_points("h", &c2);
        assert!(pts2.iter().all(|p| p.metric != "conntrack/utilization"));
    }

    #[test]
    fn neighbor_points_shape() {
        let n = NeighborSummary {
            reachable: 3,
            stale: 1,
            failed: 2,
            total: 6,
            ..Default::default()
        };
        let pts = neighbor_points("h", &n);
        let find = |m: &str| pts.iter().find(|p| p.metric == m).unwrap();
        assert_eq!(
            find("neighbors/by_state/reachable").value,
            TelemetryValue::Gauge(3.0)
        );
        assert_eq!(
            find("neighbors/by_state/failed").value,
            TelemetryValue::Gauge(2.0)
        );
        assert_eq!(find("neighbors/total").value, TelemetryValue::Gauge(6.0));
    }

    #[test]
    fn iface_points_cover_counters_and_state() {
        let pts = iface_points("host1", &sample());
        let find = |m: &str| pts.iter().find(|p| p.metric == m);
        assert_eq!(
            find("iface/eth0/rx_bytes").unwrap().value,
            TelemetryValue::Counter(1000)
        );
        assert_eq!(
            find("iface/eth0/up").unwrap().value,
            TelemetryValue::Boolean(true)
        );
        assert_eq!(
            find("iface/eth0/mtu").unwrap().value,
            TelemetryValue::Gauge(1500.0)
        );
        // Every point is sourced + labelled with the interface index.
        for p in &pts {
            assert_eq!(p.source, "host1");
            assert_eq!(p.protocol, Protocol::Netlink);
            assert_eq!(p.labels.get("ifindex").map(String::as_str), Some("2"));
        }
    }

    #[test]
    fn iface_points_omit_absent_optionals() {
        let mut s = sample();
        s.carrier = None;
        s.mtu = None;
        s.mac = None;
        let pts = iface_points("h", &s);
        assert!(pts.iter().all(|p| p.metric != "iface/eth0/carrier"));
        assert!(pts.iter().all(|p| p.metric != "iface/eth0/mtu"));
        assert!(pts.iter().all(|p| p.metric != "iface/eth0/info"));
    }

    #[test]
    fn socket_points_shape() {
        let mut by_cong = HashMap::new();
        by_cong.insert("cubic".to_string(), 4u64);
        by_cong.insert("bbr".to_string(), 1u64);
        let c = SocketCounts {
            established: 5,
            listen: 3,
            retransmits_total: 12,
            max_rtt_us: 400,
            by_cong,
            snd_buf_total: 1_000_000,
            rcv_buf_total: 2_000_000,
            // Enriched tcp_info (#108).
            delivery_rate_p50: 5_000_000,
            delivery_rate_p95: 40_000_000,
            pacing_rate_p50: 12_000_000,
            pacing_rate_p95: 60_000_000,
            rcv_rtt_p50_us: 300,
            rcv_rtt_p95_us: 900,
            bytes_retrans_total: 4096,
            total_retrans_total: 17,
            reordered_total: 3,
            lost_total: 2,
            ..Default::default()
        };
        let pts = socket_points("h", &c);
        let find = |m: &str| pts.iter().find(|p| p.metric == m).unwrap();
        assert_eq!(
            find("sockets/tcp/established").value,
            TelemetryValue::Gauge(5.0)
        );
        assert_eq!(
            find("sockets/tcp/retransmits_total").value,
            TelemetryValue::Counter(12)
        );
        // #108: enriched delivery-health metrics.
        assert_eq!(
            find("sockets/tcp/delivery_rate_p50").value,
            TelemetryValue::Gauge(5_000_000.0)
        );
        assert_eq!(
            find("sockets/tcp/pacing_rate_p95").value,
            TelemetryValue::Gauge(60_000_000.0)
        );
        assert_eq!(
            find("sockets/tcp/rcv_rtt_p50_us").value,
            TelemetryValue::Gauge(300.0)
        );
        assert_eq!(
            find("sockets/tcp/bytes_retrans_total").value,
            TelemetryValue::Counter(4096)
        );
        assert_eq!(
            find("sockets/tcp/total_retrans_total").value,
            TelemetryValue::Counter(17)
        );
        assert_eq!(
            find("sockets/tcp/reordered_total").value,
            TelemetryValue::Counter(3)
        );
        assert_eq!(
            find("sockets/tcp/lost_total").value,
            TelemetryValue::Gauge(2.0)
        );
        // #11: per-algorithm counts + buffer totals.
        assert_eq!(
            find("sockets/tcp/by_cong/cubic").value,
            TelemetryValue::Gauge(4.0)
        );
        assert_eq!(
            find("sockets/tcp/by_cong/bbr").value,
            TelemetryValue::Gauge(1.0)
        );
        assert_eq!(
            find("sockets/tcp/mem/snd_buf_total").value,
            TelemetryValue::Gauge(1_000_000.0)
        );
        assert_eq!(
            find("sockets/tcp/mem/rcv_buf_total").value,
            TelemetryValue::Gauge(2_000_000.0)
        );
    }

    #[test]
    fn socket_points_omit_buffers_when_absent() {
        // No mem info / no congestion → no by_cong or mem points (no zeros).
        let c = SocketCounts {
            established: 1,
            ..Default::default()
        };
        let pts = socket_points("h", &c);
        assert!(
            pts.iter()
                .all(|p| !p.metric.starts_with("sockets/tcp/mem/"))
        );
        assert!(
            pts.iter()
                .all(|p| !p.metric.starts_with("sockets/tcp/by_cong/"))
        );
        // #108: delivery/pacing/rcv-rtt percentiles omitted when 0 (no clobbering).
        assert!(pts.iter().all(|p| {
            !matches!(
                p.metric.as_str(),
                "sockets/tcp/delivery_rate_p50"
                    | "sockets/tcp/pacing_rate_p50"
                    | "sockets/tcp/rcv_rtt_p50_us"
            )
        }));
        // But the always-on retrans/lost counters ARE present (even at 0).
        assert!(
            pts.iter()
                .any(|p| p.metric == "sockets/tcp/bytes_retrans_total")
        );
    }

    #[test]
    fn diagnostics_points_shape() {
        // No bottleneck → no bottleneck point, score 0.
        let clean = DiagnosticsSummary {
            issues_info: 2,
            issues_warning: 1,
            ..Default::default()
        };
        let pts = diagnostics_points("h", &clean);
        let find = |m: &str| pts.iter().find(|p| p.metric == m);
        assert_eq!(
            find("diagnostics/issues/info").unwrap().value,
            TelemetryValue::Gauge(2.0)
        );
        assert_eq!(
            find("diagnostics/issues/total").unwrap().value,
            TelemetryValue::Gauge(3.0)
        );
        assert_eq!(
            find("diagnostics/bottleneck_score").unwrap().value,
            TelemetryValue::Gauge(0.0)
        );
        assert!(find("diagnostics/bottleneck").is_none());

        // With a bottleneck → Text point carrying location/recommendation labels.
        let busy = DiagnosticsSummary {
            issues_critical: 1,
            bottleneck_score: 0.82,
            bottleneck_location: Some("eth0 egress qdisc".into()),
            bottleneck_type: Some("Qdisc Drops".into()),
            bottleneck_recommendation: Some("increase txqueuelen".into()),
            bottleneck_drop_rate: 0.03,
            ..Default::default()
        };
        let pts = diagnostics_points("h", &busy);
        let b = pts
            .iter()
            .find(|p| p.metric == "diagnostics/bottleneck")
            .unwrap();
        assert_eq!(b.value, TelemetryValue::Text("Qdisc Drops".into()));
        assert_eq!(
            b.labels.get("location").map(String::as_str),
            Some("eth0 egress qdisc")
        );
        assert_eq!(
            b.labels.get("recommendation").map(String::as_str),
            Some("increase txqueuelen")
        );
    }

    fn sock(local: &str, remote: &str, state: &str) -> SocketRecord {
        SocketRecord {
            local: local.into(),
            remote: remote.into(),
            state: state.into(),
            uid: 0,
            recv_q: 0,
            send_q: 0,
            rtt_us: 0,
            retrans: 0,
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

    #[test]
    fn socket_selector_parse() {
        let s = SocketSelector::parse("state=Established&port=22");
        assert_eq!(s.state.as_deref(), Some("established"));
        assert_eq!(s.port, Some(22));

        // Empty / partial / junk.
        assert_eq!(SocketSelector::parse(""), SocketSelector::default());
        assert_eq!(SocketSelector::parse("port=notnum").port, None);
        assert_eq!(SocketSelector::parse("foo=bar"), SocketSelector::default());
    }

    #[test]
    fn socket_selector_matches() {
        let rec = sock("10.0.0.1:5555", "1.1.1.1:22", "established");
        // No filter → matches.
        assert!(SocketSelector::default().matches(&rec));
        // State filter is case-insensitive.
        assert!(SocketSelector::parse("state=ESTABLISHED").matches(&rec));
        assert!(!SocketSelector::parse("state=listen").matches(&rec));
        // Port matches either endpoint (remote here).
        assert!(SocketSelector::parse("port=22").matches(&rec));
        // Port matches local endpoint.
        assert!(SocketSelector::parse("port=5555").matches(&rec));
        // Non-matching port (and not a substring false-positive on :555).
        assert!(!SocketSelector::parse("port=555").matches(&rec));
        // Combined: state AND port must both hold.
        assert!(SocketSelector::parse("state=established&port=22").matches(&rec));
        assert!(!SocketSelector::parse("state=listen&port=22").matches(&rec));
    }

    #[test]
    fn ethtool_points_present_and_absent() {
        let s = EthtoolSample {
            iface: "eth0".into(),
            carrier: Some(true),
            speed_mbps: Some(1000),
            duplex: Some(DuplexKind::Full),
            autoneg: Some(true),
            rx_ring: Some(256),
            tx_ring: Some(256),
            rx_ring_max: Some(4096),
            tx_ring_max: Some(4096),
            pause_rx: Some(true),
            pause_tx: None,
            pause_autoneg: None,
            pause_rx_frames: Some(7),
            pause_tx_frames: None,
            features: vec![("tso".into(), true), ("gro".into(), false)],
            fec_modes: Some("RS".into()),
            fec_auto: Some(true),
            eee_enabled: Some(true),
            eee_active: Some(false),
        };
        let pts = ethtool_points("h", &s);
        let find = |m: &str| pts.iter().find(|p| p.metric == m);
        assert_eq!(
            find("ethtool/eth0/speed_mbps").unwrap().value,
            TelemetryValue::Gauge(1000.0)
        );
        assert_eq!(
            find("ethtool/eth0/duplex").unwrap().value,
            TelemetryValue::Text("full".into())
        );
        assert_eq!(
            find("ethtool/eth0/full_duplex").unwrap().value,
            TelemetryValue::Boolean(true)
        );
        assert_eq!(
            find("ethtool/eth0/rings/rx_max").unwrap().value,
            TelemetryValue::Gauge(4096.0)
        );
        assert_eq!(
            find("ethtool/eth0/pause/rx_frames").unwrap().value,
            TelemetryValue::Counter(7)
        );
        assert_eq!(
            find("ethtool/eth0/features/tso").unwrap().value,
            TelemetryValue::Boolean(true)
        );
        assert_eq!(
            find("ethtool/eth0/fec/modes").unwrap().value,
            TelemetryValue::Text("RS".into())
        );
        assert_eq!(
            find("ethtool/eth0/eee/active").unwrap().value,
            TelemetryValue::Boolean(false)
        );
        // Absent optionals produce no point (no misleading zeros).
        assert!(find("ethtool/eth0/pause/tx").is_none());
        assert!(find("ethtool/eth0/pause/tx_frames").is_none());

        // A NIC exposing nothing yields no points at all.
        let empty = EthtoolSample {
            iface: "lo".into(),
            ..Default::default()
        };
        assert!(ethtool_points("h", &empty).is_empty());
    }

    #[test]
    fn address_points_shape() {
        let a = AddressSummary {
            ipv4_count: 3,
            ipv6_count: 2,
            global_count: 4,
            total: 5,
        };
        let pts = address_points("h", &a);
        let find = |m: &str| pts.iter().find(|p| p.metric == m).unwrap();
        assert_eq!(
            find("addresses/ipv4_count").value,
            TelemetryValue::Gauge(3.0)
        );
        assert_eq!(
            find("addresses/global_count").value,
            TelemetryValue::Gauge(4.0)
        );
        assert_eq!(find("addresses/total").value, TelemetryValue::Gauge(5.0));
    }

    #[test]
    fn tc_points_shape() {
        let s = TcQdiscSample {
            iface: "eth0".into(),
            kind: "fq_codel".into(),
            handle: "8001:".into(),
            bytes: 5000,
            packets: 40,
            drops: 7,
            overlimits: 2,
            requeues: 1,
            backlog_bytes: 1448,
            backlog_pkts: 1,
        };
        let pts = tc_points("h", &s);
        let find = |m: &str| pts.iter().find(|p| p.metric == m).unwrap();
        assert_eq!(
            find("tc/eth0/fq_codel/drops").value,
            TelemetryValue::Counter(7)
        );
        assert_eq!(
            find("tc/eth0/fq_codel/overlimits").value,
            TelemetryValue::Counter(2)
        );
        assert_eq!(
            find("tc/eth0/fq_codel/backlog_bytes").value,
            TelemetryValue::Gauge(1448.0)
        );
        assert_eq!(
            find("tc/eth0/fq_codel/backlog_pkts").value,
            TelemetryValue::Gauge(1.0)
        );
        // Every point carries the qdisc handle label.
        for p in &pts {
            assert_eq!(p.labels.get("handle").map(String::as_str), Some("8001:"));
        }
        // #110: the derived health_score Gauge and aqm_class Text are emitted.
        assert!(matches!(
            find("tc/eth0/fq_codel/health_score").value,
            TelemetryValue::Gauge(_)
        ));
        let aqm = find("tc/eth0/aqm_class");
        assert_eq!(aqm.value, TelemetryValue::Text("aqm".into()));
        assert_eq!(aqm.labels.get("kind").map(String::as_str), Some("fq_codel"));
        // Raw counters preserved alongside the derived signals (additive).
        assert_eq!(
            find("tc/eth0/fq_codel/packets").value,
            TelemetryValue::Counter(40)
        );
    }

    #[test]
    fn aqm_class_maps_kinds() {
        // Active queue management.
        for k in ["fq_codel", "cake", "fq_pie", "codel", "pie"] {
            assert_eq!(aqm_class(k), "aqm", "{k} should be aqm");
        }
        // Dumb FIFOs.
        for k in ["pfifo_fast", "pfifo", "bfifo"] {
            assert_eq!(aqm_class(k), "fifo", "{k} should be fifo");
        }
        // The noqueue pseudo-qdisc.
        assert_eq!(aqm_class("noqueue"), "noqueue");
        // Everything else (shapers/classful) has no AQM of its own.
        for k in ["htb", "tbf", "mq", "prio", "ingress", ""] {
            assert_eq!(aqm_class(k), "none", "{k} should be none");
        }
    }

    #[test]
    fn tc_health_score_clean_fq_codel_scores_high() {
        // Clean AQM under light load: tiny drop fraction, ~empty backlog, no
        // overlimits => should be near 1.0.
        let s = TcQdiscSample {
            iface: "eth0".into(),
            kind: "fq_codel".into(),
            handle: "8001:".into(),
            bytes: 10_000_000,
            packets: 100_000,
            drops: 5, // 0.005% drop fraction
            overlimits: 0,
            requeues: 0,
            backlog_bytes: 1448,
            backlog_pkts: 1,
        };
        let score = tc_health_score(&s);
        assert!(
            score > 0.99,
            "clean fq_codel scored {score}, expected > 0.99"
        );
        assert!((0.0..=1.0).contains(&score));
    }

    #[test]
    fn tc_health_score_idle_qdisc_is_healthy() {
        // No traffic at all: no drops, no backlog => fully healthy (score 1.0),
        // not a divide-by-zero NaN.
        let s = TcQdiscSample {
            iface: "eth0".into(),
            kind: "pfifo_fast".into(),
            ..Default::default()
        };
        assert_eq!(tc_health_score(&s), 1.0);
    }

    #[test]
    fn tc_health_score_congested_fifo_scores_low() {
        // A dumb FIFO that has dropped >10% of traffic, with a deep sustained
        // backlog and heavy overlimits => penalties saturate => near 0.0.
        let s = TcQdiscSample {
            iface: "eth0".into(),
            kind: "pfifo_fast".into(),
            handle: "0:".into(),
            bytes: 100_000_000,
            packets: 100_000,
            drops: 50_000,      // 33% drop fraction (>> 5% threshold)
            overlimits: 50_000, // 50% overlimit ratio (>> 10% threshold)
            requeues: 0,
            backlog_bytes: 4_000_000,
            backlog_pkts: 5_000, // >> 1000-pkt threshold
        };
        let score = tc_health_score(&s);
        assert!(
            score < 0.05,
            "congested fifo scored {score}, expected < 0.05"
        );
        assert!((0.0..=1.0).contains(&score));
    }

    #[test]
    fn tc_health_score_is_monotonic_in_drops() {
        // More drops (all else equal) must not raise the score.
        let base = TcQdiscSample {
            iface: "eth0".into(),
            kind: "fq_codel".into(),
            packets: 100_000,
            ..Default::default()
        };
        let mut worse = base.clone();
        worse.drops = 2_000; // 2% drop fraction
        assert!(tc_health_score(&worse) < tc_health_score(&base));
    }

    #[test]
    fn xfrm_points_shape() {
        let mut by_mode = HashMap::new();
        by_mode.insert("tunnel".to_string(), 2u64);
        let mut by_proto = HashMap::new();
        by_proto.insert("esp".to_string(), 2u64);
        let x = XfrmSummary {
            sa_total: 2,
            sa_by_mode: by_mode,
            sa_by_proto: by_proto,
            policy_total: 4,
        };
        let pts = xfrm_points("h", &x);
        let find = |m: &str| pts.iter().find(|p| p.metric == m).unwrap();
        assert_eq!(find("xfrm/sa/total").value, TelemetryValue::Gauge(2.0));
        assert_eq!(
            find("xfrm/sa/by_mode/tunnel").value,
            TelemetryValue::Gauge(2.0)
        );
        assert_eq!(
            find("xfrm/sa/by_proto/esp").value,
            TelemetryValue::Gauge(2.0)
        );
        assert_eq!(find("xfrm/policy/total").value, TelemetryValue::Gauge(4.0));
    }

    #[test]
    fn nft_points_shape() {
        let s = NftSummary {
            tables: vec![
                NftTableSample {
                    family: "inet".into(),
                    table: "filter".into(),
                    chains: 3,
                    rules: 12,
                    packets: 100,
                    bytes: 6400,
                },
                NftTableSample {
                    family: "ip".into(),
                    table: "nat".into(),
                    chains: 2,
                    rules: 4,
                    packets: 0,
                    bytes: 0,
                },
            ],
            tables_total: 2,
            chains_total: 5,
            rules_total: 16,
            packets_total: 100,
            bytes_total: 6400,
        };
        let pts = nft_points("h", &s);
        let find = |m: &str| pts.iter().find(|p| p.metric == m).unwrap();
        assert_eq!(find("nft/tables_total").value, TelemetryValue::Gauge(2.0));
        assert_eq!(find("nft/rules_total").value, TelemetryValue::Gauge(16.0));
        assert_eq!(
            find("nft/inet/filter/rules").value,
            TelemetryValue::Gauge(12.0)
        );
        assert_eq!(find("nft/ip/nat/chains").value, TelemetryValue::Gauge(2.0));
        // #115: decoded firewall hit counters are monotonic Counters.
        assert_eq!(
            find("nft/packets_total").value,
            TelemetryValue::Counter(100)
        );
        assert_eq!(find("nft/bytes_total").value, TelemetryValue::Counter(6400));
        assert_eq!(
            find("nft/inet/filter/packets").value,
            TelemetryValue::Counter(100)
        );
    }

    /// #115: encode a synthetic `NFTA_RULE_EXPRESSIONS` blob the way the kernel
    /// would — a `counter` expression carrying big-endian packets/bytes nested in
    /// a list element — and confirm the decoder recovers it. Rules with no counter
    /// statement decode to `None`.
    #[test]
    fn decode_nft_counter_roundtrip() {
        // Build a netlink attribute: (len u16 ne)(type u16 ne)(payload, 4-aligned).
        fn attr(typ: u16, payload: &[u8]) -> Vec<u8> {
            let len = 4 + payload.len();
            let mut v = Vec::new();
            v.extend_from_slice(&(len as u16).to_ne_bytes());
            v.extend_from_slice(&typ.to_ne_bytes());
            v.extend_from_slice(payload);
            while v.len() % 4 != 0 {
                v.push(0);
            }
            v
        }

        // counter data: BYTES then PACKETS, big-endian u64 (NET_BYTEORDER flag set).
        let mut counter_data = Vec::new();
        counter_data.extend(attr(NFTA_COUNTER_BYTES | 0x4000, &6400u64.to_be_bytes()));
        counter_data.extend(attr(NFTA_COUNTER_PACKETS | 0x4000, &100u64.to_be_bytes()));

        // one expression element: NAME="counter" + nested DATA.
        let mut elem = Vec::new();
        elem.extend(attr(NFTA_EXPR_NAME, b"counter\0"));
        elem.extend(attr(NFTA_EXPR_DATA | 0x8000, &counter_data));

        // the rule's expression list: a single nested LIST_ELEM.
        let expr_bytes = attr(NFTA_LIST_ELEM | 0x8000, &elem);

        let ctr = decode_nft_counter(&expr_bytes).expect("counter present");
        assert_eq!(ctr.packets, 100);
        assert_eq!(ctr.bytes, 6400);

        // A non-counter expression (e.g. "accept" verdict) → no counter.
        let mut other = Vec::new();
        other.extend(attr(NFTA_EXPR_NAME, b"immediate\0"));
        let other_bytes = attr(NFTA_LIST_ELEM | 0x8000, &other);
        assert_eq!(decode_nft_counter(&other_bytes), None);

        // Garbage / truncated input never panics.
        assert_eq!(decode_nft_counter(&[0xff, 0x00, 0x01]), None);
        assert_eq!(decode_nft_counter(&[]), None);
    }

    // ---- eBPF module (#114) -----------------------------------------------

    #[test]
    fn test_connlat_percentiles_empty_and_single() {
        assert_eq!(connlat_percentiles(&[0u64; 27]), (0, 0));
        let mut h = [0u64; 27];
        h[8] = 50;
        assert_eq!(connlat_percentiles(&h), (1 << 8, 1 << 8));
    }

    #[test]
    fn test_connlat_percentiles_bimodal() {
        let mut h = [0u64; 27];
        h[3] = 90; // fast
        h[18] = 10; // slow tail
        let (p50, p95) = connlat_percentiles(&h);
        assert_eq!(p50, 1 << 3);
        assert_eq!(p95, 1 << 18);
    }

    #[test]
    fn test_connlat_points_omit_on_zero() {
        assert!(connlat_points("h", 0, 0).is_empty());
        let pts = connlat_points("h", 100, 250);
        assert_eq!(pts.len(), 2);
        assert_eq!(pts[0].metric, "sockets/tcp/connlat_us_p50");
        assert_eq!(pts[1].metric, "sockets/tcp/connlat_us_p95");
    }

    #[test]
    fn test_top_k_retransmits_orders_and_bounds() {
        let mk = |b: u8, c: u64| {
            let mut addr = [0u8; 16];
            addr[0] = 10;
            addr[3] = b;
            (
                RetransKey {
                    family: 2,
                    _pad: [0; 3],
                    addr,
                },
                c,
            )
        };
        let snap = vec![mk(1, 5), mk(2, 50), mk(3, 20)];
        let top = top_k_retransmits(&snap, 2);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].count, 50);
        assert_eq!(top[0].peer, "10.0.0.2");
        assert_eq!(top[0].family, 4);
        assert_eq!(top[1].count, 20);
    }

    #[test]
    fn test_conn_view_from_record() {
        let mut comm = [0u8; 16];
        comm[..4].copy_from_slice(b"curl");
        let mut saddr = [0u8; 16];
        saddr[..4].copy_from_slice(&[10, 0, 0, 5]);
        let mut daddr = [0u8; 16];
        daddr[..4].copy_from_slice(&[93, 184, 216, 34]);
        let rec = ConnRecord {
            ts_ns: 0,
            pid: 4821,
            comm,
            family: 2,
            _pad: [0; 3],
            saddr,
            daddr,
            sport: 51234,
            dport: 443,
            duration_ns: 318_000_000,
            tx_bytes: 742,
            rx_bytes: 56012,
            segs_out: 12,
            segs_in: 41,
            retrans: 0,
            _pad2: 0,
        };
        let v = ConnView::from_record(&rec);
        assert_eq!(v.comm, "curl");
        assert_eq!(v.family, 4);
        assert_eq!(v.local, "10.0.0.5");
        assert_eq!(v.remote, "93.184.216.34");
        assert_eq!(v.rport, 443);
        assert_eq!(v.duration_ms, 318);
        // serde round-trip
        let json = serde_json::to_string(&v).unwrap();
        let back: ConnView = serde_json::from_str(&json).unwrap();
        assert_eq!(v, back);
    }
}
