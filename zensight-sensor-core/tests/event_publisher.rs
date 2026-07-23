//! `EventPublisher` over an in-process Zenoh peer (#534): events-class
//! records land on `v1/<origin>/events/<producer>/<subject...>/<id>` with
//! one key per record (append-only, never overwriting).

use std::sync::Arc;
use std::time::Duration;

use zensight_common::{AlertSeverity, EventRecord, Format, Protocol, decode_auto};
use zensight_sensor_core::{EventPublisher, Publisher};

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
async fn events_are_appended_under_their_ids() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let sub = session
        .declare_subscriber("v1/*/events/snmp/**")
        .await
        .expect("subscriber");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let publisher = Publisher::new(session.clone(), "snmp", Format::Json);
    let events = EventPublisher::new(publisher);

    let first = EventRecord::new(
        "router01",
        Protocol::Snmp,
        "trap/link_down",
        AlertSeverity::Warning,
        "eth0 went down",
    )
    .with_field("if_index", "1");
    let second = EventRecord::new(
        "router01",
        Protocol::Snmp,
        "trap/link_up",
        AlertSeverity::Info,
        "eth0 came back",
    );

    events
        .publish(&["router01", "trap"], &first)
        .await
        .expect("publish first");
    events
        .publish(&["router01", "trap"], &second)
        .await
        .expect("publish second");

    let mut keys = Vec::new();
    for expected in [&first, &second] {
        let sample = tokio::time::timeout(Duration::from_secs(5), sub.recv_async())
            .await
            .expect("recv timed out")
            .expect("recv");
        let got: EventRecord = decode_auto(&sample.payload().to_bytes()).expect("decode");
        assert_eq!(got.id, expected.id);
        assert_eq!(got.kind, expected.kind);
        let key = sample.key_expr().to_string();
        assert!(
            key.ends_with(&format!("events/snmp/router01/trap/{}", expected.id)),
            "unexpected key {key}"
        );
        keys.push(key);
    }
    // Append-only: two records, two distinct keys.
    assert_ne!(keys[0], keys[1]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_subject_chunks_error_instead_of_panicking() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let publisher = Publisher::new(session, "snmp", Format::Json);
    let events = EventPublisher::new(publisher);

    let record = EventRecord::new(
        "dev",
        Protocol::Snmp,
        "k",
        AlertSeverity::Info,
        "bad chunk test",
    );
    let result = events.publish(&["Not A Chunk!"], &record).await;
    assert!(result.is_err(), "invalid chunk must be a publish error");
}
