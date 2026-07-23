use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use async_snmp::{Auth, Client, EngineCache, Retry, UdpHandle, Value, v3::EngineState};
use bytes::Bytes;
use zenoh::Session as ZenohSession;

use zensight_common::{Format, Protocol, TelemetryPoint, TelemetryValue, encode};

use crate::config::{AuthProtocol, DeviceConfig, OidGroup, PrivProtocol, SnmpVersion};
use crate::mib::MibResolver;
use crate::oid::{oid_to_string, parse_oid};
use crate::rate::RateTracker;

/// sysUpTime.0 — auto-polled every cycle for reset detection (#527).
const SYS_UPTIME_OID: &str = "1.3.6.1.2.1.1.3.0";

/// SNMP poller for a single device.
pub struct SnmpPoller {
    device: DeviceConfig,
    /// Declared-publisher registry for the telemetry path (declare-on-first-use +
    /// cache per key, drop QoS) — never a one-shot `session.put`.
    registry: Arc<zensight_common::PublisherRegistry>,
    telemetry_prefix: String,
    mib_resolver: Arc<MibResolver>,
    format: Format,
    oids: Vec<String>,
    walks: Vec<String>,
    /// Persistent client (one UDP socket per device, every SNMP version).
    /// Timeout, retry, and GETBULK sizing are configured at build time;
    /// v3 engine discovery/resync and tooBig recovery are handled inside.
    ///
    /// Behind a lock because the poller rebuilds it from `&self` when a v3
    /// engine identity changes (see [`poll_once`](Self::poll_once)).
    client: tokio::sync::RwLock<Option<Client<UdpHandle>>>,
    /// Counter→rate state (#527). Std mutex: locked only for synchronous
    /// bookkeeping, never across an await.
    rate: std::sync::Mutex<RateTracker>,
    /// Threshold alerting (#528), when enabled for this device.
    alerts: Option<tokio::sync::Mutex<crate::alerts::AlertEvaluator>>,
    /// Interface state-doc publishing (#529): the shared advanced-publisher
    /// registry (cached, late-join seed) and this device's doc key.
    interfaces_doc: Option<(Arc<zensight_sensor_core::AdvancedPublisherRegistry>, String)>,
    /// Device profiles (#531): the shared loaded set, and this device's
    /// selection once sysObjectID has been read successfully.
    profiles: Option<Arc<crate::profile::ProfileSet>>,
    selection: std::sync::Mutex<Option<crate::profile::Selection>>,
    /// Loaded SMI MIBs (#532): the naming/typing fallback behind the
    /// explicit resolver tables, plus enum labels and UNITS.
    smi: Option<Arc<crate::smi::SmiResolver>>,
    /// Observed-device identity evidence (#537): the shared Evidence-QoS
    /// advanced registry + refresh cadence in cycles.
    evidence: Option<(Arc<zensight_sensor_core::AdvancedPublisherRegistry>, u32)>,
    /// Completed poll cycles (drives the evidence cadence).
    cycles: std::sync::atomic::AtomicU64,
    /// Resilience tuning (#539) + circuit-breaker state (consecutive
    /// fully-failed cycles).
    resilience: crate::config::ResilienceConfig,
    consecutive_failures: std::sync::atomic::AtomicU32,
    /// Per-device health recording, when wired (#539).
    health: Option<Arc<zensight_sensor_core::SensorHealth>>,
}

/// What one full poll cycle saw (#539) — feeds backoff/breaker/health.
#[derive(Debug, Clone, Copy)]
pub struct CycleOutcome {
    pub requests: usize,
    pub failures: usize,
    pub transport_failures: usize,
}

impl CycleOutcome {
    /// The device answered nothing at the transport level.
    pub fn all_transport_failed(&self) -> bool {
        self.requests > 0
            && self.failures == self.requests
            && self.transport_failures == self.failures
    }
}

/// One scheduling turn of the poller (#539).
#[derive(Debug)]
pub enum CycleKind {
    /// A full poll ran.
    Full(CycleOutcome),
    /// The breaker was open: only the cheap sysUpTime probe ran.
    Probe { ok: bool },
}

