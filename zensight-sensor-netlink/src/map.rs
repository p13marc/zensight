//! Pure mapping from observed kernel state to [`TelemetryPoint`]s.
//!
//! The collector reads nlink and fills these plain sample structs; the mapping
//! to telemetry is kept here, free of any kernel/nlink dependency, so it is
//! unit-testable without privileges or a live netlink socket.

use std::collections::HashMap;

use serde::Serialize;
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
/// `sockets/tcp/<stat>`.
pub fn socket_points(host: &str, c: &SocketCounts) -> Vec<TelemetryPoint> {
    vec![
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
    ]
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
        labels.insert("drop_rate".to_string(), format!("{}", d.bottleneck_drop_rate));
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
// On-demand detail records (principle P2): served via the query channel
// (`@/query/{routes,neighbors,sockets}`), never streamed onto the telemetry bus.
// These are the full, higher-cardinality tables the GUI fetches when a user
// drills into a host. Kept here (pure + serde) so their shape is unit-tested
// without a kernel; `query.rs` builds them from live nlink dumps.
// ---------------------------------------------------------------------------

/// One row of the routing table (full detail).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RouteRecord {
    /// IP family: 4 or 6.
    pub family: u8,
    /// Destination: `"default"` or `"<cidr>"`.
    pub dst: String,
    pub gateway: Option<String>,
    /// Output interface index.
    pub oif: Option<u32>,
    pub priority: Option<u32>,
    pub protocol: String,
    pub scope: String,
    pub table: u32,
}

/// One ARP/NDP neighbor entry (full detail).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NeighborRecord {
    pub family: u8,
    pub ip: Option<String>,
    pub mac: Option<String>,
    pub ifindex: u32,
    pub state: String,
    pub is_router: bool,
}

/// One TCP socket (full detail), served filterable by state/port.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SocketRecord {
    pub local: String,
    pub remote: String,
    pub state: String,
    pub uid: u32,
    pub recv_q: u32,
    pub send_q: u32,
    pub rtt_us: u32,
    pub retrans: u32,
    pub inode: u32,
}

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
        let c = SocketCounts {
            established: 5,
            listen: 3,
            retransmits_total: 12,
            max_rtt_us: 400,
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
}
