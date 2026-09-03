//! An operator's threshold rule over a **polled device's** metrics (#931,
//! epic #901).
//!
//! Two properties are under test, and both are about a proxy specifically.
//!
//! First, the wiring: an `SnmpPoller` builds its own `PublisherRegistry` — one
//! per device, inside `SnmpPoller::new` — and until #931 encoded each point
//! itself and called `put`. An evaluator installed on `runner.publisher()`
//! would have reported itself installed, served both procedures, accepted
//! rules over `@rpc`, and evaluated nothing this sensor polls.
//!
//! Second, the vocabulary: `ThresholdsConfig` has no notion of a proxy. It
//! matches on labels, and `source` is a label — so "this one device" and
//! "every device" are the same mechanism with and without one line. That is
//! what a per-device rule table would have made a special case of.

mod harness;

use std::sync::Arc;
use std::time::Duration;

use harness::{IF_TABLE, SimAgent, SimMib, isolated_zenoh_config, rig, v2c_device};
use zensight_common::comparison::ComparisonOp;
use zensight_common::threshold::{ThresholdRule, ThresholdsConfig};
use zensight_common::{Alert, AlertState, Format, Protocol, decode_auto};
use zensight_sensor_core::{AlertReporter, Publisher, threshold::ThresholdEvaluator};

/// `ifInErrors.1`, which `base_mib` seeds and this test drives above a rule.
const IF_IN_ERRORS_1: &str = "1.3.6.1.2.1.2.2.1.14.1";

fn rules(labels: &[(&str, &str)]) -> ThresholdsConfig {
    let mut rule = ThresholdRule::new(
        "if-errors",
        "if/*/in_errors",
        ComparisonOp::GreaterThan,
        100.0,
    );
    for (k, v) in labels {
        rule.labels.insert((*k).to_string(), (*v).to_string());
    }
    ThresholdsConfig {
        rules: vec![rule],
        ..Default::default()
    }
}

/// Poll once with `rules` installed on the poller's own registry, and return
/// the first alert to reach `state/snmp/alert/*` — or `None` if none does.
async fn poll_and_wait(device_name: &str, config: ThresholdsConfig) -> Option<Alert> {
    let mib = SimMib::new().with_system_group().with_if_table(2);
    // Well over the rule's 100.
    mib.set(IF_IN_ERRORS_1, async_snmp::Value::Counter32(4_242));
    let agent = SimAgent::start(mib).await;

    let mut device = v2c_device(device_name, agent.addr());
    device.oids = vec![format!("{IF_TABLE}.14.1")];

    let mut rig = rig(device).await;
    let alert_sub = rig
        .session
        .declare_subscriber("v1/*/state/snmp/alert/*")
        .await
        .expect("declare alert subscriber");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let publisher = Publisher::new(rig.session.clone(), "snmp", Format::Json);
    let reporter = Arc::new(AlertReporter::new(publisher, Protocol::Snmp, Format::Json));
    let (evaluator, task) = ThresholdEvaluator::new(config, Protocol::Snmp, reporter);
    let handle = tokio::spawn(task);
    rig.poller.with_thresholds(evaluator);

    rig.poller.poll_once().await.expect("poll");

    let got = tokio::time::timeout(Duration::from_secs(3), alert_sub.recv_async())
        .await
        .ok()
        .and_then(|s| s.ok())
        .map(|s| decode_auto::<Alert>(&s.payload().to_bytes()).expect("decode alert"));
    handle.abort();
    agent.shutdown();
    let _ = isolated_zenoh_config(); // keeps the import honest on every cfg
    got
}

/// The wiring, and the "all devices" form of the rule: no `source` label, so
/// it matches whatever this proxy polls.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rule_fires_through_the_pollers_own_registry() {
    let alert = poll_and_wait("router01", rules(&[])).await.expect(
        "a polled point above the rule produced no alert — the evaluator is not on the \
                 registry this poller publishes through",
    );
    assert_eq!(alert.rule, "threshold:if-errors");
    assert_eq!(alert.state, AlertState::Firing);
    // #883: the alert is filed under the POLLED DEVICE, not the polling host.
    assert_eq!(alert.source, "router01");
    assert!(
        alert.labels["metric"].ends_with("in_errors"),
        "{:?}",
        alert.labels
    );
}

/// The "this one device" form. `source` is a label like any other, so naming
/// one device costs one line and needs nothing in the vocabulary that knows
/// what a proxy is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_source_label_scopes_a_rule_to_one_device() {
    let matched = poll_and_wait("router01", rules(&[("source", "router01")])).await;
    assert!(
        matched.is_some(),
        "a rule naming this device did not fire on it"
    );

    let other = poll_and_wait("router01", rules(&[("source", "switch-99")])).await;
    assert!(
        other.is_none(),
        "a rule naming a DIFFERENT device fired on this one: {other:?}"
    );
}
