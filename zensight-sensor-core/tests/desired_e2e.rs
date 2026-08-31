//! End-to-end for the `@desired` reconciler (#816) over an isolated pair.
//!
//! Two peers, multicast off, timestamping ON (LWW needs stamps): a
//! "controller" session publishes a desired `ExpectationsConfig` for this
//! host through an **AdvancedPublisher with a cache** — so a reconciler
//! that starts LATE still receives it (the reconcile-on-connect proof
//! without standing up a storage; in a deployment the `@desired` storage
//! plays this role and the seed GET is the primary path).
//!
//! What is pinned here:
//! 1. late-start convergence: the cached doc is applied, marker
//!    `source: desired`;
//! 2. an INVALID doc is rejected loudly — apply count unchanged, marker
//!    restates the last GOOD config with `last_rejected` riding beside it;
//! 3. a `Delete` reverts to the file baseline (`source: file`);
//! 4. the kill switch: `enabled: false` applies nothing, ever, and the
//!    marker says `file`;
//! 5. the zenoh-ext canary: nothing logs "malformed liveliness token" — the
//!    `@desired` verbatim chunk sits exactly where that bug class lives
//!    (see adv_publisher_detection.rs, which pins the same property for
//!    `@adv`).
//!
//! NOT testable here, stated honestly: true offline-window durability
//! (controller dies, router restarts, sensor returns hours later) needs a
//! zenohd + storage-manager deployment — the issue itself scopes that to
//! the downstream fleet.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use zenoh_ext::{AdvancedPublisherBuilderExt, CacheConfig};
use zensight_common::desired::{AppliedConfig, AppliedSource, DesiredConfig};
use zensight_common::hostspec::{AbsentExpectation, ExpectationsConfig};
use zensight_sensor_core::desired::{DesiredTopic, reconcile_topic};
use zensight_sensor_core::{Format, Publisher};

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

