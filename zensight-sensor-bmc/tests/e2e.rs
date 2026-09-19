//! The BMC sensor against a real socket speaking real Redfish JSON (#953).
//!
//! An in-process `axum` service, the `pve` pattern: a mocked client would only
//! prove the mock. Every fixture here is a shape real firmware serves, and the
//! two that matter most are the ones this sensor exists to get right — an
//! **absent** supply bay, and a BMC that answers **neither** Redfish surface.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};
use zensight_common::bmc::{Health, RedfishSurface, State as BmcState};
use zensight_sensor_bmc::alerts::{self, Observation};
use zensight_sensor_bmc::config::AlertsConfig;
use zensight_sensor_bmc::redfish::RedfishClient;

/// Which shape the fake serves. One service, both Redfish generations, so a
/// test can assert the client *discovers* rather than assumes.
#[derive(Clone)]
struct Fixture {
    /// Serve `PowerSubsystem`/`ThermalSubsystem` rather than `Power`/`Thermal`.
    modern: bool,
    /// Serve neither — a BMC with identity and nothing else.
    bare: Arc<AtomicBool>,
    /// Flip the failing supply back to healthy.
    repaired: Arc<AtomicBool>,
}

/// **Every** bay this service fronts, which is what a blade enclosure or a
/// four-node Twin looks like.
///
/// It listed only chassis 1 until #1130, while routing 2 and 3 — so no test
/// ever reached the poller's multi-chassis path, and the poller graded
/// `sweeps.first()` and wrote every chassis's series under the ENDPOINT's name
/// for two releases without a single assertion noticing.
async fn chassis_collection() -> Json<Value> {
    Json(json!({"Members": [
        {"@odata.id": "/redfish/v1/Chassis/1"},
        {"@odata.id": "/redfish/v1/Chassis/2"},
        {"@odata.id": "/redfish/v1/Chassis/3"},
    ]}))
}

async fn chassis_one() -> Json<Value> {
    Json(json!({
        "Id": "1",
        // The literal string Dell, HPE and Supermicro all ship. It is a schema
        // DESCRIPTION, not a name — the sensor must not claim it as a hostname
        // (#1110), or every such machine on the fleet claims the same one.
        "Name": "Computer System Chassis",
        "Manufacturer": "ACME",
        "Model": "R740",
        "SerialNumber": "CH-0001",
        "AssetTag": "rack-a-1",
        "PowerState": "On",
        "PhysicalSecurity": {"IntrusionSensor": "Normal"},
        "Status": {"Health": "OK", "State": "Enabled"},
        // The chassis's own statement of which machine is in it. Scoping the
        // identity claim through this is the whole of #1110.
        "Links": {"ComputerSystems": [{"@odata.id": "/redfish/v1/Systems/1"}]},
    }))
}

/// The second bay of the same enclosure — a different machine, behind the same
/// Redfish service.
async fn chassis_two() -> Json<Value> {
    Json(json!({
        "Id": "2",
        "Name": "Computer System Chassis",
        "Manufacturer": "ACME",
        "Model": "R740",
        "Status": {"Health": "OK", "State": "Enabled"},
        "Links": {"ComputerSystems": [{"@odata.id": "/redfish/v1/Systems/2"}]},
    }))
}

/// A bay linking a system this account cannot read — the "unreadable resource
/// is data, not a failure" case.
async fn chassis_three() -> Json<Value> {
    Json(json!({
        "Id": "3",
        "Name": "Computer System Chassis",
        "Status": {"Health": "OK", "State": "Enabled"},
        "Links": {"ComputerSystems": [{"@odata.id": "/redfish/v1/Systems/forbidden"}]},
    }))
}

async fn system_two() -> Json<Value> {
    Json(json!({
        "Id": "2",
        "HostName": "node-b",
        "EthernetInterfaces": {"@odata.id": "/redfish/v1/Systems/2/EthernetInterfaces"},
    }))
}

