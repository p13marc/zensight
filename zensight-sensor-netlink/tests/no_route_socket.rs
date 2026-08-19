//! netlink's `@rpc` channel with no RTNETLINK socket (#666).
//!
//! `query::run` opened the route socket before it declared anything and
//! returned on failure, leaving ten registry-advertised procedures unserved —
//! the same shape as the systemd bug, through the same door. Opening an
//! RTNETLINK socket is unprivileged, so this is unreachable on an ordinary
//! host; a sandbox that restricts `AF_NETLINK` reaches it, which is what a
//! hardened unit or a seccomp profile does.
//!
//! That is also why this test drives `serve_without_route` directly instead of
//! reproducing the trigger: what it pins is the contract — all ten declared,
//! answering an error that names the missing resource. The single line that
//! calls it from `run`'s error arm is by inspection, and mirrors systemd's,
//! which `zensight-sensor-systemd/tests/no_system_bus.rs` covers end to end.
//!
//! Its own test binary: the served set is process-global.

use std::sync::Arc;
use std::time::{Duration, Instant};

const PROCEDURES: [&str; 10] = [
    "routes",
    "neighbors",
    "sockets",
    "addresses",
    "events",
    "route_changes",
    "tc",
    "xfrm",
    "nft",
    "bandwidth",
];

fn isolated_config() -> zenoh::Config {
    let mut config = zenoh::Config::default();
    config
        .insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    config.insert_json5("listen/endpoints", "[]").unwrap();
    config.insert_json5("connect/endpoints", "[]").unwrap();
    config
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_route_socket_declares_every_procedure_and_says_why() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open session"));

    // Before: advertised by the registry, served by nobody — the state the bug
    // shipped in. Without this the "after" check could pass vacuously.
    let before = zensight_common::served::unserved_procedures("netlink");
    for p in PROCEDURES {
        assert!(
            before.iter().any(|u| u == p),
            "`{p}` should start out unserved, got {before:?}"
        );
    }

    tokio::spawn(zensight_sensor_netlink::query::serve_without_route(
        session.clone(),
        "netlink",
        "Operation not permitted (os error 1)".to_string(),
    ));

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let missing = zensight_common::served::unserved_procedures("netlink");
        if PROCEDURES.iter().all(|p| !missing.iter().any(|u| u == p)) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "still unserved after 10s: {missing:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The reply must distinguish "this host refused the socket" from "no
    // netlink producer on the bus" and from "switched off in config" (#648).
    let key = zensight_common::command::query_key("netlink", "routes");
    let replies = session.get(&key).await.expect("get routes");
    let reply = tokio::time::timeout(Duration::from_secs(5), replies.recv_async())
        .await
        .expect("a reply within 5s")
        .expect("a reply");
    let payload = match reply.result() {
        Ok(_) => panic!("`routes` must answer an error, not a value, with no route socket"),
        Err(e) => e.payload().to_bytes().to_vec(),
    };
    let err: zensight_common::rpc::RpcError =
        serde_json::from_slice(&payload).expect("the error payload is an RpcError");
    assert_eq!(
        err.error, "error/netlink/no-route-socket",
        "the reply must name the missing resource: {err:?}"
    );
}
