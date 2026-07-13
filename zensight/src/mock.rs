//! Mock telemetry generator for testing.
//!
//! Provides functions to generate realistic telemetry data without
//! connecting to actual sensors or Zenoh.

use std::collections::HashMap;

use zensight_common::{HostEntity, MemberClaim, Protocol, TelemetryPoint, TelemetryValue};

/// Generate a mock telemetry point.
pub fn telemetry_point(
    protocol: Protocol,
    source: &str,
    metric: &str,
    value: TelemetryValue,
) -> TelemetryPoint {
    TelemetryPoint {
        timestamp: now_ms(),
        source: source.to_string(),
        protocol,
        metric: metric.to_string(),
        value,
        labels: HashMap::new(),
    }
}

/// Generate a mock telemetry point with labels.
pub fn telemetry_point_with_labels(
    protocol: Protocol,
    source: &str,
    metric: &str,
    value: TelemetryValue,
    labels: HashMap<String, String>,
) -> TelemetryPoint {
    TelemetryPoint {
        timestamp: now_ms(),
        source: source.to_string(),
        protocol,
        metric: metric.to_string(),
        value,
        labels,
    }
}

/// Get current time in milliseconds.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Mock SNMP device data.
pub mod snmp {
    use super::*;

    /// Generate mock router telemetry.
    pub fn router(name: &str) -> Vec<TelemetryPoint> {
        vec![
            telemetry_point(
                Protocol::Snmp,
                name,
                "system/sysUpTime",
                TelemetryValue::Counter(86400000), // 1 day in centiseconds
            ),
            telemetry_point(
                Protocol::Snmp,
                name,
                "system/sysName",
                TelemetryValue::Text(name.to_string()),
            ),
            telemetry_point(
                Protocol::Snmp,
                name,
                "if/1/ifInOctets",
                TelemetryValue::Counter(1_234_567_890),
            ),
            telemetry_point(
                Protocol::Snmp,
                name,
                "if/1/ifOutOctets",
                TelemetryValue::Counter(987_654_321),
            ),
            telemetry_point(
                Protocol::Snmp,
                name,
                "if/1/ifOperStatus",
                TelemetryValue::Gauge(1.0), // up
            ),
            telemetry_point(
                Protocol::Snmp,
                name,
                "if/2/ifInOctets",
                TelemetryValue::Counter(555_666_777),
            ),
            telemetry_point(
                Protocol::Snmp,
                name,
                "if/2/ifOutOctets",
                TelemetryValue::Counter(111_222_333),
            ),
        ]
    }

    /// Generate mock switch telemetry.
    pub fn switch(name: &str, port_count: u32) -> Vec<TelemetryPoint> {
        let mut points = vec![telemetry_point(
            Protocol::Snmp,
            name,
            "system/sysUpTime",
            TelemetryValue::Counter(172800000), // 2 days
        )];

        for port in 1..=port_count {
            points.push(telemetry_point(
                Protocol::Snmp,
                name,
                &format!("if/{}/ifInOctets", port),
                TelemetryValue::Counter((port as u64) * 1_000_000),
            ));
            points.push(telemetry_point(
                Protocol::Snmp,
                name,
                &format!("if/{}/ifOutOctets", port),
                TelemetryValue::Counter((port as u64) * 500_000),
            ));
        }

        points
    }
}

/// Mock sysinfo (system metrics) data.
pub mod sysinfo {
    use super::*;

    /// Generate mock host telemetry.
    pub fn host(name: &str) -> Vec<TelemetryPoint> {
        vec![
            telemetry_point(
                Protocol::Sysinfo,
                name,
                "cpu/usage",
                TelemetryValue::Gauge(45.5),
            ),
            telemetry_point(
                Protocol::Sysinfo,
                name,
                "cpu/0/usage",
                TelemetryValue::Gauge(52.3),
            ),
            telemetry_point(
                Protocol::Sysinfo,
                name,
                "cpu/1/usage",
                TelemetryValue::Gauge(38.7),
            ),
            telemetry_point(
                Protocol::Sysinfo,
                name,
                "memory/used_bytes",
                TelemetryValue::Gauge(8_589_934_592.0), // 8 GB
            ),
            telemetry_point(
                Protocol::Sysinfo,
                name,
                "memory/total_bytes",
                TelemetryValue::Gauge(17_179_869_184.0), // 16 GB
            ),
            telemetry_point(
                Protocol::Sysinfo,
                name,
                "memory/usage_percent",
                TelemetryValue::Gauge(50.0),
            ),
            telemetry_point(
                Protocol::Sysinfo,
                name,
                "disk/root/used_bytes",
                TelemetryValue::Gauge(107_374_182_400.0), // 100 GB
            ),
            telemetry_point(
                Protocol::Sysinfo,
                name,
                "disk/root/total_bytes",
                TelemetryValue::Gauge(536_870_912_000.0), // 500 GB
            ),
            telemetry_point(
                Protocol::Sysinfo,
                name,
                "network/eth0/rx_bytes",
                TelemetryValue::Counter(1_073_741_824), // 1 GB
            ),
            telemetry_point(
                Protocol::Sysinfo,
                name,
                "network/eth0/tx_bytes",
                TelemetryValue::Counter(536_870_912), // 512 MB
            ),
        ]
    }
}

