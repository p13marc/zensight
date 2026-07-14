//! Cutover acceptance test (epic #453, #465): everything the framework
//! publishes rides `zensight/v1/**` — nothing escapes it.
//!
//! Two real Zenoh sessions (sensor + observer) over an explicit localhost
//! endpoint, scouting off (isolated-pair pattern). The observer holds two
//! debug subscribers:
//!
//! - `zensight/**` — the whole deployment root, v1 and otherwise. Every key it
//!   sees that is **not** under `zensight/v1/` is a leak: a framework channel
//!   still publishing outside the convention.
//! - `zensight/v1/**` — the v1 root. Telemetry, health, and alert state
//!   published through the framework must all land here.
//!
//! The "not under v1" filter is done in the callback rather than by the
//! selector, and that is a consequence of the version chunk being plain (see
//! `grammar::VERSION_CHUNK`). While it was the verbatim `@v1`, `**` could not
//! cross it, so `zensight/**` *was* a legacy-only firehose and an empty result
//! set proved silence on its own. A plain `v1` is reachable by `**`, so the
//! subscriber now sees our own traffic too and the leak check has to say what it
//! means: anything outside v1. Same guarantee, stated explicitly instead of
//! riding on key algebra.
//!
//! # The two sessions are deliberately asymmetric (#466)
//!
//! The **sensor** is a real participant: it sets the deployment base as its
//! session `namespace` (RFC 09 §0), so every key it declares is base-relative
//! (`v1/…`) and the `zensight/` that lands on the wire is the *namespace*, not
//! a string the application concatenated.
//!
//! The **observer** is deliberately **un-namespaced** — the honest view of the
//! wire, which is exactly the posture a router, a storage selector, an ACL rule,
//! or `zenctl` has (RFC 09 §5). Its subscribers therefore spell FULL keys.
//!
//! That asymmetry is what makes this test prove #466 rather than merely survive
//! it: the assertions below are unchanged, byte for byte, from before the
//! namespace landed. The application stopped spelling the base and *the wire did
//! not move*. If the namespace were not being applied, the observer would see
//! nothing at all.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use zensight_common::alert::{Alert, AlertKind, AlertSeverity};
use zensight_common::{Format, Protocol};
use zensight_sensor_core::{AlertReporter, Publisher};

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

fn listen_config(port: u16) -> zenoh::Config {
    let mut config = isolated_config();
    config
        .insert_json5("listen/endpoints", &format!("[\"tcp/127.0.0.1:{port}\"]"))
        .unwrap();
    config
}

fn connect_config(port: u16) -> zenoh::Config {
    let mut config = isolated_config();
    config
        .insert_json5("connect/endpoints", &format!("[\"tcp/127.0.0.1:{port}\"]"))
        .unwrap();
    config
}

/// The SENSOR's session: a real participant, so it sets the base as its
/// `namespace` (#466). Everything the framework declares below is
/// base-relative; the session puts `zensight/` on the wire.
fn sensor_config(port: u16) -> zenoh::Config {
    let mut config = connect_config(port);
    config
        .insert_json5("namespace", "\"zensight\"")
        .expect("set the deployment namespace");
    config
}

