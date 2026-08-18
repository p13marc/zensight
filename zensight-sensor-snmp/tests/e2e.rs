//! End-to-end poller tests against an in-process SNMP agent (issue #540).
//!
//! Real UDP round-trips on localhost: async-snmp's agent framework serves a
//! synthetic MIB, the sensor's `SnmpPoller` polls it, and assertions read the
//! published `TelemetryPoint`s off an isolated in-process Zenoh peer.

mod harness;

use std::time::Duration;

use async_snmp::{AuthProtocol as AgentAuth, PrivProtocol as AgentPriv, Value};
use harness::{
    FlakyProxy, IF_TABLE, IF_X_TABLE, SYSTEM, SimAgent, SimMib, collect_points, rig, v2c_device,
};
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
    // TimeTicks arrive as seconds (Gauge, unit "s") since #527 — the sim
    // agent serves 123_456 centiseconds.
    assert_eq!(uptime.value, TelemetryValue::Gauge(1_234.56));
    assert_eq!(uptime.unit.as_deref(), Some("s"));
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
    // Gauge32 is a Gauge since #527 (previously mis-published as Counter).
    assert_eq!(
        points["if/1/speed"].value,
        TelemetryValue::Gauge(100_000_000.0)
    );
    assert_eq!(
        points["if/2/descr"].value,
        TelemetryValue::Text("eth1".into())
    );
    // Nothing from ifXTable (outside the walked subtree).
    assert!(!points.keys().any(|m| m.starts_with("ifx/")));
}

/// #559 acceptance: a debug-build poll cycle with the production resolver
/// (built-in MIB tables, no harness name overrides) publishes without
/// tripping the registry metric guard, and every published key refines into
/// the registry's `{device}/{metric...}` subject.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn builtin_mib_names_survive_the_metric_guard() {
    let agent = SimAgent::start(base_mib()).await;
    let mut device = v2c_device("router01", agent.addr());
    device.oids = vec![
        format!("{SYSTEM}.3.0"), // sysUpTime
        format!("{SYSTEM}.5.0"), // sysName
    ];
    device.walks = vec!["1.3.6.1.2.1.2.2".to_string()]; // ifTable

    let rig = harness::rig_with_builtins(device).await;
    // In a debug build this cycle panics inside the metric guard if any
    // builtin name violates the chunk grammar.
    rig.poller.poll_once().await.expect("poll");

    let mut seen = std::collections::HashMap::new();
    while let Ok(Ok(sample)) = tokio::time::timeout(IDLE, rig.sub.recv_async()).await {
        let key = sample.key_expr().to_string();
        let refined = zensight_common::registry::refine_key(&key);
        assert!(
            refined.is_some(),
            "published key {key:?} does not refine into a registry subject (#559)"
        );
        let point: zensight_common::TelemetryPoint =
            zensight_common::decode_auto(&sample.payload().to_bytes()).expect("decode point");
        seen.insert(point.metric.clone(), key);
    }
    // Builtin names, not the harness table, named these.
    assert!(seen.contains_key("system/uptime"), "seen: {seen:?}");
    assert!(seen.contains_key("system/name"), "seen: {seen:?}");
    assert!(seen.contains_key("if/1/in_octets"), "seen: {seen:?}");
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

/// Table walks ride GETBULK on v2c: 64 rows must take a handful of
/// round-trips, not one per row (the old GETNEXT loop's 65 requests).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2c_walk_uses_getbulk() {
    let agent = SimAgent::start(SimMib::new().with_if_table(64)).await;
    let proxy = FlakyProxy::start(agent.addr()).await;
    let mut device = v2c_device("bulk01", proxy.addr());
    device.walks = vec![format!("{IF_TABLE}.10")]; // ifInOctets column

    let rig = rig(device).await;
    rig.poller.poll_once().await.expect("poll");

    let points = collect_points(&rig, IDLE).await;
    assert_eq!(points.len(), 64);
    let requests = proxy.forwarded();
    assert!(
        requests <= 8,
        "expected GETBULK round-trips for 64 rows, saw {requests} requests"
    );
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

/// Counters grow a `<metric>.rate` sibling (per-second Gauge) once a
/// previous sample exists; the raw counter keeps publishing untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn counter_rate_published_from_second_cycle() {
    let mib = base_mib();
    let agent = SimAgent::start(mib.clone()).await;
    let mut device = v2c_device("router01", agent.addr());
    device.oids = vec![format!("{IF_TABLE}.10.1")];

    let rig = rig(device).await;
    rig.poller.poll_once().await.expect("poll");
    let points = collect_points(&rig, IDLE).await;
    assert!(
        !points.contains_key("if/1/in_octets.rate"),
        "no rate before a second sample"
    );

    mib.set(&format!("{IF_TABLE}.10.1"), Value::Counter32(9_000));
    rig.poller.poll_once().await.expect("poll");
    let points = collect_points(&rig, IDLE).await;
    assert_eq!(
        points["if/1/in_octets"].value,
        TelemetryValue::Counter(9_000)
    );
    let rate = &points["if/1/in_octets.rate"];
    assert_eq!(rate.unit.as_deref(), Some("By/s"));
    let TelemetryValue::Gauge(per_sec) = rate.value else {
        panic!("rate must be a gauge, got {:?}", rate.value);
    };
    assert!(per_sec > 0.0, "rate {per_sec}");
    assert_eq!(
        rate.labels.get("oid").map(String::as_str),
        Some(format!("{IF_TABLE}.10.1").as_str())
    );
}

/// A Counter32 wrap between cycles still yields a rate (modular delta), not
/// a garbage spike or a suppressed interval.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn counter32_wrap_still_rates() {
    let mib = base_mib();
    let agent = SimAgent::start(mib.clone()).await;
    let mut device = v2c_device("router01", agent.addr());
    device.oids = vec![format!("{IF_TABLE}.10.1")];

    mib.set(&format!("{IF_TABLE}.10.1"), Value::Counter32(u32::MAX - 99));
    let rig = rig(device).await;
    rig.poller.poll_once().await.expect("poll");
    let _ = collect_points(&rig, IDLE).await;

    // Past the wrap point: modular delta is 100 + 400 = 500.
    mib.set(&format!("{IF_TABLE}.10.1"), Value::Counter32(400));
    rig.poller.poll_once().await.expect("poll");
    let points = collect_points(&rig, IDLE).await;
    let rate = &points["if/1/in_octets.rate"];
    let TelemetryValue::Gauge(per_sec) = rate.value else {
        panic!("rate must be a gauge");
    };
    // Exact value depends on wall-clock dt; correctness of the math is
    // unit-tested. Here: it exists and is not an absurd reset artifact.
    assert!(per_sec.is_finite() && per_sec >= 0.0);
}

