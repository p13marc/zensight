//! A slave that accepts the connection and never answers (#1133).
//!
//! The realistic shape of a dead PLC: the TCP handshake completes — the kernel
//! answers that, not the application — and then nothing comes back. Not a
//! refused connection, not a reset, not an exception response. Silence.
//!
//! Before this, `timeout_ms` was applied to `tcp::connect_slave` and to
//! nothing else. `run()` awaits `poll_once`, so the first read against such a
//! slave stopped that device **permanently**: no further poll, no further
//! sample, and `alive` still declared because the sensor's liveliness token is
//! about the process rather than the device.

use std::sync::Arc;
use std::time::{Duration, Instant};

use zensight_sensor_modbus::config::{
    ConnectionConfig, DeviceConfig, ModbusConfig, RegisterConfig,
};
use zensight_sensor_modbus::poller::ModbusPoller;

/// Accept connections and read nothing, forever. The accepted sockets are
/// held so the peer sees an established connection rather than a reset.
async fn silent_slave() -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((sock, _)) = listener.accept().await {
            held.push(sock);
        }
    });
    addr
}

/// One holding register at address 0 — the smallest legal read there is.
fn holding_0() -> RegisterConfig {
    let json = r#"{"type": "holding", "address": 0, "count": 1}"#;
    serde_json::from_str(json).expect("register config parses")
}

async fn session() -> Arc<zenoh::Session> {
    let mut c = zenoh::Config::default();
    c.insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    c.insert_json5("scouting/gossip/enabled", "false").unwrap();
    Arc::new(zenoh::open(c).await.unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slave_that_never_answers_does_not_hang_the_poll() {
    let addr = silent_slave().await;
    let device = DeviceConfig {
        name: "plc-1".into(),
        connection: ConnectionConfig::Tcp {
            host: addr.ip().to_string(),
            port: addr.port(),
        },
        unit_id: 1,
        poll_interval_secs: 1,
        timeout_ms: 300,
        // Two retries, so the bound has to hold across all of them and not
        // only the first.
        retries: 2,
        registers: vec![holding_0()],
        register_group: None,
    };
    let cfg: ModbusConfig = serde_json::from_str(r#"{"devices": []}"#).expect("modbus config");
    let health = Arc::new(zensight_sensor_core::SensorHealth::new("modbus"));
    let poller = ModbusPoller::new(
        device,
        &cfg,
        session().await,
        zensight_common::serialization::Format::Cbor,
    )
    .with_health(health.clone());

    // One cycle of `run`, bounded generously: the assertion is that the poll
    // returns at all, and by how much less than this it does.
    let started = Instant::now();
    let ran = tokio::time::timeout(Duration::from_secs(10), async {
        poller.poll_once_for_test().await
    })
    .await;

    let elapsed = started.elapsed();
    assert!(
        ran.is_ok(),
        "the poll never returned — a silent slave stops this device forever \
         (#1133)"
    );
    assert!(
        ran.unwrap().is_err(),
        "a slave that answered nothing must not report a successful poll"
    );
    // `timeout_ms` bounds ONE read; `retries` is allowed to multiply it, and
    // nothing else may.
    let ceiling = Duration::from_millis(300 * 4);
    assert!(
        elapsed < ceiling,
        "the poll took {elapsed:?}, past the {ceiling:?} that timeout_ms × \
         (retries + 1) allows — something on the read path is unbounded"
    );
}
