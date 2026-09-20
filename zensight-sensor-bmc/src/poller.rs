//! The poll loop (#953).

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use zensight_common::bmc::RedfishSurface;
use zensight_common::{QosClass, TelemetryValue};
use zensight_sensor_core::{AlertReporter, Publisher, SensorHealth, SweepOpts};

use crate::alerts::{self, Observation};
use crate::config::{BmcConfig, Endpoint};
use crate::redfish::{ChassisSweep, RedfishClient};
use crate::telemetry_guard::checked_point;

/// State documents ride an advanced publisher with cache 1, so a late joiner
/// (the GUI, a storage) seeds the current document instead of waiting a whole
/// interval to learn a supply failed.
pub const STATE_QOS: QosClass = QosClass::HealthLiveness;

/// What one endpoint has taught us that a single sweep cannot.
#[derive(Default)]
struct EndpointState {
    consecutive_failures: u32,
    /// Bays each CHASSIS has reported present at some point. A bay that was
    /// never populated is not a bay someone emptied — see `psu-absent`.
    ///
    /// Keyed by chassis id, not flat (#1130): one Redfish service fronts
    /// several chassis and every one of them numbers its bays from `0`, so a
    /// flat set let chassis 1 having ever held a supply make chassis 2's empty
    /// bay 0 fire `psu-absent`.
    known_present: HashMap<String, BTreeSet<String>>,
    /// Chassis ids the endpoint's `/redfish/v1/Chassis` collection listed last
    /// time it answered. A chassis that LEAVES the collection has its rules
    /// reconciled to empty; one that is merely unsweepable this cycle does
    /// not, because "we could not read it" is not "it recovered".
    known_chassis: BTreeSet<String>,
    due: Option<Instant>,
}

pub struct Poller {
    cfg: BmcConfig,
    source: String,
    clients: HashMap<String, Arc<RedfishClient>>,
    state: HashMap<String, EndpointState>,
    publisher: Publisher,
    states: Arc<zensight_sensor_core::AdvancedPublisherRegistry>,
    evidence: Option<Arc<zensight_sensor_core::AdvancedPublisherRegistry>>,
    reporter: Option<Arc<AlertReporter>>,
    health: Arc<SensorHealth>,
}

