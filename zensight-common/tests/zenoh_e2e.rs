//! End-to-end tests with Zenoh pub/sub.
//!
//! These tests verify that telemetry can be published and received through Zenoh.
//!
//! Note: Zenoh requires multi-thread tokio runtime.
//!
//! ## Isolation — two mechanisms, and both are needed (#785)
//!
//! Each test uses a unique key prefix. That was the only mechanism until #785,
//! and it is not sufficient: the sessions opened here were plain
//! `zenoh::Config::default()`, i.e. peer mode with **multicast scouting and
//! gossip both on**, so four sessions in one process discovered each other —
//! along with `publisher_registry.rs`'s, and any live sensor on the host. Under
//! `cargo test --workspace` on a loaded machine, this file's telemetry test
//! would occasionally receive the CBOR test's sample and fail with
//! `left: "cbor-device", right: "test-device"`.
//!
//! The prefixes were never the bug — a `test_<nanos>/**` subscription cannot
//! match a sibling's tree. The session layer was. [`isolated_config`] closes
//! it, exactly as `publisher_registry.rs` and `router_storage.rs` in this same
//! directory already did.
//!
//! These sessions are deliberately raw — no namespace, no v1 grammar, no forced
//! timestamping — because what is under test is the transport and the
//! serialization, not `zensight_common::session`. Nothing here should be read
//! as evidence about the session this crate hands to production code.

use std::sync::Arc;
use std::time::Duration;
use zensight_common::{Format, Protocol, TelemetryPoint, TelemetryValue, decode_auto, encode};

/// A Zenoh config that cannot find another peer: multicast scouting and gossip
/// both off, and no endpoints (#785).
///
/// Local declare/put/subscribe within one session still works, which is all
/// these tests need. Same helper, same reason, as `publisher_registry.rs:9-19`
/// and `router_storage.rs`.
fn isolated_config() -> zenoh::Config {
    let mut config = zenoh::Config::default();
    config
        .insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    config
        .insert_json5("scouting/gossip/enabled", "false")
        .unwrap();
    config
}

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

    // An isolated session: no peer can reach it, and it can reach no peer.
    let session = zenoh::open(isolated_config())
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

    let session = zenoh::open(isolated_config())
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
        TelemetryValue::Gauge(2.5),
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
        assert!((v - 2.5).abs() < 0.0001);
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

    let session = zenoh::open(isolated_config())
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

    let session = Arc::new(
        zenoh::open(isolated_config())
            .await
            .expect("Failed to open session"),
    );

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