/// Mock syslog data.
pub mod syslog {
    use super::*;

    /// Generate mock log lines. Each line is a per-line event (#104): the metric
    /// is `events/<uid>` (unique, time-sortable) and the facility/severity travel
    /// in labels with the OTel logs data model, matching the logs sensor's contract.
    pub fn server(name: &str) -> Vec<TelemetryPoint> {
        [
            ("auth", "info", "User admin logged in successfully"),
            ("kern", "warning", "Low memory condition detected"),
            ("daemon", "err", "Service nginx failed to start"),
        ]
        .into_iter()
        .enumerate()
        .map(|(seq, (facility, severity, msg))| {
            let mut labels = HashMap::new();
            labels.insert("facility".to_string(), facility.to_string());
            labels.insert("severity".to_string(), severity.to_string());
            let (num, text) = otel_severity(severity);
            labels.insert("severity_number".to_string(), num.to_string());
            labels.insert("severity_text".to_string(), text.to_string());
            // Mirror the sensor's `<timestamp_ms><seq>` uid shape (#104).
            let uid = format!("{:013}{:012}", 0, seq);
            labels.insert("log.record.uid".to_string(), uid.clone());
            telemetry_point_with_labels(
                Protocol::Logs,
                name,
                &format!("events/{uid}"),
                TelemetryValue::Text(msg.to_string()),
                labels,
            )
        })
        .collect()
    }

    /// Map an RFC-5424 severity slug to the OTel (`severity_number`, `severity_text`)
    /// pair the logs sensor publishes — keep in sync with `parser::Severity`.
    fn otel_severity(slug: &str) -> (u8, &'static str) {
        match slug {
            "emerg" => (24, "FATAL"),
            "alert" => (23, "FATAL"),
            "crit" => (22, "FATAL"),
            "err" => (17, "ERROR"),
            "warning" => (13, "WARN"),
            "notice" => (10, "INFO"),
            "info" => (9, "INFO"),
            "debug" => (5, "DEBUG"),
            _ => (9, "INFO"),
        }
    }
}

/// Mock netlink (Linux kernel networking) data.
pub mod netlink {
    use super::*;

    /// Generate mock netlink telemetry for a host.
    pub fn host(name: &str) -> Vec<TelemetryPoint> {
        let mut labels = HashMap::new();
        labels.insert("ifindex".to_string(), "2".to_string());
        vec![
            telemetry_point_with_labels(
                Protocol::Netlink,
                name,
                "iface/eth0/rx_bytes",
                TelemetryValue::Counter(1_073_741_824),
                labels.clone(),
            ),
            telemetry_point_with_labels(
                Protocol::Netlink,
                name,
                "iface/eth0/tx_bytes",
                TelemetryValue::Counter(536_870_912),
                labels.clone(),
            ),
            telemetry_point_with_labels(
                Protocol::Netlink,
                name,
                "iface/eth0/up",
                TelemetryValue::Boolean(true),
                labels,
            ),
            telemetry_point(
                Protocol::Netlink,
                name,
                "sockets/tcp/established",
                TelemetryValue::Gauge(120.0),
            ),
            telemetry_point(
                Protocol::Netlink,
                name,
                "sockets/tcp/listen",
                TelemetryValue::Gauge(12.0),
            ),
            telemetry_point(
                Protocol::Netlink,
                name,
                "routes/total",
                TelemetryValue::Gauge(20.0),
            ),
            // Default gateway (#391): drives the topology's Gateway edges +
            // wire-only router node in --demo.
            telemetry_point(
                Protocol::Netlink,
                name,
                "routes/default_v4_present",
                TelemetryValue::Boolean(true),
            ),
            telemetry_point(
                Protocol::Netlink,
                name,
                "routes/default_v4_gw",
                TelemetryValue::Text("10.0.0.254".to_string()),
            ),
            telemetry_point(
                Protocol::Netlink,
                name,
                "neighbors/total",
                TelemetryValue::Gauge(18.0),
            ),
        ]
    }
}

