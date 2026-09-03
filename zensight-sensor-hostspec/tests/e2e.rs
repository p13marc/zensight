//! End-to-end: the hostspec sentinel over an in-process Zenoh peer (#821).
//!
//! hostspec has no external dependency (no bus daemon, no device, no
//! privileges), so unlike the systemd sensor a FULL e2e is cheap: a tempdir
//! is a complete host to assert about. One isolated session (scouting off —
//! a test that scouts is a participant, not a test, RFC 09 §0.1), the real
//! `AlertReporter`, `Evaluator` and `@rpc` surface, and the four contracts
//! this crate makes:
//!
//! 1. a failing assertion fires an alert with the failing clause in the
//!    labels, and fixing the host resolves it;
//! 2. `@rpc/hostspec/spec` answers "what is this host held to" with honest
//!    per-assertion statuses;
//! 3. a hot-swap that DELETES an expectation resolves its alerts (the
//!    seen-rules GC);
//! 4. an invalid submitted set refuses with `error/invalid-args` and the
//!    previous good set keeps running.

use std::sync::Arc;
use std::time::Duration;

use zensight_common::{Alert, AlertState, Format, decode_auto};
use zensight_sensor_core::{AlertReporter, Publisher};

use zensight_sensor_hostspec::command;
use zensight_sensor_hostspec::sentinel::{
    AbsentExpectation, AssertionStatus, ContentExpectation, Evaluator, ExpectationsConfig,
    HostspecEvaluation,
};

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

fn expectations(leftover: &std::path::Path, hosts: &std::path::Path) -> ExpectationsConfig {
    ExpectationsConfig {
        eval_interval_secs: 1,
        default_for_secs: 0,
        absent: vec![AbsentExpectation {
            name: "leftover".into(),
            path: leftover.to_string_lossy().into_owned(),
            severity: zensight_common::AlertSeverity::Warning,
            for_secs: None,
            recover_after_secs: None,
        }],
        content: vec![ContentExpectation {
            name: "hosts-hairpin".into(),
            path: hosts.to_string_lossy().into_owned(),
            contains: vec!["10.0.0.5 registry.internal".into()],
            matches: Vec::new(),
            severity: zensight_common::AlertSeverity::Critical,
            for_secs: None,
            recover_after_secs: None,
        }],
        ..Default::default()
    }
}

