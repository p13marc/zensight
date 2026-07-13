//! Integration tests for zensight-sensor-snmp.

use zensight_common::{Format, Protocol, TelemetryPoint, TelemetryValue, decode_auto, encode};

/// Test that we can encode telemetry and it would be decodable by the frontend.
#[test]
fn test_snmp_telemetry_encoding() {
    // Simulate what the SNMP sensor produces
    let point = TelemetryPoint::new(
        "router01",
        Protocol::Snmp,
        "system/sysUpTime",
        TelemetryValue::Counter(123456),
    )
    .with_label("oid", "1.3.6.1.2.1.1.3.0");

    // The sensor encodes as JSON by default
    let encoded = encode(&point, Format::Json).expect("Encoding failed");

    // The frontend should be able to decode it
    let decoded: TelemetryPoint = decode_auto(&encoded).expect("Decoding failed");
    assert_eq!(decoded.source, "router01");
    assert_eq!(decoded.protocol, Protocol::Snmp);
    assert_eq!(decoded.metric, "system/sysUpTime");
}

/// Test key expression generation for SNMP metrics (v1: proxy shape — the
/// observed device rides as the first subject chunk after the producer).
#[test]
fn test_snmp_key_expressions() {
    let prefix = zensight_sensor_core::v1::v1_telemetry_prefix("zensight/snmp");
    let origin = zensight_sensor_core::v1::host_id().as_str();

    let sys_uptime = format!("{prefix}/router01/system/sysUpTime");
    assert_eq!(
        sys_uptime,
        format!("zensight/@v1/{origin}/telemetry/snmp/router01/system/sysUpTime")
    );
}

/// Test various SNMP value types that could come from polling.
#[test]
fn test_snmp_value_types() {
    // Counter32/Counter64 -> Counter
    let counter_point = TelemetryPoint::new(
        "device",
        Protocol::Snmp,
        "ifInOctets",
        TelemetryValue::Counter(1234567890),
    );
    assert!(matches!(counter_point.value, TelemetryValue::Counter(_)));

    // Gauge32 -> Gauge
    let gauge_point = TelemetryPoint::new(
        "device",
        Protocol::Snmp,
        "tcpCurrEstab",
        TelemetryValue::Gauge(42.0),
    );
    assert!(matches!(gauge_point.value, TelemetryValue::Gauge(_)));

    // DisplayString -> Text
    let text_point = TelemetryPoint::new(
        "device",
        Protocol::Snmp,
        "sysDescr",
        TelemetryValue::Text("Cisco IOS Software".to_string()),
    );
    assert!(matches!(text_point.value, TelemetryValue::Text(_)));

    // TimeTicks -> Counter (as centiseconds)
    let timeticks_point = TelemetryPoint::new(
        "device",
        Protocol::Snmp,
        "sysUpTime",
        TelemetryValue::Counter(123456789), // centiseconds
    );
    assert!(matches!(timeticks_point.value, TelemetryValue::Counter(_)));
}

/// Test that IP addresses in device names work correctly.
#[test]
fn test_ip_address_device_names() {
    let devices = [
        "192.168.1.1",
        "10.0.0.1",
        "172.16.0.1",
        "2001:db8::1", // IPv6 with colons - might need encoding
    ];

    for device in devices {
        // v1 chunks slug the separators away (RFC 03 §2) — the slug must be
        // a single valid chunk with no dots or colons.
        let slug = device.replace(['.', ':'], "-");
        assert!(!slug.contains(['.', ':', '/']));
    }
}

/// Test SNMP interface index in metric paths (v1 proxy shape).
#[test]
fn test_interface_index_metrics() {
    let prefix = zensight_sensor_core::v1::v1_telemetry_prefix("zensight/snmp");
    for idx in 1..=10 {
        let key = format!("{prefix}/switch/if/{idx}/ifInOctets");
        assert!(key.contains(&format!("if/{}/", idx)));
    }
}