/// sysUpTime going backwards (device reboot) suppresses every rate for one
/// cycle; the next cycle re-baselines and rates return.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reboot_suppresses_rates_for_one_cycle() {
    let mib = base_mib();
    let agent = SimAgent::start(mib.clone()).await;
    let mut device = v2c_device("router01", agent.addr());
    device.oids = vec![format!("{IF_TABLE}.10.1")];

    let rig = rig(device).await;
    rig.poller.poll_once().await.expect("poll");
    let _ = collect_points(&rig, IDLE).await;

    // Reboot: uptime restarts near zero, counters reset low.
    mib.set(&format!("{SYSTEM}.3.0"), Value::TimeTicks(50));
    mib.set(&format!("{IF_TABLE}.10.1"), Value::Counter32(10));
    rig.poller.poll_once().await.expect("poll");
    let points = collect_points(&rig, IDLE).await;
    assert!(
        !points.contains_key("if/1/in_octets.rate"),
        "reboot cycle must not publish rates"
    );

    // Next cycle: fresh baseline exists, rates resume.
    mib.set(&format!("{SYSTEM}.3.0"), Value::TimeTicks(150));
    mib.set(&format!("{IF_TABLE}.10.1"), Value::Counter32(1_010));
    rig.poller.poll_once().await.expect("poll");
    let points = collect_points(&rig, IDLE).await;
    assert!(
        points.contains_key("if/1/in_octets.rate"),
        "rates must resume after re-baselining: {:?}",
        points.keys()
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

/// With retries configured, dropped datagrams are retransmitted and the SAME
/// poll cycle succeeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retries_recover_within_one_cycle() {
    let agent = SimAgent::start(base_mib()).await;
    let proxy = FlakyProxy::start(agent.addr()).await;
    let mut device = v2c_device("flaky02", proxy.addr());
    device.oids = vec![format!("{SYSTEM}.5.0")];
    device.retries = 2;

    let rig = rig(device).await;

    // Two lost requests, two retries: the third attempt lands.
    proxy.drop_next(2);
    rig.poller.poll_once().await.expect("poll");
    let points = collect_points(&rig, IDLE).await;
    assert_eq!(
        points["system/name"].value,
        TelemetryValue::Text("sim-device".into())
    );
}

/// More losses than retries: that cycle publishes nothing, the next recovers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exhausted_retries_recover_by_next_cycle() {
    let agent = SimAgent::start(base_mib()).await;
    let proxy = FlakyProxy::start(agent.addr()).await;
    let mut device = v2c_device("flaky03", proxy.addr());
    device.oids = vec![format!("{SYSTEM}.5.0")];
    device.retries = 1;

    let rig = rig(device).await;

    // Four lost datagrams against a single retry: the sysUpTime probe eats
    // its two attempts, then the sysName GET's two attempts both die.
    proxy.drop_next(4);
    rig.poller
        .poll_once()
        .await
        .expect("poll cycle must survive");
    let points = collect_points(&rig, IDLE).await;
    assert!(
        points.is_empty(),
        "cycle with exhausted retries published: {:?}",
        points.keys()
    );

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

/// A throwaway authoritative engine for the sim agent (0.17 requires one for
/// any v3 role; no persistence in tests).
fn test_engine(engine_id: Vec<u8>) -> async_snmp::AuthoritativeEngine {
    async_snmp::AuthoritativeEngine::install(engine_id, |_| Ok::<(), std::convert::Infallible>(()))
        .expect("install test engine")
}

/// Poll one sysName GET through the given client-side v3 security against an
/// agent provisioned with `provision`; return whether a point arrived.
async fn v3_roundtrip(
    security: SnmpV3Security,
    provision: impl FnOnce(async_snmp::AgentBuilder) -> async_snmp::AgentBuilder,
) -> bool {
    // Default engine identity first; a test's `provision` may override it.
    let agent = SimAgent::start_with(base_mib(), |b| {
        provision(b.authoritative_engine(test_engine(b"\x80\x00\x00\x00\x01v3test".to_vec())))
    })
    .await;
    let mut device = v2c_device("v3dev", agent.addr());
    device.version = SnmpVersion::V3;
    device.security = Some(security);
    device.oids = vec![format!("{SYSTEM}.5.0")];
    // v3 report flows (time-sync, engine resync) consume a retry attempt
    // before the request proper — a retry budget is part of normal operation.
    device.retries = 1;

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

/// noAuthNoPriv used to crash the sensor outright: snmp2 0.4.14 panicked
/// (index out of bounds) building the USM key from an empty password.
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
                u.auth_priv(
                    AgentAuth::Sha1,
                    b"authpass123",
                    AgentPriv::Aes128,
                    b"privpass123",
                )
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
                u.auth_priv(
                    AgentAuth::Sha256,
                    b"authpass123",
                    AgentPriv::Aes128,
                    b"privpass123",
                )
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
                u.auth_priv(
                    AgentAuth::Sha256,
                    b"authpass123",
                    AgentPriv::Aes256,
                    b"privpass123",
                )
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
                u.auth_priv(
                    AgentAuth::Sha256,
                    b"authpass123",
                    AgentPriv::Aes128,
                    b"privpass123",
                )
            })
        },
    )
    .await;
    assert!(!ok, "wrong privacy password must not produce telemetry");
}

/// Engine re-discovery after the agent restarts with a fresh engine identity.
/// The snmp2 persistent v3 session could not resynchronize; async-snmp can.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v3_engine_rediscovery_after_agent_restart() {
    let provision = |engine: &'static [u8]| {
        move |b: async_snmp::AgentBuilder| {
            b.authoritative_engine(test_engine(engine.to_vec()))
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
    device.retries = 1;

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

/// A configured `engine_id` pre-seeds the engine cache (no discovery
/// round-trip) and the first authenticated exchange time-syncs normally.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v3_configured_engine_id_polls() {
    const ENGINE: &[u8] = b"\x80\x00\x00\x00\x01engineC";
    let engine_hex: String = ENGINE.iter().map(|b| format!("{b:02x}")).collect();

    let mut security = v3_security(
        AuthProtocol::Sha256,
        Some("authpass123"),
        PrivProtocol::None,
        None,
    );
    security.engine_id = Some(engine_hex);

    let ok = v3_roundtrip(security, |b| {
        b.authoritative_engine(test_engine(ENGINE.to_vec()))
            .usm_user("monitor", |u| u.auth(AgentAuth::Sha256, b"authpass123"))
    })
    .await;
    assert!(ok, "pre-seeded engine id must poll successfully");
}

// ---------------------------------------------------------------------------
// Threshold alerts (#528)
// ---------------------------------------------------------------------------

use harness::{collect_alerts, rig_with_alerts};
use zensight_common::AlertState;
use zensight_sensor_snmp::alerts::SnmpAlertsConfig;

fn firing<'a>(
    events: &'a [(zenoh::sample::SampleKind, Option<zensight_common::Alert>)],
    rule: &str,
) -> Vec<&'a zensight_common::Alert> {
    events
        .iter()
        .filter_map(|(kind, alert)| match (kind, alert) {
            (zenoh::sample::SampleKind::Put, Some(a))
                if a.rule == rule && a.state == AlertState::Firing =>
            {
                Some(a)
            }
            _ => None,
        })
        .collect()
}

