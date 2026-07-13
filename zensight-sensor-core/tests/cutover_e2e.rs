//! Cutover acceptance test (epic #453, #465): everything the framework
//! publishes rides `zensight/@v1/**` — the legacy bus is silent.
//!
//! Two real Zenoh sessions (sensor + observer) over an explicit localhost
//! endpoint, scouting off (isolated-pair pattern). The observer holds two
//! debug subscribers:
//!
//! - `zensight/**` — the LEGACY firehose. `**` never crosses the verbatim
//!   `@v1` chunk (RFC 03, guard D1), so if any framework channel still
//!   published a legacy key this subscriber would catch it. It must stay
//!   empty.
//! - `zensight/@v1/**` — the v1 root. Telemetry, health, and alert state
//!   published through the framework must all land here.

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
            legacy_log
                .lock()
                .unwrap()
                .push(sample.key_expr().as_str().to_string());
        })
        .await
        .expect("declare legacy debug subscriber");

    let v1_log = v1_hits.clone();
    let _v1_sub = observer
        .declare_subscriber("zensight/@v1/**")
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
        zenoh::open(connect_config(port))
            .await
            .expect("open sensor session"),
    );
    tokio::time::sleep(Duration::from_millis(400)).await;

    let publisher = Publisher::new(sensor.clone(), "netlink", Format::Json);

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
    // Every observed key parses as base + @v1.
    for key in &v1 {
        assert!(key.starts_with("zensight/@v1/"), "malformed v1 key: {key}");
    }
}
