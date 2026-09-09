//! The Redfish client (#953) — read-only, bounded, and forgiving of the
//! resources a given firmware does not serve.
//!
//! Four deliberate choices, each of which the alternative gets wrong:
//!
//! - **Every response is `serde_json::Value` first**, then read field by
//!   field. Redfish is a large schema that vendors implement in patches: HPE,
//!   Dell, Lenovo and Supermicro disagree about which members exist, and every
//!   firmware release moves something. A strict struct would turn a cosmetic
//!   upstream difference into a sensor that reports nothing.
//! - **The surface is discovered, not assumed.** Redfish 2020.4 deprecated
//!   `Chassis/{id}/Power` and `Thermal` in favour of `PowerSubsystem` and
//!   `ThermalSubsystem`, and a great deal of shipped firmware serves only the
//!   old pair. The client tries the new one, falls back, and **records which
//!   answered** — because a field absent on one is a different fact from one
//!   absent on the other.
//! - **404 and 403 are data.** A chassis with no thermal resource is a fact
//!   about that hardware, not a failed poll; grading it as one would make a
//!   correctly-scoped read-only account look like a broken sensor. Logged once
//!   per transition, never once per cycle.
//! - **The client takes a full base URL string.** That is the seam that lets
//!   the e2e point it at a plain-HTTP fake: standing up a TLS listener would
//!   test rustls, not this sensor.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::Semaphore;
use zensight_common::bmc::{
    Chassis, Fan, Health, PowerSupply, RedfishSurface, Redundancy, State, ThermalSensor,
};

#[derive(Debug)]
pub enum ApiError {
    Transport(String),
    Status { code: u16, body: String },
    Malformed(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Transport(e) => write!(f, "transport: {e}"),
            ApiError::Status { code, body } => write!(f, "HTTP {code}: {body}"),
            ApiError::Malformed(e) => write!(f, "malformed reply: {e}"),
        }
    }
}

impl std::error::Error for ApiError {}

pub type Result<T> = std::result::Result<T, ApiError>;

/// Everything one chassis reported in one sweep.
#[derive(Debug, Clone)]
pub struct ChassisSweep {
    pub chassis: Chassis,
    pub supplies: Vec<PowerSupply>,
    pub fans: Vec<Fan>,
    pub thermal: Vec<ThermalSensor>,
    /// MACs the BMC knows about this machine, for the identity claim.
    /// Scoped to the systems **this chassis links** (#1110).
    pub macs: Vec<String>,
    /// The managed machine's own `ComputerSystem.HostName`, when it reports
    /// one (#1110). Deliberately **not** `Chassis.Name`, which is a schema
    /// description that ships as the literal "Computer System Chassis" on Dell,
    /// HPE and Supermicro — claiming it as a hostname asserts that every such
    /// machine is the same host. `None` means the BMC did not say, which is a
    /// missing claim rather than a wrong one.
    pub hostname: Option<String>,
}

pub struct RedfishClient {
    http: reqwest::Client,
    base: String,
    username: String,
    password: String,
    limit: Arc<Semaphore>,
    /// Paths currently answering 403/404/501, so a refusal is logged once per
    /// transition rather than once per poll (the #880 lesson).
    refused: Arc<Mutex<HashSet<String>>>,
}