fn resolved<'a>(
    events: &'a [(zenoh::sample::SampleKind, Option<zensight_common::Alert>)],
    rule: &str,
) -> Vec<&'a zensight_common::Alert> {
    events
        .iter()
        .filter_map(|(kind, alert)| match (kind, alert) {
            (zenoh::sample::SampleKind::Put, Some(a))
                if a.rule == rule && a.state == AlertState::Resolved =>
            {
                Some(a)
            }
            _ => None,
        })
        .collect()
}

/// Killing the device fires `device_unreachable` after N cycles; recovery
/// resolves it (Put(Resolved) + Delete tombstone).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreachable_alert_fires_and_resolves() {
    let agent = SimAgent::start(base_mib()).await;
    let proxy = FlakyProxy::start(agent.addr()).await;
    let mut device = v2c_device("dead01", proxy.addr());
    device.oids = vec![format!("{SYSTEM}.5.0")];

    let mut cfg = SnmpAlertsConfig::default();
    cfg.unreachable.cycles = 2;
    // Only the rule under test — the interface rules would auto-add walks.
    cfg.interface_down.enabled = false;
    cfg.interface_errors.enabled = false;
    cfg.utilization.enabled = false;
    let ar = rig_with_alerts(device, cfg).await;

    proxy.set_blackhole(true);
    ar.rig.poller.poll_once().await.expect("poll");
    let events = collect_alerts(&ar, IDLE).await;
    assert!(
        firing(&events, "device_unreachable").is_empty(),
        "one failed cycle must not fire yet"
    );

    ar.rig.poller.poll_once().await.expect("poll");
    let events = collect_alerts(&ar, IDLE).await;
    let f = firing(&events, "device_unreachable");
    assert_eq!(f.len(), 1, "second failed cycle fires");
    assert_eq!(f[0].labels["device"], "dead01");

    // Recovery: resolve + tombstone.
    proxy.set_blackhole(false);
    ar.rig.poller.poll_once().await.expect("poll");
    let events = collect_alerts(&ar, IDLE).await;
    assert_eq!(resolved(&events, "device_unreachable").len(), 1);
    assert!(
        events
            .iter()
            .any(|(kind, _)| *kind == zenoh::sample::SampleKind::Delete),
        "resolve must tombstone the alert key"
    );
}

/// ifOperStatus down while admin-up fires `interface_down`; link recovery
/// resolves it. The needed IF-MIB columns are auto-walked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interface_down_alert_lifecycle() {
    let mib = base_mib();
    let agent = SimAgent::start(mib.clone()).await;
    let device = v2c_device("edge01", agent.addr());

    let ar = rig_with_alerts(device, SnmpAlertsConfig::default()).await;

    ar.rig.poller.poll_once().await.expect("poll");
    let events = collect_alerts(&ar, IDLE).await;
    assert!(firing(&events, "interface_down").is_empty(), "links are up");

    // eth0 goes oper-down.
    mib.set(&format!("{IF_TABLE}.8.1"), Value::Integer(2));
    ar.rig.poller.poll_once().await.expect("poll");
    let events = collect_alerts(&ar, IDLE).await;
    let f = firing(&events, "interface_down");
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].labels["if_index"], "1");
    assert_eq!(f[0].labels["if_name"], "eth0");

    // Link recovers.
    mib.set(&format!("{IF_TABLE}.8.1"), Value::Integer(1));
    ar.rig.poller.poll_once().await.expect("poll");
    let events = collect_alerts(&ar, IDLE).await;
    assert_eq!(resolved(&events, "interface_down").len(), 1);
}

/// A fast-growing error counter fires `interface_errors` once its rate
/// crosses the threshold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interface_error_rate_alert() {
    let mib = base_mib();
    let agent = SimAgent::start(mib.clone()).await;
    let device = v2c_device("noisy01", agent.addr());

    let ar = rig_with_alerts(device, SnmpAlertsConfig::default()).await;
    ar.rig.poller.poll_once().await.expect("poll");
    let _ = collect_alerts(&ar, IDLE).await;

    // Thousands of input errors between cycles: rate >> 1/s.
    mib.set(&format!("{IF_TABLE}.14.1"), Value::Counter32(50_000));
    ar.rig.poller.poll_once().await.expect("poll");
    let events = collect_alerts(&ar, IDLE).await;
    let f = firing(&events, "interface_errors");
    assert!(!f.is_empty(), "error burst must fire");
    assert_eq!(f[0].labels["direction"], "in");
    assert_eq!(f[0].labels["kind"], "errors");
}

/// Saturating the link (octet rate vs ifHighSpeed) fires
/// `interface_utilization`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interface_utilization_alert() {
    let mib = base_mib();
    let agent = SimAgent::start(mib.clone()).await;
    let device = v2c_device("hot01", agent.addr());

    let ar = rig_with_alerts(device, SnmpAlertsConfig::default()).await;
    ar.rig.poller.poll_once().await.expect("poll");
    let _ = collect_alerts(&ar, IDLE).await;

    // A giant octet delta on the HC input counter: rate far above 90% of
    // the 100 Mb/s ifHighSpeed.
    mib.set(&format!("{IF_X_TABLE}.6.1"), Value::Counter64(500_000_000));
    ar.rig.poller.poll_once().await.expect("poll");
    let events = collect_alerts(&ar, IDLE).await;
    let f = firing(&events, "interface_utilization");
    assert!(!f.is_empty(), "saturated link must fire");
    assert_eq!(f[0].labels["direction"], "in");
}

/// sysUpTime going backwards fires the informational `device_rebooted`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reboot_alert_fires() {
    let mib = base_mib();
    let agent = SimAgent::start(mib.clone()).await;
    let mut device = v2c_device("boot01", agent.addr());
    device.oids = vec![format!("{SYSTEM}.5.0")];

    let mut cfg = SnmpAlertsConfig::default();
    cfg.interface_down.enabled = false;
    cfg.interface_errors.enabled = false;
    cfg.utilization.enabled = false;
    let ar = rig_with_alerts(device, cfg).await;

    ar.rig.poller.poll_once().await.expect("poll");
    let _ = collect_alerts(&ar, IDLE).await;

    mib.set(&format!("{SYSTEM}.3.0"), Value::TimeTicks(10));
    ar.rig.poller.poll_once().await.expect("poll");
    let events = collect_alerts(&ar, IDLE).await;
    assert_eq!(firing(&events, "device_rebooted").len(), 1);
}

/// A disabled rule stays silent even when its condition holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_rule_stays_silent() {
    let mib = base_mib();
    let agent = SimAgent::start(mib.clone()).await;
    let device = v2c_device("quiet01", agent.addr());

    let mut cfg = SnmpAlertsConfig::default();
    cfg.interface_down.enabled = false;
    let ar = rig_with_alerts(device, cfg).await;

    mib.set(&format!("{IF_TABLE}.8.1"), Value::Integer(2));
    ar.rig.poller.poll_once().await.expect("poll");
    let events = collect_alerts(&ar, IDLE).await;
    assert!(firing(&events, "interface_down").is_empty());
}