/// Mock netring (passive flow monitor) data.
pub mod netring {
    use super::*;

    /// Mock enriched passive-asset inventory (#329) for `--demo`: a small fleet
    /// spanning roles (router / iot / phone / host) with first-seen, source-count
    /// confidence, and JA4 fingerprint pivots — so the enriched inventory view is
    /// developable without live capture (per the demo/mock contract).
    #[allow(clippy::too_many_arguments)]
    pub fn assets() -> Vec<zensight_common::AssetRecord> {
        use zensight_common::AssetRecord;
        let row = |mac: &str,
                   ip: &str,
                   host: &str,
                   vendor: &str,
                   role: &str,
                   srcs: u32,
                   first: i64,
                   last: i64,
                   ja4: Option<&str>| AssetRecord {
            mac: mac.into(),
            ipv4: vec![ip.into()],
            hostname: Some(host.into()),
            hostnames: vec![host.into()],
            vendor: Some(vendor.into()),
            role: role.into(),
            source_count: srcs,
            first_seen: first,
            last_seen: last,
            seen_via: vec!["arp".into(), "lldp".into()],
            ja4: ja4.map(Into::into),
            ..Default::default()
        };
        vec![
            row(
                "aa:bb:cc:00:01:01",
                "10.0.0.1",
                "core-rtr",
                "Cisco",
                "router",
                3,
                1_700_000_000_000,
                1_700_100_000_000,
                None,
            ),
            row(
                "aa:bb:cc:00:02:02",
                "10.0.0.42",
                "cam-lobby",
                "Hikvision",
                "iot",
                2,
                1_700_050_000_000,
                1_700_100_000_000,
                Some("t13d1516h2_8daaf6152771_deadbeef00"),
            ),
            row(
                "aa:bb:cc:00:03:03",
                "10.0.0.55",
                "desk-phone",
                "Polycom",
                "phone",
                2,
                1_700_060_000_000,
                1_700_100_000_000,
                None,
            ),
            row(
                "aa:bb:cc:00:04:04",
                "10.0.0.101",
                "workstation7",
                "Dell",
                "host",
                4,
                1_700_070_000_000,
                1_700_100_000_000,
                Some("t13d1516h2_8daaf6152771_cafebabe11"),
            ),
            // Matches the wire-only demo entity (host_entities: 10.0.0.200) so
            // the topology's passive node picks up an asset role in --demo.
            row(
                "aa:bb:cc:00:05:05",
                "10.0.0.200",
                "wire-host-a",
                "Hikvision",
                "iot",
                2,
                1_700_080_000_000,
                1_700_100_000_000,
                None,
            ),
        ]
    }

    /// Mock netring traffic matrix (#391) for `--demo`: directed bytes/sec
    /// between the demo entities' IPs (host_entities: server01 = 10.0.0.11,
    /// server02 = 10.0.0.12, wire-host-a = 10.0.0.200), so the topology shows
    /// rated, direction-arrowed edges without live capture.
    pub fn matrix() -> Vec<zensight_common::MatrixRecord> {
        let row = |src: &str, dst: &str, rate: f64| zensight_common::MatrixRecord {
            src: src.into(),
            dst: dst.into(),
            bytes_per_sec: rate,
            names: Vec::new(),
        };
        vec![
            row("10.0.0.11", "10.0.0.12", 830_000.0),
            row("10.0.0.12", "10.0.0.11", 145_000.0),
            row("10.0.0.200", "10.0.0.11", 42_000.0),
        ]
    }

