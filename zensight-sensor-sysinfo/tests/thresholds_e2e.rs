//! An operator's threshold rule, evaluated on **sysinfo's own publish path**
//! (#931, epic #901).
//!
//! This test exists because of a specific way the adoption can be wrong and
//! look right. sysinfo does not publish through `runner.publisher()`: its
//! collector builds its own `zensight_common::PublisherRegistry` and, until
//! this issue, encoded each point itself and called `put` — bypassing the
//! observer seam entirely. An evaluator installed on the runner's publisher
//! would have been installed, would have reported "threshold evaluator
//! installed" at startup, would have accepted rules over `@rpc`, and would
//! have evaluated **none of this sensor's 138 metric families**.
//!
//! So the assertion is not "the evaluator works" — `zensight-sensor-core`'s
//! `threshold_path` tests cover that. It is "a point that left *this crate's*
//! collector reached it".

use std::sync::Arc;
use std::time::Duration;

use zensight_common::comparison::ComparisonOp;
use zensight_common::threshold::{ThresholdRule, ThresholdsConfig};
use zensight_common::{Alert, AlertState, Format, Protocol, decode_auto};
use zensight_sensor_core::{AlertReporter, Publisher, threshold::ThresholdEvaluator};
use zensight_sensor_sysinfo::collector::SystemCollector;
use zensight_sensor_sysinfo::config::SysinfoConfig;

/// Multicast scouting OFF: a default-config session joins whatever mesh it can
/// reach — including a live fleet on this host — so a test that scouts is not a
/// test, it is a participant (RFC 09 §0.1).
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rule_fires_through_sysinfos_own_registry() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let sub = session
        .declare_subscriber("v1/*/state/sysinfo/alert/*")
        .await
        .expect("subscriber");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let source = format!(
        "host-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );

    let publisher = Publisher::new(session.clone(), "sysinfo", Format::Json);
    let reporter = Arc::new(AlertReporter::new(
        publisher.clone(),
        Protocol::Sysinfo,
        Format::Json,
    ));

    // `memory/total` on a host that is running this test is above zero, so the
    // rule is a statement about the wiring and not about the machine.
    let rules = ThresholdsConfig {
        rules: vec![ThresholdRule::new(
            "memory-present",
            "memory/total",
            ComparisonOp::GreaterThan,
            0.0,
        )],
        ..Default::default()
    };
    let (evaluator, task) = ThresholdEvaluator::new(rules, Protocol::Sysinfo, reporter);
    tokio::spawn(task);

    let mut cfg: SysinfoConfig = json5::from_str("{}").expect("the shipped defaults");
    cfg.source = source.clone();
    cfg.poll_interval_secs = 1;
    // One tick of the default families is enough, and this keeps the test off
    // sysinfo's own alert evaluator — the rule under test is the operator's.
    cfg.alerts.enabled = false;

    let collector = SystemCollector::new(source.clone(), cfg, session.clone(), Format::Json)
        .with_thresholds(evaluator);
    let handle = tokio::spawn(collector.run());

    let sample = tokio::time::timeout(Duration::from_secs(15), sub.recv_async())
        .await
        .expect(
            "sysinfo published memory/total but no threshold alert reached the bus — \
                 the evaluator is not on the registry this collector publishes through",
        )
        .expect("recv");
    let alert: Alert = decode_auto(&sample.payload().to_bytes()).expect("decode alert");
    assert_eq!(alert.rule, "threshold:memory-present");
    assert_eq!(alert.state, AlertState::Firing);
    assert_eq!(alert.source, source);
    assert_eq!(alert.labels["metric"], "memory/total");

    handle.abort();
}
