//! The advanced tier's publisher detection must actually work — end to end.
//!
//! `zensight-keyspace/tests/adv_token.rs` pins the *reason* (a verbatim version
//! chunk makes zenoh-ext's `@adv` token unparseable, because `**` never crosses
//! an `@`). This test pins the *symptom*: it wires a real
//! [`AdvancedPublisherRegistry`] — the exact path every sensor publishes
//! through — to an `AdvancedSubscriber` configured exactly as the GUI's Standard
//! profile is (`zensight/src/subscription.rs`), with the publisher joining
//! **late**, and asserts zenoh-ext never logs
//! *"malformed liveliness token key expression"*.
//!
//! That warning was the user-visible face of a silent failure: the parse is the
//! first thing `detect_late_publishers()`'s callback does, so every failure was
//! also a late-joining publisher whose cached history was never fetched.
//!
//! **This test only bites because the log capture is global.** `set_default` is
//! thread-local and zenoh-ext logs from its own threads — with it, the test
//! captured nothing and passed vacuously even against a broken keyspace. It is
//! written the way it is on purpose.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use zenoh_ext::{AdvancedSubscriberBuilderExt, HistoryConfig, RecoveryConfig};
use zensight_common::{Format, Protocol, TelemetryPoint, TelemetryValue};
use zensight_sensor_core::{AdvancedPublisherConfig, AdvancedPublisherRegistry};

/// A `MakeWriter` that accumulates log output so the test can assert on it.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Capture {
        self.clone()
    }
}

/// Isolated-pair session config: multicast off, so this can never join a mesh
/// beyond the endpoint it is told about.
fn session_config(listen: Option<&str>, connect: Option<&str>) -> zenoh::Config {
    let mut c = zenoh::Config::default();
    c.insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    // AdvancedPublisher's Sequencing::Timestamp requires it.
    c.insert_json5("timestamping/enabled", "true").unwrap();
    if let Some(l) = listen {
        c.insert_json5("listen/endpoints", &format!("[{l:?}]"))
            .unwrap();
    }
    if let Some(x) = connect {
        c.insert_json5("connect/endpoints", &format!("[{x:?}]"))
            .unwrap();
    }
    c
}

#[tokio::test(flavor = "multi_thread")]
async fn late_publisher_detection_does_not_warn() {
    let capture = Capture::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_max_level(tracing::Level::WARN)
            .finish(),
    )
    .expect("install global log capture");

    let endpoint = "tcp/127.0.0.1:17799";
    let subscriber_session = zenoh::open(session_config(Some(endpoint), None))
        .await
        .expect("open subscriber session");
    let publisher_session = Arc::new(
        zenoh::open(session_config(None, Some(endpoint)))
            .await
            .expect("open publisher session"),
    );
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The GUI's Standard profile, verbatim.
    let _sub = subscriber_session
        .declare_subscriber(zensight_common::all_telemetry_wildcard())
        .callback(|_| {})
        .history(HistoryConfig::default().detect_late_publishers())
        .recovery(RecoveryConfig::default())
        .subscriber_detection()
        .await
        .expect("declare advanced subscriber");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The publisher appears *after* the subscriber — the late-joiner path, which
    // is the one that used to fail. Publisher detection must be on for this test
    // to mean anything.
    let registry = AdvancedPublisherRegistry::new(
        publisher_session.clone(),
        "zensight/v1/h-9706b31ddad3/telemetry/netring",
        Format::Cbor,
        AdvancedPublisherConfig::default(),
    );
    assert!(
        registry.config().publisher_detection,
        "publisher_detection must be enabled or this test proves nothing"
    );

    for metric in ["tls/pq_ratio", "bandwidth/ntp/bytes_per_sec"] {
        let point = TelemetryPoint::new(
            "h-9706b31ddad3",
            Protocol::Netring,
            metric,
            TelemetryValue::Gauge(1.0),
        );
        registry.publish(metric, &point).await.expect("publish");
    }
    tokio::time::sleep(Duration::from_secs(2)).await;

    let logs = String::from_utf8_lossy(&capture.0.lock().unwrap()).to_string();
    assert!(
        !logs.contains("malformed liveliness token"),
        "zenoh-ext cannot parse our @adv tokens — publisher detection is dead and \
         the logs are filling up. Did the version chunk become verbatim again? \
         See zensight_keyspace::grammar::VERSION_CHUNK.\n\n{logs}"
    );
}