/// Debounce: with `for_secs` set, a single violating cycle publishes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn debounce_suppresses_first_violation() {
    let mib = base_mib();
    let agent = SimAgent::start(mib.clone()).await;
    let device = v2c_device("flap01", agent.addr());

    let cfg = SnmpAlertsConfig {
        for_secs: 3600,
        ..SnmpAlertsConfig::default()
    };
    let ar = rig_with_alerts(device, cfg).await;

    mib.set(&format!("{IF_TABLE}.8.1"), Value::Integer(2));
    ar.rig.poller.poll_once().await.expect("poll");
    let events = collect_alerts(&ar, IDLE).await;
    assert!(
        firing(&events, "interface_down").is_empty(),
        "debounce must suppress the first violation"
    );
}

/// Two devices share one reporter: one device's sweep must not resolve the
/// other's firing alerts (label-scoped reconcile).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_devices_do_not_stomp_each_other() {
    let mib_a = base_mib();
    let agent_a = SimAgent::start(mib_a.clone()).await;
    let mib_b = base_mib();
    let agent_b = SimAgent::start(mib_b).await;

    // Shared session + reporter, one poller per device (as in main.rs).
    let session = std::sync::Arc::new(
        zenoh::open(harness::isolated_zenoh_config())
            .await
            .expect("open zenoh"),
    );
    let publisher = zensight_sensor_core::Publisher::new(
        session.clone(),
        "snmp",
        zensight_common::Format::Json,
    );
    let reporter = std::sync::Arc::new(zensight_sensor_core::AlertReporter::new(
        publisher,
        zensight_common::Protocol::Snmp,
        zensight_common::Format::Json,
    ));

    let mut resolver = zensight_sensor_snmp::mib::MibResolver::new();
    resolver.add_custom_mappings(&harness::test_oid_names());
    let resolver = std::sync::Arc::new(resolver);

    let mut pollers = Vec::new();
    for (name, addr) in [("dev-a", agent_a.addr()), ("dev-b", agent_b.addr())] {
        let mut poller = zensight_sensor_snmp::poller::SnmpPoller::new(
            v2c_device(name, addr),
            session.clone(),
            resolver.clone(),
            &std::collections::HashMap::new(),
            zensight_common::Format::Json,
        );
        poller.with_alerts(zensight_sensor_snmp::alerts::AlertEvaluator::new(
            name.to_string(),
            SnmpAlertsConfig::default(),
            reporter.clone(),
        ));
        poller.init().await.expect("init");
        pollers.push(poller);
    }

    // dev-a's eth0 goes down; dev-b stays healthy.
    mib_a.set(&format!("{IF_TABLE}.8.1"), Value::Integer(2));
    pollers[0].poll_once().await.expect("poll a");
    assert_eq!(reporter.firing_alerts().len(), 1);

    // dev-b's clean sweep must NOT resolve dev-a's alert.
    pollers[1].poll_once().await.expect("poll b");
    let still = reporter.firing_alerts();
    assert_eq!(still.len(), 1, "dev-b's sweep stomped dev-a's alert");
    assert_eq!(still[0].labels["device"], "dev-a");
}

// ---------------------------------------------------------------------------
// Joined InterfaceTable state doc (#529)
// ---------------------------------------------------------------------------

use harness::{interfaces_sub, latest_interfaces_doc};
use zensight_common::IfStatus;

/// Walking ifTable+ifXTable publishes a coherent joined doc on
/// `state/snmp/<device>/interfaces`: decoded statuses, ifName/ifHighSpeed/HC
/// preference, MAC formatting, and rates from the second cycle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interfaces_doc_joins_both_tables() {
    let mib = base_mib();
    // Give eth0 a MAC and make eth1 oper-down for decode assertions.
    mib.set(
        &format!("{IF_TABLE}.6.1"),
        Value::OctetString(bytes::Bytes::from_static(&[
            0x02, 0x42, 0xac, 0x11, 0x00, 0x07,
        ])),
    );
    mib.set(&format!("{IF_TABLE}.8.2"), Value::Integer(2));
    let agent = SimAgent::start(mib.clone()).await;
    let mut device = v2c_device("router01", agent.addr());
    device.walks = vec![
        "1.3.6.1.2.1.2.2".to_string(),
        "1.3.6.1.2.1.31.1.1".to_string(),
    ];

    let rig = rig(device).await;
    let sub = interfaces_sub(&rig.session).await;

    rig.poller.poll_once().await.expect("poll");
    let doc = latest_interfaces_doc(&sub, IDLE)
        .await
        .expect("interfaces doc published");
    assert_eq!(doc.device, "router01");
    assert_eq!(doc.interfaces.len(), 2);

    let eth0 = &doc.interfaces[0];
    assert_eq!(eth0.index, 1);
    assert_eq!(eth0.name.as_deref(), Some("eth0")); // ifName
    assert_eq!(eth0.alias.as_deref(), Some("uplink"));
    assert_eq!(eth0.mac.as_deref(), Some("02:42:ac:11:00:07"));
    assert_eq!(eth0.admin_status, Some(IfStatus::Up));
    assert_eq!(eth0.oper_status, Some(IfStatus::Up));
    assert_eq!(eth0.speed_bits, Some(100_000_000)); // ifHighSpeed (100 Mb)
    assert_eq!(eth0.counters.in_octets, Some(0)); // HC preferred
    assert!(
        eth0.rates.in_octets_per_sec.is_none(),
        "no rates on cycle 1"
    );

    let eth1 = &doc.interfaces[1];
    assert_eq!(eth1.oper_status, Some(IfStatus::Down));

    // Second cycle with HC counter movement: rates appear in the doc.
    mib.set(&format!("{IF_X_TABLE}.6.1"), Value::Counter64(90_000));
    rig.poller.poll_once().await.expect("poll");
    let doc = latest_interfaces_doc(&sub, IDLE)
        .await
        .expect("refreshed doc");
    let eth0 = &doc.interfaces[0];
    assert_eq!(eth0.counters.in_octets, Some(90_000));
    assert!(
        eth0.rates.in_octets_per_sec.unwrap_or(0.0) > 0.0,
        "HC rate must appear from the second cycle"
    );
}

/// A device without ifXTable still yields a coherent doc (ifDescr naming,
/// ifSpeed, 32-bit counters).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interfaces_doc_without_ifx_table() {
    // Only ifTable columns — simulate a legacy device.
    let mib = SimMib::new().with_if_table(1);
    for col in [1, 6, 10, 15, 18] {
        mib.remove(&format!("{IF_X_TABLE}.{col}.1"));
    }
    let agent = SimAgent::start(mib).await;
    let mut device = v2c_device("legacy01", agent.addr());
    device.walks = vec!["1.3.6.1.2.1.2.2".to_string()];

    let rig = rig(device).await;
    let sub = interfaces_sub(&rig.session).await;
    rig.poller.poll_once().await.expect("poll");

    let doc = latest_interfaces_doc(&sub, IDLE).await.expect("doc");
    let e = &doc.interfaces[0];
    assert_eq!(e.name.as_deref(), Some("eth0")); // ifDescr fallback
    assert_eq!(e.speed_bits, Some(100_000_000)); // ifSpeed fallback
    assert!(e.alias.is_none());
    assert_eq!(e.counters.in_octets, Some(0)); // 32-bit counter
}

