//! registry ⊆ served, end to end (#484, RFC 08 §6.1).
//!
//! `zensight_common::served` unit-tests the bookkeeping; this asserts the
//! wiring that makes it true of a real producer: serving a procedure through
//! the sensor-core `@rpc` path is what marks it served, on the exact key the
//! coverage check looks for. If those two ever spell the key differently the
//! guard would be vacuous — reporting everything as unserved, or nothing.

use std::sync::Arc;

use zensight_common::rpc::{RpcError, RpcResult};
use zensight_common::served;
use zensight_sensor_core::v1::V1Context;

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
async fn serving_a_procedure_closes_its_coverage_gap() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open session"));
    let ctx = V1Context::for_producer(&zensight_common::PROFILE, "sysinfo");

    // sysinfo's registry declares `processes` among its procedures. Before it
    // is served, the guard must name it — this is the state the #453 audit
    // shipped in, with seven such surfaces advertised and none served.
    let before = served::unserved_procedures("sysinfo");
    assert!(
        before.iter().any(|p| p == "processes"),
        "expected `processes` reported unserved, got {before:?}"
    );

    // Serve it the way a sensor does.
    let _task =
        zensight_sensor_core::rpc::serve(session.clone(), &ctx, &["processes"], |_req| async {
            RpcResult::Err(RpcError::unsupported("test"))
        })
        .await
        .expect("serve processes");

    // ...and the gap closes — same key, both sides.
    let after = served::unserved_procedures("sysinfo");
    assert!(
        !after.iter().any(|p| p == "processes"),
        "serving `processes` must close its gap, still got {after:?}"
    );
    assert_eq!(
        after.len(),
        before.len() - 1,
        "serving one procedure closes exactly one gap"
    );

    // The guard stays silent only when nothing is missing; with the rest of
    // sysinfo's surface unserved here, it still has something to report.
    assert!(
        !after.is_empty(),
        "this test serves one procedure, so others remain unserved"
    );
}
