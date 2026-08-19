//! `query::run` on a host with no reachable system bus (#666).
//!
//! The seven `@rpc/systemd/*` procedures are advertised unconditionally by the
//! registry, and `introspect` hands that slice to the fleet as truth (RFC 08
//! §6.1). Before this, `run` connected to the bus *before* it declared them and
//! returned on failure, so a host without a system bus — a container started
//! without the `/run/dbus/system_bus_socket` mount is the everyday case — got a
//! producer advertising seven procedures that answered nothing at all.
//!
//! Its own test binary on purpose: the served set behind
//! `zensight_common::served` is process-global, and `DBUS_SYSTEM_BUS_ADDRESS`
//! is set once here before anything can touch the bus.

use std::sync::Arc;
use std::time::{Duration, Instant};

use zensight_sensor_systemd::config::CgroupConfig;
use zensight_sensor_systemd::events::EventState;

/// The seven this channel owns. `unit/file` is the nested one.
const PROCEDURES: [&str; 7] = [
    "units",
    "failed",
    "unit",
    "unit/file",
    "events",
    "timers",
    "cgroups",
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
async fn no_system_bus_declares_every_procedure_and_says_why() {
    // A socket path that cannot exist, so `zbus::Connection::system()` fails
    // the way it does on a host without a bus. Safe here: one test, one
    // process, and nothing has opened a connection yet.
    unsafe {
        std::env::set_var(
            "DBUS_SYSTEM_BUS_ADDRESS",
            "unix:path=/nonexistent/zensight-no-system-bus",
        );
    }

    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open session"));

    // Before: the registry advertises them and nothing serves them. This is
    // the state the bug shipped in — assert it, or a fix that declared
    // nothing would pass the "after" check vacuously.
    let before = zensight_common::served::unserved_procedures("systemd");
    for p in PROCEDURES {
        assert!(
            before.iter().any(|u| u == p),
            "`{p}` should start out unserved, got {before:?}"
        );
    }

    tokio::spawn(zensight_sensor_systemd::query::run(
        session.clone(),
        "systemd".to_string(),
        EventState::new(16),
        CgroupConfig::default(),
        false,
    ));

    // Declaration is asynchronous; wait for the set to close rather than
    // sampling it once (the #648 race).
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let missing = zensight_common::served::unserved_procedures("systemd");
        if PROCEDURES.iter().all(|p| !missing.iter().any(|u| u == p)) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "still unserved after 10s: {missing:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Declared is half of it. A caller must be able to tell "this host has no
    // system bus" from "there is no systemd producer here" and from "the
    // capability is switched off" — which is the whole reason the reply is an
    // error with a name rather than silence or an empty list (#648).
    let key = zensight_common::command::query_key("systemd", "units");
    let replies = session.get(&key).await.expect("get units");
    let reply = tokio::time::timeout(Duration::from_secs(5), replies.recv_async())
        .await
        .expect("a reply within 5s")
        .expect("a reply");
    let payload = match reply.result() {
        Ok(_) => panic!("`units` must answer an error, not a value, with no system bus"),
        Err(e) => e.payload().to_bytes().to_vec(),
    };
    let err: zensight_common::rpc::RpcError =
        serde_json::from_slice(&payload).expect("the error payload is an RpcError");
    assert_eq!(
        err.error, "error/systemd/no-system-bus",
        "the reply must name the missing resource, not `gated` (nothing is \
         switched off) or `unsupported` (the build has the capability): {err:?}"
    );
}
