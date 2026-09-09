//! The poll loop (#953).

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use zensight_common::bmc::RedfishSurface;
use zensight_common::{QosClass, TelemetryValue};
use zensight_sensor_core::{AlertReporter, Publisher, SensorHealth};

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
    /// Bays this endpoint has reported present at some point. A bay that was
    /// never populated is not a bay someone emptied — see `psu-absent`.
    known_present: BTreeSet<String>,
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
            Ok(sweeps) => {
                self.health.record_device_success(&endpoint.name);
                if let Some(state) = self.state.get_mut(&endpoint.name) {
                    state.consecutive_failures = 0;
                }
                for sweep in &sweeps {
                    self.publish(endpoint, sweep).await;
                }
                self.assert_endpoint(endpoint, sweeps.first()).await;
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
                self.publish_point(endpoint, "reachable", 0.0, &[]).await;
                self.assert_endpoint(endpoint, None).await;
            }
        }
    }

    async fn collect(client: &RedfishClient) -> Result<Vec<ChassisSweep>, String> {
        let ids = client.chassis_ids().await.map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for id in ids {
            match client.sweep(&id).await {
                Ok(sweep) => out.push(sweep),
                // One unreadable chassis must not cost the others.
                Err(e) => tracing::warn!(chassis = %id, error = %e, "bmc: chassis sweep failed"),
            }
        }
        Ok(out)
    }

    async fn publish(&mut self, endpoint: &Endpoint, sweep: &ChassisSweep) {
        let chassis = zenkey::Chunk::slug(&endpoint.name).as_str().to_string();
        self.publish_point(endpoint, "reachable", 1.0, &[]).await;

        for psu in &sweep.supplies {
            let id = zenkey::Chunk::slug(&psu.id).as_str().to_string();
            let labels = [("psu", psu.id.clone())];
            self.publish_point(
                endpoint,
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
                        endpoint,
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
            let id = zenkey::Chunk::slug(&fan.id).as_str().to_string();
            if let Some(rpm) = fan.rpm {
                self.publish_point(
                    endpoint,
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
            let id = zenkey::Chunk::slug(&sensor.id).as_str().to_string();
            let labels = [("sensor", sensor.id.clone())];
            for (suffix, value) in [
                ("celsius", sensor.celsius),
                ("upper_critical_c", sensor.upper_critical_c),
                ("upper_warning_c", sensor.upper_warning_c),
            ] {
                if let Some(v) = value {
                    self.publish_point(
                        endpoint,
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

        if let Some(key) = self.state_key(&["chassis", &chassis]) {
            self.states
                .publish_serializable(&key, &sweep.chassis)
                .await
                .ok();
        }
        if sweep.chassis.surface == RedfishSurface::None {
            tracing::warn!(
                chassis = %endpoint.name,
                "bmc: this BMC served neither PowerSubsystem/ThermalSubsystem nor the legacy \
                 Power/Thermal resources — the chassis document is identity only"
            );
        }

        self.publish_evidence(endpoint, sweep).await;

        // Remember which bays have ever held a supply, so `psu-absent` can
        // tell "someone pulled it" from "this model ships with one".
        if let Some(state) = self.state.get_mut(&endpoint.name) {
            for psu in sweep.supplies.iter().filter(|p| p.present) {
                state.known_present.insert(psu.id.clone());
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
        let device = zenkey::Chunk::slug(&endpoint.name).as_str().to_string();
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

    async fn assert_endpoint(&mut self, endpoint: &Endpoint, sweep: Option<&ChassisSweep>) {
        let Some(reporter) = self.reporter.clone() else {
            return;
        };
        let (failures, known): (u32, Vec<String>) = self
            .state
            .get(&endpoint.name)
            .map(|s| {
                (
                    s.consecutive_failures,
                    s.known_present.iter().cloned().collect(),
                )
            })
            .unwrap_or_default();

        let empty_supplies = Vec::new();
        let empty_fans = Vec::new();
        let empty_thermal = Vec::new();
        let obs = Observation {
            source: &self.source,
            endpoint: &endpoint.name,
            chassis: sweep.map(|s| &s.chassis),
            supplies: sweep.map(|s| &s.supplies).unwrap_or(&empty_supplies),
            fans: sweep.map(|s| &s.fans).unwrap_or(&empty_fans),
            thermal: sweep.map(|s| &s.thermal).unwrap_or(&empty_thermal),
            known_present: &known,
            consecutive_failures: failures,
        };
        let firing = alerts::grade(&self.cfg.alerts, &obs);

        let mut by_rule: HashMap<&str, Vec<String>> = HashMap::new();
        for a in &firing {
            let rule = alerts::ALL_RULES
                .iter()
                .find(|r| **r == a.rule)
                .copied()
                .unwrap_or("");
            by_rule.entry(rule).or_default().push(a.alert_key());
        }
        for a in firing {
            if let Err(e) = reporter.observe(a, None).await {
                tracing::warn!(error = %e, "bmc: alert publish failed");
            }
        }
        // Every rule reconciles every sweep, so a cleared condition resolves
        // instead of firing until restart — and `reconcile_labeled` scopes it
        // to THIS chassis, because one process polls several and one
        // chassis's recovery must not resolve another's fault.
        for rule in alerts::ALL_RULES {
            let still = by_rule.remove(*rule).unwrap_or_default();
            if let Err(e) = reporter
                .reconcile_labeled(rule, "chassis", &endpoint.name, &still)
                .await
            {
                tracing::warn!(rule = %rule, error = %e, "bmc: reconcile failed");
            }
        }
    }

    async fn publish_point(
        &self,
        endpoint: &Endpoint,
        metric: &str,
        value: f64,
        labels: &[(&str, String)],
    ) {
        let metric = if metric == "reachable" {
            format!("{}/reachable", zenkey::Chunk::slug(&endpoint.name).as_str())
        } else {
            metric.to_string()
        };
        let mut point = checked_point(&self.source, metric, TelemetryValue::Gauge(value))
            .with_label("chassis", endpoint.name.clone());
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
