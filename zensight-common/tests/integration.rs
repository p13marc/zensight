//! Integration tests for zensight-common library.

use std::collections::HashMap;
use zensight_common::{
    Format, Protocol, TelemetryPoint, TelemetryValue, all_telemetry_wildcard, decode, decode_auto,
    encode,
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
fn test_wildcard_key_expressions() {
    // v1: the telemetry class selector (RFC 04 §4).
    let all = all_telemetry_wildcard();
    assert_eq!(all, "v1/*/telemetry/**");

    // A per-protocol narrowing keeps one `*` for the origin (RFC 09 §1).
    let netring_all = "v1/*/telemetry/netring/**";
    let one = zenoh::key_expr::KeyExpr::try_from("v1/h-3fa9c2d41b7e/telemetry/netring/flow/count")
        .unwrap();
    assert!(
        zenoh::key_expr::KeyExpr::try_from(netring_all)
            .unwrap()
            .intersects(&one)
    );
}

#[test]
fn test_all_protocol_variants() {
    let protocols = [
        (Protocol::Snmp, "snmp"),
        (Protocol::Logs, "logs"),
        (Protocol::Gnmi, "gnmi"),
        (Protocol::Netflow, "netflow"),
        (Protocol::Opcua, "opcua"),
        (Protocol::Modbus, "modbus"),
        (Protocol::Systemd, "systemd"),
        (Protocol::Parallax, "parallax"),
    ];

    for (protocol, expected_str) in protocols {
        assert_eq!(protocol.as_str(), expected_str);
        assert_eq!(format!("{}", protocol), expected_str);
    }
}

#[test]
fn test_all_telemetry_value_types() {
    let values = [
        (TelemetryValue::Counter(42), "counter"),
        (TelemetryValue::Gauge(2.5), "gauge"),
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
    let mut protocols = [
        Protocol::Modbus,
        Protocol::Snmp,
        Protocol::Gnmi,
        Protocol::Logs,
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