impl SnmpPoller {
    /// Create a new poller for a device.
    pub fn new(
        device: DeviceConfig,
        zenoh: Arc<ZenohSession>,
        mib_resolver: Arc<MibResolver>,
        oid_groups: &HashMap<String, OidGroup>,
        format: Format,
    ) -> Self {
        let telemetry_prefix =
            zensight_sensor_core::v1::V1Context::for_producer(&zensight_common::PROFILE, "snmp")
                .telemetry_prefix();

        let oids = device.all_oids(oid_groups);
        let walks = device.all_walks(oid_groups);

        Self {
            device,
            registry: Arc::new(zensight_common::PublisherRegistry::new(zenoh)),
            telemetry_prefix: telemetry_prefix.into(),
            mib_resolver,
            format,
            oids,
            walks,
            client: tokio::sync::RwLock::new(None),
            rate: std::sync::Mutex::new(RateTracker::new()),
            alerts: None,
            interfaces_doc: None,
            profiles: None,
            selection: std::sync::Mutex::new(None),
            smi: None,
            evidence: None,
            cycles: std::sync::atomic::AtomicU64::new(0),
            resilience: crate::config::ResilienceConfig::default(),
            consecutive_failures: std::sync::atomic::AtomicU32::new(0),
            health: None,
        }
    }

    /// Apply resilience tuning (#539).
    pub fn with_resilience(&mut self, resilience: crate::config::ResilienceConfig) {
        self.resilience = resilience;
    }

    /// Record per-device success/failure into the sensor health doc (#539).
    pub fn with_health(&mut self, health: Arc<zensight_sensor_core::SensorHealth>) {
        self.health = Some(health);
    }

    /// Publish observed-device `HostEvidence` (#537) every `refresh_cycles`
    /// successful cycles (and on the first).
    pub fn with_evidence(
        &mut self,
        registry: Arc<zensight_sensor_core::AdvancedPublisherRegistry>,
        refresh_cycles: u32,
    ) {
        self.evidence = Some((registry, refresh_cycles.max(1)));
    }

    /// Attach loaded SMI MIBs (#532).
    pub fn with_smi(&mut self, smi: Arc<crate::smi::SmiResolver>) {
        self.smi = Some(smi);
    }

    /// Enable profile-driven polling (#531): on the first cycle that reads
    /// sysObjectID, the matching profiles' OID sets extend the configured
    /// ones.
    pub fn with_profiles(&mut self, profiles: Arc<crate::profile::ProfileSet>) {
        self.profiles = Some(profiles);
    }

    /// Swap the OID→name resolver (tests compose custom + profile tables).
    pub fn set_resolver(&mut self, resolver: Arc<MibResolver>) {
        self.mib_resolver = resolver;
    }

    /// Publish the joined `InterfaceTable` state doc each cycle (#529).
    pub fn with_interfaces_doc(
        &mut self,
        registry: Arc<zensight_sensor_core::AdvancedPublisherRegistry>,
    ) {
        let key =
            zensight_sensor_core::v1::V1Context::for_producer(&zensight_common::PROFILE, "snmp")
                .state_key(&[&self.device.name, "interfaces"])
                .into();
        self.interfaces_doc = Some((registry, key));
    }

    /// Attach threshold alerting (#528). When the interface rules are on,
    /// the IF-MIB columns they read are added to the walk set unless an
    /// existing walk already covers them.
    pub fn with_alerts(&mut self, evaluator: crate::alerts::AlertEvaluator) {
        if evaluator.wants_interface_columns() {
            for column in crate::alerts::INTERFACE_RULE_COLUMNS {
                let covered = self.walks.iter().any(|w| {
                    column == w
                        || column
                            .strip_prefix(w.as_str())
                            .is_some_and(|r| r.starts_with('.'))
                });
                if !covered {
                    self.walks.push(column.to_string());
                }
            }
        }
        self.alerts = Some(tokio::sync::Mutex::new(evaluator));
    }

    /// Build the persistent SNMP client for this device.
    pub async fn init(&mut self) -> Result<()> {
        let client = self.build_client(true).await?;
        *self.client.get_mut() = Some(client);

        tracing::info!(
            device = %self.device.name,
            version = ?self.device.version,
            timeout_secs = self.device.timeout_secs,
            retries = self.device.retries,
            "SNMP client initialized"
        );
        Ok(())
    }

    /// `seed_engine`: honor a configured v3 `engine_id`. Off on rebuilds —
    /// a rebuild means the device's engine identity looks changed, so a
    /// stale configured id must not short-circuit rediscovery.
    async fn build_client(&self, seed_engine: bool) -> Result<Client<UdpHandle>> {
        let auth = build_auth(&self.device)?;

        let mut builder = Client::builder(self.device.address.as_str(), auth)
            .timeout(Duration::from_secs(self.device.timeout_secs))
            // Each attempt already waits out the full request timeout, so
            // retransmit immediately (classic SNMP retry behavior).
            .retry(Retry::fixed(self.device.retries, Duration::ZERO))
            .max_repetitions(self.device.max_repetitions);

        if seed_engine && let Some(cache) = seeded_engine_cache(&self.device) {
            builder = builder.engine_cache(cache);
        }

        builder
            .connect()
            .await
            .with_context(|| format!("Failed to create SNMP client for {}", self.device.address))
    }