async fn system_two_nics() -> Json<Value> {
    Json(json!({"Members": [{"@odata.id": "/redfish/v1/Systems/2/EthernetInterfaces/nic1"}]}))
}

async fn system_two_nic1() -> Json<Value> {
    Json(json!({"Id": "nic1", "MACAddress": "AA:BB:CC:00:00:02"}))
}

async fn system_one() -> Json<Value> {
    Json(json!({
        "Id": "1",
        // What the machine calls ITSELF — the honest hostname claim.
        "HostName": "node-a",
        "EthernetInterfaces": {"@odata.id": "/redfish/v1/Systems/1/EthernetInterfaces"},
    }))
}

async fn system_one_nics() -> Json<Value> {
    Json(json!({"Members": [{"@odata.id": "/redfish/v1/Systems/1/EthernetInterfaces/nic1"}]}))
}

async fn system_one_nic1() -> Json<Value> {
    Json(json!({"Id": "nic1", "MACAddress": "AA:BB:CC:00:00:01"}))
}

fn supplies(repaired: bool) -> Value {
    json!([
        {
            "MemberId": "0",
            "Name": "PSU 1",
            "Status": {"Health": "OK", "State": "Enabled"},
            "PowerInputWatts": 210.0,
            "PowerOutputWatts": 190.0,
            "PowerCapacityWatts": 750.0,
        },
        {
            "MemberId": "1",
            "Name": "PSU 2",
            // Critical, with redundancy lost — the pair the issue names.
            "Status": {
                "Health": if repaired { "OK" } else { "Critical" },
                "State": "Enabled",
            },
            "PowerInputWatts": 0.0,
            "Redundancy": [{
                "Name": "PSU group",
                "Status": {"Health": if repaired { "OK" } else { "Warning" }, "State": "Enabled"},
            }],
        },
        {
            // An EMPTY BAY. Some firmware leaves a stale zero in it, which is
            // exactly the trap: 0 W reads as a supply drawing nothing.
            "MemberId": "2",
            "Name": "PSU 3",
            "Status": {"State": "Absent"},
            "PowerInputWatts": 0.0,
        },
    ])
}

fn fans() -> Value {
    json!([
        {"MemberId": "0", "Name": "Fan 1", "Status": {"Health": "OK", "State": "Enabled"}, "Reading": 4800},
        // Stopped, and the BMC says so. The zero here IS the measurement.
        {"MemberId": "1", "Name": "Fan 2", "Status": {"Health": "Critical", "State": "Enabled"}, "Reading": 0},
        // Speed as a percentage of maximum — a DIFFERENT quantity, and it
        // must not become an `rpm` series.
        {"MemberId": "2", "Name": "Fan 3", "Status": {"Health": "OK", "State": "Enabled"}, "SpeedPercent": {"Reading": 55}},
    ])
}

fn temperatures() -> Value {
    json!([
        {
            "MemberId": "0", "Name": "Inlet Temp",
            "Status": {"Health": "OK", "State": "Enabled"},
            "ReadingCelsius": 22.0,
            "UpperThresholdCritical": 45.0,
            "UpperThresholdNonCritical": 40.0,
        },
        {
            // Over the BMC's OWN critical threshold, while Health still reads
            // OK — the firmware that updates one and not the other.
            "MemberId": "1", "Name": "CPU 1 Temp",
            "Status": {"Health": "OK", "State": "Enabled"},
            "ReadingCelsius": 96.0,
            "UpperThresholdCritical": 90.0,
        },
    ])
}

async fn legacy_power(State(f): State<Fixture>) -> Result<Json<Value>, axum::http::StatusCode> {
    if f.modern || f.bare.load(Ordering::Relaxed) {
        return Err(axum::http::StatusCode::NOT_FOUND);
    }
    Ok(Json(
        json!({"PowerSupplies": supplies(f.repaired.load(Ordering::Relaxed))}),
    ))
}

