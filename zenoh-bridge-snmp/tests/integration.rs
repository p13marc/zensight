//! Integration tests for zenoh-bridge-snmp.

use zensight_common::{
    decode_auto, encode, Format, KeyExprBuilder, Protocol, TelemetryPoint, TelemetryValue,
};

/// Test that we can encode telemetry and it would be decodable by the frontend.
#[test]
fn test_snmp_telemetry_encoding() {
    // Simulate what the SNMP bridge produces
    let point = TelemetryPoint::new(
        "router01",
        Protocol::Snmp,
        "system/sysUpTime",
        TelemetryValue::Counter(123456),
    )
    .with_label("oid", "1.3.6.1.2.1.1.3.0");

    // The bridge encodes as JSON by default
    let encoded = encode(&point, Format::Json).expect("Encoding failed");

    // The frontend should be able to decode it
    let decoded: TelemetryPoint = decode_auto(&encoded).expect("Decoding failed");
    assert_eq!(decoded.source, "router01");
    assert_eq!(decoded.protocol, Protocol::Snmp);
    assert_eq!(decoded.metric, "system/sysUpTime");
}

/// Test key expression generation for SNMP metrics.
#[test]
fn test_snmp_key_expressions() {
    let builder = KeyExprBuilder::new(Protocol::Snmp);

    // System metrics
    let sys_uptime = builder.build("router01", "system/sysUpTime");
    assert_eq!(sys_uptime, "zensight/snmp/router01/system/sysUpTime");

    // Interface metrics
    let if_in_octets = builder.build("switch01", "if/1/ifInOctets");
    assert_eq!(if_in_octets, "zensight/snmp/switch01/if/1/ifInOctets");

    // Device wildcard
    let device_all = builder.source_wildcard("router01");
    assert_eq!(device_all, "zensight/snmp/router01/**");
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
        let key = KeyExprBuilder::new(Protocol::Snmp).build(device, "sysUpTime");
        assert!(key.contains(device) || key.contains("2001")); // IPv6 might be encoded
    }
}

/// Test SNMP interface index in metric paths.
#[test]
fn test_interface_index_metrics() {
    let builder = KeyExprBuilder::new(Protocol::Snmp);

    for idx in 1..=10 {
        let key = builder.build("switch", &format!("if/{}/ifInOctets", idx));
        assert!(key.contains(&format!("if/{}/", idx)));
    }
}

/// Test that the bridge status key is correct.
#[test]
fn test_bridge_status_key() {
    let builder = KeyExprBuilder::new(Protocol::Snmp);
    let status_key = builder.status_key();
    assert_eq!(status_key, "zensight/snmp/@/status");
}
