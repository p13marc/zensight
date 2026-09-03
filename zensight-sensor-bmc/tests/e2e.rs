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

async fn chassis_collection() -> Json<Value> {
    Json(json!({"Members": [{"@odata.id": "/redfish/v1/Chassis/1"}]}))
}

async fn chassis_one() -> Json<Value> {
    Json(json!({
        "Id": "1",
        "Name": "Computer System Chassis",
        "Manufacturer": "ACME",
        "Model": "R740",
        "SerialNumber": "CH-0001",
        "AssetTag": "rack-a-1",
        "PowerState": "On",
        "PhysicalSecurity": {"IntrusionSensor": "Normal"},
        "Status": {"Health": "OK", "State": "Enabled"},
    }))
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
async fn thermal_collection() -> Json<Value> {
    Json(member_links(
        2,
        "/redfish/v1/Chassis/1/ThermalSubsystem/ThermalMetrics",
    ))
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
async fn thermal_member(axum::extract::Path(i): axum::extract::Path<usize>) -> Json<Value> {
    Json(temperatures().as_array().unwrap()[i].clone())
}

async fn not_found() -> axum::http::StatusCode {
    axum::http::StatusCode::NOT_FOUND
}

async fn spawn(fixture: Fixture) -> SocketAddr {
    let app = Router::new()
        .route("/redfish/v1/Chassis", get(chassis_collection))
        .route("/redfish/v1/Chassis/1", get(chassis_one))
        .route("/redfish/v1/Chassis/1/Power", get(legacy_power))
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
            get(thermal_collection),
        )
        .route(
            "/redfish/v1/Chassis/1/ThermalSubsystem/ThermalMetrics/{i}",
            get(thermal_member),
        )
        // A read-only account legitimately cannot see Systems on some
        // firmware. It comes back as no MACs, not as a failed sweep.
        .route("/redfish/v1/Systems", get(not_found))
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
        assert_eq!(a.labels["chassis"], "rack-a-1");
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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreadable_resource_is_data_not_a_failure() {
    let addr = spawn(fixture(true)).await;
    let sweep = client(addr).sweep("1").await.expect("sweep");
    assert!(sweep.macs.is_empty());
    assert_eq!(sweep.chassis.health, Health::OK);
}
