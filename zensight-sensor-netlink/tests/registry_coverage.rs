//! A stock build declares every procedure the registry advertises (#648).
//!
//! `registry/netlink.toml` lists `retransmits` and `connections`
//! unconditionally. They used to be served only by `run_ebpf_queries`, which
//! was `#[cfg(feature = "ebpf")]` *and* spawned only under `collect.ebpf` —
//! both off by default. So a stock `cargo run -p zensight-sensor-netlink`
//! reached `check_registry_coverage` with two procedures advertised and
//! unserved, and `debug_assert!`ed itself to death at startup.
//!
//! This asserts the property that fix rests on: with **no** eBPF state at all,
//! the two procedures are still declared.

use std::sync::Arc;

use zensight_common::served;

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
async fn ebpf_procedures_are_declared_without_the_module() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open session"));

    let before = served::unserved_procedures("netlink");
    for p in ["retransmits", "connections"] {
        assert!(
            before.iter().any(|q| q == p),
            "fixture: `{p}` should start unserved, got {before:?}"
        );
    }

    // Exactly what `main` now does on a build with no eBPF module: spawn the
    // channel with an empty handle rather than skipping it.
    tokio::spawn(zensight_sensor_netlink::query::run_ebpf_queries(
        session.clone(),
        "netlink".to_string(),
        None,
        10,
    ));

    // The declares happen inside that task, so poll rather than sampling once
    // and racing it. Deliberately NOT `await_registry_coverage`: that asserts
    // the producer's WHOLE surface is served, and this test stands up two
    // procedures, not a sensor.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let now = served::unserved_procedures("netlink");
        if !now.iter().any(|q| q == "retransmits" || q == "connections") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let after = served::unserved_procedures("netlink");
    for p in ["retransmits", "connections"] {
        assert!(
            !after.iter().any(|q| q == p),
            "`{p}` must be declared even with no eBPF module, still unserved: {after:?}"
        );
    }
}
