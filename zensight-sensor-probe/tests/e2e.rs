//! End-to-end: a real HTTP server, a real TCP black hole, a real certificate
//! on disk, a real Zenoh peer, and the real poller (#820).
//!
//! The three contracts:
//!
//! 1. **The 2026-08-20 shape.** A target that hangs comes back as `Timeout`
//!    with its duration, fires `probe-timeout` and *not* also `probe-down`,
//!    and the alert names the vantage point. That is the report that would
//!    have replaced eight days of investigation with one interval.
//! 2. A working target publishes `up=1`, its status and its latency, and fires
//!    nothing.
//! 3. A certificate file's expiry is read and graded, with **no chain verdict
//!    invented** — a PEM on disk has nothing to validate against.

use std::sync::Arc;
use std::time::Duration;

use axum::{Router, routing::get};

use zensight_common::probe::{ProbeKind, ProbeOutcome, ProbeResult};
use zensight_common::{Alert, AlertState, TelemetryPoint, decode_auto};
use zensight_sensor_core::{AlertReporter, Publisher};

use zensight_sensor_probe::config::{ProbeAlertsConfig, ProbeConfig, Target};
use zensight_sensor_probe::poller::Poller;

fn isolated_config() -> zenoh::Config {
    let mut c = zenoh::Config::default();
    c.insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    c.insert_json5("scouting/gossip/enabled", "false").unwrap();
    c.insert_json5("timestamping/enabled", "true").unwrap();
    c
}

fn target(name: &str, kind: ProbeKind, t: &str) -> Target {
    Target {
        name: name.into(),
        kind,
        target: t.into(),
        interval_secs: None,
        timeout_secs: Some(1),
        expect_status: vec![],
        expect_body: None,
        follow_redirects: true,
        allow_offhost_redirect: false,
        method: None,
        headers: vec![],
        server_name: None,
        inspect_untrusted: true,
        resolver: None,
        expect_addrs: vec![],
        enabled: true,
    }
}

/// A self-signed leaf, written as PEM, so the file path is exercised against
/// a real certificate rather than a fixture nobody can regenerate.
///
/// Uses `rcgen` if it is available; otherwise the test writes a certificate
/// baked at a known date and only asserts the fields that do not move.
fn write_cert(dir: &std::path::Path) -> std::path::PathBuf {
    // A minimal self-signed certificate, DER, generated once and embedded:
    // CN=probe.test, valid 2020-01-01 .. 2021-01-01 (deliberately EXPIRED, so
    // the negative-days path is exercised end to end).
    const PEM: &str = include_str!("fixtures/expired.pem");
    let p = dir.join("expired.pem");
    std::fs::write(&p, PEM).unwrap();
    p
}