    /// Generate mock netring telemetry for a probe.
    pub fn probe(name: &str) -> Vec<TelemetryPoint> {
        let mut bw_labels = HashMap::new();
        bw_labels.insert("app".to_string(), "https".to_string());
        vec![
            telemetry_point(
                Protocol::Netring,
                name,
                "flow/active",
                TelemetryValue::Gauge(240.0),
            ),
            telemetry_point(
                Protocol::Netring,
                name,
                "flow/bytes_total",
                TelemetryValue::Counter(12_884_901_888),
            ),
            telemetry_point(
                Protocol::Netring,
                name,
                "flow/by_l4/tcp/flows_total",
                TelemetryValue::Counter(4_096),
            ),
            telemetry_point_with_labels(
                Protocol::Netring,
                name,
                "bandwidth/https/bytes_per_sec",
                TelemetryValue::Gauge(6_000_000.0),
                bw_labels,
            ),
            telemetry_point(
                Protocol::Netring,
                name,
                "dns/queries_total",
                TelemetryValue::Counter(8_192),
            ),
            telemetry_point(
                Protocol::Netring,
                name,
                "tls/handshakes_total",
                TelemetryValue::Counter(2_048),
            ),
            // Capture self-health (#227/#224): resolved backend + a per-NIC leg
            // with a light, non-overload drop rate so the GUI's capture panel and
            // backend badge render in demo mode.
            telemetry_point(
                Protocol::Netring,
                name,
                "capture/backend",
                TelemetryValue::Text("af_packet".to_string()),
            ),
            telemetry_point(
                Protocol::Netring,
                name,
                "capture/0/packets",
                TelemetryValue::Counter(1_048_576),
            ),
            telemetry_point(
                Protocol::Netring,
                name,
                "capture/0/drops",
                TelemetryValue::Counter(2_100),
            ),
            telemetry_point(
                Protocol::Netring,
                name,
                "capture/0/drop_rate",
                TelemetryValue::Gauge(0.002),
            ),
            telemetry_point(
                Protocol::Netring,
                name,
                "capture/focus/packets",
                TelemetryValue::Counter(512),
            ),
        ]
    }
}

/// Mock netflow data.
pub mod netflow {
    use super::*;

    /// Generate mock netflow telemetry for an exporter.
    pub fn exporter(name: &str) -> Vec<TelemetryPoint> {
        let mut labels = HashMap::new();
        labels.insert("version".to_string(), "v9".to_string());
        labels.insert("exporter_ip".to_string(), "10.0.0.1".to_string());
        labels.insert("protocol".to_string(), "tcp".to_string());
        vec![
            telemetry_point_with_labels(
                Protocol::Netflow,
                name,
                "10.0.0.50/93.184.216.34/tcp",
                TelemetryValue::Counter(2_500_000),
                labels.clone(),
            ),
            telemetry_point_with_labels(
                Protocol::Netflow,
                name,
                "10.0.0.52/10.0.0.20/tcp",
                TelemetryValue::Counter(1_200_000),
                labels,
            ),
        ]
    }
}

/// Mock gNMI data.
pub mod gnmi {
    use super::*;

    /// Generate mock gNMI telemetry for a target.
    pub fn target(name: &str) -> Vec<TelemetryPoint> {
        vec![
            telemetry_point(
                Protocol::Gnmi,
                name,
                "interfaces/interface[name=eth0]/state/counters/in-octets",
                TelemetryValue::Counter(1_073_741_824),
            ),
            telemetry_point(
                Protocol::Gnmi,
                name,
                "interfaces/interface[name=eth0]/state/oper-status",
                TelemetryValue::Text("UP".to_string()),
            ),
        ]
    }
}

/// Mock parallax (live media) data.
pub mod parallax {
    use super::*;
    use zensight_common::StreamDescriptor;

    /// Mock stream catalogue — mirrors the real `@rpc/parallax/streams` reply
    /// shape (demo mirrors the wire contract).
    pub fn streams() -> Vec<StreamDescriptor> {
        vec![
            StreamDescriptor {
                stream: "video0".to_string(),
                codecs: vec!["h264".to_string(), "mjpeg".to_string()],
                active: false,
                description: Some("Integrated Webcam".to_string()),
            },
            StreamDescriptor {
                stream: "door".to_string(),
                codecs: vec!["h264".to_string(), "mjpeg".to_string()],
                active: true,
                description: Some("front door (rtsp)".to_string()),
            },
            StreamDescriptor {
                stream: "test0".to_string(),
                codecs: vec!["h264".to_string(), "mjpeg".to_string()],
                active: false,
                description: Some("test pattern smpte 640x360@15".to_string()),
            },
        ]
    }