impl RedfishClient {
    /// `base` is the full scheme+authority, e.g. `https://10.0.0.10` — or
    /// `http://127.0.0.1:PORT` in a test.
    pub fn new(
        base: String,
        username: String,
        password: String,
        timeout: Duration,
        insecure: bool,
        ca_pem: Option<Vec<u8>>,
        max_concurrent: usize,
    ) -> anyhow::Result<Self> {
        let mut builder = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(concat!("zensight-sensor-bmc/", env!("CARGO_PKG_VERSION")));
        if insecure {
            builder = builder.danger_accept_invalid_certs(true);
        }
        if let Some(pem) = ca_pem {
            // Added, not replaced: a BMC behind an internal CA still needs the
            // public roots for anything else the same client might reach.
            builder = builder.add_root_certificate(reqwest::Certificate::from_pem(&pem)?);
        }
        Ok(Self {
            http: builder.build()?,
            base,
            username,
            password,
            limit: Arc::new(Semaphore::new(max_concurrent.max(1))),
            refused: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    /// GET one Redfish resource. `Ok(None)` for 403/404/501 — a resource this
    /// firmware does not serve, or this account may not read.
    pub async fn get(&self, path: &str) -> Result<Option<Value>> {
        let _permit = self
            .limit
            .acquire()
            .await
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let resp = self
            .http
            .get(format!("{}{}", self.base, path))
            .basic_auth(&self.username, Some(&self.password))
            .send()
            .await
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let status = resp.status();
        if matches!(status.as_u16(), 403 | 404 | 501) {
            if self.refused.lock().unwrap().insert(path.to_string()) {
                tracing::warn!(
                    path = %path,
                    status = status.as_u16(),
                    "bmc: this Redfish resource is not served here — what it carries will be \
                     reported as unknown, never as zero"
                );
            }
            return Ok(None);
        }
        if self.refused.lock().unwrap().remove(path) {
            tracing::info!(path = %path, "bmc: resource readable again");
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status {
                code: status.as_u16(),
                // Never echo an unbounded body into a log line.
                body: body.chars().take(200).collect(),
            });
        }
        resp.json::<Value>()
            .await
            .map(Some)
            .map_err(|e| ApiError::Malformed(e.to_string()))
    }

    /// The chassis ids this service exposes.
    pub async fn chassis_ids(&self) -> Result<Vec<String>> {
        let Some(body) = self.get("/redfish/v1/Chassis").await? else {
            return Ok(Vec::new());
        };
        Ok(members(&body)
            .into_iter()
            .filter_map(|link| link.rsplit('/').find(|s| !s.is_empty()).map(str::to_string))
            .collect())
    }

    /// Poll one chassis end to end.
    pub async fn sweep(&self, id: &str) -> Result<ChassisSweep> {
        let root = self
            .get(&format!("/redfish/v1/Chassis/{id}"))
            .await?
            .unwrap_or(Value::Null);

        // The modern surface first, then the legacy pair. Which one answered
        // is recorded: a reading absent on one is a different fact from the
        // same reading absent on the other.
        let (mut supplies, mut fans, mut thermal, surface) = {
            let power_sub = self
                .get(&format!("/redfish/v1/Chassis/{id}/PowerSubsystem"))
                .await?;
            let thermal_sub = self
                .get(&format!("/redfish/v1/Chassis/{id}/ThermalSubsystem"))
                .await?;
            if power_sub.is_some() || thermal_sub.is_some() {
                let supplies = self
                    .collection(&format!(
                        "/redfish/v1/Chassis/{id}/PowerSubsystem/PowerSupplies"
                    ))
                    .await?;
                let fans = self
                    .collection(&format!("/redfish/v1/Chassis/{id}/ThermalSubsystem/Fans"))
                    .await?;
                let sensors = self
                    .collection(&format!(
                        "/redfish/v1/Chassis/{id}/ThermalSubsystem/ThermalMetrics"
                    ))
                    .await?;
                (
                    supplies.iter().map(parse_supply).collect::<Vec<_>>(),
                    fans.iter().map(parse_fan).collect::<Vec<_>>(),
                    sensors.iter().map(parse_thermal).collect::<Vec<_>>(),
                    RedfishSurface::Subsystem,
                )
            } else {
                let power = self.get(&format!("/redfish/v1/Chassis/{id}/Power")).await?;
                let therm = self
                    .get(&format!("/redfish/v1/Chassis/{id}/Thermal"))
                    .await?;
                let surface = if power.is_some() || therm.is_some() {
                    RedfishSurface::Legacy
                } else {
                    RedfishSurface::None
                };
                let supplies = array(power.as_ref(), "PowerSupplies")
                    .iter()
                    .map(parse_supply)
                    .collect::<Vec<_>>();
                let fans = array(therm.as_ref(), "Fans")
                    .iter()
                    .map(parse_fan)
                    .collect::<Vec<_>>();
                let thermal = array(therm.as_ref(), "Temperatures")
                    .iter()
                    .map(parse_thermal)
                    .collect::<Vec<_>>();
                (supplies, fans, thermal, surface)
            }
        };

        // Ids have to be stable and unique, or two bays take turns overwriting
        // one document. Anything unnamed falls back to its position.
        fill_ids(&mut supplies, |s| &mut s.id);
        fill_ids(&mut fans, |f| &mut f.id);
        fill_ids(&mut thermal, |t| &mut t.id);

        let chassis = parse_chassis(id, &root, surface);
        let (macs, hostname) = self.identity(&root).await.unwrap_or_default();
        Ok(ChassisSweep {
            chassis,
            supplies,
            fans,
            thermal,
            macs,
            hostname,
        })
    }

    /// Every member of a Redfish collection, fetched. An unreadable member is
    /// skipped rather than failing the sweep: one bad bay must not cost the
    /// other seven.
    async fn collection(&self, path: &str) -> Result<Vec<Value>> {
        let Some(body) = self.get(path).await? else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for link in members(&body) {
            if let Ok(Some(member)) = self.get(&link).await {
                out.push(member);
            }
        }
        Ok(out)
    }

    /// What the BMC knows about the machine (or machines) in **this** chassis,
    /// for the identity claim.
    ///
    /// Scoped through `Chassis/{id}/Links/ComputerSystems` (#1110). It used to
    /// ignore its chassis argument entirely and walk every member of
    /// `/redfish/v1/Systems`, so on a 4-node Twin or a blade enclosure — one
    /// Redfish service in front of several machines — the union of all nodes'
    /// MACs landed on *each* chassis's evidence. MAC is the catalog's strongest
    /// merge key after `host_id`, so that claim asks it to fuse every node in
    /// the enclosure into one host.
    ///
    /// A chassis that links no system yields nothing, which is correct: this
    /// is a claim about a machine, and a chassis with no machine in it has none
    /// to make.
    ///
    /// Best-effort throughout: absent on plenty of firmware, and absent is
    /// fine — it is a *missing* claim, not a wrong one.
    async fn identity(&self, chassis_root: &Value) -> Result<(Vec<String>, Option<String>)> {
        let mut macs = Vec::new();
        let mut hostname = None;
        for system in linked_systems(chassis_root) {
            let Ok(Some(sys)) = self.get(&system).await else {
                continue;
            };
            // The machine's OWN name, as it knows it. Not `Chassis.Name`,
            // which is a schema description — Dell, HPE and Supermicro all
            // ship the literal "Computer System Chassis", so claiming it as a
            // hostname asserts that every such machine is the same host.
            if hostname.is_none()
                && let Some(h) = sys.get("HostName").and_then(Value::as_str)
                && !h.trim().is_empty()
            {
                hostname = Some(h.to_string());
            }
            let Ok(Some(body)) = self.get(&format!("{system}/EthernetInterfaces")).await else {
                continue;
            };
            for iface in members(&body) {
                if let Ok(Some(nic)) = self.get(&iface).await
                    && let Some(mac) = nic.get("MACAddress").and_then(Value::as_str)
                    && !mac.is_empty()
                {
                    macs.push(mac.to_ascii_lowercase());
                }
            }
        }
        macs.sort();
        macs.dedup();
        Ok((macs, hostname))
    }
}

// ── parsing ─────────────────────────────────────────────────────────────────
//
// Free functions over `Value`, so every shape below is testable against a
// fixture without a socket — which is most of what there is to get wrong.

/// The `ComputerSystem` links a chassis declares (#1110).
///
/// `Chassis/{id}/Links/ComputerSystems` is Redfish's own statement of which
/// machines are in this enclosure, and it is the difference between one
/// blade's identity claim and the whole chassis's.
pub fn linked_systems(chassis: &Value) -> Vec<String> {
    chassis
        .get("Links")
        .and_then(|l| l.get("ComputerSystems"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|m| m.get("@odata.id").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// `@odata.id` links out of a Redfish collection.
pub fn members(body: &Value) -> Vec<String> {
    body.get("Members")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|m| m.get("@odata.id").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// A legacy embedded array (`Power.PowerSupplies`, `Thermal.Fans`).
fn array(body: Option<&Value>, key: &str) -> Vec<Value> {
    body.and_then(|b| b.get(key))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn status(v: &Value) -> (Health, State) {
    let s = v.get("Status");
    let health = s
        .and_then(|s| s.get("Health"))
        .and_then(|h| serde_json::from_value(h.clone()).ok())
        .unwrap_or(Health::Unknown);
    let state = s
        .and_then(|s| s.get("State"))
        .and_then(|h| serde_json::from_value(h.clone()).ok())
        .unwrap_or(State::Unknown);
    (health, state)
}

fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// A number from any of several member names, in preference order.
///
/// Redfish moved several readings between releases (`PowerInputWatts` is the
/// modern spelling of `PowerInputWatts`/`LastPowerOutputWatts`), and a vendor
/// may serve either. Absent from all of them stays absent.
fn number(v: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| {
        v.get(*k).and_then(|n| {
            n.as_f64()
                .or_else(|| n.get("Reading").and_then(Value::as_f64))
        })
    })
}

fn redundancy(v: &Value) -> (Option<String>, Option<Redundancy>) {
    let group = text(v, "RedundancyGroup").or_else(|| {
        v.get("Redundancy")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|r| text(r, "Name"))
    });
    let status_value = v
        .get("Redundancy")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .cloned();
    let verdict = status_value.as_ref().map(|r| {
        let (health, _) = status(r);
        match health {
            Health::OK => Redundancy::Full,
            Health::Warning => Redundancy::Degraded,
            _ => Redundancy::Failed,
        }
    });
    (group, verdict)
}

pub fn parse_supply(v: &Value) -> PowerSupply {
    let (health, state) = status(v);
    let (redundancy_group, redundancy) = redundancy(v);
    let present = state.is_present();
    PowerSupply {
        id: text(v, "Id")
            .or_else(|| text(v, "MemberId"))
            .unwrap_or_default(),
        name: text(v, "Name"),
        present,
        health,
        state,
        // An empty bay reports no watts. Publishing 0 would read as a supply
        // drawing nothing, which is a different and wrong statement.
        input_watts: present
            .then(|| number(v, &["PowerInputWatts", "LineInputVoltage_W"]))
            .flatten(),
        output_watts: present
            .then(|| number(v, &["PowerOutputWatts", "LastPowerOutputWatts"]))
            .flatten(),
        capacity_watts: number(v, &["PowerCapacityWatts", "CapacityWatts"]),
        redundancy_group,
        redundancy,
        model: text(v, "Model"),
        serial: text(v, "SerialNumber"),
    }
}

pub fn parse_fan(v: &Value) -> Fan {
    let (health, state) = status(v);
    let (redundancy_group, redundancy) = redundancy(v);
    let present = state.is_present();
    Fan {
        id: text(v, "Id")
            .or_else(|| text(v, "MemberId"))
            .unwrap_or_default(),
        name: text(v, "Name").or_else(|| text(v, "FanName")),
        present,
        health,
        state,
        // `SpeedPercent` is deliberately NOT read: it is a percentage of
        // maximum, a different quantity, and publishing it on a series named
        // `rpm` would be a wrong number rather than a missing one (#954).
        rpm: present
            .then(|| number(v, &["SpeedRPM", "Reading", "ReadingRPM"]))
            .flatten(),
        redundancy_group,
        redundancy,
    }
}

pub fn parse_thermal(v: &Value) -> ThermalSensor {
    let (health, state) = status(v);
    ThermalSensor {
        id: text(v, "Id")
            .or_else(|| text(v, "MemberId"))
            .unwrap_or_default(),
        name: text(v, "Name"),
        health,
        state,
        celsius: number(v, &["ReadingCelsius", "Reading", "TemperatureCelsius"]),
        upper_critical_c: number(v, &["UpperThresholdCritical", "ReadingRangeMax"]),
        upper_warning_c: number(v, &["UpperThresholdNonCritical"]),
    }
}

pub fn parse_chassis(id: &str, v: &Value, surface: RedfishSurface) -> Chassis {
    let (health, state) = status(v);
    Chassis {
        id: id.to_string(),
        name: text(v, "Name"),
        manufacturer: text(v, "Manufacturer"),
        model: text(v, "Model"),
        serial: text(v, "SerialNumber"),
        asset_tag: text(v, "AssetTag"),
        power_state: text(v, "PowerState"),
        intrusion: v
            .get("PhysicalSecurity")
            .and_then(|p| text(p, "IntrusionSensor")),
        health,
        state,
        firmware: text(v, "FirmwareVersion"),
        surface,
    }
}

/// Give anything unnamed a stable id from its position.
///
/// Redfish's legacy embedded arrays often omit `MemberId`, and an empty id
/// would collapse every bay onto one key — eight supplies taking turns
/// overwriting one document, which reads as a chassis that keeps changing its
/// mind.
fn fill_ids<T>(items: &mut [T], id: impl Fn(&mut T) -> &mut String) {
    for (i, item) in items.iter_mut().enumerate() {
        let slot = id(item);
        if slot.is_empty() {
            *slot = i.to_string();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// #1110: a chassis claims the systems **it links**, not every system the
    /// Redfish service knows.
    ///
    /// On a 4-node Twin or a blade enclosure one service fronts several
    /// machines, and `macs()` ignored its chassis argument and walked
    /// `/redfish/v1/Systems` wholesale — so the union of every node's MACs
    /// landed on *each* chassis's evidence. MAC is the catalog's strongest
    /// merge key after `host_id`, so that claim asks it to fuse the whole
    /// enclosure into one host.
    #[test]
    fn a_chassis_links_its_own_systems_only() {
        let blade_a = json!({
            "Id": "1",
            "Links": { "ComputerSystems": [{"@odata.id": "/redfish/v1/Systems/1"}] },
        });
        let blade_b = json!({
            "Id": "2",
            "Links": { "ComputerSystems": [{"@odata.id": "/redfish/v1/Systems/2"}] },
        });
        assert_eq!(linked_systems(&blade_a), vec!["/redfish/v1/Systems/1"]);
        assert_eq!(linked_systems(&blade_b), vec!["/redfish/v1/Systems/2"]);
        assert!(
            linked_systems(&blade_a)
                .iter()
                .all(|s| !linked_systems(&blade_b).contains(s)),
            "two blades must not claim each other's systems"
        );
    }

    /// A chassis that links no system makes no identity claim, which is
    /// correct: a chassis with no machine in it has no machine to describe.
    /// Firmware that omits `Links` entirely lands here too — a *missing* claim
    /// rather than a wrong one.
    #[test]
    fn a_chassis_with_no_linked_system_claims_nothing() {
        assert!(linked_systems(&json!({"Id": "1"})).is_empty());
        assert!(linked_systems(&json!({"Id": "1", "Links": {}})).is_empty());
        assert!(linked_systems(&json!({"Id": "1", "Links": {"ComputerSystems": []}})).is_empty());
    }

    /// An enclosure that fronts several machines links them all, and then the
    /// union is correct rather than a merge hazard.
    #[test]
    fn a_multi_system_chassis_links_all_of_them() {
        let enclosure = json!({
            "Id": "encl",
            "Links": { "ComputerSystems": [
                {"@odata.id": "/redfish/v1/Systems/1"},
                {"@odata.id": "/redfish/v1/Systems/2"},
            ]},
        });
        assert_eq!(linked_systems(&enclosure).len(), 2);
    }

    /// A healthy modern supply.
    #[test]
    fn a_present_supply_carries_its_watts() {
        let s = parse_supply(&json!({
            "Id": "0",
            "Name": "PSU 1",
            "Status": {"Health": "OK", "State": "Enabled"},
            "PowerInputWatts": 210.5,
            "PowerOutputWatts": 190.0,
            "PowerCapacityWatts": 800.0,
            "Model": "PS-2801",
            "SerialNumber": "SN123",
        }));
        assert!(s.present);
        assert_eq!(s.health, Health::OK);
        assert_eq!(s.input_watts, Some(210.5));
        assert_eq!(s.capacity_watts, Some(800.0));
    }

    /// An empty bay publishes NO watts. Zero would read as a supply drawing
    /// nothing, which is a different and wrong statement — and the one an
    /// operator would act on.
    #[test]
    fn an_absent_bay_reports_no_watts_rather_than_zero() {
        let s = parse_supply(&json!({
            "Id": "1",
            "Status": {"State": "Absent"},
            // Some firmware leaves stale readings in an absent slot.
            "PowerInputWatts": 0,
            "PowerOutputWatts": 0,
        }));
        assert!(!s.present);
        assert_eq!(s.input_watts, None, "an absent bay measures nothing");
        assert_eq!(s.output_watts, None);
    }

    /// A fan's speed as a percentage of maximum is a different quantity from
    /// RPM. Publishing it as `rpm` would be a wrong number, not a missing one.
    #[test]
    fn a_percentage_fan_speed_is_not_an_rpm() {
        let f = parse_fan(&json!({
            "Id": "0",
            "Status": {"Health": "OK", "State": "Enabled"},
            "SpeedPercent": {"Reading": 42.0},
        }));
        assert_eq!(f.rpm, None, "SpeedPercent must not become rpm");

        let f = parse_fan(&json!({
            "Id": "0",
            "Status": {"Health": "OK", "State": "Enabled"},
            "SpeedRPM": 4800,
        }));
        assert_eq!(f.rpm, Some(4800.0));
    }

    /// A fan at zero RPM that the BMC calls Critical is a failed fan, and the
    /// zero is a real reading — the one case where zero IS the measurement.
    #[test]
    fn a_stopped_fan_keeps_its_zero() {
        let f = parse_fan(&json!({
            "Id": "3",
            "Status": {"Health": "Critical", "State": "Enabled"},
            "Reading": 0,
        }));
        assert_eq!(f.rpm, Some(0.0));
        assert!(f.health.is_faulted());
    }

    /// The legacy and modern spellings of the same reading both land.
    #[test]
    fn both_redfish_generations_of_a_temperature_parse() {
        let legacy = parse_thermal(&json!({
            "MemberId": "0",
            "Name": "Inlet Temp",
            "Status": {"Health": "OK", "State": "Enabled"},
            "ReadingCelsius": 23.0,
            "UpperThresholdCritical": 45.0,
            "UpperThresholdNonCritical": 40.0,
        }));
        assert_eq!(legacy.celsius, Some(23.0));
        assert_eq!(legacy.upper_critical_c, Some(45.0));
        assert_eq!(legacy.over_critical(), Some(false));

        let modern = parse_thermal(&json!({
            "Id": "cpu1",
            "Status": {"Health": "Critical", "State": "Enabled"},
            "Reading": 96.0,
            "UpperThresholdCritical": 90.0,
        }));
        assert_eq!(modern.over_critical(), Some(true));
    }

    /// Redfish's legacy arrays often omit an id. Without a fallback every bay
    /// collapses onto one key and the chassis reads as if it kept changing
    /// its mind.
    #[test]
    fn unnamed_members_get_stable_positional_ids() {
        let mut fans: Vec<Fan> = vec![
            parse_fan(&json!({"Status": {"State": "Enabled"}, "Reading": 1000})),
            parse_fan(&json!({"Status": {"State": "Enabled"}, "Reading": 2000})),
        ];
        fill_ids(&mut fans, |f| &mut f.id);
        assert_eq!(fans[0].id, "0");
        assert_eq!(fans[1].id, "1");
    }

    #[test]
    fn collection_members_are_read_as_links() {
        let body = json!({"Members": [
            {"@odata.id": "/redfish/v1/Chassis/1"},
            {"@odata.id": "/redfish/v1/Chassis/2"},
        ]});
        assert_eq!(
            members(&body),
            vec!["/redfish/v1/Chassis/1", "/redfish/v1/Chassis/2"]
        );
        assert!(members(&json!({})).is_empty());
    }

    /// Which surface answered is part of the document, because a reading
    /// absent on the legacy pair is a different fact from one absent on the
    /// modern one.
    #[test]
    fn the_chassis_records_which_surface_answered() {
        let c = parse_chassis(
            "1",
            &json!({
                "Name": "Computer System Chassis",
                "Manufacturer": "ACME",
                "SerialNumber": "CH-1",
                "PowerState": "On",
                "PhysicalSecurity": {"IntrusionSensor": "Normal"},
                "Status": {"Health": "OK", "State": "Enabled"},
            }),
            RedfishSurface::Legacy,
        );
        assert_eq!(c.surface, RedfishSurface::Legacy);
        assert_eq!(c.power_state.as_deref(), Some("On"));
        assert_eq!(c.intrusion.as_deref(), Some("Normal"));
    }

    /// A BMC that answers with a document missing every optional member must
    /// still parse. A strict struct here would turn a thin firmware into a
    /// sensor that reports nothing.
    #[test]
    fn a_nearly_empty_document_still_parses() {
        let c = parse_chassis("1", &json!({}), RedfishSurface::None);
        assert_eq!(c.health, Health::Unknown);
        assert_eq!(c.state, State::Unknown);
        assert!(c.name.is_none());

        let s = parse_supply(&json!({}));
        assert_eq!(s.input_watts, None);
        let t = parse_thermal(&json!({}));
        assert_eq!(t.over_critical(), None);
    }
}