async fn recv_alert(
    sub: &zenoh::pubsub::Subscriber<zenoh::handlers::FifoChannelHandler<zenoh::sample::Sample>>,
) -> (zenoh::sample::SampleKind, Option<Alert>) {
    let s = tokio::time::timeout(Duration::from_secs(10), sub.recv_async())
        .await
        .expect("alert sample timed out")
        .expect("alert sample");
    let alert = (s.kind() == zenoh::sample::SampleKind::Put)
        .then(|| decode_auto::<Alert>(&s.payload().to_bytes()).expect("alert decodes"));
    (s.kind(), alert)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sentinel_contract_end_to_end() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let sub = session
        .declare_subscriber("v1/*/state/hostspec/alert/*")
        .await
        .expect("subscriber");
    tokio::time::sleep(Duration::from_millis(150)).await;

    // A tempdir is the whole host under assertion: a leftover file that
    // should be absent, and a hosts file MISSING its hairpin line.
    let dir = tempfile::tempdir().expect("tempdir");
    let leftover = dir.path().join("debug.sock");
    let hosts = dir.path().join("hosts");
    std::fs::write(&leftover, b"x").unwrap();
    std::fs::write(&hosts, b"127.0.0.1 localhost\n").unwrap();

    let publisher = Publisher::new(session.clone(), "hostspec", Format::Json);
    let reporter = Arc::new(AlertReporter::new(
        publisher.clone(),
        zensight_common::Protocol::Hostspec,
        Format::Json,
    ));
    let evaluator = Evaluator::new(
        "e2e-host",
        expectations(&leftover, &hosts),
        reporter,
        publisher,
    );
    let handle = evaluator.handle();
    let eval_task = tokio::spawn(evaluator.run());
    let marker = zensight_sensor_core::desired::AppliedMarker::new(
        Publisher::new(session.clone(), "hostspec", Format::Json),
        "expectations",
    );
    let cmd_task = tokio::spawn(command::run(
        session.clone(),
        "hostspec".to_string(),
        handle.clone(),
        marker,
    ));

    // 1. Both assertions fire, each carrying its failing clause.
    let mut fired = std::collections::BTreeMap::new();
    for _ in 0..2 {
        let (kind, alert) = recv_alert(&sub).await;
        assert_eq!(kind, zenoh::sample::SampleKind::Put);
        let a = alert.unwrap();
        assert_eq!(a.state, AlertState::Firing);
        fired.insert(a.rule.clone(), a);
    }
    let absent = &fired["absent:leftover"];
    assert_eq!(
        absent.labels.get("check").map(String::as_str),
        Some("present")
    );
    assert!(absent.labels.contains_key("path"));
    let content = &fired["content:hosts-hairpin"];
    assert_eq!(
        content.labels.get("check").map(String::as_str),
        Some("contains")
    );
    assert_eq!(
        content.severity,
        zensight_common::AlertSeverity::Critical,
        "per-expectation severity rides the alert"
    );

    // 2. `spec` answers with honest per-assertion statuses. The alerts
    // publish DURING the sweep and the evaluation snapshot lands at its end,
    // so a GET fired the instant the alerts arrive can honestly see
    // `evaluated_at_ms == 0` ("not yet evaluated") — poll briefly for the
    // completed snapshot instead of racing it.
    let mut eval = HostspecEvaluation::default();
    for _ in 0..50 {
        let replies = session
            .get("v1/*/@rpc/hostspec/spec")
            .timeout(Duration::from_secs(5))
            .await
            .expect("spec get");
        let reply = replies.recv_async().await.expect("spec reply");
        let sample = reply.result().expect("spec ok");
        eval = serde_json::from_slice(&sample.payload().to_bytes()).expect("spec decodes");
        if eval.evaluated_at_ms > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(eval.evaluated_at_ms > 0, "a sweep completed within 5s");
    let status = |rule: &str| {
        eval.assertions
            .iter()
            .find(|a| a.rule == rule)
            .unwrap_or_else(|| panic!("{rule} in spec"))
            .status
    };
    assert_eq!(status("absent:leftover"), AssertionStatus::Fail);
    assert_eq!(status("content:hosts-hairpin"), AssertionStatus::Fail);

    // 3. Fix the host: the leftover disappears -> Resolved put + tombstone.
    std::fs::remove_file(&leftover).unwrap();
    let mut saw_resolved = false;
    let mut saw_delete = false;
    for _ in 0..2 {
        let (kind, alert) = recv_alert(&sub).await;
        match kind {
            zenoh::sample::SampleKind::Put => {
                let a = alert.unwrap();
                assert_eq!(a.state, AlertState::Resolved);
                assert_eq!(a.rule, "absent:leftover");
                saw_resolved = true;
            }
            zenoh::sample::SampleKind::Delete => saw_delete = true,
        }
    }
    assert!(saw_resolved && saw_delete);

    // 4. An INVALID set refuses with invalid-args and changes nothing.
    let bad = r#"{ "content": [ { "name": "x", "path": "/etc/hosts", "matches": ["["] } ] }"#;
    let replies = session
        .get("v1/*/@rpc/hostspec/expectations/set")
        .payload(bad)
        .timeout(Duration::from_secs(5))
        .await
        .expect("set get");
    let reply = replies.recv_async().await.expect("set reply");
    assert!(reply.result().is_err(), "a bad regex must refuse");
    let still = handle.snapshot().await;
    assert_eq!(still.content.len(), 1, "previous good set kept");
    assert_eq!(still.content[0].name, "hosts-hairpin");

    // 5. A VALID set that deletes the content expectation resolves its
    //    still-firing alert via the seen-rules GC.
    let empty = serde_json::to_string(&ExpectationsConfig {
        eval_interval_secs: 1,
        ..Default::default()
    })
    .unwrap();
    let replies = session
        .get("v1/*/@rpc/hostspec/expectations/set")
        .payload(empty)
        .timeout(Duration::from_secs(5))
        .await
        .expect("set get");
    let reply = replies.recv_async().await.expect("set reply");
    assert!(reply.result().is_ok(), "the empty set is valid");
    let mut saw_resolved = false;
    let mut saw_delete = false;
    for _ in 0..2 {
        let (kind, alert) = recv_alert(&sub).await;
        match kind {
            zenoh::sample::SampleKind::Put => {
                let a = alert.unwrap();
                assert_eq!(a.rule, "content:hosts-hairpin");
                assert_eq!(a.state, AlertState::Resolved);
                saw_resolved = true;
            }
            zenoh::sample::SampleKind::Delete => saw_delete = true,
        }
    }
    assert!(
        saw_resolved && saw_delete,
        "a deleted expectation resolves its alerts"
    );

    eval_task.abort();
    cmd_task.abort();
}

/// **The recovery hold, end to end** (#932): a per-assertion
/// `recover_after_secs` must reach the reporter's `reconcile_opts`, which is
/// the whole of what this issue plumbs.
///
/// It is easy to add the field, thread it through a tuple, and have it reach
/// nothing — the sweep's `report` funnel is four call frames from the config.
/// So: assert a satisfied assertion whose hold has NOT elapsed publishes no
/// resolution, and that the same assertion without a hold resolves at once.
/// The first half is the one that fails if the plumbing is missing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recovery_hold_delays_the_resolution() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let sub = session
        .declare_subscriber("v1/*/state/hostspec/alert/*")
        .await
        .expect("subscriber");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let leftover = dir.path().join("debug.sock");
    std::fs::write(&leftover, b"x").unwrap();

    let cfg = ExpectationsConfig {
        eval_interval_secs: 1,
        default_for_secs: 0,
        absent: vec![AbsentExpectation {
            name: "leftover".into(),
            path: leftover.to_string_lossy().into_owned(),
            severity: zensight_common::AlertSeverity::Warning,
            for_secs: None,
            // Far longer than this test runs: a resolution inside it is the
            // hold not being applied at all.
            recover_after_secs: Some(600),
        }],
        ..Default::default()
    };

    let publisher = Publisher::new(session.clone(), "hostspec", Format::Json);
    let reporter = Arc::new(AlertReporter::new(
        publisher.clone(),
        zensight_common::Protocol::Hostspec,
        Format::Json,
    ));
    let evaluator = Evaluator::new("e2e-host", cfg, reporter, publisher);
    let eval_task = tokio::spawn(evaluator.run());

    // It fires.
    let (kind, alert) = recv_alert(&sub).await;
    assert_eq!(kind, zenoh::sample::SampleKind::Put);
    assert_eq!(alert.unwrap().state, AlertState::Firing);

    // Fix the host. Without a hold this resolves on the next sweep (one
    // second); with a ten-minute hold nothing may reach the bus.
    std::fs::remove_file(&leftover).unwrap();
    let quiet = tokio::time::timeout(Duration::from_secs(4), sub.recv_async()).await;
    assert!(
        quiet.is_err(),
        "the assertion is satisfied again but its recovery hold has not elapsed — \
         nothing should have been published, got {quiet:?}"
    );

    eval_task.abort();
}