async fn legacy_thermal(State(f): State<Fixture>) -> Result<Json<Value>, axum::http::StatusCode> {
    if f.modern || f.bare.load(Ordering::Relaxed) {
        return Err(axum::http::StatusCode::NOT_FOUND);
    }
    Ok(Json(
        json!({"Fans": fans(), "Temperatures": temperatures()}),
    ))
}

/// Chassis 2's own supplies, on the legacy surface, with a **failing bay
/// `0`** — the same bay id chassis 1 uses.
///
/// Two chassis of one service numbering their bays from zero is the normal
/// case, and it is what made #1130 sharp: with the endpoint's name in the key
/// and in the `chassis` label, these two supplies shared a key AND an
/// `alert_key`.
async fn chassis_two_power() -> Json<Value> {
    Json(json!({"PowerSupplies": [{
        "MemberId": "0",
        "Name": "PSU 1",
        "Status": {"Health": "Critical", "State": "Enabled"},
        "PowerInputWatts": 0.0,
    }]}))
}

async fn power_subsystem(State(f): State<Fixture>) -> Result<Json<Value>, axum::http::StatusCode> {
    if !f.modern || f.bare.load(Ordering::Relaxed) {
        return Err(axum::http::StatusCode::NOT_FOUND);
    }
    Ok(Json(json!({"Id": "PowerSubsystem"})))
}

async fn thermal_subsystem(
    State(f): State<Fixture>,
) -> Result<Json<Value>, axum::http::StatusCode> {
    if !f.modern || f.bare.load(Ordering::Relaxed) {
        return Err(axum::http::StatusCode::NOT_FOUND);
    }
    Ok(Json(json!({"Id": "ThermalSubsystem"})))
}

fn member_links(n: usize, base: &str) -> Value {
    json!({
        "Members": (0..n)
            .map(|i| json!({"@odata.id": format!("{base}/{i}")}))
            .collect::<Vec<_>>()
    })
}

async fn psu_collection() -> Json<Value> {
    Json(member_links(
        3,
        "/redfish/v1/Chassis/1/PowerSubsystem/PowerSupplies",
    ))
}
async fn fan_collection() -> Json<Value> {
    Json(member_links(
        3,
        "/redfish/v1/Chassis/1/ThermalSubsystem/Fans",
    ))
}
/// `ThermalMetrics` is a **singleton** — no `Members`, the readings on the
/// body (#1131).
///
/// This fixture used to serve it as a collection, with per-index member
/// routes, which is a shape Redfish does not have. That is why the bug
/// survived a passing test suite: the fixture had been written to match the
/// client rather than the protocol, so the client's collection read found the
/// `Members` array the fixture invented, and the assertion below passed
/// against a fiction. Against real firmware it found nothing and this sensor
/// published no temperature at all on the modern surface.
async fn thermal_metrics() -> Json<Value> {
    Json(json!({
        "@odata.id": "/redfish/v1/Chassis/1/ThermalSubsystem/ThermalMetrics",
        "Id": "ThermalMetrics",
        "Name": "Thermal Metrics",
        "TemperatureReadingsCelsius": temperatures(),
    }))
}

async fn psu_member(
    State(f): State<Fixture>,
    axum::extract::Path(i): axum::extract::Path<usize>,
) -> Json<Value> {
    let all = supplies(f.repaired.load(Ordering::Relaxed));
    Json(all.as_array().unwrap()[i].clone())
}
async fn fan_member(axum::extract::Path(i): axum::extract::Path<usize>) -> Json<Value> {
    Json(fans().as_array().unwrap()[i].clone())
}