    /// Mock per-stream stats telemetry so a parallax device card appears in
    /// demo mode (mirrors the sensor's stats ticker keys).
    pub fn host(name: &str) -> Vec<TelemetryPoint> {
        vec![
            telemetry_point(
                Protocol::Parallax,
                name,
                "streams/advertised",
                TelemetryValue::Gauge(3.0),
            ),
            telemetry_point(
                Protocol::Parallax,
                name,
                "door/stats/fps",
                TelemetryValue::Gauge(15.2),
            ),
            telemetry_point(
                Protocol::Parallax,
                name,
                "door/stats/kbps",
                TelemetryValue::Gauge(1850.0),
            ),
            telemetry_point(
                Protocol::Parallax,
                name,
                "door/stats/viewers",
                TelemetryValue::Gauge(1.0),
            ),
        ]
    }
}

pub mod modbus {
    use super::*;

    /// Generate mock PLC telemetry.
    pub fn plc(name: &str) -> Vec<TelemetryPoint> {
        vec![
            telemetry_point(
                Protocol::Modbus,
                name,
                "holding/0",
                TelemetryValue::Gauge(1234.0),
            ),
            telemetry_point(
                Protocol::Modbus,
                name,
                "holding/1",
                TelemetryValue::Gauge(5678.0),
            ),
            telemetry_point(
                Protocol::Modbus,
                name,
                "coil/0",
                TelemetryValue::Boolean(true),
            ),
            telemetry_point(
                Protocol::Modbus,
                name,
                "coil/1",
                TelemetryValue::Boolean(false),
            ),
            telemetry_point(
                Protocol::Modbus,
                name,
                "input/0",
                TelemetryValue::Gauge(42.0),
            ),
        ]
    }
}

/// Generate a complete mock environment with multiple devices.
/// Deterministic `h_<12hex>` entity id from a host name (FNV-1a). Mirrors the
/// correlator's stable-id scheme so demo/mock ids are consistent across runs.
fn entity_id_for(name: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in name.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("h-{:012x}", hash & 0xffff_ffff_ffff)
}

/// One correlator [`HostEntity`] merging the given `(sensor, source)` members
/// under a host name. `ips`/`macs` are illustrative but stable.
fn mock_entity(
    name: &str,
    members: &[(&str, &str)],
    ips: &[&str],
    macs: &[&str],
    now: i64,
) -> HostEntity {
    let mut e = HostEntity {
        entity_id: entity_id_for(name),
        aliases: vec![],
        host_id: Some(format!("{:064x}", entity_id_for(name).len() as u64)),
        boot_id: None,
        ips: ips.iter().map(|s| s.to_string()).collect(),
        macs: macs.iter().map(|s| s.to_string()).collect(),
        container_ids: vec![],
        hostname: Some(name.to_string()),
        fqdn: Some(format!("{name}.lab.example")),
        names: vec![],
        vendor: None,
        platform: Some("linux".to_string()),
        members: members
            .iter()
            .map(|(sensor, source)| MemberClaim {
                sensor: (*sensor).to_string(),
                source: (*source).to_string(),
                rule: "machine-id".to_string(),
                confidence: 1.0,
                last_seen: now,
            })
            .collect(),
        status: Some("online".to_string()),
        last_updated: now,
    };
    e.canonicalize();
    e
}

/// Mock per-process bandwidth records (#319, epic #320) for the Bandwidth view's
/// Processes mode in `--demo`: demo never serves the `@rpc/netlink/bandwidth`
/// queryable, so the demo fetch branch returns these instead. Tagged sock_diag /
/// app-goodput / TCP, including the explicit `unattributed` bucket the real
/// aggregator emits.
pub mod bandwidth {
    use zensight_common::bandwidth::{
        BandwidthKey, BandwidthRecord, BandwidthSource, ByteSemantics, ProtoScope,
    };

    fn proc_row(pid: i32, comm: &str, tx_bps: f64, rx_bps: f64) -> BandwidthRecord {
        BandwidthRecord {
            key: BandwidthKey::Process {
                pid,
                start_time: 1000 + pid.max(0) as u64,
                comm: comm.to_string(),
            },
            tx_bps,
            rx_bps,
            source: BandwidthSource::SockDiag,
            semantics: ByteSemantics::AppGoodput,
            proto: ProtoScope::Tcp,
            host: Some("server01".to_string()),
        }
    }