/// The other half: no hold, and the same fix resolves at once. Without this,
/// the test above would also pass on a sentinel that had simply stopped
/// resolving anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_hold_resolves_on_the_next_sweep() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let sub = session
        .declare_subscriber("v1/*/state/hostspec/alert/*")
        .await
        .expect("subscriber");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let leftover = dir.path().join("debug.sock");
    std::fs::write(&leftover, b"x").unwrap();

    let cfg = ExpectationsConfig {
        eval_interval_secs: 1,
        default_for_secs: 0,
        absent: vec![AbsentExpectation {
            name: "leftover".into(),
            path: leftover.to_string_lossy().into_owned(),
            severity: zensight_common::AlertSeverity::Warning,
            for_secs: None,
            recover_after_secs: None,
        }],
        ..Default::default()
    };

    let publisher = Publisher::new(session.clone(), "hostspec", Format::Json);
    let reporter = Arc::new(AlertReporter::new(
        publisher.clone(),
        zensight_common::Protocol::Hostspec,
        Format::Json,
    ));
    let evaluator = Evaluator::new("e2e-host", cfg, reporter, publisher);
    let eval_task = tokio::spawn(evaluator.run());

    let (_, alert) = recv_alert(&sub).await;
    assert_eq!(alert.unwrap().state, AlertState::Firing);

    std::fs::remove_file(&leftover).unwrap();
    let mut saw_resolved = false;
    for _ in 0..2 {
        if let (zenoh::sample::SampleKind::Put, Some(a)) = recv_alert(&sub).await {
            assert_eq!(a.state, AlertState::Resolved);
            saw_resolved = true;
        }
    }
    assert!(saw_resolved, "with no hold the fix must resolve at once");

    eval_task.abort();
}