fn session_config(listen: Option<&str>, connect: Option<&str>) -> zenoh::Config {
    let mut c = zenoh::Config::default();
    c.insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    c.insert_json5("scouting/gossip/enabled", "false").unwrap();
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

fn candidate_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn desired_key() -> zenkey::Key {
    use zensight_common::registry::desired;
    // The reconciling host's own id — the sensor side builds it exactly so.
    let host = zensight_common::PROFILE.host_id().clone();
    desired::key(&desired::Subject::hostspec_expectations(&host))
}

fn doc(name: &str) -> ExpectationsConfig {
    ExpectationsConfig {
        eval_interval_secs: 5,
        absent: vec![AbsentExpectation {
            name: name.into(),
            path: "/tmp/desired-e2e".into(),
            severity: zensight_common::AlertSeverity::Warning,
            for_secs: None,
        }],
        ..Default::default()
    }
}

async fn recv_marker(
    sub: &zenoh::pubsub::Subscriber<zenoh::handlers::FifoChannelHandler<zenoh::sample::Sample>>,
) -> AppliedConfig {
    let s = tokio::time::timeout(Duration::from_secs(10), sub.recv_async())
        .await
        .expect("marker timed out")
        .expect("marker sample");
    serde_json::from_slice(&s.payload().to_bytes()).expect("marker decodes")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_reconciler_converges_rejects_and_reverts() {
    let capture = Capture::default();
    let _ = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_env_filter("info,zenoh_ext=debug")
        .try_init();

    let port = candidate_port();
    let controller = Arc::new(
        zenoh::open(session_config(Some(&format!("tcp/127.0.0.1:{port}")), None))
            .await
            .expect("controller session"),
    );
    let key = desired_key();

    // The controller publishes THROUGH A CACHE before the reconciler exists.
    let publisher = controller
        .declare_publisher(key.to_string())
        .cache(CacheConfig::default().max_samples(8))
        .await
        .expect("cached publisher");
    publisher
        .put(serde_json::to_vec(&doc("from-desired")).unwrap())
        .await
        .expect("desired put");

    // Marker watcher, on the controller side (fleet view of drift).
    let marker_sub = controller
        .declare_subscriber("v1/*/state/hostspec/applied/expectations")
        .await
        .expect("marker sub");

    // NOW the sensor side starts — late.
    let sensor = Arc::new(
        zenoh::open(session_config(None, Some(&format!("tcp/127.0.0.1:{port}"))))
            .await
            .expect("sensor session"),
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    let applied: Arc<Mutex<Vec<ExpectationsConfig>>> = Arc::default();
    let applied_in = applied.clone();
    let (_marker, task) = reconcile_topic(
        sensor.clone(),
        Publisher::new(sensor.clone(), "hostspec", Format::Json),
        DesiredTopic {
            topic: "expectations",
            desired_key: key.clone(),
        },
        DesiredConfig::default(),
        ExpectationsConfig::default(), // the file baseline
        move |cfg: ExpectationsConfig| {
            let a = applied_in.clone();
            async move {
                // The sensor-side gate: hostspec's real validate().
                if cfg.absent.iter().any(|e| e.name == "reject-me") {
                    return Err("refused by validation".to_string());
                }
                a.lock().unwrap().push(cfg);
                Ok(())
            }
        },
    );

    // 1. Convergence: baseline marker (file), then the cached doc applies.
    let mut saw_desired = None;
    for _ in 0..4 {
        let m = recv_marker(&marker_sub).await;
        if m.source == AppliedSource::Desired {
            saw_desired = Some(m);
            break;
        }
        assert_eq!(
            m.source,
            AppliedSource::File,
            "only file may precede desired"
        );
    }
    let m = saw_desired.expect("the cached desired doc reached a late reconciler");
    assert!(m.desired_timestamp.is_some(), "LWW handle rides the marker");
    let eff: ExpectationsConfig =
        serde_json::from_str(&m.effective_json).expect("effective parses");
    assert_eq!(eff.absent[0].name, "from-desired");
    assert_eq!(applied.lock().unwrap().len(), 1);

    // 2. An invalid doc (refused by apply/validation): loud, kept off the
    //    handle, marker restates the GOOD config with the rejection beside.
    publisher
        .put(serde_json::to_vec(&doc("reject-me")).unwrap())
        .await
        .expect("bad put");
    let m = recv_marker(&marker_sub).await;
    assert_eq!(
        m.source,
        AppliedSource::Desired,
        "the good config is restated"
    );
    let rej = m.last_rejected.expect("the rejection rides the marker");
    assert!(rej.error.contains("refused by validation"), "{}", rej.error);
    let eff: ExpectationsConfig = serde_json::from_str(&m.effective_json).unwrap();
    assert_eq!(eff.absent[0].name, "from-desired", "previous good kept");
    assert_eq!(applied.lock().unwrap().len(), 1, "nothing applied");

    // And undecodable bytes take the same path.
    publisher
        .put(&b"not a config"[..])
        .await
        .expect("garbage put");
    let m = recv_marker(&marker_sub).await;
    assert!(
        m.last_rejected
            .expect("decode rejection surfaces")
            .error
            .contains("decode")
    );
    assert_eq!(applied.lock().unwrap().len(), 1);

    // 3. Delete reverts to the file baseline.
    publisher.delete().await.expect("desired delete");
    let mut reverted = None;
    for _ in 0..3 {
        let m = recv_marker(&marker_sub).await;
        if m.source == AppliedSource::File {
            reverted = Some(m);
            break;
        }
    }
    let m = reverted.expect("delete reverts to baseline");
    let eff: ExpectationsConfig = serde_json::from_str(&m.effective_json).unwrap();
    assert!(eff.is_empty(), "the baseline is the empty file set");
    assert_eq!(applied.lock().unwrap().len(), 2, "baseline re-applied once");

    // 5. The zenoh-ext canary: the @desired verbatim chunk must not trip the
    //    liveliness-keyexpr bug class.
    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(
        !logs.contains("malformed liveliness token"),
        "zenoh-ext rejected a @desired-derived key:\n{logs}"
    );

    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_kill_switch_applies_nothing_and_says_file() {
    let port = candidate_port();
    let session = Arc::new(
        zenoh::open(session_config(Some(&format!("tcp/127.0.0.1:{port}")), None))
            .await
            .expect("session"),
    );
    let key = desired_key();
    let marker_sub = session
        .declare_subscriber("v1/*/state/hostspec/applied/expectations")
        .await
        .expect("marker sub");
    // A cached desired doc is already waiting…
    let publisher = session
        .declare_publisher(key.to_string())
        .cache(CacheConfig::default().max_samples(8))
        .await
        .expect("cached publisher");
    publisher
        .put(serde_json::to_vec(&doc("never-applied")).unwrap())
        .await
        .expect("put");

    let applied: Arc<Mutex<Vec<ExpectationsConfig>>> = Arc::default();
    let applied_in = applied.clone();
    let (_marker, task) = reconcile_topic(
        session.clone(),
        Publisher::new(session.clone(), "hostspec", Format::Json),
        DesiredTopic {
            topic: "expectations",
            desired_key: key,
        },
        DesiredConfig {
            enabled: false, // the kill switch
            ..Default::default()
        },
        ExpectationsConfig::default(),
        move |cfg: ExpectationsConfig| {
            let a = applied_in.clone();
            async move {
                a.lock().unwrap().push(cfg);
                Ok(())
            }
        },
    );

    // The marker says file — "disabled" never reads as "silent"…
    let m = recv_marker(&marker_sub).await;
    assert_eq!(m.source, AppliedSource::File);
    // …and nothing is ever applied, cached doc or not.
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(applied.lock().unwrap().is_empty(), "kill switch means OFF");
    task.abort();
}
