//! Reverse registry conformance for netlink (#654, RFC 08 §6.1).

use zensight_common::registry::netlink::Subject;
use zensight_common::registry_audit;
use zensight_sensor_netlink::events::EventState;
use zensight_sensor_netlink::map::{self, *};
use zensight_sensor_netlink::route_history::RouteHistory;

/// Registered netlink telemetry families this build can never emit, and why.
///
/// The connect-latency percentiles are the workspace's canonical
/// build-conditional *subjects* — `map::connlat_points` compiles everywhere,
/// but its only caller is behind `#[cfg(feature = "ebpf")]`, so a default build
/// has no path that reaches it. Unlike a conditional *procedure*, which can be
/// declared and answer `error/unsupported`, a gauge with no reading has no
/// honest wire value: a sentinel corrupts every consumer downstream and
/// publishing nothing is indistinguishable from a quiet host. So they stay
/// registered, stay conditional, and are excused here with the gate that
/// governs them (zenkey RFC 08 §6.1 v1.20, "Conditional surfaces").
#[cfg(not(feature = "ebpf"))]
const CONDITIONAL_FAMILIES: &[(&str, &str)] = &[
    (
        "sockets/tcp/connlat_us_p50",
        "eBPF: needs `--features ebpf` + `collect.ebpf` + CAP_BPF/CAP_PERFMON (#114)",
    ),
    (
        "sockets/tcp/connlat_us_p95",
        "eBPF: needs `--features ebpf` + `collect.ebpf` + CAP_BPF/CAP_PERFMON (#114)",
    ),
];

/// On an `ebpf` build the collector does reach `connlat_points`, so the ledger
/// is empty — and the audit helper *fails* on a stale excuse, which is what
/// keeps these two lists from drifting apart.
#[cfg(feature = "ebpf")]
const CONDITIONAL_FAMILIES: &[(&str, &str)] = &[];