    /// A netring wire-L2 row (#318): full-frame throughput is undirected, so the
    /// whole rate sits in `tx_bps` behind the wire-L2 badge.
    fn wire_row(pid: i32, comm: &str, tx_bps: f64) -> BandwidthRecord {
        BandwidthRecord {
            key: BandwidthKey::Process {
                pid,
                start_time: 1000 + pid.max(0) as u64,
                comm: comm.to_string(),
            },
            tx_bps,
            rx_bps: 0.0,
            source: BandwidthSource::Netring,
            semantics: ByteSemantics::WireL2,
            proto: ProtoScope::All,
            host: Some("server01".to_string()),
        }
    }

    /// A ranked per-process snapshot mixing the netlink sock_diag goodput tier and
    /// the netring wire-L2 attribution tier (#318) so the GUI shows both badges.
    pub fn processes() -> Vec<BandwidthRecord> {
        vec![
            proc_row(1421, "nginx", 1_800_000.0, 240_000.0),
            proc_row(2210, "postgres", 90_000.0, 1_200_000.0),
            proc_row(3312, "curl", 40_000.0, 2_400_000.0),
            proc_row(880, "sshd", 12_000.0, 5_000.0),
            proc_row(-1, "unattributed", 60_000.0, 30_000.0),
            wire_row(1421, "nginx", 2_350_000.0),
            wire_row(4102, "chrome", 980_000.0),
            wire_row(-1, "unattributed", 145_000.0),
        ]
    }
}

/// Mock correlator host entities (#306), consistent with the sources emitted by
/// [`mock_environment`]: server01 (sysinfo+logs+netlink) and server02
/// (sysinfo+netlink) each merge into one host, and a wire-only entity models a
/// host observed purely on the wire (passive topology node) with no live sensor
/// device. `now` stamps `last_updated` so freshness indicators animate.
pub fn host_entities_at(now: i64) -> Vec<HostEntity> {
    vec![
        mock_entity(
            "server01",
            &[
                ("sysinfo", "server01"),
                ("logs", "server01"),
                ("netlink", "server01"),
            ],
            &["10.0.0.11"],
            &["02:42:0a:00:00:0b"],
            now,
        ),
        mock_entity(
            "server02",
            &[("sysinfo", "server02"), ("netlink", "server02")],
            &["10.0.0.12"],
            &["02:42:0a:00:00:0c"],
            now,
        ),
        // Wire-only host: a netring observation with no live device of its own.
        mock_entity(
            "wire-host-a",
            &[("netring", "10.0.0.200")],
            &["10.0.0.200"],
            &[],
            now,
        ),
    ]
}

/// [`host_entities_at`] stamped with the current wall clock.
pub fn host_entities() -> Vec<HostEntity> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    host_entities_at(now)
}

