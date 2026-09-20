//! The health doc's `published_total` counts what the sensor actually
//! publishes (#1078, #1079).
//!
//! Two ways it read zero or near-zero for a whole process lifetime:
//! `SensorRunner::with_format` built a new `Publisher` whose fresh registry
//! had fresh counters while the health tracker kept the old ones (eleven
//! sensors); and the advanced tier — the bulk telemetry path for netlink,
//! netring, snmp and logs — counted nothing at all.

use std::sync::Arc;
use zensight_common::{Format, TelemetryPoint, TelemetryValue};
use zensight_sensor_core::{
    AdvancedPublisherConfig, AdvancedPublisherRegistry, Publish, Publisher,
};

fn isolated_config() -> zenoh::Config {
    let mut c = zenoh::Config::default();
    c.insert_json5("mode", r#""peer""#).unwrap();
    c.insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    c.insert_json5("scouting/gossip/enabled", "false").unwrap();
    c.insert_json5("listen/endpoints", r#"["tcp/127.0.0.1:0"]"#)
        .unwrap();
    // A cache-only advanced publisher sequences by timestamp.
    c.insert_json5("timestamping/enabled", "true").unwrap();
    c
}

/// Re-formatting a publisher keeps its registry, hence its counters — the
/// property `SensorRunner::with_format` relies on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_format_keeps_the_counters() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let base = Publisher::new(session.clone(), "sysinfo", Format::Json);
    let counters = base.counters();
    let reformatted = base.clone().with_format(Format::Cbor);
    assert!(
        Arc::ptr_eq(&counters, &reformatted.counters()),
        "with_format must not mint a new counter set"
    );
    assert_eq!(reformatted.format(), Format::Cbor);
}

/// The advanced tier counts its deliveries into the shared set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_advanced_tier_counts_into_the_shared_set() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let base = Publisher::new(session.clone(), "sysinfo", Format::Json);
    let counters = base.counters();
    let registry = AdvancedPublisherRegistry::new(
        session.clone(),
        base.telemetry_prefix(),
        Format::Json,
        AdvancedPublisherConfig::default(),
        counters.clone(),
    );
    let point = TelemetryPoint::new("host", "system/uptime", TelemetryValue::Gauge(1.0));
    registry
        .publish("system/uptime", &point)
        .await
        .expect("publish");
    registry
        .publish("system/uptime", &point)
        .await
        .expect("publish again");
    assert_eq!(counters.published_total(), 2, "both deliveries counted");
    assert!(counters.published_bytes_total() > 0);
}

/// The advanced tier records the class a key was declared with and reports a
/// second class on the same key (#1155) — the rule the baseline tier had and
/// this one did not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[should_panic(expected = "one key, one class")]
async fn a_second_class_on_the_advanced_tier_is_caught() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let registry = AdvancedPublisherRegistry::new(
        session.clone(),
        zensight_sensor_core::v1::for_producer("sysinfo").telemetry_prefix(),
        Format::Json,
        AdvancedPublisherConfig::cache_only(1),
        Default::default(),
    );
    let key = "v1/h-0123456789ab/state/sysinfo/pubtrait/a";
    registry
        .publish_serializable(key, &serde_json::json!({"a": 1}))
        .await
        .expect("first put declares under Telemetry");
    // Through the trait a caller can ask for another class; it is reported.
    let _ = Publish::put_encoded(
        &registry,
        key,
        b"{}".to_vec(),
        zensight_common::QosClass::Alert,
        Format::Json.encoding(),
    )
    .await;
}

/// The advanced tier's tombstone runs the registry guard (#1155) — it used to
/// be one of three paths on that tier that skipped it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[should_panic(expected = "unregistered telemetry subject")]
async fn an_advanced_tombstone_is_guarded() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let registry = AdvancedPublisherRegistry::new(
        session.clone(),
        zensight_sensor_core::v1::for_producer("sysinfo").telemetry_prefix(),
        Format::Json,
        AdvancedPublisherConfig::cache_only(1),
        Default::default(),
    );
    let _ = registry
        .tombstone("v1/h-0123456789ab/telemetry/sysinfo/not/a/real/metric")
        .await;
}

/// `publish_to_key` on the advanced tier is guarded too (#1155): the guard
/// used to run only when the tier built the key itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[should_panic(expected = "unregistered telemetry subject")]
async fn an_advanced_full_key_put_is_guarded() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let registry = AdvancedPublisherRegistry::new(
        session.clone(),
        zensight_sensor_core::v1::for_producer("sysinfo").telemetry_prefix(),
        Format::Json,
        AdvancedPublisherConfig::cache_only(1),
        Default::default(),
    );
    let point = TelemetryPoint::new("host", "not/a/real/metric", TelemetryValue::Gauge(1.0));
    let _ = registry
        .publish_to_key(
            "v1/h-0123456789ab/telemetry/sysinfo/not/a/real/metric",
            &point,
        )
        .await;
}

/// A `RelationSet` counts into the set it is given (#1155) — it used to mint
/// its own, so every relation claim in the tree went uncounted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_relation_set_counts_into_the_shared_set() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let base = Publisher::new(session.clone(), "probe", Format::Json);
    let counters = base.counters();
    let mut relations = zensight_sensor_core::relation::RelationSet::new(
        "probe",
        session.clone(),
        Format::Json,
        counters.clone(),
    );
    let claim = zensight_common::relation::RelationshipEvidence {
        sensor: "probe".to_string(),
        source: "host1".to_string(),
        kind: zensight_common::relation::RelationKind::Hosts,
        from: zensight_common::relation::EndpointClaim::host("h-0123456789ab"),
        to: zensight_common::relation::EndpointClaim {
            device: Some("vm1".into()),
            name: Some("vm1".into()),
            ..Default::default()
        },
        attrs: Default::default(),
        last_updated: 1_700_000_000_000,
    };
    let outcome = relations.sync(&[claim]).await;
    assert_eq!(outcome.published, 1, "{outcome:?}");
    assert_eq!(
        counters.published_total(),
        1,
        "the claim was counted into the shared set"
    );
}
