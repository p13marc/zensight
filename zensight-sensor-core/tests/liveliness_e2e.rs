//! End-to-end liveliness contract test: the frontend learns a sensor died
//! because the sensor's liveliness token disappears when its session closes.
//!
//! Two real Zenoh sessions (sensor + frontend) over an explicit localhost
//! endpoint, so the DELETE genuinely crosses the wire — a same-session
//! subscriber would die with the sensor and prove nothing. The subscriber
//! uses the frontend's v1 presence pattern (`zensight/v1/*/state/*/alive`,
//! `SENSOR_LIVELINESS_SCOPED_EXPR` in zensight/src/subscription.rs) so this
//! test pins the cross-crate key-shape contract.

use std::sync::Arc;
use std::time::Duration;

use zenoh::sample::SampleKind;
use zensight_sensor_core::LivelinessManager;

/// The frontend's host-scoped sensor-liveliness pattern.
const FRONTEND_SENSOR_LIVELINESS_EXPR: &str = "v1/*/state/*/alive";

/// Scouting off so concurrent tests can't cross-contaminate; the two peers
/// are wired together with an explicit listen/connect endpoint instead.
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

/// A port unlikely to collide: derived from the pid and time, in the
/// dynamic range. Retried by the caller if the listen fails.
fn candidate_port(attempt: u16) -> u16 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos() as u16;
    49152
        + ((std::process::id() as u16)
            .wrapping_add(nanos)
            .wrapping_add(attempt * 131))
            % 16000
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_delete_reaches_frontend_pattern_on_session_close() {
    // "Frontend" peer: listens, subscribes with the frontend's pattern.
    let (frontend, port) = {
        let mut opened = None;
        for attempt in 0..8 {
            let port = candidate_port(attempt);
            if let Ok(s) = zenoh::open(listen_config(port)).await {
                opened = Some((s, port));
                break;
            }
        }
        opened.expect("open listening frontend session")
    };

    let sub = frontend
        .liveliness()
        .declare_subscriber(FRONTEND_SENSOR_LIVELINESS_EXPR)
        .await
        .expect("liveliness subscriber");

    // "Sensor" peer: connects, declares its token via the real manager.
    let sensor = Arc::new(
        zenoh::open(connect_config(port))
            .await
            .expect("open sensor session"),
    );
    let ctx = zensight_sensor_core::v1::V1Context::for_producer("testproto");
    let expected_key = ctx.alive_key();
    let manager = LivelinessManager::new(sensor.clone(), ctx)
        .await
        .expect("declare sensor token");

    // Token appears as a PUT on the frontend's pattern.
    let sample = tokio::time::timeout(Duration::from_secs(10), sub.recv_async())
        .await
        .expect("timed out waiting for liveliness PUT")
        .expect("subscriber closed");
    assert_eq!(sample.kind(), SampleKind::Put);
    assert_eq!(sample.key_expr().as_str(), expected_key);

    // Sensor shuts down: manager dropped, session closed — exactly what
    // SensorRunner does. The frontend must see the token DELETE.
    drop(manager);
    sensor.close().await.expect("close sensor session");

    let sample = tokio::time::timeout(Duration::from_secs(10), sub.recv_async())
        .await
        .expect("timed out waiting for liveliness DELETE")
        .expect("subscriber closed");
    assert_eq!(sample.kind(), SampleKind::Delete);
    assert_eq!(sample.key_expr().as_str(), expected_key);
}