pub fn mock_environment() -> Vec<TelemetryPoint> {
    let mut points = Vec::new();

    // Network devices
    points.extend(snmp::router("router01"));
    points.extend(snmp::router("router02"));
    points.extend(snmp::switch("switch01", 24));

    // Servers
    points.extend(sysinfo::host("server01"));
    points.extend(sysinfo::host("server02"));
    points.extend(syslog::server("server01"));

    // Linux kernel networking (netlink runs on the hosts)
    points.extend(netlink::host("server01"));
    points.extend(netlink::host("server02"));

    // Passive flow monitoring + flow export + streamed telemetry
    points.extend(netring::probe("netprobe01"));
    points.extend(netflow::exporter("edge-fw"));
    points.extend(gnmi::target("router01"));

    // Industrial devices
    points.extend(modbus::plc("plc01"));

    // Live media (parallax): stats telemetry makes the camera host's device
    // card appear, so the stream catalogue + tile views are reachable in
    // demo (demo mirrors the wire contract).
    points.extend(parallax::host("camhost01"));

    points
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_environment_generates_data() {
        let points = mock_environment();
        assert!(!points.is_empty());

        // Check we have multiple protocols
        let protocols: std::collections::HashSet<_> = points.iter().map(|p| p.protocol).collect();
        assert!(protocols.contains(&Protocol::Snmp));
        assert!(protocols.contains(&Protocol::Sysinfo));
        assert!(protocols.contains(&Protocol::Logs));
        assert!(protocols.contains(&Protocol::Modbus));
        assert!(protocols.contains(&Protocol::Netlink));
        assert!(protocols.contains(&Protocol::Netring));
        assert!(protocols.contains(&Protocol::Netflow));
        assert!(protocols.contains(&Protocol::Gnmi));
        assert!(protocols.contains(&Protocol::Parallax));
    }

    #[test]
    fn test_snmp_router_metrics() {
        let points = snmp::router("test-router");
        assert!(!points.is_empty());
        assert!(points.iter().all(|p| p.protocol == Protocol::Snmp));
        assert!(points.iter().all(|p| p.source == "test-router"));
    }

    #[test]
    fn test_sysinfo_host_metrics() {
        let points = sysinfo::host("test-host");
        assert!(!points.is_empty());
        assert!(points.iter().any(|p| p.metric.contains("cpu")));
        assert!(points.iter().any(|p| p.metric.contains("memory")));
    }

    #[test]
    fn host_entity_ids_are_stable_and_prefixed() {
        // Deterministic across runs and correctly formatted.
        assert_eq!(entity_id_for("server01"), entity_id_for("server01"));
        assert_ne!(entity_id_for("server01"), entity_id_for("server02"));
        let ents = host_entities_at(1_000);
        assert!(ents.iter().all(|e| e.entity_id.starts_with("h-")));
        assert!(ents.iter().all(|e| e.entity_id.len() == 14));
    }

    #[test]
    fn mock_entity_members_track_mock_device_sources() {
        // Every server-backed mock entity member must correspond to a real mock
        // device source so grouping actually merges (demo/mock contract, #306).
        let env = mock_environment();
        let device_sources: std::collections::HashSet<(Protocol, String)> =
            env.iter().map(|p| (p.protocol, p.source.clone())).collect();

        for entity in host_entities_at(1_000) {
            // The wire-only host is intentionally NOT backed by a device.
            if entity.hostname.as_deref() == Some("wire-host-a") {
                for m in &entity.members {
                    let proto: Protocol = m.sensor.parse().unwrap();
                    assert!(
                        !device_sources.contains(&(proto, m.source.clone())),
                        "wire-only member unexpectedly has a live device: {m:?}"
                    );
                }
                continue;
            }
            for m in &entity.members {
                let proto: Protocol = m.sensor.parse().unwrap();
                assert!(
                    device_sources.contains(&(proto, m.source.clone())),
                    "mock entity member {}/{} has no mock device",
                    m.sensor,
                    m.source
                );
            }
        }
    }
}

/// Mock `introspect` replies for the Fleet view in `--demo` (#469): demo never
/// serves the `@rpc/<producer>/introspect` queryable, so the demo fetch branch
/// returns these instead.
///
/// The slices are the *real* compiled ones — the demo must mirror the sensor
/// contract, not a hand-written fiction of it — with one deliberate exception:
/// `edge01` serves a bumped `[registry] version`, so the demo actually shows
/// what the view is for. A demo where every host is in sync demonstrates
/// nothing.
pub mod fleet {
    use crate::view::fleet::FleetReply;

    fn slice(name: &str) -> String {
        zensight_keyspace::registry::REGISTRIES
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, t)| (*t).to_string())
            .unwrap_or_default()
    }

    /// Bump a slice's `[registry] version` — a synthetic skew, so the demo has
    /// an odd one out to find.
    fn skewed(name: &str, version: &str) -> String {
        let src = slice(name);
        let mut out = String::with_capacity(src.len());
        let mut bumped = false;
        for line in src.lines() {
            if !bumped && line.trim_start().starts_with("version =") {
                out.push_str(&format!("version = \"{version}\"\n"));
                bumped = true;
            } else {
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }

    pub fn replies() -> Vec<FleetReply> {
        let server01 = "h-1a2b3c4d5e6f";
        let edge01 = "h-9f8e7d6c5b4a";
        vec![
            FleetReply {
                origin: server01.into(),
                producer: "sysinfo".into(),
                toml: slice("sysinfo"),
            },
            FleetReply {
                origin: server01.into(),
                producer: "netlink".into(),
                toml: slice("netlink"),
            },
            FleetReply {
                origin: server01.into(),
                producer: "netring".into(),
                toml: slice("netring"),
            },
            FleetReply {
                origin: edge01.into(),
                producer: "sysinfo".into(),
                toml: slice("sysinfo"),
            },
            // The odd one out: an older deployment still on registry 1.0.
            FleetReply {
                origin: edge01.into(),
                producer: "netlink".into(),
                toml: skewed("netlink", "1.0"),
            },
        ]
    }
}
