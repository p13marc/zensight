//! Mock telemetry generator for testing.
//!
//! Provides functions to generate realistic telemetry data without
//! connecting to actual bridges or Zenoh.

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

    /// Generate mock syslog messages.
    pub fn server(name: &str) -> Vec<TelemetryPoint> {
        vec![
            telemetry_point(
                Protocol::Syslog,
                name,
                "auth/info",
                TelemetryValue::Text("User admin logged in successfully".to_string()),
            ),
            telemetry_point(
                Protocol::Syslog,
                name,
                "kernel/warning",
                TelemetryValue::Text("Low memory condition detected".to_string()),
            ),
            telemetry_point(
                Protocol::Syslog,
                name,
                "daemon/error",
                TelemetryValue::Text("Service nginx failed to start".to_string()),
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
        assert!(protocols.contains(&Protocol::Syslog));
        assert!(protocols.contains(&Protocol::Modbus));
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
