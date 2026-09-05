//! `session::await_peer` — the wait that turns "I asked too early" into "I
//! waited" (#1039).
//!
//! `zenoh::open` returns before the link to a `connect` endpoint is up, so a
//! GET issued straight away reaches nobody and answers with **zero replies**.
//! Every caller in this tree reads that as "the answer is empty". That is how
//! `zensight-desired apply` came to compile against a fleet of zero hosts and
//! report publishing nothing under exit 0.
//!
//! Two sessions over a real TCP link, because there is nothing to observe in a
//! single-process test: the whole subject is whether a neighbour has appeared.

use std::time::Duration;

fn config(listen: Option<&str>, connect: Option<&str>) -> zenoh::Config {
    let mut c = zenoh::Config::default();
    // Isolated, like every other session test in this directory: multicast and
    // gossip off, so the only neighbour either session can have is the other
    // one — which is what makes the negative case meaningful.
    c.insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    c.insert_json5("scouting/gossip/enabled", "false").unwrap();
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

/// The positive case: a session that dials an endpoint gets a neighbour, and
/// `await_peer` says so well inside the budget a one-shot command can afford.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dialled_endpoint_becomes_a_neighbour() {
    let port = candidate_port();
    let _hub = zenoh::open(config(Some(&format!("tcp/127.0.0.1:{port}")), None))
        .await
        .expect("hub session");

    let client = zenoh::open(config(None, Some(&format!("tcp/127.0.0.1:{port}"))))
        .await
        .expect("client session");

    // Immediately after `open` there may be no neighbour yet — that IS the
    // bug. This asserts the wait, not the absence of the race, because the
    // race is timing-dependent and a test that pinned it would be the flake it
    // is meant to prevent.
    assert!(
        zensight_common::session::await_peer(&client, Duration::from_secs(10)).await,
        "a session that dialled a live endpoint must see a neighbour"
    );
}

/// The negative case, and the reason this returns a bool instead of failing: a
/// process started before its hub must still come up. `await_peer` reports and
/// continues; the caller decides what an unanswered bus is worth.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_neighbour_is_reported_not_fatal() {
    // A port nothing is listening on, chosen below the ephemeral range so the
    // kernel's allocator cannot hand it to somebody else mid-test — the #1004
    // and #1036 lesson.
    let dead = (21_500..21_600)
        .map(|p| std::net::SocketAddr::from(([127, 0, 0, 1], p)))
        .find(|a| std::net::TcpStream::connect_timeout(a, Duration::from_millis(50)).is_err())
        .expect("a closed loopback port below the ephemeral range");

    let lonely = zenoh::open(config(None, Some(&format!("tcp/{dead}"))))
        .await
        .expect("session opens even with nobody to dial");

    let start = std::time::Instant::now();
    assert!(
        !zensight_common::session::await_peer(&lonely, Duration::from_millis(300)).await,
        "there is nobody there, and saying otherwise would be the original bug"
    );
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "the wait must be bounded by its own timeout, not by a network one"
    );
}