// ---------------------------------------------------------------------------
// Device profiles (#531)
// ---------------------------------------------------------------------------

use harness::rig_with_profiles;

/// A device configured with only name+address gets system + interface
/// metrics automatically from the default profiles, and the applied set is
/// observable as `system/profile`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bare_device_polls_via_default_profiles() {
    let agent = SimAgent::start(base_mib()).await;
    // No oids, no walks — profiles only.
    let device = v2c_device("bare01", agent.addr());

    let rig = rig_with_profiles(device).await;
    rig.poller.poll_once().await.expect("poll");

    let points = collect_points(&rig, IDLE).await;
    // generic-device scalars…
    assert_eq!(
        points["system/name"].value,
        TelemetryValue::Text("sim-device".into())
    );
    assert!(points.contains_key("system/uptime"));
    // …and network-interfaces walks (both tables).
    assert!(points.contains_key("if/1/in_octets"));
    assert!(points.contains_key("ifx/1/hc_in_octets"));
    // The applied profile set is published.
    let applied = &points["system/profile"];
    let TelemetryValue::Text(applied) = &applied.value else {
        panic!("system/profile must be text");
    };
    assert!(applied.contains("generic-device"), "{applied}");
    assert!(applied.contains("network-interfaces"), "{applied}");
    assert!(!applied.contains("host-resources"), "{applied}");
}

/// Pinning `host-resources` adds its tables on top of the defaults, and
/// configured walks still merge in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pinned_profile_merges_with_defaults_and_config() {
    let mib = base_mib().with_host_resources();
    let agent = SimAgent::start(mib).await;
    let mut device = v2c_device("srv01", agent.addr());
    device.profile = Some("host-resources".to_string());
    device.oids = vec![format!("{SYSTEM}.1.0")]; // configured scalar merges on top

    let rig = rig_with_profiles(device).await;
    rig.poller.poll_once().await.expect("poll");

    let points = collect_points(&rig, IDLE).await;
    assert!(points.contains_key("storage/1/used"), "{:?}", points.keys());
    assert!(points.contains_key("cpu/1/load"));
    assert!(points.contains_key("system/descr")); // configured oid
    assert!(points.contains_key("if/1/in_octets")); // defaults still apply
    let TelemetryValue::Text(applied) = &points["system/profile"].value else {
        panic!("system/profile must be text");
    };
    assert!(applied.contains("host-resources"), "{applied}");
}

/// Profile selection defers while the device is unreachable and applies on
/// the first answering cycle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn profile_selection_defers_until_device_answers() {
    let agent = SimAgent::start(base_mib()).await;
    let proxy = FlakyProxy::start(agent.addr()).await;
    let device = v2c_device("late01", proxy.addr());

    let rig = rig_with_profiles(device).await;

    proxy.set_blackhole(true);
    rig.poller.poll_once().await.expect("poll");
    let points = collect_points(&rig, IDLE).await;
    assert!(points.is_empty());

    proxy.set_blackhole(false);
    rig.poller.poll_once().await.expect("poll");
    let points = collect_points(&rig, IDLE).await;
    assert!(points.contains_key("system/name"));
    assert!(points.contains_key("if/1/in_octets"));
}

// ---------------------------------------------------------------------------
// Real SMI MIBs (#532)
// ---------------------------------------------------------------------------

const VENDOR_MIB: &str = r#"
ZENTEST-MIB DEFINITIONS ::= BEGIN

IMPORTS
    MODULE-IDENTITY, OBJECT-TYPE, Counter64, Integer32, enterprises
        FROM SNMPv2-SMI;

zentest MODULE-IDENTITY
    LAST-UPDATED "202601010000Z"
    ORGANIZATION "zensight"
    CONTACT-INFO "test"
    DESCRIPTION  "test module"
    ::= { enterprises 4242 }

zenTemp OBJECT-TYPE
    SYNTAX      Integer32
    UNITS       "Cel"
    MAX-ACCESS  read-only
    STATUS      current
    DESCRIPTION "chassis temperature"
    ::= { zentest 1 }

zenPortTable OBJECT-TYPE
    SYNTAX      SEQUENCE OF ZenPortEntry
    MAX-ACCESS  not-accessible
    STATUS      current
    DESCRIPTION "ports"
    ::= { zentest 2 }

zenPortEntry OBJECT-TYPE
    SYNTAX      ZenPortEntry
    MAX-ACCESS  not-accessible
    STATUS      current
    DESCRIPTION "port row"
    INDEX       { zenPortIndex }
    ::= { zenPortTable 1 }

ZenPortEntry ::= SEQUENCE {
    zenPortIndex   Integer32,
    zenPortState   INTEGER,
    zenPortOctets  Counter64
}

zenPortIndex OBJECT-TYPE
    SYNTAX      Integer32
    MAX-ACCESS  read-only
    STATUS      current
    DESCRIPTION "index"
    ::= { zenPortEntry 1 }

zenPortState OBJECT-TYPE
    SYNTAX      INTEGER { up(1), down(2), degraded(3) }
    MAX-ACCESS  read-only
    STATUS      current
    DESCRIPTION "state"
    ::= { zenPortEntry 2 }

zenPortOctets OBJECT-TYPE
    SYNTAX      Counter64
    MAX-ACCESS  read-only
    STATUS      current
    DESCRIPTION "octets"
    ::= { zenPortEntry 3 }

END
"#;