    /// Cheap handle to the current client (`Client` is internally shared).
    async fn client(&self) -> Result<Client<UdpHandle>> {
        self.client
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow!("SNMP client not initialized"))
    }

    /// Replace the client, dropping all cached v3 engine state — the recovery
    /// path when a polled device comes back with a new engine identity (agent
    /// replaced/reset), which the client itself cannot resynchronize from.
    async fn rebuild_client(&self) {
        match self.build_client(false).await {
            Ok(client) => {
                *self.client.write().await = Some(client);
                tracing::info!(device = %self.device.name, "SNMP client rebuilt");
            }
            Err(e) => {
                tracing::warn!(device = %self.device.name, error = %e, "Failed to rebuild SNMP client");
            }
        }
    }

    /// Run the polling loop (#539): randomized initial phase, ±jitter per
    /// cycle, exponential backoff while the device fails, probe-only cycles
    /// while the breaker is open.
    pub async fn run(self) {
        use rand::Rng;

        let base = Duration::from_secs(self.device.poll_interval_secs.max(1));

        tracing::info!(
            device = %self.device.name,
            address = %self.device.address,
            interval_secs = self.device.poll_interval_secs,
            oids = self.oids.len(),
            walks = self.walks.len(),
            "Starting SNMP poller"
        );

        // De-synchronize device start phases across the fleet.
        let phase = rand::rng().random_range(0.0..1.0f64);
        tokio::time::sleep(base.mul_f64(phase)).await;

        loop {
            self.cycle().await;

            let backoff = self.backoff_multiplier();
            let jitter_span = f64::from(self.resilience.jitter_percent) / 100.0;
            let jitter = 1.0 + rand::rng().random_range(-jitter_span..=jitter_span);
            let delay = base.mul_f64(f64::from(backoff) * jitter.max(0.1));
            if backoff > 1 {
                tracing::debug!(
                    device = %self.device.name,
                    backoff,
                    delay_secs = delay.as_secs_f64(),
                    "backing off"
                );
            }
            tokio::time::sleep(delay).await;
        }
    }

    /// Perform a single poll cycle.
    ///
    /// Public so integration tests can drive individual cycles against an
    /// in-process agent without the endless [`run`](Self::run) loop.
    pub async fn poll_once(&self) -> Result<CycleOutcome> {
        let mut requests = 0usize;
        let mut failures = 0usize;
        let mut auth_failures = 0usize;
        let mut transport_failures = 0usize;
        let mut observation = crate::alerts::CycleObservation::default();
        let mut table = crate::interfaces::TableBuilder::new();
        let mut identity = crate::evidence::EvidenceCollector::new();

        // Read sysUpTime up front: it anchors reset detection for every
        // counter this cycle (a reboot must suppress the whole interval).
        let uptime_ticks = self.fetch_uptime_ticks().await;
        let device_reset = self.rate.lock().unwrap().begin_cycle(uptime_ticks);
        if device_reset {
            tracing::info!(
                device = %self.device.name,
                "sysUpTime went backwards — device rebooted; suppressing rates this cycle"
            );
        }
        let mut seen_counters = std::collections::HashSet::new();

        // Profile selection (#531): once, on the first cycle that can read
        // sysObjectID (a device offline at startup selects when it answers).
        self.ensure_profile_selection().await;
        let (oids, walks) = self.effective_sets();

        // Poll individual OIDs with GET
        for oid_str in &oids {
            requests += 1;
            match self.snmp_get(oid_str).await {
                Ok(Some((oid, value))) => {
                    let rate = self.publish(&oid, &value, &mut seen_counters).await;
                    observation.ingest(&oid, &value, rate);
                    table.ingest(&oid, &value, rate);
                    identity.ingest(&oid, &value);
                }
                Ok(None) => {
                    tracing::debug!(device = %self.device.name, oid = %oid_str, "No value returned");
                }
                Err(e) => {
                    failures += 1;
                    auth_failures += usize::from(is_auth_error(&e));
                    transport_failures += usize::from(is_transport_error(&e));
                    tracing::warn!(device = %self.device.name, oid = %oid_str, error = %e, "GET failed");
                }
            }
        }

        // Walk OID subtrees (GETBULK on v2c/v3, GETNEXT on v1)
        for subtree in &walks {
            requests += 1;
            match self.snmp_walk(subtree).await {
                Ok(entries) => {
                    for (oid, value) in entries {
                        let rate = self.publish(&oid, &value, &mut seen_counters).await;
                        observation.ingest(&oid, &value, rate);
                        table.ingest(&oid, &value, rate);
                        identity.ingest(&oid, &value);
                    }
                }
                Err(e) => {
                    failures += 1;
                    auth_failures += usize::from(is_auth_error(&e));
                    transport_failures += usize::from(is_transport_error(&e));
                    tracing::warn!(device = %self.device.name, subtree = %subtree, error = %e, "WALK failed");
                }
            }
        }

        // Drop rate baselines for counters that vanished (removed table rows)
        // — but only after a fully successful cycle, so a transient failure
        // doesn't wipe baselines that just happened to go unobserved.
        if requests > 0 && failures == 0 {
            self.rate.lock().unwrap().retain(&seen_counters);
        }

        // Joined interface state doc (#529): refresh whenever the cycle saw
        // interface rows (LWW; a failed cycle keeps the previous doc).
        if let Some((registry, key)) = &self.interfaces_doc
            && !table.is_empty()
        {
            let doc = table.build(&self.device.name);
            if let Err(e) = registry.publish_serializable(key, &doc).await {
                tracing::warn!(device = %self.device.name, error = %e, "interfaces doc publish failed");
            }
        }

        // Observed-device identity evidence (#537): first successful cycle,
        // then every Nth (claims refresh within the correlator's TTL).
        if let Some((registry, refresh)) = &self.evidence
            && requests > 0
            && failures < requests
        {
            let cycle = self
                .cycles
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if cycle.is_multiple_of(u64::from(*refresh)) && !identity.is_empty() {
                identity.note_polled_address(&self.device.address);
                let claim = identity.build(&self.device.name);
                let key = zensight_common::host_evidence_key("snmp", &self.device.name);
                if let Err(e) = registry.publish_serializable(&key, &claim).await {
                    tracing::warn!(device = %self.device.name, error = %e, "evidence publish failed");
                }
            }
        }

        // Threshold alerting (#528) on what this cycle saw.
        if let Some(evaluator) = &self.alerts {
            observation.all_transport_failed =
                requests > 0 && failures == requests && transport_failures == failures;
            observation.reset_detected = device_reset;
            evaluator.lock().await.tick(&observation).await;
        }

        // A whole v3 cycle failing authentication usually means the device's
        // engine identity changed (agent replaced/reset) — the client cannot
        // resynchronize that itself, so rebuild it to force rediscovery.
        if self.device.version == SnmpVersion::V3 && requests > 0 && auth_failures == requests {
            tracing::warn!(
                device = %self.device.name,
                "all requests failed authentication — rebuilding client to rediscover engine"
            );
            self.rebuild_client().await;
        }

        Ok(CycleOutcome {
            requests,
            failures,
            transport_failures,
        })
    }

    /// One scheduling turn (#539): a full poll normally; only the cheap
    /// sysUpTime probe while the circuit breaker is open. Updates the
    /// consecutive-failure counter and per-device health.
    pub async fn cycle(&self) -> CycleKind {
        use std::sync::atomic::Ordering;

        // A device that failed at startup (or lost its client) keeps
        // retrying with the same backoff policy instead of being dropped.
        if self.client().await.is_err() {
            match self.build_client(true).await {
                Ok(client) => {
                    *self.client.write().await = Some(client);
                    tracing::info!(device = %self.device.name, "SNMP client (re)built");
                }
                Err(e) => {
                    tracing::warn!(device = %self.device.name, error = %e, "SNMP client build failed; backing off");
                    self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
                    self.record_health(false, "client build failed");
                    return CycleKind::Probe { ok: false };
                }
            }
        }

        let started = std::time::Instant::now();
        let breaker_open =
            self.consecutive_failures.load(Ordering::Relaxed) >= self.resilience.breaker_after;

        let kind = if breaker_open {
            // Open: one cheap request instead of the full OID set.
            let ok = self.snmp_get(SYS_UPTIME_OID).await.is_ok();
            if ok {
                // Close the breaker; the next cycle polls fully.
                self.consecutive_failures.store(0, Ordering::Relaxed);
                tracing::info!(device = %self.device.name, "device answered probe; resuming full polling");
            } else {
                self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
            }
            self.record_health(ok, "probe failed");
            CycleKind::Probe { ok }
        } else {
            match self.poll_once().await {
                Ok(outcome) => {
                    if outcome.all_transport_failed() {
                        self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
                    } else {
                        self.consecutive_failures.store(0, Ordering::Relaxed);
                    }
                    self.record_health(
                        !outcome.all_transport_failed(),
                        "all requests failed (transport)",
                    );
                    CycleKind::Full(outcome)
                }
                Err(e) => {
                    tracing::warn!(device = %self.device.name, error = %e, "SNMP poll failed");
                    self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
                    self.record_health(false, "poll cycle error");
                    CycleKind::Probe { ok: false }
                }
            }
        };
        if let Some(health) = &self.health {
            health.record_poll_duration(started.elapsed().as_millis() as u64);
        }
        kind
    }

    fn record_health(&self, ok: bool, error: &str) {
        if let Some(health) = &self.health {
            if ok {
                health.record_device_success(&self.device.name);
            } else {
                health.record_device_failure(&self.device.name, error);
            }
        }
    }

    /// The current backoff multiplier: 2^consecutive-failures, capped.
    pub fn backoff_multiplier(&self) -> u32 {
        backoff_multiplier(
            self.consecutive_failures
                .load(std::sync::atomic::Ordering::Relaxed),
            self.resilience.backoff_cap,
        )
    }

    /// Run profile selection once (#531): read sysObjectID, match, remember.
    async fn ensure_profile_selection(&self) {
        let Some(profiles) = &self.profiles else {
            return;
        };
        if self.selection.lock().unwrap().is_some() {
            return;
        }
        // sysObjectID.0 — selection also proceeds (defaults only) when the
        // device answers but doesn't serve it.
        let sys_object_id = match self.snmp_get("1.3.6.1.2.1.1.2.0").await {
            Ok(Some((_, Value::ObjectIdentifier(oid)))) => Some(oid.to_string()),
            Ok(_) => None,
            Err(e) => {
                tracing::debug!(device = %self.device.name, error = %e, "sysObjectID probe failed — deferring profile selection");
                return;
            }
        };

        match profiles.select(sys_object_id.as_deref(), self.device.profile.as_deref()) {
            Ok(selection) => {
                tracing::info!(
                    device = %self.device.name,
                    sys_object_id = sys_object_id.as_deref().unwrap_or("-"),
                    applied = %selection.applied.join(","),
                    "device profiles applied"
                );
                self.publish_point(
                    "1.3.6.1.2.1.1.2.0",
                    "system/profile",
                    TelemetryValue::Text(selection.applied.join(",")),
                    None,
                    None,
                )
                .await;
                *self.selection.lock().unwrap() = Some(selection);
            }
            Err(e) => {
                // Only a bad pin can fail here; it will not fix itself.
                tracing::error!(device = %self.device.name, error = %e, "profile selection failed");
                *self.selection.lock().unwrap() = Some(crate::profile::Selection::default());
            }
        }
    }

    /// The cycle's polling sets: configured OIDs/walks plus the profile
    /// selection, deduplicated (a walk covered by a broader walk is
    /// dropped).
    fn effective_sets(&self) -> (Vec<String>, Vec<String>) {
        let selection = self.selection.lock().unwrap();
        let extra = selection.as_ref();

        let mut oids = self.oids.clone();
        for oid in extra.map(|s| s.oids.as_slice()).unwrap_or_default() {
            if !oids.contains(oid) {
                oids.push(oid.clone());
            }
        }

        let mut walks = self.walks.clone();
        for walk in extra.map(|s| s.walks.as_slice()).unwrap_or_default() {
            if !walks.contains(walk) {
                walks.push(walk.clone());
            }
        }
        // Drop any walk strictly covered by another (dot-boundary prefix).
        let covered: Vec<bool> = walks
            .iter()
            .map(|w| {
                walks.iter().any(|other| {
                    other != w
                        && w.strip_prefix(other.as_str())
                            .is_some_and(|r| r.starts_with('.'))
                })
            })
            .collect();
        let walks = walks
            .into_iter()
            .zip(covered)
            .filter_map(|(w, c)| (!c).then_some(w))
            .collect();

        (oids, walks)
    }

    /// Read sysUpTime for reset detection; best-effort (None on any failure).
    async fn fetch_uptime_ticks(&self) -> Option<u32> {
        let oid = parse_oid(SYS_UPTIME_OID).ok()?;
        match self.client().await.ok()?.get(&oid).await {
            Ok(varbind) => match varbind.value {
                Value::TimeTicks(ticks) => Some(ticks),
                _ => None,
            },
            Err(e) => {
                tracing::debug!(device = %self.device.name, error = %e, "sysUpTime probe failed");
                None
            }
        }
    }

    /// Perform an SNMP GET operation, returning the wire value.
    async fn snmp_get(&self, oid_str: &str) -> Result<Option<(String, Value)>> {
        let oid = parse_oid(oid_str)?;
        let varbind = self
            .client()
            .await?
            .get(&oid)
            .await
            .context("SNMP GET error")?;

        if matches!(
            varbind.value,
            Value::Null | Value::NoSuchObject | Value::NoSuchInstance | Value::EndOfMibView
        ) {
            return Ok(None);
        }
        let oid_string = oid_to_string(&varbind.oid);
        Ok(Some((oid_string, varbind.value)))
    }

    /// Walk an OID subtree.
    ///
    /// The client picks GETBULK for v2c/v3 and GETNEXT for v1, stops at the
    /// subtree boundary / EndOfMibView, and bisects on tooBig.
    async fn snmp_walk(&self, subtree_str: &str) -> Result<Vec<(String, Value)>> {
        let subtree = parse_oid(subtree_str)?;
        let mut stream = self
            .client()
            .await?
            .walk(subtree)
            .context("SNMP WALK error")?;

        let mut results = Vec::new();
        while let Some(varbind) = stream.next().await {
            let varbind = varbind.context("SNMP WALK error")?;
            let oid_string = oid_to_string(&varbind.oid);
            results.push((oid_string, varbind.value));
        }
        Ok(results)
    }

    /// Publish the raw point for a polled value and, for counters, a derived
    /// `<metric>.rate` sibling (per-second Gauge) when the tracker has a
    /// plausible previous sample.
    async fn publish(
        &self,
        oid_str: &str,
        value: &Value,
        seen_counters: &mut std::collections::HashSet<String>,
    ) -> Option<f64> {
        // Naming: explicit tables (builtins/config/profiles) first; loaded
        // SMI MIBs fill the gaps; unresolvable stays the dotted OID (#532).
        let mut metric_name = self.mib_resolver.resolve(oid_str);
        if metric_name == oid_str
            && let Some(name) = self.smi.as_ref().and_then(|s| s.metric_name(oid_str))
        {
            metric_name = name;
        }
        let syntax = self
            .mib_resolver
            .syntax(oid_str)
            .map(str::to_string)
            .or_else(|| {
                self.smi
                    .as_ref()
                    .and_then(|s| s.syntax(oid_str))
                    .map(str::to_string)
            });
        let syntax = syntax.as_deref();

        let (telemetry_value, unit) = snmp_value_to_telemetry(value)?;
        // Unit: wire-derived (TimeTicks seconds) wins; else the UNITS clause.
        let smi_unit = if unit.is_none() {
            self.smi.as_ref().and_then(|s| s.unit(oid_str))
        } else {
            None
        };
        let unit = unit.map(str::to_string).or(smi_unit);
        // Enum decode: named INTEGER values ride an `enum` label (#532).
        let enum_label = match value {
            Value::Integer(n) => self
                .smi
                .as_ref()
                .and_then(|s| s.enum_label(oid_str, i64::from(*n))),
            _ => None,
        };
        self.publish_point(
            oid_str,
            &metric_name,
            telemetry_value,
            unit.as_deref(),
            enum_label,
        )
        .await;

        // Counter → rate. The wire tag decides; MIB SYNTAX backs it up for
        // agents that mis-tag counters as Gauge32/Unsigned32.
        let counter = match value {
            Value::Counter32(n) => Some((u64::from(*n), true)),
            Value::Counter64(n) => Some((*n, false)),
            Value::Gauge32(n) | Value::UInteger32(n)
                if matches!(syntax, Some("Counter32" | "Counter64")) =>
            {
                Some((u64::from(*n), syntax == Some("Counter32")))
            }
            _ => None,
        };
        let (counter_value, is_32bit) = counter?;

        seen_counters.insert(oid_str.to_string());
        let rate = self.rate.lock().unwrap().observe(
            oid_str,
            counter_value,
            is_32bit,
            std::time::Instant::now(),
        );
        if let Some(rate) = rate {
            let rate_metric = format!("{metric_name}.rate");
            let unit = rate_unit_for(&metric_name);
            self.publish_point(
                oid_str,
                &rate_metric,
                TelemetryValue::Gauge(rate),
                Some(unit),
                None,
            )
            .await;
        }
        rate
    }

    async fn publish_point(
        &self,
        oid_str: &str,
        metric_name: &str,
        value: TelemetryValue,
        unit: Option<&str>,
        enum_label: Option<String>,
    ) {
        let mut point = TelemetryPoint::new(&self.device.name, Protocol::Snmp, metric_name, value)
            .with_label("oid", oid_str);
        if let Some(unit) = unit {
            point = point.with_unit(unit);
        }
        if let Some(label) = enum_label {
            point = point.with_label("enum", label);
        }

        let key = format!(
            "{}/{}/{}",
            self.telemetry_prefix, self.device.name, metric_name
        );

        match encode(&point, self.format) {
            Ok(payload) => {
                if let Err(e) = self
                    .registry
                    .put(&key, payload, zensight_common::QosClass::Telemetry)
                    .await
                {
                    tracing::error!(key = %key, error = %e, "Failed to publish to Zenoh");
                } else {
                    tracing::trace!(key = %key, "Published telemetry");
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to encode telemetry");
            }
        }
    }
}

