//! Integration tests for `PublisherRegistry` over an in-process Zenoh peer:
//! publishers are *declared once per key* and reused, never re-declared per put.

use std::sync::Arc;

use zensight_common::{PublisherRegistry, QosClass};

/// Standalone Zenoh config with scouting disabled so concurrent test peers don't
/// discover each other. Local declare/put within one session still works.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn declares_once_per_key_and_reuses() {
    let session = Arc::new(zenoh::open(isolated_config()).await.unwrap());
    let registry = PublisherRegistry::new(session);

    assert!(registry.is_empty().await);

    // Two puts on the same key → one declared publisher.
    registry
        .put("zensight/test/a", b"one".to_vec(), QosClass::Telemetry)
        .await
        .unwrap();
    registry
        .put("zensight/test/a", b"two".to_vec(), QosClass::Telemetry)
        .await
        .unwrap();
    assert_eq!(registry.len().await, 1, "same key must reuse one publisher");

    // A distinct key declares a second publisher.
    registry
        .put("zensight/test/b", b"x".to_vec(), QosClass::Alert)
        .await
        .unwrap();
    assert_eq!(registry.len().await, 2);

    // Delete on an already-declared key reuses its publisher (no growth).
    registry
        .delete("zensight/test/a", QosClass::Alert)
        .await
        .unwrap();
    assert_eq!(registry.len().await, 2);
}