fn candidate_port(attempt: u16) -> u16 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos() as u16;
    49152
        + ((std::process::id() as u16)
            .wrapping_add(nanos)
            .wrapping_add(attempt * 137))
            % 16000
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_bus_is_silent_and_v1_carries_everything() {
    // Observer peer: listens, holds the two debug subscribers.
    let (observer, port) = {
        let mut opened = None;
        for attempt in 0..8 {
            let port = candidate_port(attempt);
            if let Ok(s) = zenoh::open(listen_config(port)).await {
                opened = Some((s, port));
                break;
            }
        }
        opened.expect("open listening observer session")
    };

    let legacy_hits: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let v1_hits: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let legacy_log = legacy_hits.clone();
    let _legacy_sub = observer
        .declare_subscriber("zensight/**")
        .callback(move |sample| {
            let key = sample.key_expr().as_str();
            // `zensight/**` now reaches v1 too (plain version chunk), so the
            // leak is what falls OUTSIDE it.
            if !key.starts_with("zensight/v1/") {
                legacy_log.lock().unwrap().push(key.to_string());
            }
        })
        .await
        .expect("declare legacy debug subscriber");

    let v1_log = v1_hits.clone();
    let _v1_sub = observer
        .declare_subscriber("zensight/v1/**")
        .callback(move |sample| {
            v1_log
                .lock()
                .unwrap()
                .push(sample.key_expr().as_str().to_string());
        })
        .await
        .expect("declare v1 debug subscriber");

    // Sensor peer: publish through the real framework channels — telemetry,
    // health, and a firing alert.
    let sensor = Arc::new(
        zenoh::open(sensor_config(port))
            .await
            .expect("open sensor session"),
    );
    tokio::time::sleep(Duration::from_millis(400)).await;

    let publisher = Publisher::new(sensor.clone(), "netlink", Format::Json);

    // #466, half one: the application's OWN view of its key never contains the
    // base. There is no `V1Context::base()` to get it wrong with.
    let built = publisher.v1().telemetry_prefix();
    assert!(
        built.starts_with("v1/"),
        "an application key must start at the version chunk: {built}"
    );
    assert!(
        !built.starts_with("zensight"),
        "the deployment base leaked into an application key: {built}"
    );

    let point = zensight_common::TelemetryPoint::new(
        "cutover-host",
        zensight_common::Protocol::Netlink,
        "iface/eth0/rx_bytes",
        zensight_common::TelemetryValue::Counter(1),
    );
    publisher
        .publish("iface/eth0/rx_bytes", &point)
        .await
        .expect("publish telemetry");

    let health =
        zensight_sensor_core::SensorHealth::new("netlink").with_publisher(publisher.clone());
    health.publish_health().await.expect("publish health");

    let reporter = AlertReporter::new(publisher.clone(), Protocol::Netlink, Format::Json);
    let alert = Alert::new(
        "cutover-host",
        Protocol::Netlink,
        AlertKind::Anomaly,
        "cutover-rule",
        AlertSeverity::Warning,
        "cutover acceptance alert",
    );
    reporter
        .observe(alert, Some(Duration::ZERO))
        .await
        .expect("fire alert");

    tokio::time::sleep(Duration::from_millis(800)).await;

    let v1 = v1_hits.lock().unwrap().clone();
    let legacy = legacy_hits.lock().unwrap().clone();

    assert!(
        legacy.is_empty(),
        "the legacy bus must be silent — leaked keys: {legacy:?}"
    );
    assert!(
        v1.iter()
            .any(|k| k.contains("/telemetry/netlink/iface/eth0/rx_bytes")),
        "v1 telemetry missing: {v1:?}"
    );
    assert!(
        v1.iter().any(|k| k.ends_with("/state/netlink/health")),
        "v1 health doc missing: {v1:?}"
    );
    assert!(
        v1.iter().any(|k| k.contains("/state/netlink/alert/")),
        "v1 alert doc missing: {v1:?}"
    );
    // Every observed key parses as base + v1.
    for key in &v1 {
        assert!(key.starts_with("zensight/v1/"), "malformed v1 key: {key}");
    }

    // #466, half two: the SAME key, in two spellings. The application built
    // `v1/<origin>/telemetry/netlink`; the un-namespaced observer saw
    // `zensight/v1/<origin>/telemetry/netlink`. The namespace is the bridge, and
    // it is the only thing that put `zensight/` there.
    assert!(
        v1.iter()
            .any(|k| k == &format!("zensight/{built}/iface/eth0/rx_bytes")),
        "the namespace must prefix exactly the key the application built \
         ({built}/iface/eth0/rx_bytes); saw: {v1:?}"
    );
}

/// #466: the namespace is an **isolation boundary**, not a prefix.
///
/// RFC 03 §1.1 promises that a namespaced session *filters* ingress from
/// outside its namespace — "the base is an isolation boundary, not just a
/// prefix". That is the property that makes two deployments able to share a
/// Zenoh network, and it is the reason this is worth doing at all rather than
/// just concatenating a string. Nothing else in the suite pins it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_namespace_filters_ingress_from_other_deployments() {
    let (observer, port) = {
        let mut opened = None;
        for attempt in 0..8 {
            let port = candidate_port(attempt + 64);
            if let Ok(s) = zenoh::open(listen_config(port)).await {
                opened = Some((s, port));
                break;
            }
        }
        opened.expect("open listening observer session")
    };

    // A participant of the `zensight` deployment.
    let member = zenoh::open(sensor_config(port))
        .await
        .expect("open namespaced session");

    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    // Base-relative, exactly as application code writes it.
    let _sub = member
        .declare_subscriber("v1/**")
        .callback(move |s| log.lock().unwrap().push(s.key_expr().as_str().to_string()))
        .await
        .expect("declare a base-relative subscriber");

    tokio::time::sleep(Duration::from_millis(300)).await;

    // The observer is un-namespaced, so it publishes literal wire keys. Two
    // well-formed v1 keys, differing only in their base.
    for key in [
        "other-deployment/v1/h-000000000000/telemetry/netlink/x",
        "zensight/v1/h-000000000000/telemetry/netlink/x",
    ] {
        observer.put(key, vec![1u8]).await.expect("put");
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen.len(),
        1,
        "ingress from outside the namespace must be FILTERED, not merely unmatched — saw {seen:?}"
    );
    assert_eq!(
        seen[0], "v1/h-000000000000/telemetry/netlink/x",
        "ingress must arrive with the base already stripped"
    );
}
