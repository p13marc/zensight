//! Mock telemetry generator for testing.
//!
//! Provides functions to generate realistic telemetry data without
//! connecting to actual sensors or Zenoh.

use std::collections::HashMap;

use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

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

/// Mock modbus data.
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
}
