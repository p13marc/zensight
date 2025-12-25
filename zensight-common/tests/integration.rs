//! Integration tests for zensight-common library.

use std::collections::HashMap;
use zensight_common::{
    all_telemetry_wildcard, decode, decode_auto, encode, parse_key_expr, Format, KeyExprBuilder,
    Protocol, TelemetryPoint, TelemetryValue,
};

#[test]
fn test_full_telemetry_workflow() {
    // Create a telemetry point
    let point = TelemetryPoint::new(
        "router01",
        Protocol::Snmp,
        "system/sysUpTime",
        TelemetryValue::Counter(123456789),
    )
    .with_label("oid", "1.3.6.1.2.1.1.3.0")
    .with_label("community", "public");

    // Encode as JSON
    let json_bytes = encode(&point, Format::Json).expect("JSON encode failed");
    assert!(!json_bytes.is_empty());

    // Decode from JSON
    let decoded: TelemetryPoint = decode(&json_bytes, Format::Json).expect("JSON decode failed");
    assert_eq!(decoded.source, "router01");
    assert_eq!(decoded.protocol, Protocol::Snmp);
    assert_eq!(decoded.metric, "system/sysUpTime");
    assert_eq!(decoded.value, TelemetryValue::Counter(123456789));
    assert_eq!(
        decoded.labels.get("oid"),
        Some(&"1.3.6.1.2.1.1.3.0".to_string())
    );

    // Encode as CBOR
    let cbor_bytes = encode(&point, Format::Cbor).expect("CBOR encode failed");
    assert!(!cbor_bytes.is_empty());
    assert!(
        cbor_bytes.len() < json_bytes.len(),
        "CBOR should be smaller than JSON"
    );

    // Auto-decode CBOR
    let auto_decoded: TelemetryPoint = decode_auto(&cbor_bytes).expect("Auto decode failed");
    assert_eq!(auto_decoded.source, decoded.source);
    assert_eq!(auto_decoded.metric, decoded.metric);
}

#[test]
fn test_key_expression_building_and_parsing() {
    // Build a key expression
    let key = KeyExprBuilder::new(Protocol::Snmp).build("switch01", "if/1/ifInOctets");

    assert_eq!(key, "zensight/snmp/switch01/if/1/ifInOctets");

    // Parse it back
    let parsed = parse_key_expr(&key).expect("Parse failed");
    assert_eq!(parsed.protocol, Protocol::Snmp);
    assert_eq!(parsed.source, "switch01");
    assert_eq!(parsed.metric, "if/1/ifInOctets");
}

#[test]
fn test_wildcard_key_expressions() {
    // All telemetry wildcard
    let all = all_telemetry_wildcard();
    assert_eq!(all, "zensight/**");

    // Protocol wildcard
    let snmp_all = KeyExprBuilder::new(Protocol::Snmp).protocol_wildcard();
    assert_eq!(snmp_all, "zensight/snmp/**");

    // Source wildcard
    let router_all = KeyExprBuilder::new(Protocol::Snmp).source_wildcard("router01");
    assert_eq!(router_all, "zensight/snmp/router01/**");
}

#[test]
fn test_all_protocol_variants() {
    let protocols = [
        (Protocol::Snmp, "snmp"),
        (Protocol::Syslog, "syslog"),
        (Protocol::Gnmi, "gnmi"),
        (Protocol::Netflow, "netflow"),
        (Protocol::Opcua, "opcua"),
        (Protocol::Modbus, "modbus"),
    ];

    for (protocol, expected_str) in protocols {
        assert_eq!(protocol.as_str(), expected_str);
        assert_eq!(format!("{}", protocol), expected_str);

        // Build key and verify
        let key = KeyExprBuilder::new(protocol).build("device01", "test");
        assert!(key.contains(expected_str));
    }
}

#[test]
fn test_all_telemetry_value_types() {
    let values = [
        (TelemetryValue::Counter(42), "counter"),
        (TelemetryValue::Gauge(3.14159), "gauge"),
        (TelemetryValue::Text("hello".to_string()), "text"),
        (TelemetryValue::Boolean(true), "boolean"),
        (TelemetryValue::Binary(vec![0x01, 0x02, 0x03]), "binary"),
    ];

    for (value, _type_name) in values {
        let point = TelemetryPoint::new("test", Protocol::Snmp, "metric", value.clone());

        // Roundtrip through JSON
        let encoded = encode(&point, Format::Json).unwrap();
        let decoded: TelemetryPoint = decode(&encoded, Format::Json).unwrap();
        assert_eq!(decoded.value, value);

        // Roundtrip through CBOR
        let encoded = encode(&point, Format::Cbor).unwrap();
        let decoded: TelemetryPoint = decode(&encoded, Format::Cbor).unwrap();
        assert_eq!(decoded.value, value);
    }
}

#[test]
fn test_telemetry_with_many_labels() {
    let mut labels = HashMap::new();
    for i in 0..100 {
        labels.insert(format!("key_{}", i), format!("value_{}", i));
    }

    let point = TelemetryPoint::new(
        "device",
        Protocol::Snmp,
        "metric",
        TelemetryValue::Gauge(1.0),
    )
    .with_labels(labels.clone());

    assert_eq!(point.labels.len(), 100);

    // Roundtrip
    let encoded = encode(&point, Format::Json).unwrap();
    let decoded: TelemetryPoint = decode(&encoded, Format::Json).unwrap();
    assert_eq!(decoded.labels.len(), 100);

    for (k, v) in &labels {
        assert_eq!(decoded.labels.get(k), Some(v));
    }
}

#[test]
fn test_protocol_ordering() {
    // Protocol should be Ord for sorting
    let mut protocols = vec![
        Protocol::Modbus,
        Protocol::Snmp,
        Protocol::Gnmi,
        Protocol::Syslog,
    ];
    protocols.sort();

    // Verify they can be sorted (order is enum variant order)
    assert_eq!(protocols[0], Protocol::Snmp);
}

#[test]
fn test_large_counter_values() {
    let point = TelemetryPoint::new(
        "device",
        Protocol::Snmp,
        "ifInOctets",
        TelemetryValue::Counter(u64::MAX),
    );

    // JSON roundtrip
    let encoded = encode(&point, Format::Json).unwrap();
    let decoded: TelemetryPoint = decode(&encoded, Format::Json).unwrap();
    assert_eq!(decoded.value, TelemetryValue::Counter(u64::MAX));

    // CBOR roundtrip
    let encoded = encode(&point, Format::Cbor).unwrap();
    let decoded: TelemetryPoint = decode(&encoded, Format::Cbor).unwrap();
    assert_eq!(decoded.value, TelemetryValue::Counter(u64::MAX));
}

#[test]
fn test_special_characters_in_source() {
    let sources = ["router-01", "switch_02", "device.local", "192.168.1.1"];

    for source in sources {
        let key = KeyExprBuilder::new(Protocol::Snmp).build(source, "metric");
        let parsed = parse_key_expr(&key).unwrap();
        assert_eq!(parsed.source, source);
    }
}

#[test]
fn test_nested_metric_paths() {
    let metrics = [
        "system/sysUpTime",
        "if/1/ifInOctets",
        "ip/routing/table/entry/1",
        "deeply/nested/metric/path/value",
    ];

    for metric in metrics {
        let key = KeyExprBuilder::new(Protocol::Snmp).build("device", metric);
        let parsed = parse_key_expr(&key).unwrap();
        assert_eq!(parsed.metric, metric);
    }
}