/// A stock vendor MIB dropped into a directory resolves polled OIDs to
/// names, decodes enums onto the `enum` label, and applies UNITS — with no
/// code changes (only `mib.dirs` config).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vendor_mib_dir_names_enums_and_units() {
    // "Drop the file in a directory" — the acceptance path.
    let dir = std::env::temp_dir().join(format!("zensight-smi-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mib dir");
    std::fs::write(dir.join("ZENTEST-MIB.mib"), VENDOR_MIB).expect("write mib");
    let smi =
        zensight_sensor_snmp::smi::SmiResolver::load_dirs(&[dir.to_string_lossy().into_owned()])
            .expect("load mib dir");

    let mib = SimMib::new();
    mib.set("1.3.6.1.4.1.4242.1.0", Value::Integer(42));
    mib.set("1.3.6.1.4.1.4242.2.1.2.7", Value::Integer(3));
    mib.set("1.3.6.1.4.1.4242.2.1.3.7", Value::Counter64(1_000));
    let agent = SimAgent::start_with(mib.clone(), |b| {
        b.community(b"public").handler(
            harness::oid("1.3.6.1.4.1"),
            std::sync::Arc::new(mib.clone()),
        )
    })
    .await;

    let mut device = v2c_device("vendor01", agent.addr());
    device.oids = vec!["1.3.6.1.4.1.4242.1.0".to_string()];
    device.walks = vec!["1.3.6.1.4.1.4242.2".to_string()];

    let mut rig = rig(device).await;
    rig.poller.with_smi(std::sync::Arc::new(smi));
    rig.poller.poll_once().await.expect("poll");

    let points = collect_points(&rig, IDLE).await;
    // Scalar: SMI name + UNITS clause.
    let temp = &points["zen_temp"];
    assert_eq!(temp.value, TelemetryValue::Gauge(42.0));
    assert_eq!(temp.unit.as_deref(), Some("Cel"));
    // Enum column: numeric value stays, label decodes.
    let state = &points["zen_port_state/7"];
    assert_eq!(state.value, TelemetryValue::Gauge(3.0));
    assert_eq!(
        state.labels.get("enum").map(String::as_str),
        Some("degraded")
    );
    // Counter column named via SMI.
    assert_eq!(
        points["zen_port_octets/7"].value,
        TelemetryValue::Counter(1_000)
    );

    // Second cycle: the SMI-typed counter rates like any other.
    mib.set("1.3.6.1.4.1.4242.2.1.3.7", Value::Counter64(90_000));
    rig.poller.poll_once().await.expect("poll");
    let points = collect_points(&rig, IDLE).await;
    assert!(
        points.contains_key("zen_port_octets/7.rate"),
        "{:?}",
        points.keys()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Trap pipeline (#535)
// ---------------------------------------------------------------------------

use zensight_common::{AlertState as EvAlertState, EventRecord};
use zensight_sensor_snmp::config::TrapListenerConfig;
use zensight_sensor_snmp::trap::TrapReceiver;

const LINK_DOWN: &str = "1.3.6.1.6.3.1.1.5.3";
const LINK_UP: &str = "1.3.6.1.6.3.1.1.5.4";
const IF_INDEX_1: &str = "1.3.6.1.2.1.2.2.1.1.1";

struct TrapRig {
    session: std::sync::Arc<zenoh::Session>,
    event_sub:
        zenoh::pubsub::Subscriber<zenoh::handlers::FifoChannelHandler<zenoh::sample::Sample>>,
    reporter: std::sync::Arc<zensight_sensor_core::AlertReporter>,
    addr: std::net::SocketAddr,
    _task: tokio::task::JoinHandle<()>,
}

/// A running trap receiver on 127.0.0.1:0 over an isolated peer, with an
/// events subscriber and a shared alert reporter attached.
async fn trap_rig(config: TrapListenerConfig) -> TrapRig {
    let session = std::sync::Arc::new(
        zenoh::open(harness::isolated_zenoh_config())
            .await
            .expect("open zenoh"),
    );
    let event_sub = session
        .declare_subscriber("v1/*/events/snmp/**")
        .await
        .expect("events subscriber");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let mut resolver = zensight_sensor_snmp::mib::MibResolver::new();
    resolver.add_custom_mappings(&harness::test_oid_names());
    let reporter = std::sync::Arc::new(zensight_sensor_core::AlertReporter::new(
        zensight_sensor_core::Publisher::new(
            session.clone(),
            "snmp",
            zensight_common::Format::Json,
        ),
        zensight_common::Protocol::Snmp,
        zensight_common::Format::Json,
    ));

    let mut receiver = TrapReceiver::new(
        config,
        session.clone(),
        std::sync::Arc::new(resolver),
        zensight_common::Format::Json,
    );
    receiver.with_alerts(reporter.clone());
    let bound = receiver.bind().await.expect("bind trap listener");
    let addr = bound.local_addr();
    let task = tokio::spawn(async move {
        let _ = bound.run().await;
    });

    TrapRig {
        session,
        event_sub,
        reporter,
        addr,
        _task: task,
    }
}

async fn next_event(rig: &TrapRig) -> (String, EventRecord) {
    let sample = tokio::time::timeout(Duration::from_secs(5), rig.event_sub.recv_async())
        .await
        .expect("event timed out")
        .expect("event recv");
    let record = zensight_common::decode_auto(&sample.payload().to_bytes()).expect("decode event");
    (sample.key_expr().to_string(), record)
}

/// A v2c trap becomes a durable event record with translated fields and a
/// telemetry counter; linkDown fires an alert and linkUp resolves it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trap_v2c_event_alert_lifecycle() {
    let rig = trap_rig(TrapListenerConfig {
        enabled: true,
        bind: "127.0.0.1:0".to_string(),
        communities: vec!["public".to_string()],
        ..Default::default()
    })
    .await;
    let alert_sub = rig
        .session
        .declare_subscriber("v1/*/state/snmp/alert/*")
        .await
        .expect("alert subscriber");
    tokio::time::sleep(Duration::from_millis(150)).await;

    // A sim agent that sends notifications at our listener.
    let agent = SimAgent::start_with(SimMib::new(), |b| {
        b.community(b"public")
            .trap_sink(rig.addr.to_string(), async_snmp::Auth::v2c("public"))
    })
    .await;

    let varbinds = vec![async_snmp::VarBind::new(
        harness::oid(IF_INDEX_1),
        Value::Integer(1),
    )];
    agent
        .agent()
        .send_trap(&harness::oid(LINK_DOWN), 4200, varbinds.clone())
        .await
        .expect("send linkDown");

    let (key, event) = next_event(&rig).await;
    assert!(key.contains("/events/snmp/127-0-0-1/trap/"), "{key}");
    assert!(key.ends_with(&event.id), "ULID is the last chunk: {key}");
    assert_eq!(event.kind, "trap/1.3.6.1.6.3.1.1.5.3"); // no MIB entry in rig
    assert_eq!(event.fields["trap_oid"], LINK_DOWN);
    assert_eq!(event.fields["confirmed"], "false");
    assert_eq!(event.severity, zensight_common::AlertSeverity::Warning);

    // linkDown fired the built-in alert with device + interface labels.
    let sample = tokio::time::timeout(Duration::from_secs(5), alert_sub.recv_async())
        .await
        .expect("alert timed out")
        .expect("alert recv");
    let alert: zensight_common::Alert =
        zensight_common::decode_auto(&sample.payload().to_bytes()).expect("decode alert");
    assert_eq!(alert.rule, "trap_link_down");
    assert_eq!(alert.state, EvAlertState::Firing);
    assert_eq!(alert.labels["device"], "127-0-0-1");
    assert_eq!(alert.labels["if_index"], "1");

    // #651: the record names the alert it raised, and names it exactly — the
    // key the reporter published under, not the rule name, which would be
    // ambiguous the moment a second interface goes down on the same device.
    assert_eq!(
        event.alert_key.as_deref(),
        Some(alert.alert_key().as_str()),
        "the trap record must carry the alert key the reporter used"
    );

    // linkUp resolves exactly that alert.
    agent
        .agent()
        .send_trap(&harness::oid(LINK_UP), 4300, varbinds)
        .await
        .expect("send linkUp");
    let (_, up_event) = next_event(&rig).await;
    assert_eq!(up_event.kind, "trap/1.3.6.1.6.3.1.1.5.4");
    // #651: the clearing trap links to the SAME alert it cleared, so an
    // operator following the link lands on the incident rather than nowhere.
    assert_eq!(
        up_event.alert_key, event.alert_key,
        "linkUp must name the alert linkDown raised"
    );

    let mut resolved = false;
    while let Ok(Ok(sample)) =
        tokio::time::timeout(Duration::from_secs(5), alert_sub.recv_async()).await
    {
        match sample.kind() {
            zenoh::sample::SampleKind::Put => {
                let alert: zensight_common::Alert =
                    zensight_common::decode_auto(&sample.payload().to_bytes()).expect("decode");
                if alert.state == EvAlertState::Resolved {
                    resolved = true;
                    break;
                }
            }
            zenoh::sample::SampleKind::Delete => {
                resolved = true;
                break;
            }
        }
    }
    assert!(resolved, "linkUp must resolve the linkDown alert");
    assert_eq!(rig.reporter.firing_alerts().len(), 0);
}

/// An inform round-trip: `send_inform` only returns Ok once the receiver's
/// automatic acknowledgement arrives (no retransmit), and the record notes
/// it was confirmed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inform_v2c_is_acknowledged() {
    let rig = trap_rig(TrapListenerConfig {
        enabled: true,
        bind: "127.0.0.1:0".to_string(),
        ..Default::default()
    })
    .await;

    let agent = SimAgent::start_with(SimMib::new(), |b| {
        b.community(b"public")
            .trap_sink(rig.addr.to_string(), async_snmp::Auth::v2c("public"))
            .inform_timeout(Duration::from_secs(2))
    })
    .await;

    agent
        .agent()
        .send_inform(&harness::oid(LINK_DOWN), 100, Vec::new())
        .await
        .expect("inform must be acknowledged (no retransmit timeout)");

    let (_, event) = next_event(&rig).await;
    assert_eq!(event.fields["confirmed"], "true");
}

/// A v3 authPriv trap decodes end-to-end through the configured user.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trap_v3_authpriv_end_to_end() {
    let user = zensight_sensor_snmp::config::SnmpV3Security {
        username: "trapuser".to_string(),
        auth_protocol: AuthProtocol::Sha256,
        auth_password: Some("authpass123".to_string()),
        priv_protocol: PrivProtocol::Aes128,
        priv_password: Some("privpass123".to_string()),
        engine_id: None,
    };
    let rig = trap_rig(TrapListenerConfig {
        enabled: true,
        bind: "127.0.0.1:0".to_string(),
        users: vec![user],
        ..Default::default()
    })
    .await;

    let sink_auth: async_snmp::Auth = async_snmp::Auth::usm("trapuser")
        .auth_priv(
            AgentAuth::Sha256,
            "authpass123",
            AgentPriv::Aes128,
            "privpass123",
        )
        .into();
    let agent = SimAgent::start_with(SimMib::new(), |b| {
        // 0.17: a v3 trap sink makes the agent authoritative — engine required.
        b.community(b"public")
            .authoritative_engine(test_engine(b"\x80\x00\x00\x00\x01trapsend".to_vec()))
            .trap_sink(rig.addr.to_string(), sink_auth)
    })
    .await;

    agent
        .agent()
        .send_trap(&harness::oid(LINK_DOWN), 7, Vec::new())
        .await
        .expect("send v3 trap");

    let (_, event) = next_event(&rig).await;
    assert_eq!(event.fields["trap_oid"], LINK_DOWN);
    assert_eq!(event.fields["snmp_version"], "V3");
}