async fn spawn(fixture: Fixture) -> SocketAddr {
    let app = Router::new()
        .route("/redfish/v1/Chassis", get(chassis_collection))
        .route("/redfish/v1/Chassis/1", get(chassis_one))
        .route("/redfish/v1/Chassis/2", get(chassis_two))
        .route("/redfish/v1/Chassis/3", get(chassis_three))
        .route("/redfish/v1/Systems/1", get(system_one))
        .route(
            "/redfish/v1/Systems/1/EthernetInterfaces",
            get(system_one_nics),
        )
        .route(
            "/redfish/v1/Systems/1/EthernetInterfaces/nic1",
            get(system_one_nic1),
        )
        .route("/redfish/v1/Chassis/1/Power", get(legacy_power))
        .route("/redfish/v1/Chassis/2/Power", get(chassis_two_power))
        .route("/redfish/v1/Chassis/1/Thermal", get(legacy_thermal))
        .route("/redfish/v1/Chassis/1/PowerSubsystem", get(power_subsystem))
        .route(
            "/redfish/v1/Chassis/1/ThermalSubsystem",
            get(thermal_subsystem),
        )
        .route(
            "/redfish/v1/Chassis/1/PowerSubsystem/PowerSupplies",
            get(psu_collection),
        )
        .route(
            "/redfish/v1/Chassis/1/PowerSubsystem/PowerSupplies/{i}",
            get(psu_member),
        )
        .route(
            "/redfish/v1/Chassis/1/ThermalSubsystem/Fans",
            get(fan_collection),
        )
        .route(
            "/redfish/v1/Chassis/1/ThermalSubsystem/Fans/{i}",
            get(fan_member),
        )
        .route(
            "/redfish/v1/Chassis/1/ThermalSubsystem/ThermalMetrics",
            get(thermal_metrics),
        )
        // A read-only account legitimately cannot see Systems on some
        // firmware. It comes back as no MACs, not as a failed sweep.
        // A Twin/blade service fronts SEVERAL machines. Walking this
        // collection wholesale is exactly what #1110 was: every node's MACs
        // landed on every chassis's evidence.
        .route(
            "/redfish/v1/Systems",
            get(|| async {
                Json(json!({"Members": [
                    {"@odata.id": "/redfish/v1/Systems/1"},
                    {"@odata.id": "/redfish/v1/Systems/2"},
                ]}))
            }),
        )
        .route("/redfish/v1/Systems/2", get(system_two))
        .route(
            "/redfish/v1/Systems/2/EthernetInterfaces",
            get(system_two_nics),
        )
        .route(
            "/redfish/v1/Systems/2/EthernetInterfaces/nic1",
            get(system_two_nic1),
        )
        .with_state(fixture);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// The fake speaks plain HTTP; the client builds an `https://` base, so the
/// test overrides it. Standing up a TLS listener would test rustls, not this
/// sensor.
fn client(addr: SocketAddr) -> RedfishClient {
    RedfishClient::new(
        format!("http://{addr}"),
        "monitor".into(),
        "secret".into(),
        Duration::from_secs(5),
        false,
        None,
        4,
    )
    .unwrap()
}

fn fixture(modern: bool) -> Fixture {
    Fixture {
        modern,
        bare: Arc::new(AtomicBool::new(false)),
        repaired: Arc::new(AtomicBool::new(false)),
    }
}

/// Both Redfish generations produce the SAME observation, and the document
/// records which one answered — because a reading absent on one is a
/// different fact from the same reading absent on the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn both_redfish_generations_yield_the_same_chassis() {
    for (modern, expected) in [
        (false, RedfishSurface::Legacy),
        (true, RedfishSurface::Subsystem),
    ] {
        let addr = spawn(fixture(modern)).await;
        let sweep = client(addr).sweep("1").await.expect("sweep");

        assert_eq!(sweep.chassis.surface, expected);
        assert_eq!(sweep.chassis.serial.as_deref(), Some("CH-0001"));
        assert_eq!(sweep.chassis.power_state.as_deref(), Some("On"));
        assert_eq!(sweep.supplies.len(), 3, "modern={modern}");
        assert_eq!(sweep.fans.len(), 3);
        assert_eq!(sweep.thermal.len(), 2);
    }
}

