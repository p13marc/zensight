//! End-to-end poller tests against an in-process SNMP agent (issue #540).
//!
//! Real UDP round-trips on localhost: async-snmp's agent framework serves a
//! synthetic MIB, the sensor's `SnmpPoller` polls it, and assertions read the
//! published `TelemetryPoint`s off an isolated in-process Zenoh peer.

mod harness;

use std::time::Duration;

use async_snmp::{AuthProtocol as AgentAuth, PrivProtocol as AgentPriv, Value};
use harness::{FlakyProxy, IF_TABLE, SYSTEM, SimAgent, SimMib, collect_points, rig, v2c_device};
use zensight_common::TelemetryValue;
use zensight_sensor_snmp::config::{AuthProtocol, PrivProtocol, SnmpV3Security, SnmpVersion};

const IDLE: Duration = Duration::from_millis(500);

fn base_mib() -> SimMib {
    SimMib::new().with_system_group().with_if_table(2)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2c_get_publishes_points() {
    let agent = SimAgent::start(base_mib()).await;
    let mut device = v2c_device("router01", agent.addr());
    device.oids = vec![
        format!("{SYSTEM}.3.0"), // sysUpTime
        format!("{SYSTEM}.5.0"), // sysName
    ];

    let rig = rig(device).await;
    rig.poller.poll_once().await.expect("poll");

    let points = collect_points(&rig, IDLE).await;
    let uptime = &points["system/uptime"];
    assert_eq!(uptime.value, TelemetryValue::Counter(123_456));
    assert_eq!(
        uptime.labels.get("oid").map(String::as_str),
        Some("1.3.6.1.2.1.1.3.0")
    );
    assert_eq!(uptime.source, "router01");
    let name = &points["system/name"];
    assert_eq!(name.value, TelemetryValue::Text("sim-device".into()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2c_walk_stays_in_subtree() {
    let agent = SimAgent::start(base_mib()).await;
    let mut device = v2c_device("router01", agent.addr());
    device.walks = vec!["1.3.6.1.2.1.2.2".to_string()]; // ifTable only

    let rig = rig(device).await;
    rig.poller.poll_once().await.expect("poll");

    let points = collect_points(&rig, IDLE).await;
    // 12 ifTable columns × 2 interfaces, resolved to `if/<index>/<name>` names.
    assert_eq!(points.len(), 24, "unexpected metrics: {:?}", points.keys());
    assert_eq!(points["if/1/in_octets"].value, TelemetryValue::Counter(0));
    assert_eq!(
        points["if/2/descr"].value,
        TelemetryValue::Text("eth1".into())
    );
    // Nothing from ifXTable (outside the walked subtree).
    assert!(!points.keys().any(|m| m.starts_with("ifx/")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v1_get_and_walk() {
    let agent = SimAgent::start(base_mib()).await;
    let mut device = v2c_device("legacy01", agent.addr());
    device.version = SnmpVersion::V1;
    device.oids = vec![format!("{SYSTEM}.5.0")];
    device.walks = vec![format!("{IF_TABLE}.2")]; // ifDescr column

    let rig = rig(device).await;
    rig.poller.poll_once().await.expect("poll");

    let points = collect_points(&rig, IDLE).await;
    assert_eq!(
        points["system/name"].value,
        TelemetryValue::Text("sim-device".into())
    );
    assert_eq!(
        points["if/1/descr"].value,
        TelemetryValue::Text("eth0".into())
    );
    assert_eq!(points.len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn walk_large_table_is_complete() {
    let agent = SimAgent::start(SimMib::new().with_if_table(64)).await;
    let mut device = v2c_device("switch01", agent.addr());
    device.walks = vec![format!("{IF_TABLE}.10")]; // ifInOctets column

    let rig = rig(device).await;
    rig.poller.poll_once().await.expect("poll");

    let points = collect_points(&rig, IDLE).await;
    assert_eq!(points.len(), 64, "row(s) missing from large walk");
    assert!(points.contains_key("if/64/in_octets"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutated_values_show_up_next_cycle() {
    let mib = base_mib();
    let agent = SimAgent::start(mib.clone()).await;
    let mut device = v2c_device("router01", agent.addr());
    device.oids = vec![format!("{IF_TABLE}.10.1")];

    let rig = rig(device).await;
    rig.poller.poll_once().await.expect("poll");
    let points = collect_points(&rig, IDLE).await;
    assert_eq!(points["if/1/in_octets"].value, TelemetryValue::Counter(0));

    mib.set(&format!("{IF_TABLE}.10.1"), Value::Counter32(9_000));
    rig.poller.poll_once().await.expect("poll");
    let points = collect_points(&rig, IDLE).await;
    assert_eq!(
        points["if/1/in_octets"].value,
        TelemetryValue::Counter(9_000)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreachable_device_then_recovery() {
    let agent = SimAgent::start(base_mib()).await;
    let proxy = FlakyProxy::start(agent.addr()).await;
    let mut device = v2c_device("flaky01", proxy.addr());
    device.oids = vec![format!("{SYSTEM}.5.0")];

    let rig = rig(device).await;

    // Blackholed: the poll cycle survives (errors are logged, not fatal) and
    // publishes nothing.
    proxy.set_blackhole(true);
    rig.poller
        .poll_once()
        .await
        .expect("poll cycle must survive");
    let points = collect_points(&rig, IDLE).await;
    assert!(
        points.is_empty(),
        "blackholed poll published: {:?}",
        points.keys()
    );

    // Recovered: next cycle publishes again.
    proxy.set_blackhole(false);
    rig.poller.poll_once().await.expect("poll");
    let points = collect_points(&rig, IDLE).await;
    assert_eq!(
        points["system/name"].value,
        TelemetryValue::Text("sim-device".into())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_datagram_recovers_by_next_cycle() {
    let agent = SimAgent::start(base_mib()).await;
    let proxy = FlakyProxy::start(agent.addr()).await;
    let mut device = v2c_device("flaky02", proxy.addr());
    device.oids = vec![format!("{SYSTEM}.5.0")];

    let rig = rig(device).await;

    // One lost request. The current client has no within-request retry, so
    // this cycle may come up empty — but the next cycle must succeed.
    // (#526 tightens this: with retries configured, the SAME cycle succeeds.)
    proxy.drop_next(1);
    rig.poller
        .poll_once()
        .await
        .expect("poll cycle must survive");
    let _ = collect_points(&rig, IDLE).await;

    rig.poller.poll_once().await.expect("poll");
    let points = collect_points(&rig, IDLE).await;
    assert_eq!(
        points["system/name"].value,
        TelemetryValue::Text("sim-device".into())
    );
}

// ---------------------------------------------------------------------------
// SNMPv3 matrix
// ---------------------------------------------------------------------------

/// Poll one sysName GET through the given client-side v3 security against an
/// agent provisioned with `provision`; return whether a point arrived.
async fn v3_roundtrip(
    security: SnmpV3Security,
    provision: impl FnOnce(async_snmp::AgentBuilder) -> async_snmp::AgentBuilder,
) -> bool {
    let agent = SimAgent::start_with(base_mib(), provision).await;
    let mut device = v2c_device("v3dev", agent.addr());
    device.version = SnmpVersion::V3;
    device.security = Some(security);
    device.oids = vec![format!("{SYSTEM}.5.0")];

    let session = std::sync::Arc::new(
        zenoh::open(harness::isolated_zenoh_config())
            .await
            .expect("open zenoh"),
    );
    let sub = session
        .declare_subscriber("v1/*/telemetry/snmp/**")
        .await
        .expect("subscriber");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let mut resolver = zensight_sensor_snmp::mib::MibResolver::new();
    resolver.add_custom_mappings(&harness::test_oid_names());
    let mut poller = zensight_sensor_snmp::poller::SnmpPoller::new(
        device,
        session.clone(),
        std::sync::Arc::new(resolver),
        &std::collections::HashMap::new(),
        zensight_common::Format::Json,
    );

    // With bad credentials either init (engine sync) or the GET itself fails.
    if poller.init().await.is_err() {
        return false;
    }
    if poller.poll_once().await.is_err() {
        return false;
    }
    tokio::time::timeout(Duration::from_secs(3), sub.recv_async())
        .await
        .is_ok()
}

fn v3_security(
    auth: AuthProtocol,
    auth_pw: Option<&str>,
    privacy: PrivProtocol,
    priv_pw: Option<&str>,
) -> SnmpV3Security {
    SnmpV3Security {
        username: "monitor".to_string(),
        auth_protocol: auth,
        auth_password: auth_pw.map(String::from),
        priv_protocol: privacy,
        priv_password: priv_pw.map(String::from),
        engine_id: None,
    }
}

/// snmp2 0.4.14 panics (index out of bounds, `v3.rs:121`) building the USM
/// key from an empty password, so a noAuthNoPriv v3 device crashes the
/// current sensor outright.
#[ignore = "snmp2 panics on noAuthNoPriv (empty password) — enabled by the #526 migration"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v3_no_auth_no_priv() {
    let ok = v3_roundtrip(
        v3_security(AuthProtocol::None, None, PrivProtocol::None, None),
        |b| b.usm_user("monitor", |u| u),
    )
    .await;
    assert!(ok);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v3_auth_no_priv_sha1() {
    let ok = v3_roundtrip(
        v3_security(
            AuthProtocol::Sha1,
            Some("authpass123"),
            PrivProtocol::None,
            None,
        ),
        |b| b.usm_user("monitor", |u| u.auth(AgentAuth::Sha1, b"authpass123")),
    )
    .await;
    assert!(ok);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v3_auth_no_priv_sha256() {
    let ok = v3_roundtrip(
        v3_security(
            AuthProtocol::Sha256,
            Some("authpass123"),
            PrivProtocol::None,
            None,
        ),
        |b| b.usm_user("monitor", |u| u.auth(AgentAuth::Sha256, b"authpass123")),
    )
    .await;
    assert!(ok);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v3_auth_priv_sha1_aes128() {
    let ok = v3_roundtrip(
        v3_security(
            AuthProtocol::Sha1,
            Some("authpass123"),
            PrivProtocol::Aes128,
            Some("privpass123"),
        ),
        |b| {
            b.usm_user("monitor", |u| {
                u.auth(AgentAuth::Sha1, b"authpass123")
                    .privacy(AgentPriv::Aes128, b"privpass123")
            })
        },
    )
    .await;
    assert!(ok);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v3_auth_priv_sha256_aes128() {
    let ok = v3_roundtrip(
        v3_security(
            AuthProtocol::Sha256,
            Some("authpass123"),
            PrivProtocol::Aes128,
            Some("privpass123"),
        ),
        |b| {
            b.usm_user("monitor", |u| {
                u.auth(AgentAuth::Sha256, b"authpass123")
                    .privacy(AgentPriv::Aes128, b"privpass123")
            })
        },
    )
    .await;
    assert!(ok);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v3_auth_priv_sha256_aes256() {
    let ok = v3_roundtrip(
        v3_security(
            AuthProtocol::Sha256,
            Some("authpass123"),
            PrivProtocol::Aes256,
            Some("privpass123"),
        ),
        |b| {
            b.usm_user("monitor", |u| {
                u.auth(AgentAuth::Sha256, b"authpass123")
                    .privacy(AgentPriv::Aes256, b"privpass123")
            })
        },
    )
    .await;
    assert!(ok);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v3_wrong_auth_password_yields_nothing() {
    let ok = v3_roundtrip(
        v3_security(
            AuthProtocol::Sha256,
            Some("WRONG"),
            PrivProtocol::None,
            None,
        ),
        |b| b.usm_user("monitor", |u| u.auth(AgentAuth::Sha256, b"authpass123")),
    )
    .await;
    assert!(!ok, "wrong auth password must not produce telemetry");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v3_wrong_priv_password_yields_nothing() {
    let ok = v3_roundtrip(
        v3_security(
            AuthProtocol::Sha256,
            Some("authpass123"),
            PrivProtocol::Aes128,
            Some("WRONG"),
        ),
        |b| {
            b.usm_user("monitor", |u| {
                u.auth(AgentAuth::Sha256, b"authpass123")
                    .privacy(AgentPriv::Aes128, b"privpass123")
            })
        },
    )
    .await;
    assert!(!ok, "wrong privacy password must not produce telemetry");
}

/// Engine re-discovery after the agent restarts with a fresh engine identity.
/// The snmp2 persistent v3 session cannot resynchronize; async-snmp can.
#[ignore = "requires client-side engine resync — enabled by the #526 migration"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v3_engine_rediscovery_after_agent_restart() {
    let provision = |engine: &'static [u8]| {
        move |b: async_snmp::AgentBuilder| {
            b.engine_id(engine.to_vec())
                .usm_user("monitor", |u| u.auth(AgentAuth::Sha256, b"authpass123"))
        }
    };

    let agent = SimAgent::start_with(base_mib(), provision(b"\x80\x00\x00\x00\x01engineA")).await;
    let proxy = FlakyProxy::start(agent.addr()).await;
    let mut device = v2c_device("restarting01", proxy.addr());
    device.version = SnmpVersion::V3;
    device.security = Some(v3_security(
        AuthProtocol::Sha256,
        Some("authpass123"),
        PrivProtocol::None,
        None,
    ));
    device.oids = vec![format!("{SYSTEM}.5.0")];

    let rig = rig(device).await;
    rig.poller.poll_once().await.expect("poll");
    assert!(!collect_points(&rig, IDLE).await.is_empty());

    // Restart with a different engine identity on a new port.
    agent.shutdown();
    let agent2 = SimAgent::start_with(base_mib(), provision(b"\x80\x00\x00\x00\x01engineB")).await;
    proxy.set_backend(agent2.addr());

    // The client must rediscover the engine and keep polling (may take one
    // failed cycle to notice).
    let mut recovered = false;
    for _ in 0..3 {
        let _ = rig.poller.poll_once().await;
        if !collect_points(&rig, IDLE).await.is_empty() {
            recovered = true;
            break;
        }
    }
    assert!(recovered, "poller never recovered after engine change");
}