/// Unit for a derived rate metric: octet counters are bytes/second,
/// everything else (packets, errors, discards) counts per second.
fn rate_unit_for(metric_name: &str) -> &'static str {
    let last = metric_name.rsplit('/').next().unwrap_or(metric_name);
    if last.to_ascii_lowercase().contains("octets") {
        "By/s"
    } else {
        "1/s"
    }
}

/// 2^failures, saturating, capped at `cap` (>=1).
fn backoff_multiplier(consecutive_failures: u32, cap: u32) -> u32 {
    let cap = cap.max(1);
    1u32.checked_shl(consecutive_failures)
        .unwrap_or(cap)
        .min(cap)
}

/// Whether an error from the SNMP client is an authentication failure
/// (wrong credentials, engine identity mismatch, time-window rejection).
fn is_auth_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<Box<async_snmp::Error>>()
        .is_some_and(|e| matches!(e.as_ref(), async_snmp::Error::Auth { .. }))
}

/// Whether an error is transport-level (device not answering at all) as
/// opposed to an SNMP-level reply — feeds the device-unreachable rule.
fn is_transport_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<Box<async_snmp::Error>>()
        .is_some_and(|e| {
            matches!(
                e.as_ref(),
                async_snmp::Error::Timeout { .. }
                    | async_snmp::Error::Network { .. }
                    | async_snmp::Error::Closed { .. }
            )
        })
}

