//! End-to-end tests with Zenoh pub/sub.
//!
//! These tests verify that telemetry can be published and received through Zenoh.
//!
//! Note: Zenoh requires multi-thread tokio runtime.
//! Each test uses a unique key prefix to avoid interference.

use std::sync::Arc;
use std::time::Duration;
use zensight_common::{decode_auto, encode, Format, Protocol, TelemetryPoint, TelemetryValue};

/// Generate a unique test prefix to avoid test interference.
fn unique_prefix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("test_{}", nanos)
}

/// Test publishing and subscribing to telemetry through Zenoh.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_zenoh_pubsub_telemetry() {
    let prefix = unique_prefix();

    // Create a Zenoh session in peer mode
    let config = zenoh::Config::default();
    let session = zenoh::open(config)
        .await
        .expect("Failed to open Zenoh session");

    // Create a subscriber for this test's prefix
    let key_expr = format!("{}/**", prefix);
    let subscriber = session
        .declare_subscriber(&key_expr)
        .await
        .expect("Failed to create subscriber");

    // Give subscriber time to set up
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create and publish a telemetry point
    let point = TelemetryPoint::new(
        "test-device",
        Protocol::Snmp,
        "test/metric",
        TelemetryValue::Counter(42),
    );

    let publish_key = format!("{}/snmp/test-device/test/metric", prefix);
    let encoded = encode(&point, Format::Json).expect("Failed to encode");

    session
        .put(&publish_key, encoded.clone())
        .await
        .expect("Failed to publish");

    // Receive the message
    let received = tokio::time::timeout(Duration::from_secs(5), subscriber.recv_async())
        .await
        .expect("Timeout waiting for message")
        .expect("Failed to receive message");

    // Decode and verify
    let payload = received.payload().to_bytes();
    let decoded: TelemetryPoint = decode_auto(&payload).expect("Failed to decode");

    assert_eq!(decoded.source, "test-device");
    assert_eq!(decoded.protocol, Protocol::Snmp);
    assert_eq!(decoded.metric, "test/metric");
    assert_eq!(decoded.value, TelemetryValue::Counter(42));

    // Clean up
    drop(subscriber);
    session.close().await.expect("Failed to close session");
}

/// Test that CBOR-encoded messages can be received and decoded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_zenoh_cbor_encoding() {
    let prefix = unique_prefix();

    let config = zenoh::Config::default();
    let session = zenoh::open(config)
        .await
        .expect("Failed to open Zenoh session");

    let key_expr = format!("{}/**", prefix);
    let subscriber = session
        .declare_subscriber(&key_expr)
        .await
        .expect("Failed to create subscriber");

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Publish CBOR-encoded telemetry
    let point = TelemetryPoint::new(
        "cbor-device",
        Protocol::Snmp,
        "cbor/metric",
        TelemetryValue::Gauge(3.14159),
    );

    let publish_key = format!("{}/snmp/cbor-device/cbor/metric", prefix);
    let encoded = encode(&point, Format::Cbor).expect("Failed to encode CBOR");

    session
        .put(&publish_key, encoded)
        .await
        .expect("Failed to publish");

    let received = tokio::time::timeout(Duration::from_secs(5), subscriber.recv_async())
        .await
        .expect("Timeout")
        .expect("Failed to receive");

    let payload = received.payload().to_bytes();
    let decoded: TelemetryPoint = decode_auto(&payload).expect("Failed to auto-decode CBOR");

    assert_eq!(decoded.source, "cbor-device");
    assert_eq!(decoded.metric, "cbor/metric");
    if let TelemetryValue::Gauge(v) = decoded.value {
        assert!((v - 3.14159).abs() < 0.0001);
    } else {
        panic!("Expected Gauge value");
    }

    drop(subscriber);
    session.close().await.expect("Failed to close session");
}

/// Test subscribing with protocol-specific wildcard.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_zenoh_protocol_wildcard() {
    let prefix = unique_prefix();

    let config = zenoh::Config::default();
    let session = zenoh::open(config)
        .await
        .expect("Failed to open Zenoh session");

    // Subscribe only to SNMP telemetry within our prefix
    let snmp_wildcard = format!("{}/snmp/**", prefix);
    let subscriber = session
        .declare_subscriber(&snmp_wildcard)
        .await
        .expect("Failed to create subscriber");

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Publish SNMP telemetry (should be received)
    let snmp_point = TelemetryPoint::new(
        "snmp-device",
        Protocol::Snmp,
        "metric",
        TelemetryValue::Counter(1),
    );
    let snmp_key = format!("{}/snmp/snmp-device/metric", prefix);
    let encoded = encode(&snmp_point, Format::Json).unwrap();
    session.put(&snmp_key, encoded).await.unwrap();

    // Should receive the SNMP message
    let received = tokio::time::timeout(Duration::from_secs(2), subscriber.recv_async())
        .await
        .expect("Should receive SNMP message")
        .unwrap();

    let payload = received.payload().to_bytes();
    let decoded: TelemetryPoint = decode_auto(&payload).unwrap();
    assert_eq!(decoded.protocol, Protocol::Snmp);

    drop(subscriber);
    session.close().await.expect("Failed to close session");
}

/// Test multiple concurrent publishers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_zenoh_multiple_publishers() {
    let prefix = unique_prefix();

    let config = zenoh::Config::default();
    let session = Arc::new(zenoh::open(config).await.expect("Failed to open session"));

    let key_expr = format!("{}/**", prefix);
    let subscriber = session
        .declare_subscriber(&key_expr)
        .await
        .expect("Failed to create subscriber");

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Publish from multiple "devices"
    let devices = ["device1", "device2", "device3"];
    for device in &devices {
        let point = TelemetryPoint::new(
            *device,
            Protocol::Snmp,
            "metric",
            TelemetryValue::Counter(1),
        );
        let key = format!("{}/snmp/{}/metric", prefix, device);
        let encoded = encode(&point, Format::Json).unwrap();
        session.put(&key, encoded).await.unwrap();
        // Small delay between publishes to ensure ordering
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Receive all messages with longer timeout
    let mut received_devices = std::collections::HashSet::new();
    for _ in 0..3 {
        match tokio::time::timeout(Duration::from_secs(5), subscriber.recv_async()).await {
            Ok(Ok(received)) => {
                let payload = received.payload().to_bytes();
                let decoded: TelemetryPoint = decode_auto(&payload).unwrap();
                received_devices.insert(decoded.source);
            }
            Ok(Err(e)) => panic!("Receive error: {}", e),
            Err(_) => break, // Timeout, check what we have
        }
    }

    // We should have received at least some messages
    assert!(
        !received_devices.is_empty(),
        "Should receive at least one message"
    );
    // In a local peer mode, we should receive all 3
    assert_eq!(
        received_devices.len(),
        3,
        "Should receive all 3 device messages"
    );

    drop(subscriber);
    session.close().await.expect("Failed to close session");
}
