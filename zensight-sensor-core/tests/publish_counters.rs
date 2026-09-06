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
use zensight_sensor_core::{AdvancedPublisherConfig, AdvancedPublisherRegistry, Publisher};

fn isolated_config() -> zenoh::Config {
    let mut c = zenoh::Config::default();
    c.insert_json5("mode", r#""peer""#).unwrap();
    c.insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    c.insert_json5("scouting/gossip/enabled", "false").unwrap();
    c.insert_json5("listen/endpoints", r#"["tcp/127.0.0.1:0"]"#)
        .unwrap();
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
    )
    .with_counters(counters.clone());
    let point = TelemetryPoint::new(
        "host",
        zensight_common::Protocol::Sysinfo,
        "system/uptime",
        TelemetryValue::Gauge(1.0),
    );
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