/// **The fixture this sensor exists for.** A bay the BMC reports `Absent`
/// publishes no watts — even though the firmware left a stale `0.0` in the
/// document. Zero would read as a supply drawing nothing, which is a
/// different and wrong statement, and the one an operator would act on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_absent_bay_reports_no_watts_even_when_the_firmware_says_zero() {
    let addr = spawn(fixture(false)).await;
    let sweep = client(addr).sweep("1").await.expect("sweep");

    let absent = sweep.supplies.iter().find(|s| s.id == "2").unwrap();
    assert_eq!(absent.state, BmcState::Absent);
    assert!(!absent.present);
    assert_eq!(absent.input_watts, None, "an empty bay measures nothing");

    let live = sweep.supplies.iter().find(|s| s.id == "0").unwrap();
    assert_eq!(live.input_watts, Some(210.0));
}

/// A fan reporting a percentage of maximum must not become an RPM series —
/// that is a wrong number, not a missing one (#954's lesson) — while a fan
/// genuinely stopped keeps its zero, because there it IS the measurement.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_percentage_fan_publishes_no_rpm_and_a_stopped_one_publishes_zero() {
    let addr = spawn(fixture(false)).await;
    let sweep = client(addr).sweep("1").await.expect("sweep");

    let percent_only = sweep.fans.iter().find(|f| f.id == "2").unwrap();
    assert_eq!(percent_only.rpm, None);

    let stopped = sweep.fans.iter().find(|f| f.id == "1").unwrap();
    assert_eq!(stopped.rpm, Some(0.0));
    assert!(stopped.health.is_faulted());
}

/// A BMC that serves neither surface: identity, and nothing invented.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bmc_that_serves_neither_surface_yields_identity_and_no_readings() {
    let f = fixture(false);
    f.bare.store(true, Ordering::Relaxed);
    let addr = spawn(f).await;
    let sweep = client(addr).sweep("1").await.expect("sweep");

    assert_eq!(sweep.chassis.surface, RedfishSurface::None);
    assert_eq!(sweep.chassis.serial.as_deref(), Some("CH-0001"));
    assert!(sweep.supplies.is_empty(), "nothing invented");
    assert!(sweep.fans.is_empty());
    assert!(sweep.thermal.is_empty());
}

/// An unreachable BMC: no sweep at all, and — through the rules — one
/// assertion and no others.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_bmc_produces_one_assertion_and_no_readings() {
    // A port nothing is listening on. `127.0.0.1:1` needs no privileges to
    // fail against and is refused immediately rather than hanging.
    let c = RedfishClient::new(
        "http://127.0.0.1:1".into(),
        "monitor".into(),
        "secret".into(),
        Duration::from_millis(500),
        false,
        None,
        1,
    )
    .unwrap();
    assert!(c.chassis_ids().await.is_err());

    let firing = alerts::grade(
        &AlertsConfig::default(),
        &Observation {
            source: "mgmt01",
            endpoint: "rack-a-1",
            chassis: None,
            supplies: &[],
            fans: &[],
            thermal: &[],
            known_present: &[],
            consecutive_failures: 3,
        },
    );
    assert_eq!(firing.len(), 1);
    assert_eq!(firing[0].rule, alerts::RULE_UNREACHABLE);
    assert_eq!(firing[0].labels["chassis"], "rack-a-1");
}