/// Convert an SNMP wire value to a `TelemetryValue` + optional unit (#527):
/// Gauge32/Unsigned32 are gauges (the pre-#527 code published them as
/// `Counter`), and TimeTicks converts from centiseconds to a `Gauge` in
/// seconds so consumers render durations without special-casing.
fn snmp_value_to_telemetry(value: &Value) -> Option<(TelemetryValue, Option<&'static str>)> {
    match value {
        Value::Integer(n) => Some((TelemetryValue::Gauge(f64::from(*n)), None)),
        Value::OctetString(s) => {
            // Try to interpret as UTF-8 string, fall back to binary
            match String::from_utf8(s.to_vec()) {
                Ok(text)
                    if text
                        .chars()
                        .all(|c| !c.is_control() || c == '\n' || c == '\t') =>
                {
                    Some((TelemetryValue::Text(text), None))
                }
                _ => Some((TelemetryValue::Binary(s.to_vec()), None)),
            }
        }
        Value::ObjectIdentifier(oid) => Some((TelemetryValue::Text(oid_to_string(oid)), None)),
        Value::IpAddress(ip) => Some((
            TelemetryValue::Text(format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])),
            None,
        )),
        Value::Counter32(n) => Some((TelemetryValue::Counter(u64::from(*n)), None)),
        Value::Gauge32(n) | Value::UInteger32(n) => {
            Some((TelemetryValue::Gauge(f64::from(*n)), None))
        }
        Value::TimeTicks(n) => Some((TelemetryValue::Gauge(f64::from(*n) / 100.0), Some("s"))),
        Value::Counter64(n) => Some((TelemetryValue::Counter(*n), None)),
        _ => None,
    }
}

