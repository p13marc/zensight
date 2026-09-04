//! An acknowledgement is bus state, not process state (#900, #924, #925).
//!
//! # What this pins, and why reading the code was not enough
//!
//! The whole argument of epic #900 is that acking an alert used to be a
//! `HashSet` insert in one GUI: close the window and the fact that a human was
//! already on the problem vanished, and no second operator ever saw it. The
//! replacement writes the ack to `@catalog/state/ack/<alert_ref>` so that it
//! outlives any one process.
//!
//! That claim has two halves and only the first was ever exercised:
//!
//! 1. **Live.** A session subscribed *before* the write sees it. This works,
//!    and it is what the unit tests in `incidents.rs` reason about.
//! 2. **Late.** A session that connects *after* the write — a second operator
//!    joining a running incident, or the same operator's GUI after a restart —
//!    must still learn about it. That is the half that names the epic, and it
//!    is pure transport: no unit test can see it, because there is no bus in a
//!    unit test.
//!
//! Half 2 was broken when this test was written. The GUI's late-joiner seed
//! (`zensight/src/subscription.rs`) GETs three wildcards — `incident/*`,
//! `ack/*`, `silence/*` — and the correlator served a queryable for only the
//! first. `publish_ack` uses a plain publisher that is dropped at the end of
//! the call, so there is no publisher cache for the subscriber's `history()`
//! to recover from either. In a deployment with no router storage — which is
//! the default the `configs/` ship and what `demo-verify.sh` runs — a
//! restarted GUI showed every acknowledged alert as unacknowledged, silently.
//! Nothing failed; the seed GET simply returned zero replies.
//!
//! Two real sessions over an explicit localhost endpoint, following
//! `entity_seed_stamped.rs`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;
use zensight_common::Protocol;
use zensight_common::alert::{Alert, AlertKind, AlertRef, AlertSeverity};
use zensight_correlator::engine::{CorrelatorState, EvidenceMsg};

/// Scouting off so concurrent tests cannot discover each other; the peers are
/// wired together with an explicit listen/connect endpoint instead.
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

/// A port unlikely to collide: derived from the pid and time, in the dynamic
/// range, retried by the caller if the listen fails.
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

const ORIGIN: &str = "h-aaaaaaaaaaaa";
const PRODUCER: &str = "netlink";
const ALERT_KEY: &str = "k1";
/// The moment the condition started, which is what an ack names — deliberately
/// not the moment of clicking.
const FIRED_AT: i64 = 1_700_000_000_000;

fn the_ref() -> AlertRef {
    AlertRef::new(ORIGIN, PRODUCER, ALERT_KEY)
}

/// A catalog state with exactly one alert firing, so `ack` has an occurrence
/// to name and is not refused by its own `not-firing` gate.
fn state_with_one_firing_alert() -> zensight_correlator::engine::SharedState {
    let mut s = CorrelatorState::new(Default::default());
    let mut alert = Alert::new(
        "web01",
        Protocol::Netlink,
        AlertKind::Expectation,
        "ssh-listening",
        AlertSeverity::Warning,
        "ssh stopped listening",
    );
    alert.timestamp = FIRED_AT;
    s.apply(EvidenceMsg::Alert {
        r: Box::new(the_ref()),
        alert: Some(Box::new(alert)),
    });
    assert!(
        s.firing_alert(&the_ref()).is_some(),
        "the fixture must have something to acknowledge"
    );
    Arc::new(Mutex::new(s))
}

/// Stand the catalog up on a free port: the write procedures (ungated) and the
/// ack/silence seed queryables.
async fn catalog() -> (Arc<zenoh::Session>, u16, watch::Sender<bool>) {
    let (session, port) = {
        let mut opened = None;
        for attempt in 0..8 {
            let port = candidate_port(attempt);
            if let Ok(s) = zenoh::open(listen_config(port)).await {
                opened = Some((Arc::new(s), port));
                break;
            }
        }
        opened.expect("open listening catalog session")
    };
    let (tx, shutdown) = watch::channel(false);
    let state = state_with_one_firing_alert();

    tokio::spawn(zensight_correlator::query::serve_ack_and_silence(
        session.clone(),
        state.clone(),
        zensight_common::Format::Json,
        true, // ungated: the gate itself is covered by unit tests
        shutdown.clone(),
    ));
    tokio::spawn(zensight_correlator::query::serve_acks(
        session.clone(),
        state.clone(),
        shutdown.clone(),
    ));
    tokio::spawn(zensight_correlator::query::serve_silences(
        session.clone(),
        state,
        shutdown,
    ));
    tokio::time::sleep(Duration::from_millis(300)).await;
    (session, port, tx)
}