/// End to end over the fixture: the failing supply, its lost redundancy, the
/// stopped fan and the sensor over the BMC's own threshold each assert once —
/// and all of it resolves when the hardware is repaired.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_documented_faults_each_assert_once_and_then_resolve() {
    let f = fixture(false);
    let repaired = f.repaired.clone();
    let addr = spawn(f).await;
    let c = client(addr);

    let sweep = c.sweep("1").await.expect("sweep");
    let firing = alerts::grade(
        &AlertsConfig::default(),
        &Observation {
            source: "mgmt01",
            endpoint: "rack-a-1",
            chassis: Some(&sweep.chassis),
            supplies: &sweep.supplies,
            fans: &sweep.fans,
            thermal: &sweep.thermal,
            known_present: &[],
            consecutive_failures: 0,
        },
    );
    let mut rules: Vec<&str> = firing.iter().map(|a| a.rule.as_str()).collect();
    rules.sort_unstable();
    assert_eq!(
        rules,
        vec![
            alerts::RULE_FAN_FAILED,
            alerts::RULE_PSU_FAILED,
            alerts::RULE_PSU_REDUNDANCY,
            alerts::RULE_THERMAL_CRITICAL,
        ],
        "the empty bay must NOT assert — psu_absent is off by default and it \
         was never seen populated"
    );

    // Every alert is filed under the reporting host, with the chassis as a
    // label (#883). Filing it under the chassis would put it on no host's card.
    for a in &firing {
        assert_eq!(a.source, "mgmt01", "{} is filed under {}", a.rule, a.source);
        assert_eq!(a.labels["chassis"], "rack-a-1-1");
    }

    // The supply is replaced; the fan and the temperature are not.
    repaired.store(true, Ordering::Relaxed);
    let sweep = c.sweep("1").await.expect("sweep");
    let firing = alerts::grade(
        &AlertsConfig::default(),
        &Observation {
            source: "mgmt01",
            endpoint: "rack-a-1",
            chassis: Some(&sweep.chassis),
            supplies: &sweep.supplies,
            fans: &sweep.fans,
            thermal: &sweep.thermal,
            known_present: &[],
            consecutive_failures: 0,
        },
    );
    let rules: Vec<&str> = firing.iter().map(|a| a.rule.as_str()).collect();
    assert!(!rules.contains(&alerts::RULE_PSU_FAILED), "{rules:?}");
    assert!(!rules.contains(&alerts::RULE_PSU_REDUNDANCY), "{rules:?}");
    assert!(rules.contains(&alerts::RULE_FAN_FAILED), "{rules:?}");
}

/// A Redfish resource this account may not read is a fact about the deployment
/// and not a failed poll: the sweep succeeds with no MACs rather than erroring.
///
/// Chassis 2 links a system the fixture does not serve, which is what a 403 on
/// `ComputerSystems` looks like from here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreadable_resource_is_data_not_a_failure() {
    let addr = spawn(fixture(true)).await;
    let sweep = client(addr).sweep("3").await.expect("sweep");
    assert!(sweep.macs.is_empty());
    assert_eq!(sweep.hostname, None, "a missing claim, not a wrong one");
    assert_eq!(sweep.chassis.health, Health::OK);
}

/// #1110: the identity claim describes the machine in **this** chassis.
///
/// `macs()` ignored its chassis argument and walked `/redfish/v1/Systems`
/// wholesale, so on a Twin or a blade enclosure every node's MACs landed on
/// every chassis's evidence — and MAC is the catalog's strongest merge key
/// after `host_id`. The hostname came from `Chassis.Name`, which is a schema
/// description that ships as the literal "Computer System Chassis": every such
/// machine on the fleet claimed the same hostname, and hostname is a merge
/// rule too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_identity_claim_is_scoped_to_this_chassis() {
    let addr = spawn(fixture(true)).await;
    let sweep = client(addr).sweep("1").await.expect("sweep");
    assert_eq!(
        sweep.macs,
        vec!["aa:bb:cc:00:00:01"],
        "only the system this chassis links"
    );
    assert_eq!(
        sweep.hostname.as_deref(),
        Some("node-a"),
        "the machine's own HostName, not the chassis's schema description"
    );
    assert_eq!(
        sweep.chassis.name.as_deref(),
        Some("Computer System Chassis"),
        "the chassis name is still read — it is simply not an identity claim"
    );

    // The other bay of the same enclosure, behind the same Redfish service:
    // a DISJOINT claim, which is the whole acceptance criterion.
    let other = client(addr).sweep("2").await.expect("sweep");
    assert_eq!(other.macs, vec!["aa:bb:cc:00:00:02"]);
    assert_eq!(other.hostname.as_deref(), Some("node-b"));
    assert!(
        other.macs.iter().all(|m| !sweep.macs.contains(m)),
        "two bays of one enclosure claimed each other's MACs — the catalog's \
         strongest merge key after host_id"
    );
}