impl Poller {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: BmcConfig,
        source: String,
        clients: HashMap<String, Arc<RedfishClient>>,
        publisher: Publisher,
        states: Arc<zensight_sensor_core::AdvancedPublisherRegistry>,
        evidence: Option<Arc<zensight_sensor_core::AdvancedPublisherRegistry>>,
        reporter: Option<Arc<AlertReporter>>,
        health: Arc<SensorHealth>,
    ) -> Self {
        let state = cfg
            .endpoints
            .iter()
            .filter(|e| e.enabled)
            .map(|e| (e.name.clone(), EndpointState::default()))
            .collect();
        Self {
            cfg,
            source,
            clients,
            state,
            publisher,
            states,
            evidence,
            reporter,
            health,
        }
    }

    /// Run until the session closes.
    ///
    /// The tick is the interval floor and each endpoint decides for itself
    /// whether it is due, so a per-chassis override costs no extra task — the
    /// `probe` shape.
    pub async fn run(mut self) {
        self.health
            .set_devices_total(self.cfg.endpoints.iter().filter(|e| e.enabled).count() as u64);
        let mut tick = tokio::time::interval(Duration::from_secs(crate::config::MIN_INTERVAL_SECS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            self.sweep_due().await;
        }
    }

    /// One pass over every endpoint whose interval has elapsed.
    pub async fn sweep_due(&mut self) {
        let now = Instant::now();
        let endpoints: Vec<Endpoint> = self
            .cfg
            .endpoints
            .iter()
            .filter(|e| e.enabled)
            .filter(|e| {
                self.state
                    .get(&e.name)
                    .and_then(|s| s.due)
                    .is_none_or(|due| now >= due)
            })
            .cloned()
            .collect();
        for endpoint in endpoints {
            let interval = Duration::from_secs(endpoint.interval(self.cfg.interval_secs));
            self.poll_endpoint(&endpoint).await;
            if let Some(state) = self.state.get_mut(&endpoint.name) {
                state.due = Some(Instant::now() + interval);
            }
        }
    }

    /// Poll one chassis and publish everything it said — or, when it said
    /// nothing, publish exactly that and no gauges.
    pub async fn poll_endpoint(&mut self, endpoint: &Endpoint) {
        let Some(client) = self.clients.get(&endpoint.name).cloned() else {
            return;
        };
        let started = Instant::now();
        let outcome = Self::collect(&client).await;
        self.health
            .record_poll_duration(started.elapsed().as_millis() as u64);

        match outcome {
            Ok((listed, sweeps)) => {
                self.health.record_device_success(&endpoint.name);
                if let Some(state) = self.state.get_mut(&endpoint.name) {
                    state.consecutive_failures = 0;
                }
                // Endpoint-level, and hoisted out of the per-chassis loop:
                // `reachable` is a fact about the BMC that answered, and
                // publishing it once per chassis wrote the same key N times.
                self.publish_reachable(endpoint, 1.0).await;
                for sweep in &sweeps {
                    self.publish(endpoint, sweep).await;
                }
                self.assert_endpoint(endpoint, &listed, &sweeps).await;
            }
            Err(e) => {
                // One failure is the BMC's, not each component's.
                self.health.record_device_failure(&endpoint.name, &e);
                if let Some(state) = self.state.get_mut(&endpoint.name) {
                    state.consecutive_failures += 1;
                }
                // The ONLY gauge an unreachable BMC produces. Publishing a
                // chassis of zeroes would be inventing readings; publishing
                // nothing at all would be indistinguishable from a sensor that
                // is not running.
                self.publish_reachable(endpoint, 0.0).await;
                self.assert_endpoint(endpoint, &[], &[]).await;
            }
        }
    }

    /// The chassis the collection LISTED, and the ones that could be swept.
    ///
    /// Both, because they answer different questions (#1130). A chassis the
    /// collection stopped listing is gone and its alerts must resolve; a
    /// chassis that is listed but whose sweep failed is unreadable this cycle
    /// and its alerts must NOT — resolving those announces that a failed
    /// supply is fine because we could not see it.
    #[allow(clippy::type_complexity)]
    async fn collect(client: &RedfishClient) -> Result<(Vec<String>, Vec<ChassisSweep>), String> {
        let ids = client.chassis_ids().await.map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for id in &ids {
            match client.sweep(id).await {
                Ok(sweep) => out.push(sweep),
                // One unreadable chassis must not cost the others.
                Err(e) => tracing::warn!(chassis = %id, error = %e, "bmc: chassis sweep failed"),
            }
        }
        Ok((ids, out))
    }

    async fn publish(&mut self, endpoint: &Endpoint, sweep: &ChassisSweep) {
        // THE CHASSIS, not the endpoint (#1130). Every key below used to carry
        // the endpoint name, so on a blade enclosure or a four-node twin —
        // one Redfish service, several chassis — chassis 1 and chassis 2 both
        // wrote `telemetry/bmc/rack-a-1/psu/0/input_watts` and took turns
        // overwriting each other, sweep by sweep.
        let chassis = crate::chassis_chunk(&endpoint.name, &sweep.chassis.id);

        for psu in &sweep.supplies {
            let id = zensight_sensor_core::key::device_chunk(&psu.id)
                .as_str()
                .to_string();
            let labels = [("psu", psu.id.clone())];
            self.publish_point(
                &chassis,
                &format!("{chassis}/psu/{id}/present"),
                f64::from(u8::from(psu.present)),
                &labels,
            )
            .await;
            for (suffix, value) in [
                ("input_watts", psu.input_watts),
                ("output_watts", psu.output_watts),
                ("capacity_watts", psu.capacity_watts),
            ] {
                // Absent stays absent: `None` publishes nothing at all, which
                // is what makes an empty bay legible next to a metered one.
                if let Some(v) = value {
                    self.publish_point(
                        &chassis,
                        &format!("{chassis}/psu/{id}/{suffix}"),
                        v,
                        &labels,
                    )
                    .await;
                }
            }
            if let Some(key) = self.state_key(&["chassis", &chassis, "psu", &id]) {
                self.states.publish_serializable(&key, psu).await.ok();
            }
        }

        for fan in &sweep.fans {
            let id = zensight_sensor_core::key::device_chunk(&fan.id)
                .as_str()
                .to_string();
            if let Some(rpm) = fan.rpm {
                self.publish_point(
                    &chassis,
                    &format!("{chassis}/fan/{id}/rpm"),
                    rpm,
                    &[("fan", fan.id.clone())],
                )
                .await;
            }
            if let Some(key) = self.state_key(&["chassis", &chassis, "fan", &id]) {
                self.states.publish_serializable(&key, fan).await.ok();
            }
        }

        for sensor in &sweep.thermal {
            let id = zensight_sensor_core::key::device_chunk(&sensor.id)
                .as_str()
                .to_string();
            let labels = [("sensor", sensor.id.clone())];
            for (suffix, value) in [
                ("celsius", sensor.celsius),
                ("upper_critical_c", sensor.upper_critical_c),
                ("upper_warning_c", sensor.upper_warning_c),
            ] {
                if let Some(v) = value {
                    self.publish_point(
                        &chassis,
                        &format!("{chassis}/thermal/{id}/{suffix}"),
                        v,
                        &labels,
                    )
                    .await;
                }
            }
            if let Some(key) = self.state_key(&["chassis", &chassis, "thermal", &id]) {
                self.states.publish_serializable(&key, sensor).await.ok();
            }
        }

        // Storage and memory (#1140). Until now a drive or a DIMM the BMC had
        // already marked Warning rolled up into `Chassis.Status.Health` and
        // nowhere else, so `chassis-health` fired saying "the BMC reports a
        // fault" and named nothing an operator could act on.
        for drive in &sweep.drives {
            let id = zensight_sensor_core::key::device_chunk(&drive.id)
                .as_str()
                .to_string();
            if let Some(pct) = drive.life_left_percent {
                self.publish_point(
                    &chassis,
                    &format!("{chassis}/drive/{id}/life_left_percent"),
                    pct,
                    &[("drive", drive.id.clone())],
                )
                .await;
            }
            if let Some(key) = self.state_key(&["chassis", &chassis, "drive", &id]) {
                self.states.publish_serializable(&key, drive).await.ok();
            }
        }

        for dimm in &sweep.memory {
            let id = zensight_sensor_core::key::device_chunk(&dimm.id)
                .as_str()
                .to_string();
            if let Some(key) = self.state_key(&["chassis", &chassis, "memory", &id]) {
                self.states.publish_serializable(&key, dimm).await.ok();
            }
        }

        // The redundancy GROUP's own verdict (#1140), beside the per-member
        // copy the supplies and fans already carry.
        for group in &sweep.redundancy {
            let id = zensight_sensor_core::key::device_chunk(format!(
                "{}-{}",
                group.subsystem, group.id
            ))
            .as_str()
            .to_string();
            if let Some(key) = self.state_key(&["chassis", &chassis, "redundancy", &id]) {
                self.states.publish_serializable(&key, group).await.ok();
            }
        }

        if let Some(key) = self.state_key(&["chassis", &chassis]) {
            self.states
                .publish_serializable(&key, &sweep.chassis)
                .await
                .ok();
        }
        if sweep.chassis.surface == RedfishSurface::None {
            tracing::warn!(
                chassis = %chassis,
                "bmc: this BMC served neither PowerSubsystem/ThermalSubsystem nor the legacy \
                 Power/Thermal resources — the chassis document is identity only"
            );
        }

        self.publish_evidence(endpoint, sweep).await;

        // Remember which bays have ever held a supply, so `psu-absent` can
        // tell "someone pulled it" from "this model ships with one". Per
        // CHASSIS: every chassis of a multi-chassis service numbers its bays
        // from zero, and a flat set made chassis 1's history speak for
        // chassis 2's empty bay (#1130).
        if let Some(state) = self.state.get_mut(&endpoint.name) {
            let seen = state
                .known_present
                .entry(sweep.chassis.id.clone())
                .or_default();
            for psu in sweep.supplies.iter().filter(|p| p.present) {
                seen.insert(psu.id.clone());
            }
        }
    }

    /// A third-party identity claim: the BMC's view of the machine it manages,
    /// so it fuses in the catalog with that machine's own sensors (RFC 06 §4).
    async fn publish_evidence(&self, endpoint: &Endpoint, sweep: &ChassisSweep) {
        let Some(registry) = &self.evidence else {
            return;
        };
        if sweep.macs.is_empty() && sweep.chassis.serial.is_none() {
            // Nothing to claim. An evidence document with no evidence in it is
            // a claim about identity that carries none.
            return;
        }
        // Per chassis, like every other key here. The framework table owns
        // this subject's spelling and calls the chunk `{device}` (RFC 04
        // §1.4); the device IS the chassis, and writing the endpoint's name
        // here made every chassis of one service overwrite one evidence
        // document — undoing the per-chassis identity #1110 established.
        let device = crate::chassis_chunk(&endpoint.name, &sweep.chassis.id);
        let Some(key) = self.state_key(&["evidence", "device", &device]) else {
            return;
        };
        let evidence = zensight_common::HostEvidence {
            sensor: "bmc".to_string(),
            source: endpoint.name.clone(),
            // An OBSERVER claim, not a self-claim: this is one machine's BMC
            // describing another machine, and the catalog has to know the
            // difference to rank it (RFC 06 §4).
            observer: Some("bmc".to_string()),
            host_id: None,
            boot_id: None,
            // The machine's OWN `ComputerSystem.HostName`, or nothing (#1110).
            // This used to be `Chassis.Name` — a schema *description*, which
            // Dell, HPE and Supermicro all ship as the literal "Computer System
            // Chassis". Every such machine on the fleet then claimed the same
            // hostname, and hostname is a merge rule.
            hostname: sweep.hostname.clone(),
            fqdn: None,
            ips: Vec::new(),
            macs: sweep.macs.clone(),
            vendor: sweep.chassis.manufacturer.clone(),
            platform: sweep.chassis.model.clone(),
            container_id: None,
            cloud: None,
            last_updated: zensight_common::current_timestamp_millis(),
        };
        registry.publish_serializable(&key, &evidence).await.ok();
    }

    /// Grade the endpoint, then each chassis it served — and reconcile each
    /// one in ITS OWN namespace (#1130).
    ///
    /// This used to grade `sweeps.first()` and reconcile the result under the
    /// endpoint's name. On a service fronting several chassis that was worse
    /// than missing the others: `reconcile_labeled` resolves every alert under
    /// the label that is not in the list it is handed, so chassis 2's failed
    /// supply was actively **resolved** every sweep by a `still` list computed
    /// from chassis 1.
    async fn assert_endpoint(
        &mut self,
        endpoint: &Endpoint,
        listed: &[String],
        sweeps: &[ChassisSweep],
    ) {
        let Some(reporter) = self.reporter.clone() else {
            return;
        };
        let failures = self
            .state
            .get(&endpoint.name)
            .map(|s| s.consecutive_failures)
            .unwrap_or_default();

        // --- the endpoint's own rule ----------------------------------------
        //
        // `bmc-unreachable` is a statement about the Redfish service, not
        // about any chassis behind it — a BMC that did not answer returned no
        // chassis list to attribute it to. It reconciles in the endpoint's
        // namespace, alone: handing the whole rule table to a reconcile here
        // would resolve the component alerts the chassis passes below are
        // about to re-state.
        let endpoint_chunk = crate::endpoint_chunk(&endpoint.name);
        let obs = Observation {
            source: &self.source,
            endpoint: &endpoint.name,
            chassis: None,
            supplies: &[],
            fans: &[],
            thermal: &[],
            drives: &[],
            memory: &[],
            redundancy: &[],
            known_present: &[],
            consecutive_failures: failures,
        };
        let firing = alerts::grade(&self.cfg.alerts, &obs);
        if let Err(e) = reporter
            .sweep(
                &[alerts::RULE_UNREACHABLE],
                firing,
                SweepOpts {
                    scope: Some(("chassis", &endpoint_chunk)),
                    ..Default::default()
                },
            )
            .await
        {
            tracing::warn!(rule = %alerts::RULE_UNREACHABLE, error = %e, "bmc: alert sweep failed");
        }

        // --- nothing else while the BMC is silent ---------------------------
        //
        // A BMC that did not answer produced no components. Reconciling the
        // component rules now would resolve every one of them as "recovered" —
        // announcing that a failed power supply is fine because we cannot see
        // it. The same reason `grade` returns early on `chassis: None`.
        if sweeps.is_empty() {
            return;
        }

        // --- one pass per chassis -------------------------------------------
        for sweep in sweeps {
            let chunk = crate::chassis_chunk(&endpoint.name, &sweep.chassis.id);
            let known: Vec<String> = self
                .state
                .get(&endpoint.name)
                .and_then(|s| s.known_present.get(&sweep.chassis.id))
                .map(|set| set.iter().cloned().collect())
                .unwrap_or_default();
            let obs = Observation {
                source: &self.source,
                endpoint: &endpoint.name,
                chassis: Some(&sweep.chassis),
                supplies: &sweep.supplies,
                fans: &sweep.fans,
                thermal: &sweep.thermal,
                drives: &sweep.drives,
                memory: &sweep.memory,
                redundancy: &sweep.redundancy,
                known_present: &known,
                consecutive_failures: failures,
            };
            let firing = alerts::grade(&self.cfg.alerts, &obs);
            // Every chassis-scoped rule reconciles within this chassis's
            // namespace; a graded rule outside the table is refused by the
            // reporter rather than published and stranded (#1154).
            if let Err(e) = reporter
                .sweep(
                    alerts::CHASSIS_RULES,
                    firing,
                    SweepOpts {
                        scope: Some(("chassis", &chunk)),
                        ..Default::default()
                    },
                )
                .await
            {
                tracing::warn!(chassis = %chunk, error = %e, "bmc: alert sweep failed");
            }
        }

        // --- a chassis that LEFT the collection ------------------------------
        //
        // Its rules reconcile to empty, or its alerts fire until the process
        // restarts. `listed` and not `sweeps`: a chassis the collection still
        // names but whose sweep failed is unreadable this cycle, not gone, and
        // resolving that one is the mistake the block above exists to avoid.
        let now: BTreeSet<String> = listed.iter().cloned().collect();
        let gone: Vec<String> = self
            .state
            .get(&endpoint.name)
            .map(|s| s.known_chassis.difference(&now).cloned().collect())
            .unwrap_or_default();
        for id in &gone {
            tracing::info!(
                endpoint = %endpoint.name, chassis = %id,
                "bmc: chassis left the Chassis collection; resolving its assertions"
            );
            let chunk = crate::chassis_chunk(&endpoint.name, id);
            if let Err(e) = reporter
                .sweep(
                    alerts::CHASSIS_RULES,
                    Vec::new(),
                    SweepOpts {
                        scope: Some(("chassis", &chunk)),
                        ..Default::default()
                    },
                )
                .await
            {
                tracing::warn!(chassis = %chunk, error = %e, "bmc: alert sweep failed");
            }
        }
        if let Some(state) = self.state.get_mut(&endpoint.name) {
            for id in &gone {
                state.known_present.remove(id);
            }
            state.known_chassis = now;
        }
    }

    /// `{endpoint}/reachable`: 1 when the BMC answered this cycle, 0 when it
    /// did not.
    ///
    /// The one series keyed by the ENDPOINT rather than by a chassis, because
    /// a BMC that did not answer returned no chassis list — there is nothing
    /// else to name it with, and inventing a chassis chunk for it would claim
    /// a chassis exists that we have never seen.
    async fn publish_reachable(&self, endpoint: &Endpoint, value: f64) {
        let chunk = crate::endpoint_chunk(&endpoint.name);
        self.publish_point(&chunk, &format!("{chunk}/reachable"), value, &[])
            .await;
    }

    /// Publish one gauge, labelled with the `{chassis}` chunk its key carries.
    ///
    /// The label is the chunk and not the endpoint name, so label and key name
    /// the same thing. They disagreed (#1130), which is how one chassis's
    /// series arrived under another's name in every consumer that groups by
    /// the label rather than by the key.
    async fn publish_point(
        &self,
        chassis_chunk: &str,
        metric: &str,
        value: f64,
        labels: &[(&str, String)],
    ) {
        let mut point = checked_point(
            &self.source,
            metric.to_string(),
            TelemetryValue::Gauge(value),
        )
        .with_label("chassis", chassis_chunk.to_string());
        for (k, v) in labels {
            point = point.with_label(*k, v.clone());
        }
        if let Err(e) = self.publisher.publish(&point.metric.clone(), &point).await {
            tracing::warn!(error = %e, "bmc: publish failed");
        }
    }

    /// Build a `state/bmc/<chunks…>` key, refusing anything the grammar will
    /// not mint rather than papering over it.
    fn state_key(&self, chunks: &[&str]) -> Option<String> {
        match zensight_sensor_core::v1::for_producer("bmc").state_key(chunks) {
            Ok(k) => Some(k.into()),
            Err(e) => {
                tracing::warn!(chunks = ?chunks, error = %e, "bmc: not a legal state subject");
                None
            }
        }
    }
}
