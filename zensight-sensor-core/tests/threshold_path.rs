//! The threshold evaluator, from a published point to an alert on the bus
//! (#930, epic #901).
//!
//! The unit tests in `threshold.rs` cover the state machine. These cover the
//! thing that was actually wrong in the epic's plan: **which publish paths a
//! point can take**. `Publisher::publish` appears zero times in sysinfo,
//! netlink, netring, snmp and logs combined, so a hook on it alone would have
//! covered the smallest share of the fleet's telemetry while looking complete.
//! Both paths are exercised here against a real bus.

use std::sync::Arc;
use std::time::Duration;

use zensight_common::comparison::ComparisonOp;
use zensight_common::threshold::{ThresholdRule, ThresholdsConfig};
use zensight_common::{
    Alert, AlertState, Format, Protocol, TelemetryPoint, TelemetryValue, decode_auto,
};
use zensight_sensor_core::threshold::ThresholdEvaluator;
use zensight_sensor_core::{
    AdvancedPublisherConfig, AdvancedPublisherRegistry, AlertReporter, Publisher,
};

/// Multicast scouting OFF. A default-config session joins whatever mesh it can
/// reach — including a live fleet on the same host — so a test that scouts is
/// not a test, it is a participant (RFC 09 §0.1).
fn isolated_config() -> zenoh::Config {
    let mut config = zenoh::Config::default();
    config
        .insert_json5("scouting/multicast/enabled", "false")
        .expect("disable multicast scouting");
    config
        .insert_json5("timestamping/enabled", "true")
        .expect("enable timestamping");
    config
}

fn unique_source() -> String {
    format!(
        "host-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

fn rules() -> ThresholdsConfig {
    ThresholdsConfig {
        rules: vec![ThresholdRule::new(
            "cpu-hot",
            "cpu/usage",
            ComparisonOp::GreaterThan,
            90.0,
        )],
        ..Default::default()
    }
}

fn point(source: &str, value: f64) -> TelemetryPoint {
    TelemetryPoint::new(
        source,
        Protocol::Sysinfo,
        "cpu/usage",
        TelemetryValue::Gauge(value),
    )
}

async fn next_alert(
    sub: &zenoh::pubsub::Subscriber<zenoh::handlers::FifoChannelHandler<zenoh::sample::Sample>>,
) -> (zenoh::sample::SampleKind, Option<Alert>) {
    let s = tokio::time::timeout(Duration::from_secs(5), sub.recv_async())
        .await
        .expect("no alert arrived")
        .expect("recv");
    let kind = s.kind();
    let alert = (kind == zenoh::sample::SampleKind::Put)
        .then(|| decode_auto::<Alert>(&s.payload().to_bytes()).expect("decode alert"));
    (kind, alert)
}

/// **The whole point of the epic**, over a real bus: a sensor publishes a
/// metric, and an alert an *operator wrote as a threshold* appears on the bus
/// — where the exporters, the notifier and every GUI can see it. The old
/// engine's alerts reached none of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_published_point_becomes_an_alert_on_the_bus() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let sub = session
        .declare_subscriber("v1/*/state/sysinfo/alert/*")
        .await
        .expect("subscriber");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let publisher = Publisher::new(session.clone(), "sysinfo", Format::Json);
    let reporter = Arc::new(AlertReporter::new(
        publisher.clone(),
        Protocol::Sysinfo,
        Format::Json,
    ));
    let (evaluator, task) = ThresholdEvaluator::new(rules(), Protocol::Sysinfo, reporter.clone());
    publisher.set_observer(evaluator);
    let handle = tokio::spawn(task);

    let source = unique_source();

    // Under the threshold: nothing at all.
    publisher
        .publish("cpu/usage", &point(&source, 12.0))
        .await
        .expect("publish");
    let quiet = tokio::time::timeout(Duration::from_millis(600), sub.recv_async()).await;
    assert!(quiet.is_err(), "a healthy value must not alert");

    // Over it: an alert, on the bus, with the rule and the value in it.
    publisher
        .publish("cpu/usage", &point(&source, 96.0))
        .await
        .expect("publish");
    let (kind, alert) = next_alert(&sub).await;
    assert_eq!(kind, zenoh::sample::SampleKind::Put);
    let alert = alert.unwrap();
    assert_eq!(alert.rule, "threshold:cpu-hot");
    assert_eq!(alert.state, AlertState::Firing);
    assert_eq!(alert.source, source);
    assert_eq!(alert.labels["metric"], "cpu/usage");
    assert!(alert.summary.contains("96"), "{}", alert.summary);

    // …and back under it resolves, with the tombstone.
    publisher
        .publish("cpu/usage", &point(&source, 5.0))
        .await
        .expect("publish");
    let mut saw_resolved = false;
    let mut saw_delete = false;
    for _ in 0..2 {
        match next_alert(&sub).await {
            (zenoh::sample::SampleKind::Put, Some(a)) => {
                assert_eq!(a.state, AlertState::Resolved);
                saw_resolved = true;
            }
            (zenoh::sample::SampleKind::Delete, _) => saw_delete = true,
            other => panic!("unexpected sample {other:?}"),
        }
    }
    assert!(saw_resolved && saw_delete);

    handle.abort();
}

/// **The path the epic missed.** netlink, netring, snmp and logs publish the
/// bulk of their telemetry through `AdvancedPublisherRegistry`, which has its
/// own publisher cache and its own encode and never touches `Publisher`. It
/// carries the observer too, or thresholds would silently not work on the four
/// highest-volume sensors in the tree.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_advanced_publisher_path_evaluates_too() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let sub = session
        .declare_subscriber("v1/*/state/sysinfo/alert/*")
        .await
        .expect("subscriber");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let publisher = Publisher::new(session.clone(), "sysinfo", Format::Json);
    let reporter = Arc::new(AlertReporter::new(
        publisher.clone(),
        Protocol::Sysinfo,
        Format::Json,
    ));
    let advanced = Arc::new(AdvancedPublisherRegistry::new(
        session.clone(),
        zensight_sensor_core::v1::for_producer("sysinfo").telemetry_prefix(),
        Format::Json,
        AdvancedPublisherConfig::cache_only(1),
    ));

    let (evaluator, task) = ThresholdEvaluator::new(rules(), Protocol::Sysinfo, reporter.clone());
    advanced.set_observer(evaluator);
    let handle = tokio::spawn(task);

    let source = unique_source();
    advanced
        .publish("cpu/usage", &point(&source, 99.0))
        .await
        .expect("publish");

    let (kind, alert) = next_alert(&sub).await;
    assert_eq!(kind, zenoh::sample::SampleKind::Put);
    assert_eq!(alert.unwrap().rule, "threshold:cpu-hot");

    handle.abort();
}

