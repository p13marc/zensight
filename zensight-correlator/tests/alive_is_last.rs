//! `alive` ⇒ callable: the catalog must not announce presence before it can
//! answer (RFC 04 §5).
//!
//! # What went wrong, and how it was caught
//!
//! `guard::acquire` used to do three things at once: claim, elect, and declare
//! the owner `alive` token. `main` then spawned every queryable *after* it. So
//! between winning the election and declaring `entities`/`names`/`introspect`/
//! `describe`, the correlator was on the roster and answered nothing.
//!
//! On a developer machine that window is microseconds and nothing ever
//! observed it. On a loaded two-lane CI runner it is wide enough for a judge's
//! introspect sweep to land inside, and `zensight-conformance` failed the build
//! with an `alive ⇒ callable` violation — a gate catching a real defect on its
//! first serious outing, which is the whole argument for having it.
//!
//! `zensight-sensor-core`'s runner has always declared liveliness last, after
//! `await_registry_coverage`, with the reasoning written at `DECLARATION_GRACE`
//! (#648). The correlator is not a `SensorRunner`, so it never inherited the
//! discipline. This test is what keeps it now.

use std::sync::Arc;
use std::time::Duration;

use zensight_correlator::guard::{self, GuardOutcome};

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

/// Whether anything currently holds the catalog's `alive` token.
async fn alive_is_declared(session: &zenoh::Session) -> bool {
    let replies = session
        .liveliness()
        .get(zensight_common::correlator_alive_key().as_str())
        .timeout(Duration::from_secs(2))
        .await
        .expect("liveliness get");
    let mut found = false;
    while let Ok(reply) = replies.recv_async().await {
        if reply.result().is_ok() {
            found = true;
        }
    }
    found
}

/// Winning the election does **not** put the catalog on the roster. Presence is
/// a separate, later step, so the window between "elected" and "callable" is
/// not a window in which the fleet is told to call us.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_election_does_not_announce_presence() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));

    let claim = match guard::acquire(&session, Duration::from_secs(2))
        .await
        .expect("acquire")
    {
        GuardOutcome::Acquired(claim) => claim,
        GuardOutcome::AlreadyRunning => panic!("nothing else is running in an isolated session"),
    };

    assert!(
        !alive_is_declared(&session).await,
        "the catalog is on the roster before it has declared a single queryable — \
         RFC 04 §5 says alive means callable, and this is the window that made \
         zensight-conformance fail"
    );

    let alive = guard::declare_alive(&session)
        .await
        .expect("declare alive after the queryables are up");
    assert!(
        alive_is_declared(&session).await,
        "presence must appear once declare_alive has run"
    );

    drop(alive);
    drop(claim);
}

/// The alive key is minted in exactly one place, so the split above cannot be
/// undone by a second declaration creeping back into the election.
#[test]
fn only_declare_alive_mints_the_alive_token() {
    let guard_src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/guard.rs"),
    )
    .expect("src/guard.rs");

    let code: Vec<&str> = guard_src
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !(t.starts_with("//!") || t.starts_with("///") || t.starts_with("//"))
        })
        .collect();

    // The `use` line names it without minting it; a call does.
    let mints: Vec<&&str> = code
        .iter()
        .filter(|l| l.contains("correlator_alive_key()"))
        .collect();
    assert_eq!(
        mints.len(),
        1,
        "correlator_alive_key() must be reached from exactly one place \
         (guard::declare_alive), so presence cannot drift back into the \
         election — found: {mints:?}"
    );
}