/// A community filter rejects mismatched senders (no event published).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trap_community_filter_rejects() {
    let rig = trap_rig(TrapListenerConfig {
        enabled: true,
        bind: "127.0.0.1:0".to_string(),
        communities: vec!["secret".to_string()],
        ..Default::default()
    })
    .await;

    let agent = SimAgent::start_with(SimMib::new(), |b| {
        b.community(b"public")
            .trap_sink(rig.addr.to_string(), async_snmp::Auth::v2c("wrong"))
    })
    .await;
    agent
        .agent()
        .send_trap(&harness::oid(LINK_DOWN), 1, Vec::new())
        .await
        .expect("send");

    let got = tokio::time::timeout(Duration::from_secs(2), rig.event_sub.recv_async()).await;
    assert!(got.is_err(), "mismatched community must not publish");
}

// ---------------------------------------------------------------------------
// Identity evidence (#537)
// ---------------------------------------------------------------------------

/// Polling a device publishes an observer-role `HostEvidence` claim with
/// sysName/vendor/MACs/IPs on `state/snmp/evidence/device/<device>`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn polled_device_publishes_identity_evidence() {
    let mib = base_mib();
    mib.set(
        &format!("{IF_TABLE}.6.1"),
        Value::OctetString(bytes::Bytes::from_static(&[
            0xde, 0xad, 0xbe, 0xef, 0x00, 0x07,
        ])),
    );
    mib.set(
        "1.3.6.1.2.1.4.20.1.1.10.0.0.9",
        Value::IpAddress([10, 0, 0, 9]),
    );
    let agent = SimAgent::start(mib).await;
    let mut device = v2c_device("router01", agent.addr());
    device.oids = vec![
        format!("{SYSTEM}.1.0"),
        format!("{SYSTEM}.2.0"),
        format!("{SYSTEM}.5.0"),
    ];
    device.walks = vec![format!("{IF_TABLE}.6"), "1.3.6.1.2.1.4.20.1.1".to_string()];

    let mut rig = rig(device).await;
    let evidence_sub = rig
        .session
        .declare_subscriber("v1/*/state/snmp/evidence/device/*")
        .await
        .expect("evidence subscriber");
    tokio::time::sleep(Duration::from_millis(150)).await;

    rig.poller.with_evidence(
        std::sync::Arc::new(
            zensight_sensor_core::AdvancedPublisherRegistry::new(
                rig.session.clone(),
                zensight_sensor_core::v1::V1Context::for_producer(
                    &zensight_common::PROFILE,
                    "snmp",
                )
                .telemetry_prefix(),
                zensight_common::Format::Json,
                zensight_sensor_core::AdvancedPublisherConfig::cache_only(1),
            )
            .with_qos(zensight_common::QosClass::Evidence),
        ),
        10,
    );

    rig.poller.poll_once().await.expect("poll");

    let sample = tokio::time::timeout(Duration::from_secs(5), evidence_sub.recv_async())
        .await
        .expect("evidence timed out")
        .expect("evidence recv");
    assert!(
        sample
            .key_expr()
            .as_str()
            .ends_with("state/snmp/evidence/device/router01"),
        "{}",
        sample.key_expr()
    );
    let claim: zensight_common::HostEvidence =
        zensight_common::decode_auto(&sample.payload().to_bytes()).expect("decode evidence");
    assert_eq!(claim.sensor, "snmp");
    assert_eq!(claim.source, "router01");
    assert_eq!(claim.observer.as_deref(), Some("snmp"));
    assert_eq!(claim.host_id, None);
    assert_eq!(claim.hostname.as_deref(), Some("sim-device"));
    assert_eq!(claim.vendor.as_deref(), Some("enterprise-99999"));
    assert!(claim.macs.contains(&"de:ad:be:ef:00:07".to_string()));
    assert!(claim.ips.contains(&"10.0.0.9".to_string()));
    assert!(
        !claim.ips.contains(&"127.0.0.1".to_string()),
        "loopback polled address is filtered: {:?}",
        claim.ips
    );
}