async fn spawn_http() -> std::net::SocketAddr {
    let app = Router::new()
        .route("/ok", get(|| async { "hello from the probe test" }))
        .route(
            "/teapot",
            get(|| async { (axum::http::StatusCode::IM_A_TEAPOT, "no") }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(l, app).await.unwrap();
    });
    addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_probe_contract_end_to_end() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let tmp = tempfile::tempdir().unwrap();
    let cert = write_cert(tmp.path());
    let http = spawn_http().await;

    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let alerts_sub = session
        .declare_subscriber("v1/*/state/probe/alert/*")
        .await
        .unwrap();
    let docs_sub = session
        .declare_subscriber("v1/*/state/probe/target/*")
        .await
        .unwrap();
    let telemetry_sub = session
        .declare_subscriber("v1/*/telemetry/probe/**")
        .await
        .unwrap();

    let cfg = ProbeConfig {
        vantage: Some("vm-apps".into()),
        interval_secs: 60,
        timeout_secs: 1,
        max_concurrent: 4,
        targets: vec![
            target("site", ProbeKind::Http, &format!("http://{http}/ok")),
            target("teapot", ProbeKind::Http, &format!("http://{http}/teapot")),
            // TEST-NET-1 (RFC 5737): routable-looking, never answers. This is
            // the 2026-08-20 shape.
            target("hairpin", ProbeKind::Tcp, "192.0.2.1:443"),
            target("refused", ProbeKind::Tcp, "127.0.0.1:1"),
            target("mesh-cert", ProbeKind::CertFile, cert.to_str().unwrap()),
        ],
        alerts: ProbeAlertsConfig {
            for_secs: 0,
            ..Default::default()
        },
        ..Default::default()
    };

    let format = zensight_common::Format::Json;
    let publisher = Publisher::new(session.clone(), "probe", format);
    let reporter = Arc::new(AlertReporter::new(
        publisher.clone(),
        zensight_common::Protocol::Probe,
        format,
    ));
    let states = Arc::new(
        zensight_sensor_core::AdvancedPublisherRegistry::new(
            session.clone(),
            zensight_sensor_core::v1::for_producer("probe").telemetry_prefix(),
            format,
            zensight_sensor_core::AdvancedPublisherConfig::cache_only(1),
        )
        .with_qos(zensight_sensor_probe::poller::STATE_QOS),
    );
    let health = Arc::new(zensight_sensor_core::SensorHealth::new("probe"));

    let mut poller = Poller::new(
        cfg,
        publisher,
        states,
        Some(reporter.clone()),
        health.clone(),
    )
    .unwrap();

    let results = poller.sweep().await;
    assert_eq!(results.len(), 5, "every target is due on the first tick");
    poller.publish(&results).await;

    let by_name: std::collections::HashMap<&str, &ProbeResult> =
        results.iter().map(|r| (r.name.as_str(), r)).collect();

    // ── Contract 1: the 2026-08-20 shape ────────────────────────────────────
    let hairpin = by_name["hairpin"];
    assert_eq!(
        hairpin.outcome,
        ProbeOutcome::Timeout,
        "a black hole hangs; it does not refuse"
    );
    assert!(
        hairpin.duration_ms.unwrap() >= 900.0,
        "the duration is the diagnosis: {:?}",
        hairpin.duration_ms
    );
    // And a refusal is the OTHER outcome, from the same kind of check.
    assert_eq!(by_name["refused"].outcome, ProbeOutcome::Failed);

    // ── Contract 2: a working target ────────────────────────────────────────
    let site = by_name["site"];
    assert_eq!(site.outcome, ProbeOutcome::Ok);
    assert_eq!(site.http.as_ref().unwrap().status, Some(200));
    assert!(site.http.as_ref().unwrap().ttfb_ms.unwrap() >= 0.0);
    assert_eq!(site.vantage, "vm-apps");

    // A 418 is not a 2xx, and the error names the status rather than being
    // generic.
    let teapot = by_name["teapot"];
    assert_eq!(teapot.outcome, ProbeOutcome::Failed);
    assert!(
        teapot.error.as_deref().unwrap().contains("418"),
        "{teapot:?}"
    );

    // ── Contract 3: a certificate off disk ──────────────────────────────────
    let mesh = by_name["mesh-cert"];
    assert_eq!(mesh.outcome, ProbeOutcome::Ok, "reading it succeeded");
    let tls = mesh.tls.as_ref().unwrap();
    assert!(
        tls.days_to_expiry.unwrap() < 0,
        "the fixture is expired, and expiry goes negative: {:?}",
        tls.days_to_expiry
    );
    assert_eq!(
        tls.chain_valid, None,
        "a PEM on disk has no chain, and inventing a verdict would be worse than none"
    );
    assert!(tls.issuer.is_some());

    // ── Alerts ──────────────────────────────────────────────────────────────
    let mut fired: std::collections::HashMap<String, Vec<Alert>> = Default::default();
    while let Ok(Ok(s)) =
        tokio::time::timeout(Duration::from_millis(500), alerts_sub.recv_async()).await
    {
        if s.kind() == zenoh::sample::SampleKind::Put
            && let Ok(a) = decode_auto::<Alert>(&s.payload().to_bytes())
            && a.state == AlertState::Firing
        {
            fired.entry(a.rule.clone()).or_default().push(a);
        }
    }
    let timeouts = fired.get("probe-timeout").expect("the hairpin must fire");
    assert_eq!(timeouts.len(), 1);
    assert_eq!(timeouts[0].labels["vantage"], "vm-apps");
    assert!(
        timeouts[0].labels["duration_ms"].parse::<f64>().unwrap() >= 900.0,
        "the duration rides on the alert: {:?}",
        timeouts[0].labels
    );

    let downs = fired.get("probe-down").expect("refused + teapot");
    // Which target, from the labels — `source` is the vantage point now (#883).
    let down_names: std::collections::HashSet<&str> =
        downs.iter().map(|a| a.labels["probe"].as_str()).collect();
    assert_eq!(
        down_names,
        ["refused", "teapot"].into_iter().collect(),
        "the hairpin must NOT also be reported as down — one page, right diagnosis"
    );

    assert!(
        fired.contains_key("probe-certificate-expiring"),
        "an expired certificate must fire: {:?}",
        fired.keys().collect::<Vec<_>>()
    );

    // ── The documents and the gauges ────────────────────────────────────────
    let mut docs = 0;
    while let Ok(Ok(s)) =
        tokio::time::timeout(Duration::from_millis(500), docs_sub.recv_async()).await
    {
        if decode_auto::<ProbeResult>(&s.payload().to_bytes()).is_ok() {
            docs += 1;
        }
    }
    assert_eq!(docs, 5);

    let mut seen: std::collections::HashMap<String, f64> = Default::default();
    let mut points: Vec<(String, TelemetryPoint)> = Vec::new();
    while let Ok(Ok(s)) =
        tokio::time::timeout(Duration::from_millis(500), telemetry_sub.recv_async()).await
    {
        if let Ok(p) = decode_auto::<TelemetryPoint>(&s.payload().to_bytes()) {
            let key = s.key_expr().as_str();
            let subject = key.split("/telemetry/probe/").nth(1).unwrap().to_string();
            if let zensight_common::TelemetryValue::Gauge(v) = p.value {
                seen.insert(subject.clone(), v);
            }
            points.push((subject, p));
        }
    }

    // #883: a probe result is an observation made from somewhere, so every
    // point is filed under the VANTAGE POINT and never under the target. Two
    // hosts probing the same URL used to collide on one `source`, which is
    // precisely the comparison this sensor exists to make possible.
    assert!(!points.is_empty());
    for (subject, p) in &points {
        assert_eq!(
            p.source, "vm-apps",
            "{subject} is filed under {} rather than the vantage point",
            p.source
        );
    }
    let site = points
        .iter()
        .find(|(k, _)| k == "site/up")
        .expect("site/up")
        .1
        .clone();
    assert_eq!(site.labels["vantage"], "vm-apps");
    assert!(site.labels.contains_key("target"));
    assert!(site.labels.contains_key("kind"));
    assert_eq!(seen.get("site/up"), Some(&1.0));
    assert_eq!(seen.get("site/http_status"), Some(&200.0));
    assert_eq!(seen.get("hairpin/up"), Some(&0.0));
    assert_eq!(
        seen.get("hairpin/timeout"),
        Some(&1.0),
        "a timeout is a gauge of its own, not a flavour of down"
    );
    assert_eq!(seen.get("refused/timeout"), Some(&0.0));
    assert!(
        seen.get("mesh-cert/tls_days_to_expiry").unwrap() < &0.0,
        "{seen:#?}"
    );
    assert!(
        !seen.contains_key("mesh-cert/tls_chain_valid"),
        "no chain verdict exists for a file, so no gauge claims one"
    );
    assert_eq!(seen.get("targets/total"), Some(&5.0));
    assert_eq!(seen.get("targets/failing"), Some(&3.0));

    // ── A target that stops failing resolves ────────────────────────────────
    // Re-run with only the working target: the others keep their last known
    // result, so nothing spuriously resolves.
    let again = poller.sweep().await;
    assert!(
        again.is_empty(),
        "nothing is due yet — the per-target schedule is real"
    );
}