/// Build client authentication from device configuration, preserving the
/// pre-migration validation errors.
fn build_auth(device: &DeviceConfig) -> Result<Auth> {
    match device.version {
        SnmpVersion::V1 => Ok(Auth::v1(device.community.clone())),
        SnmpVersion::V2c => Ok(Auth::v2c(device.community.clone())),
        SnmpVersion::V3 => {
            let config = device
                .security
                .as_ref()
                .ok_or_else(|| anyhow!("SNMPv3 requires security configuration"))?;

            let auth_protocol = match config.auth_protocol {
                AuthProtocol::None => None,
                AuthProtocol::Md5 => Some(async_snmp::AuthProtocol::Md5),
                AuthProtocol::Sha1 => Some(async_snmp::AuthProtocol::Sha1),
                AuthProtocol::Sha224 => Some(async_snmp::AuthProtocol::Sha224),
                AuthProtocol::Sha256 => Some(async_snmp::AuthProtocol::Sha256),
                AuthProtocol::Sha384 => Some(async_snmp::AuthProtocol::Sha384),
                AuthProtocol::Sha512 => Some(async_snmp::AuthProtocol::Sha512),
            };

            let mut usm = Auth::usm(config.username.clone());
            match (auth_protocol, config.priv_protocol) {
                // noAuthNoPriv
                (None, PrivProtocol::None) => {}
                // noAuthPriv is not valid in SNMPv3
                (None, _) => {
                    return Err(anyhow!("Privacy requires authentication in SNMPv3"));
                }
                (Some(auth_proto), priv_proto) => {
                    let auth_password = config.auth_password.as_ref().ok_or_else(|| {
                        anyhow!("Authentication password required for auth protocol")
                    })?;
                    usm = usm.auth(auth_proto, auth_password.clone());

                    if priv_proto != PrivProtocol::None {
                        let priv_password = config.priv_password.as_ref().ok_or_else(|| {
                            anyhow!("Privacy password required for privacy protocol")
                        })?;
                        let cipher = match priv_proto {
                            PrivProtocol::None => unreachable!("guarded above"),
                            PrivProtocol::Des => async_snmp::PrivProtocol::Des,
                            PrivProtocol::Aes128 => async_snmp::PrivProtocol::Aes128,
                            PrivProtocol::Aes192 => async_snmp::PrivProtocol::Aes192,
                            PrivProtocol::Aes256 => async_snmp::PrivProtocol::Aes256,
                        };
                        usm = usm.privacy(cipher, priv_password.clone());
                    }
                }
            }
            Ok(usm.into())
        }
    }
}

