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
}