/// A registry with no observer installed behaves exactly as it did — the
/// property that lets this land without touching any sensor's behaviour until
/// #931 opts each one in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_registry_with_no_observer_publishes_unchanged() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let sub = session
        .declare_subscriber("v1/*/telemetry/sysinfo/**")
        .await
        .expect("subscriber");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let publisher = Publisher::new(session.clone(), "sysinfo", Format::Json);
    let source = unique_source();
    publisher
        .publish("cpu/usage", &point(&source, 99.0))
        .await
        .expect("publish");

    let s = tokio::time::timeout(Duration::from_secs(5), sub.recv_async())
        .await
        .expect("the point still reaches the bus")
        .expect("recv");
    let got: TelemetryPoint = decode_auto(&s.payload().to_bytes()).expect("decode point");
    assert_eq!(got.metric, "cpu/usage");
    assert_eq!(got.source, source);
}

/// A sensor whose rule set arrives later — over `@desired` or `@rpc` (#931) —
/// evaluates from the moment it does, without a restart. The observer is
/// installed regardless; the empty set costs one relaxed atomic load per point.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rule_set_pushed_at_runtime_takes_effect_without_a_restart() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let sub = session
        .declare_subscriber("v1/*/state/sysinfo/alert/*")
        .await
        .expect("subscriber");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let publisher = Publisher::new(session.clone(), "sysinfo", Format::Json);
    let reporter = Arc::new(AlertReporter::new(
        publisher.clone(),
        Protocol::Sysinfo,
        Format::Json,
    ));
    // Starts with NO rules — the shape every sensor ships in.
    let (evaluator, task) =
        ThresholdEvaluator::new(ThresholdsConfig::default(), Protocol::Sysinfo, reporter);
    publisher.set_observer(evaluator.clone());
    let handle = tokio::spawn(task);

    let source = unique_source();
    publisher
        .publish("cpu/usage", &point(&source, 99.0))
        .await
        .expect("publish");
    let quiet = tokio::time::timeout(Duration::from_millis(600), sub.recv_async()).await;
    assert!(quiet.is_err(), "no rules, no alerts");

    // An operator pushes a rule.
    evaluator.set_config(rules());
    publisher
        .publish("cpu/usage", &point(&source, 99.0))
        .await
        .expect("publish");
    let (_, alert) = next_alert(&sub).await;
    assert_eq!(alert.unwrap().rule, "threshold:cpu-hot");

    // …and takes it away again. The alert must not be stranded.
    evaluator.set_config(ThresholdsConfig::default());
    let mut saw_resolved = false;
    for _ in 0..2 {
        if let (zenoh::sample::SampleKind::Put, Some(a)) = next_alert(&sub).await {
            assert_eq!(a.state, AlertState::Resolved);
            saw_resolved = true;
        }
    }
    assert!(
        saw_resolved,
        "deleting a rule must retire its alerts, not orphan them"
    );

    handle.abort();
}
