//! Reverse registry conformance for netring (#654, RFC 08 §6.1).
//!
//! Forward (*published ⊆ registered*) is enforced at runtime by the metric
//! guard and by the mapper unit tests. This is the other direction:
//! *registered ⊆ emittable*. A family the registry advertises that no build can
//! publish is a surface `introspect` promises the fleet and nobody serves.
//!
//! Netring's names all come from small pure functions in `map.rs`, so this
//! drives the real ones. Fixtures use non-zero values and `Some(...)`
//! throughout: several families are gated on a value being present or above
//! zero, and a zeroed fixture reports live families as unemitted.

use zensight_common::registry::netring::Subject;
use zensight_common::registry_audit;
use zensight_sensor_netring::map::{self, CaptureDrops, DnsRcodeClass};

/// Registered netring telemetry families this build can never emit, and why.
const CONDITIONAL_FAMILIES: &[(&str, &str)] = &[];

#[test]
fn every_registered_family_has_an_emitter() {
    let mut emitted: Vec<String> = Vec::new();
    macro_rules! push {
        ($pts:expr) => {
            emitted.extend($pts.into_iter().map(|p| p.metric))
        };
    }
    macro_rules! push1 {
        ($pt:expr) => {
            emitted.push($pt.metric)
        };
    }

    push!(map::flow_points("h", 10, 8, 2));
    push!(map::flow_volume_points("h", 1_000_000, 5_000, 7));
    push!(map::flow_red_points(
        "h",
        12.5,
        0.01,
        Some(1.0),
        Some(5.0),
        Some(9.0),
    ));
    push!(map::flow_by_l4_points("h", 900, 5, 80, 3, 20, 1));
    push!(map::tcp_closed_points("h", 4, 2, 1));
    push!(map::tcp_reset_points("h", 3, 1));

    // Both capture backends: AF_PACKET keeps only a freeze count distinct,
    // AF_XDP keeps every drop cause — different families, not different values.
    push!(map::capture_points(
        "h",
        0,
        1_000,
        3,
        0.003,
        &CaptureDrops::AfPacket { freezes: 2 },
    ));
    push!(map::capture_points(
        "h",
        1,
        2_000,
        5,
        0.0025,
        &CaptureDrops::Xdp {
            rx_dropped: 1,
            rx_invalid_descs: 1,
            rx_ring_full: 1,
            rx_fill_ring_empty_descs: 1,
            tx_invalid_descs: 1,
            tx_ring_empty_descs: 1,
        },
    ));
    // Both shed policies: the leaf name is chosen by policy, so one call only
    // ever covers one of the two families.
    push!(map::shed_points("h", 0, 12, true, "sample"));
    push!(map::shed_points("h", 0, 4, false, "new_flows"));
    push!(map::focus_points("h", 100, 4_000));
    push!(map::capture_disk_points(
        "h", "ring", 10, 1_000, 3, 9_000, 1, 2, 4
    ));
    push1!(map::capture_event_point("h", "rotated", "spool full"));
    push1!(map::backend_point("h", "af_packet"));

    push!(map::tls_points("h", 40, 12));
    push1!(map::quic_count_point("h", 6));
    push1!(map::ssh_count_point("h", 2));
    push1!(map::tls_pq_ratio_point("h", 3, 40));
    push!(map::encrypted_dns_points("h", 5, 2, 7, 1, 9));

    push!(map::dns_points(
        "h",
        120,
        &[
            (DnsRcodeClass::NoError, 100),
            (DnsRcodeClass::NxDomain, 10),
            (DnsRcodeClass::ServFail, 5),
            (DnsRcodeClass::Refused, 3),
            (DnsRcodeClass::Other, 2),
        ],
        4,
        Some([1.0, 5.0, 9.0]),
    ));
    push!(map::http_points(
        "h",
        200,
        150,
        20,
        20,
        10,
        &[("get".to_string(), 180), ("post".to_string(), 20)],
        Some([2.0, 8.0, 20.0]),
    ));
    push!(map::icmp_points(
        "h",
        7,
        3,
        1,
        &[("dest_unreachable".to_string(), 7)],
    ));

    push1!(map::asset_count_point("h", 15));
    push1!(map::anomaly_count_point("h", "port_scan", 2));
    push1!(map::bandwidth_point("h", "curl", 1_024.0));

    registry_audit::assert_families_covered(
        "netring",
        &emitted,
        |m| Subject::parse_metric(m).map(|s| s.pattern()),
        CONDITIONAL_FAMILIES,
    );
}