// ---------------------------------------------------------------------------
// Resilience (#539)
// ---------------------------------------------------------------------------

use zensight_sensor_snmp::poller::CycleKind;

/// Breaker lifecycle: consecutive dead cycles open the breaker (probe-only,
/// one datagram per cycle), backoff grows, and a single successful probe
/// closes it — full polling resumes on the next cycle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn breaker_opens_probes_and_recovers() {
    let agent = SimAgent::start(base_mib()).await;
    let proxy = FlakyProxy::start(agent.addr()).await;
    let mut device = v2c_device("frail01", proxy.addr());
    device.oids = vec![format!("{SYSTEM}.1.0"), format!("{SYSTEM}.5.0")];

    let health = std::sync::Arc::new(zensight_sensor_core::SensorHealth::new("snmp"));
    let mut rig = rig(device).await;
    rig.poller.with_health(health.clone());

    // Healthy first cycle.
    assert!(matches!(rig.poller.cycle().await, CycleKind::Full(_)));
    assert_eq!(rig.poller.backoff_multiplier(), 1);

    // Device dies: three fully-failed cycles open the breaker.
    proxy.set_blackhole(true);
    for _ in 0..3 {
        assert!(matches!(rig.poller.cycle().await, CycleKind::Full(_)));
    }
    assert!(rig.poller.backoff_multiplier() > 1, "backoff engaged");

    // Open: probe-only — exactly ONE datagram for the whole cycle.
    let before = proxy.forwarded();
    assert!(matches!(
        rig.poller.cycle().await,
        CycleKind::Probe { ok: false }
    ));
    // Blackholed datagrams are not forwarded at all; the point is the cycle
    // did not attempt the full OID set. Recovery below proves the probe path.
    assert_eq!(proxy.forwarded(), before);

    // Device returns: the probe closes the breaker within one cycle...
    proxy.set_blackhole(false);
    let before = proxy.forwarded();
    assert!(matches!(
        rig.poller.cycle().await,
        CycleKind::Probe { ok: true }
    ));
    let probe_datagrams = proxy.forwarded() - before;
    assert_eq!(probe_datagrams, 1, "probe cycle = one request");
    assert_eq!(rig.poller.backoff_multiplier(), 1, "breaker closed");

    // ...and the next cycle polls fully again, publishing telemetry.
    let _ = collect_points(&rig, Duration::from_millis(100)).await; // drain
    assert!(matches!(rig.poller.cycle().await, CycleKind::Full(_)));
    let points = collect_points(&rig, IDLE).await;
    assert!(points.contains_key("system/name"), "{:?}", points.keys());

    // Health reflects the recovery.
    let snapshot = health.snapshot();
    assert_eq!(snapshot.devices_responding, 1);
    assert_eq!(snapshot.devices_failed, 0);
}

/// A device whose client was never initialized (offline at startup) gets
/// built by the poll loop itself — no sensor restart needed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uninitialized_client_is_built_by_the_cycle() {
    let agent = SimAgent::start(base_mib()).await;
    let mut device = v2c_device("late02", agent.addr());
    device.oids = vec![format!("{SYSTEM}.5.0")];

    // rig() calls init(); build a poller by hand WITHOUT init.
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
    let poller = zensight_sensor_snmp::poller::SnmpPoller::new(
        device,
        session.clone(),
        std::sync::Arc::new(resolver),
        &std::collections::HashMap::new(),
        zensight_common::Format::Json,
    );

    assert!(matches!(poller.cycle().await, CycleKind::Full(_)));
    let sample = tokio::time::timeout(Duration::from_secs(3), sub.recv_async())
        .await
        .expect("telemetry after self-built client")
        .expect("recv");
    assert!(
        sample
            .key_expr()
            .as_str()
            .contains("/telemetry/snmp/late02/")
    );
}

// ---------------------------------------------------------------------------
// Subnet discovery (#541)
// ---------------------------------------------------------------------------

use zensight_sensor_snmp::config::CredentialSet;
use zensight_sensor_snmp::discovery::{Discovery, DiscoveryConfig};

/// Sweeping addresses proposes unconfigured responders with sysObjectID
/// identity, matched profiles and a config snippet — and never re-proposes
/// known devices or dead addresses.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_proposes_unconfigured_responders() {
    // Two live agents; one will be "already configured".
    let agent_new = SimAgent::start(base_mib()).await;
    let agent_known = SimAgent::start(base_mib()).await;
    let dead: std::net::SocketAddr = "127.0.0.1:1".parse().unwrap();

    let known_ips = std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashSet::from([
        "127.0.0.1".to_string(),
    ])));
    // With loopback in known_ips everything is filtered — first prove the
    // dedup, then clear and prove the proposal path.
    let config = DiscoveryConfig {
        subnets: vec![],
        credentials: vec!["lab".to_string()],
        interval_secs: 3600,
        port: 161,
        max_concurrency: 4,
        probe_timeout_secs: 1,
    };
    let credentials = vec![(
        "lab".to_string(),
        CredentialSet {
            community: Some("public".to_string()),
            security: None,
        },
    )];
    let mut discovery = Discovery::new(config, credentials, known_ips.clone());
    discovery.with_profiles(std::sync::Arc::new(
        zensight_sensor_snmp::profile::ProfileSet::builtin(),
    ));

    let addresses = vec![agent_new.addr(), agent_known.addr(), dead];

    // Everything on a known IP is skipped entirely.
    let report = discovery.sweep(&addresses).await;
    assert_eq!(report.scanned, 0);
    assert!(report.discovered.is_empty());

    // Un-know the IP: both agents answer, the dead port doesn't.
    known_ips.write().unwrap().clear();
    let report = discovery.sweep(&addresses).await;
    assert_eq!(report.scanned, 3);
    assert_eq!(report.discovered.len(), 2, "{report:?}");

    let found = report
        .discovered
        .iter()
        .find(|d| d.address == agent_new.addr().to_string())
        .expect("new agent proposed");
    assert_eq!(found.credentials.as_deref(), Some("lab"));
    assert_eq!(
        found.sys_object_id.as_deref(),
        Some("1.3.6.1.4.1.99999.1.1")
    );
    assert_eq!(found.sys_name.as_deref(), Some("sim-device"));
    assert!(
        found
            .matched_profiles
            .contains(&"generic-device".to_string()),
        "{:?}",
        found.matched_profiles
    );
    assert!(
        found.suggested.contains(&found.address),
        "{}",
        found.suggested
    );
    assert!(found.suggested.contains("credentials: \"lab\""));
}