#[test]
fn every_registered_family_has_an_emitter() {
    let mut emitted: Vec<String> = Vec::new();
    let mut push = |pts: Vec<zensight_common::TelemetryPoint>| {
        emitted.extend(pts.into_iter().map(|p| p.metric));
    };

    push(map::iface_points(
        "h",
        &IfaceSample {
            name: "eth0".to_string(),
            ifindex: 2,
            up: true,
            carrier: Some(true),
            mtu: Some(1500),
            mac: Some("00:11:22:33:44:55".to_string()),
            oper_state: Some("up".to_string()),
            rx_bytes: 1,
            tx_bytes: 1,
            rx_packets: 1,
            tx_packets: 1,
            rx_errors: 1,
            tx_errors: 1,
            rx_dropped: 1,
            tx_dropped: 1,
            multicast: 1,
            collisions: 1,
        },
    ));
    push(map::socket_points(
        "h",
        // Every percentile/total non-zero: `socket_points` gates each of these
        // families on the value being > 0, so a zeroed fixture reports live
        // families as unemitted.
        &SocketCounts {
            established: 12,
            listen: 3,
            time_wait: 4,
            syn_sent: 1,
            close_wait: 2,
            retransmits_total: 5,
            max_rtt_us: 9_000,
            rtt_p50_us: 1_200,
            rtt_p95_us: 8_000,
            delivery_rate_p50: 1_000_000,
            delivery_rate_p95: 5_000_000,
            pacing_rate_p50: 2_000_000,
            pacing_rate_p95: 6_000_000,
            rcv_rtt_p50_us: 900,
            rcv_rtt_p95_us: 4_000,
            bytes_retrans_total: 512,
            total_retrans_total: 7,
            reordered_total: 2,
            lost_total: 1,
            by_cong: [("cubic".to_string(), 3u64)].into_iter().collect(),
            snd_buf_total: 262_144,
            rcv_buf_total: 524_288,
        },
    ));
    push(map::route_points(
        "h",
        &RouteSummary {
            ipv4_count: 5,
            ipv6_count: 3,
            total: 8,
            default_v4_present: true,
            default_v6_present: true,
            default_v4_gw: Some("192.168.1.1".to_string()),
        },
    ));
    push(map::neighbor_points("h", &NeighborSummary::default()));
    push(map::diagnostics_points(
        "h",
        &DiagnosticsSummary {
            issues_info: 1,
            issues_warning: 1,
            issues_error: 1,
            issues_critical: 1,
            bottleneck_score: 0.5,
            bottleneck_location: Some("eth0".to_string()),
            bottleneck_type: Some("queue".to_string()),
            bottleneck_recommendation: Some("increase txqueuelen".to_string()),
            bottleneck_drop_rate: 0.01,
        },
    ));
    push(map::conntrack_points(
        "h",
        &ConntrackSummary {
            total: 10,
            tcp: 5,
            udp: 3,
            icmp: 1,
            other: 1,
            max: Some(65536),
        },
    ));
    push(map::ethtool_points(
        "h",
        &EthtoolSample {
            iface: "eth0".to_string(),
            carrier: Some(true),
            speed_mbps: Some(1000),
            duplex: Some(DuplexKind::Full),
            autoneg: Some(true),
            rx_ring: Some(256),
            tx_ring: Some(256),
            rx_ring_max: Some(4096),
            tx_ring_max: Some(4096),
            pause_rx: Some(true),
            pause_tx: Some(true),
            pause_autoneg: Some(true),
            pause_rx_frames: Some(1),
            pause_tx_frames: Some(1),
            features: vec![("rx-checksumming".to_string(), true)],
            fec_modes: Some("rs".to_string()),
            fec_auto: Some(true),
            eee_enabled: Some(true),
            eee_active: Some(true),
        },
    ));
    push(map::address_points("h", &AddressSummary::default()));
    push(map::tc_points(
        "h",
        &TcQdiscSample {
            iface: "eth0".to_string(),
            kind: "fq_codel".to_string(),
            handle: "8001:".to_string(),
            bytes: 1,
            packets: 1,
            drops: 1,
            overlimits: 1,
            requeues: 1,
            backlog_bytes: 1,
            backlog_pkts: 1,
        },
    ));
    push(map::xfrm_points(
        "h",
        &XfrmSummary {
            sa_total: 2,
            sa_by_mode: [("tunnel".to_string(), 2u64)].into_iter().collect(),
            sa_by_proto: [("esp".to_string(), 2u64)].into_iter().collect(),
            policy_total: 1,
        },
    ));
    push(map::nft_points(
        "h",
        &NftSummary {
            tables: vec![NftTableSample {
                family: "inet".to_string(),
                table: "filter".to_string(),
                chains: 2,
                rules: 5,
                packets: 10,
                bytes: 100,
            }],
            tables_total: 1,
            chains_total: 2,
            rules_total: 5,
            packets_total: 10,
            bytes_total: 100,
        },
    ));
    // Only on an `ebpf` build: the sole caller of `connlat_points` is behind
    // that feature, so calling it unconditionally here would paper over the
    // very conditionality the ledger above exists to record.
    #[cfg(feature = "ebpf")]
    push(map::connlat_points("h", 1000, 5000));

    // WireGuard peers — the whole family tree is per-interface and per-peer.
    push(map::wireguard_points(
        "h",
        "wg0",
        &[WgPeerView {
            id: "a1b2c3d4".to_string(),
            endpoint: Some("203.0.113.7:51820".to_string()),
            handshake_age_s: Some(42),
            rx_bytes: 1024,
            tx_bytes: 2048,
            allowed_ips: Some("10.0.0.0/24".to_string()),
        }],
        180,
    ));

    // RTNETLINK signal counters and the default-route flap counter live outside
    // `map.rs`, in the state objects that own them.
    push(EventState::new(16).counter_points("h"));
    push(RouteHistory::new(16).flap_points("h"));

    registry_audit::assert_families_covered(
        "netlink",
        &emitted,
        |m| Subject::parse_metric(m).map(|s| s.pattern()),
        CONDITIONAL_FAMILIES,
    );
}