/// **#1130 — the enclosure test.** One Redfish service, three chassis, and a
/// failing supply in bay `0` of chassis **2**, which is the same bay id
/// chassis 1 uses.
///
/// Three things were wrong and each one is asserted here.
///
/// 1. `publish` wrote `Chunk::slug(&endpoint.name)` into every key, so both
///    chassis published `telemetry/bmc/rack-a-1/psu/0/…` and
///    `state/bmc/chassis/rack-a-1/psu/0` — last writer wins, alternating each
///    sweep. The registry's own header says *"the chassis rides in the key
///    path and in the labels"*, and `zensight-common/src/bmc.rs` calls
///    `Chassis.id` *"the chunk in the key"*. Neither was true.
/// 2. `assert_endpoint` graded `sweeps.first()` — so chassis 2's failure was
///    never seen. Worse than missed: the per-rule reconcile is scoped to the
///    endpoint, so a `still` list computed from chassis 1 **resolved** it.
/// 3. `alert_key` hashes the discriminating labels, and `chassis` was the
///    endpoint's name, so the two bays produced the same key even if grading
///    had reached them.
///
/// The old fixture could not catch any of it: it listed one chassis in a
/// collection whose other members it was already routing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_chassis_of_an_enclosure_gets_its_own_keys_and_its_own_verdict() {
    use std::collections::HashMap;

    let addr = spawn(fixture(false)).await;
    let session = Arc::new(
        zenoh::open({
            let mut c = zenoh::Config::default();
            c.insert_json5("scouting/multicast/enabled", "false")
                .unwrap();
            c.insert_json5("scouting/gossip/enabled", "false").unwrap();
            // State documents ride AdvancedPublishers, which refuse to exist
            // without timestamping.
            c.insert_json5("timestamping/enabled", "true").unwrap();
            c
        })
        .await
        .expect("open zenoh"),
    );
    let telemetry_sub = session
        .declare_subscriber("v1/*/telemetry/bmc/**")
        .await
        .unwrap();
    let states_sub = session
        .declare_subscriber("v1/*/state/bmc/chassis/**")
        .await
        .unwrap();
    let alerts_sub = session
        .declare_subscriber("v1/*/state/bmc/alert/*")
        .await
        .unwrap();

    let format = zensight_common::Format::Json;
    let publisher = zensight_sensor_core::Publisher::new(session.clone(), "bmc", format);
    let reporter = Arc::new(zensight_sensor_core::AlertReporter::new(
        publisher.clone(),
        zensight_common::Protocol::Bmc,
        format,
    ));
    let states = Arc::new(
        zensight_sensor_core::AdvancedPublisherRegistry::new(
            session.clone(),
            zensight_sensor_core::v1::for_producer("bmc").telemetry_prefix(),
            format,
            zensight_sensor_core::AdvancedPublisherConfig::cache_only(1),
        )
        .with_qos(zensight_sensor_bmc::poller::STATE_QOS),
    );

    let endpoint = zensight_sensor_bmc::config::Endpoint {
        name: "rack-a-1".into(),
        address: addr.to_string(),
        transport: Default::default(),
        username: "monitor".into(),
        password: "secret".into(),
        interval_secs: None,
        timeout_secs: None,
        ca_file: None,
        insecure: true,
        enabled: true,
    };
    let cfg = zensight_sensor_bmc::config::BmcConfig {
        endpoints: vec![endpoint.clone()],
        evidence: false,
        ..Default::default()
    };
    let mut clients = HashMap::new();
    clients.insert("rack-a-1".to_string(), Arc::new(client(addr)));

    let mut poller = zensight_sensor_bmc::poller::Poller::new(
        cfg,
        "mgmt01".to_string(),
        clients,
        publisher,
        states,
        None,
        Some(reporter),
        Arc::new(zensight_sensor_core::SensorHealth::new("bmc")),
    );
    poller.poll_endpoint(&endpoint).await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    let mut telemetry_keys = Vec::new();
    while let Ok(Some(s)) = telemetry_sub.try_recv() {
        telemetry_keys.push(s.key_expr().as_str().to_string());
    }
    let mut state_keys = Vec::new();
    while let Ok(Some(s)) = states_sub.try_recv() {
        state_keys.push(s.key_expr().as_str().to_string());
    }
    let mut alerts = Vec::new();
    while let Ok(Some(s)) = alerts_sub.try_recv() {
        if s.kind() == zenoh::sample::SampleKind::Put {
            alerts.push(
                zensight_common::decode_auto::<zensight_common::Alert>(&s.payload().to_bytes())
                    .expect("alert decodes"),
            );
        }
    }

    // 1. Each chassis has its own key namespace, and neither is the bare
    //    endpoint name.
    let bay_zero: Vec<&String> = telemetry_keys
        .iter()
        .filter(|k| k.contains("/psu/0/"))
        .collect();
    assert!(
        bay_zero
            .iter()
            .any(|k| k.contains("/bmc/rack-a-1-1/psu/0/")),
        "chassis 1's bay 0 is missing from {telemetry_keys:#?}"
    );
    assert!(
        bay_zero
            .iter()
            .any(|k| k.contains("/bmc/rack-a-1-2/psu/0/")),
        "chassis 2's bay 0 is missing — only the first chassis was published \
         (#1130). Keys: {telemetry_keys:#?}"
    );
    assert!(
        !telemetry_keys
            .iter()
            .any(|k| k.contains("/bmc/rack-a-1/psu/")),
        "a supply was published under the ENDPOINT's chunk, which every \
         chassis of this service shares: {telemetry_keys:#?}"
    );

    // `reachable` is the one series that IS the endpoint's, because a BMC that
    // did not answer returned no chassis list to name it with.
    assert!(
        telemetry_keys
            .iter()
            .any(|k| k.ends_with("/bmc/rack-a-1/reachable")),
        "reachable must stay endpoint-scoped: {telemetry_keys:#?}"
    );

    // 2. The state documents follow the key, not the endpoint.
    for want in [
        "chassis/rack-a-1-1",
        "chassis/rack-a-1-2",
        "chassis/rack-a-1-3",
    ] {
        assert!(
            state_keys.iter().any(|k| k.contains(want)),
            "no state document for {want}: {state_keys:#?}"
        );
    }

    // 3. The failure on chassis 2 is graded and labelled with ITS chunk.
    let psu_failed: Vec<&zensight_common::Alert> = alerts
        .iter()
        .filter(|a| a.rule == alerts::RULE_PSU_FAILED)
        .collect();
    assert!(
        psu_failed
            .iter()
            .any(|a| a.labels.get("chassis").map(String::as_str) == Some("rack-a-1-2")),
        "chassis 2's failed supply was not asserted — only `sweeps.first()` \
         was graded (#1130). Alerts: {:#?}",
        alerts
            .iter()
            .map(|a| (&a.rule, &a.labels))
            .collect::<Vec<_>>()
    );

    // And chassis 1's bay 0 — failing on the same fixture — is a SEPARATE
    // alert, not the same key overwritten.
    let keys: std::collections::BTreeSet<String> =
        psu_failed.iter().map(|a| a.alert_key()).collect();
    assert!(
        keys.len() >= 2,
        "two chassis, one endpoint, the same bay id — these must be distinct \
         alerts, got {keys:?}"
    );
}