/// Pre-seed an engine cache with a configured engine ID (hex), skipping the
/// discovery round-trip. Boots/time start at zero — the first authenticated
/// exchange time-syncs through the standard report flow.
///
/// The cache is keyed by socket address, so this only works when `address`
/// is a literal `ip:port`; hostnames fall back to auto-discovery.
fn seeded_engine_cache(device: &DeviceConfig) -> Option<Arc<EngineCache>> {
    let hex = device.security.as_ref()?.engine_id.as_ref()?;

    let Some(engine_id) = parse_hex(hex) else {
        tracing::warn!(
            device = %device.name,
            engine_id = %hex,
            "Configured engine_id is not valid hex — falling back to discovery"
        );
        return None;
    };
    let Ok(target) = device.address.parse::<std::net::SocketAddr>() else {
        tracing::warn!(
            device = %device.name,
            address = %device.address,
            "Configured engine_id needs a literal ip:port address — falling back to discovery"
        );
        return None;
    };

    let cache = Arc::new(EngineCache::new());
    cache.insert(target, EngineState::new(Bytes::from(engine_id), 0, 0));
    Some(cache)
}

/// Decode a hex string, tolerating an `0x` prefix and `:` separators.
fn parse_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.strip_prefix("0x").unwrap_or(s).replace(':', "");
    if s.is_empty() || !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_the_cap() {
        assert_eq!(backoff_multiplier(0, 10), 1);
        assert_eq!(backoff_multiplier(1, 10), 2);
        assert_eq!(backoff_multiplier(2, 10), 4);
        assert_eq!(backoff_multiplier(3, 10), 8);
        assert_eq!(backoff_multiplier(4, 10), 10); // capped
        assert_eq!(backoff_multiplier(63, 10), 10); // saturates, no overflow
        assert_eq!(backoff_multiplier(40, 0), 1); // cap floor
    }

    #[test]
    fn hex_parsing() {
        assert_eq!(parse_hex("80000001"), Some(vec![0x80, 0x00, 0x00, 0x01]));
        assert_eq!(parse_hex("0x8000"), Some(vec![0x80, 0x00]));
        assert_eq!(parse_hex("80:00:00:01"), Some(vec![0x80, 0x00, 0x00, 0x01]));
        assert_eq!(parse_hex("8"), None);
        assert_eq!(parse_hex("zz"), None);
        assert_eq!(parse_hex(""), None);
    }
}