/// Call the `ack` write procedure the way an operator's GUI does.
async fn call_ack(session: &zenoh::Session, actor: &str) -> Result<Vec<u8>, String> {
    let selector = format!(
        "{}?ref={};actor={}",
        zensight_common::keyexpr::catalog_rpc_key("ack"),
        the_ref(),
        actor
    );
    let replies = session
        .get(&selector)
        .timeout(Duration::from_secs(5))
        .await
        .map_err(|e| format!("ack get: {e}"))?;
    let reply = replies
        .recv_async()
        .await
        .map_err(|_| "the ack procedure sent no reply at all".to_string())?;
    match reply.result() {
        Ok(sample) => Ok(sample.payload().to_bytes().to_vec()),
        Err(err) => Err(String::from_utf8_lossy(&err.payload().to_bytes()).into_owned()),
    }
}

/// **Half 1 — live.** The operator on the other screen, already watching, sees
/// the ack as it happens.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_operator_sees_the_ack_as_it_happens() {
    let (_catalog, port, _tx) = catalog().await;

    let watcher = zenoh::open(connect_config(port))
        .await
        .expect("open watcher session");
    let sub = watcher
        .declare_subscriber(zensight_common::keyexpr::all_acks_wildcard())
        .await
        .expect("declare ack subscriber");
    tokio::time::sleep(Duration::from_millis(500)).await;

    call_ack(&watcher, "alice").await.expect("ack accepted");

    let sample = tokio::time::timeout(Duration::from_secs(5), sub.recv_async())
        .await
        .expect("the ack never reached the subscriber")
        .expect("subscriber closed");
    let ack: zensight_common::ack::AlertAck =
        zensight_common::decode_auto(&sample.payload().to_bytes()).expect("decode ack");

    assert_eq!(ack.by, "alice", "the ack records who made it");
    assert_eq!(
        ack.fired_at, FIRED_AT,
        "an ack names the occurrence, not the moment of clicking — this is the \
         field that makes a re-fire page again (RFC 06 §5.5)"
    );
}

/// **Half 2 — late.** The half that names the epic: a GUI that was not running
/// when the ack was made must still learn about it. This is the property the
/// `HashSet` could never have.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ack_outlives_the_gui_that_made_it() {
    let (_catalog, port, _tx) = catalog().await;

    // The first GUI: connects, acks, and goes away entirely.
    {
        let gui = zenoh::open(connect_config(port))
            .await
            .expect("open first GUI session");
        tokio::time::sleep(Duration::from_millis(500)).await;
        call_ack(&gui, "alice").await.expect("ack accepted");
        gui.close().await.expect("close first GUI session");
    }
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The second GUI: a fresh process that never saw the write. It runs the
    // same late-joiner seed `zensight/src/subscription.rs` runs.
    let restarted = zenoh::open(connect_config(port))
        .await
        .expect("open restarted GUI session");
    tokio::time::sleep(Duration::from_millis(500)).await;

    let replies = restarted
        .get(zensight_common::keyexpr::all_acks_wildcard())
        .target(zenoh::query::QueryTarget::All)
        .timeout(Duration::from_secs(5))
        .await
        .expect("ack seed get");

    let mut seeded = Vec::new();
    while let Ok(reply) = replies.recv_async().await {
        let Ok(sample) = reply.result() else { continue };
        let ack: zensight_common::ack::AlertAck =
            zensight_common::decode_auto(&sample.payload().to_bytes()).expect("decode seeded ack");
        seeded.push(ack);
    }

    assert_eq!(
        seeded.len(),
        1,
        "a GUI that joins after the ack was made saw {} acks. It must see the \
         one that is held: otherwise it shows a firing alert as unacknowledged \
         and a second operator starts work someone is already doing — the exact \
         failure epic #900 exists to remove.",
        seeded.len()
    );
    assert_eq!(seeded[0].by, "alice");
    assert_eq!(seeded[0].fired_at, FIRED_AT);
    assert_eq!(seeded[0].alert_ref, the_ref());
}

/// The seed replies must be stamped, for the same reason the entity seed must
/// (RFC 04 §3.2, #782): a consumer merges seed replies with live samples by
/// HLC, and an unstamped sample cannot be reconciled.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_ack_seed_replies_are_stamped() {
    let (_catalog, port, _tx) = catalog().await;

    let gui = zenoh::open(connect_config(port))
        .await
        .expect("open GUI session");
    tokio::time::sleep(Duration::from_millis(500)).await;
    call_ack(&gui, "alice").await.expect("ack accepted");
    tokio::time::sleep(Duration::from_millis(300)).await;

    let replies = gui
        .get(zensight_common::keyexpr::all_acks_wildcard())
        .target(zenoh::query::QueryTarget::All)
        .timeout(Duration::from_secs(5))
        .await
        .expect("ack seed get");

    let mut seen = 0;
    while let Ok(reply) = replies.recv_async().await {
        let sample = reply.result().expect("seed reply is a value, not an error");
        assert!(
            sample.timestamp().is_some(),
            "the ack seed reply on {} carries no HLC timestamp",
            sample.key_expr()
        );
        seen += 1;
    }
    assert_eq!(seen, 1, "exactly the one ack that is held");
}
