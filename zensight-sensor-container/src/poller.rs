//! The poll loop (#819).
//!
//! Each cycle: list the runtime's containers, inspect each one, join the
//! kernel's cgroup view onto it, publish, and grade. The counters that matter
//! — restarts and OOM kills — are cumulative, so the previous cycle's values
//! are kept as a baseline and the *rate* rules read the delta.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use zensight_common::container::{ContainerInfo, HealthState, SignatureState};
use zensight_common::relation::{EndpointClaim, RelationKind, RelationshipEvidence};
use zensight_common::{HostEvidence, QosClass, TelemetryValue};
use zensight_sensor_core::{AdvancedPublisherRegistry, AlertReporter, Publisher, SensorHealth};

use crate::alerts::{self, Observation};
use crate::config::ContainerConfig;
use crate::runtime::RuntimeClient;
use crate::telemetry_guard::checked_point;
use crate::upstream::UpstreamChecker;

pub const STATE_QOS: QosClass = QosClass::HealthLiveness;

pub struct Poller {
    clients: Vec<Arc<RuntimeClient>>,
    cfg: ContainerConfig,
    source: String,
    cgroup_root: PathBuf,
    publisher: Publisher,
    states: Arc<AdvancedPublisherRegistry>,
    evidence: Option<Arc<AdvancedPublisherRegistry>>,
    reporter: Option<Arc<AlertReporter>>,
    health: Arc<SensorHealth>,
    upstream: Option<UpstreamChecker>,
    /// `name -> (restart_count, oom_kills)` from the previous cycle — except
    /// that the OOM half is held still for `alerts.oom_hold_secs` once a
    /// burst begins (see `oom_burst_since`), so the delta rule sees the
    /// kill on more than the one sweep it happened in.
    baseline: HashMap<String, (u64, u64)>,
    baseline_at: Option<Instant>,
    /// `name -> when the current burst of new OOM kills was first seen`.
    /// Present only while a burst is being held against the baseline.
    oom_burst_since: HashMap<String, Instant>,
    /// Resolved upstream digests, refreshed on their own slow cadence because
    /// registries rate-limit and the answer changes on a release cadence.
    upstream_cache: HashMap<String, (Option<String>, Option<bool>)>,
    upstream_at: Option<Instant>,
    /// The `Runs` claims published last cycle (#916), so a container that
    /// disappears is *retired* rather than left to age out of the map over the
    /// family's fifteen-minute TTL — which is exactly the window in which an
    /// operator looks at the topology after a migration or a redeploy.
    relations: zensight_sensor_core::relation::RelationSet,
}

impl Poller {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        clients: Vec<Arc<RuntimeClient>>,
        cfg: ContainerConfig,
        source: String,
        publisher: Publisher,
        states: Arc<AdvancedPublisherRegistry>,
        evidence: Option<Arc<AdvancedPublisherRegistry>>,
        reporter: Option<Arc<AlertReporter>>,
        health: Arc<SensorHealth>,
        upstream: Option<UpstreamChecker>,
        relations: zensight_sensor_core::relation::RelationSet,
    ) -> Self {
        let cgroup_root = PathBuf::from(&cfg.cgroup_root);
        Self {
            clients,
            cfg,
            source,
            cgroup_root,
            publisher,
            states,
            evidence,
            reporter,
            health,
            upstream,
            baseline: HashMap::new(),
            baseline_at: None,
            oom_burst_since: HashMap::new(),
            upstream_cache: HashMap::new(),
            upstream_at: None,
            relations,
        }
    }

    pub async fn run(mut self) {
        let mut tick = tokio::time::interval(Duration::from_secs(self.cfg.poll_interval_secs));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let started = Instant::now();
            match self.sweep().await {
                Ok(cs) => {
                    self.publish(&cs).await;
                    self.health
                        .record_poll_duration(started.elapsed().as_millis() as u64);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "container: poll cycle failed");
                    self.health.record_device_failure(&self.source, &e);
                }
            }
        }
    }

    /// One observation across every configured runtime socket.
    pub async fn sweep(&mut self) -> Result<Vec<ContainerInfo>, String> {
        let mut out: Vec<ContainerInfo> = Vec::new();
        let mut reachable = 0usize;
        let mut errors = Vec::new();

        for client in &self.clients {
            let list = match client.list().await {
                Ok(l) => {
                    reachable += 1;
                    l
                }
                Err(e) => {
                    errors.push(format!("{}: {e}", client.socket().display()));
                    continue;
                }
            };
            for entry in list {
                let Some(id) = entry.get("Id").and_then(|v| v.as_str()) else {
                    continue;
                };
                let inspect = match client.inspect(id).await {
                    Ok(i) => i,
                    Err(e) => {
                        // One container's inspect failing is that container's
                        // problem, not the socket's; the rest still publish.
                        tracing::debug!(id = %id, error = %e, "container: inspect failed");
                        continue;
                    }
                };
                // libpod returns a single object; the Docker compatibility API
                // sometimes wraps it in a one-element array.
                let inspect = match inspect {
                    serde_json::Value::Array(mut a) if !a.is_empty() => a.remove(0),
                    other => other,
                };
                let Some(mut info) = crate::inspect::build(&entry, &inspect, client.rootless)
                else {
                    continue;
                };
                if self.cfg.ignore.contains(&info.name) {
                    continue;
                }
                if let Some(dir) =
                    crate::cgroup::resolve(&self.cgroup_root, info.cgroup_path.as_deref(), &info.id)
                {
                    info.resources = crate::cgroup::read_resources(&dir);
                }
                self.health.record_device_success(&info.name);
                out.push(info);
            }
        }

        // Every socket unreachable is a real failure. Some unreachable is not:
        // a host with rootful podman and no rootless session is the norm, and
        // grading that unhealthy would make the common case look broken.
        if reachable == 0 {
            return Err(if errors.is_empty() {
                "no container runtime socket configured".to_string()
            } else {
                errors.join("; ")
            });
        }
        if !errors.is_empty() {
            tracing::debug!(errors = ?errors, "container: some sockets unreachable");
        }

        self.refresh_upstream(&mut out).await;
        out.sort_by(|a, b| a.name.cmp(&b.name));
        self.health.set_devices_total(out.len() as u64);
        self.health.record_device_success(&self.source);
        Ok(out)
    }

    /// Fold in the upstream digest and signature answers, on their own slow
    /// cadence. Entirely absent unless the egressing collector is on.
    async fn refresh_upstream(&mut self, containers: &mut [ContainerInfo]) {
        let Some(checker) = &self.upstream else {
            return;
        };
        let due = self
            .upstream_at
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(self.cfg.upstream.interval_secs));
        if due {
            let mut cache = HashMap::new();
            for c in containers.iter() {
                if cache.contains_key(&c.image.reference) {
                    continue;
                }
                let digest = checker.digest_for(&c.image.reference).await;
                let signed = match c.image.digest.as_deref() {
                    Some(d) => checker.signature_present(&c.image.reference, d).await,
                    None => None,
                };
                cache.insert(c.image.reference.clone(), (digest, signed));
            }
            self.upstream_cache = cache;
            self.upstream_at = Some(Instant::now());
        }
        for c in containers.iter_mut() {
            if let Some((digest, signed)) = self.upstream_cache.get(&c.image.reference) {
                c.image.upstream_digest = digest.clone();
                c.image.signature = match signed {
                    Some(true) => SignatureState::Present,
                    Some(false) => SignatureState::Absent,
                    None => SignatureState::NotChecked,
                };
            }
        }
    }

    pub async fn publish(&mut self, containers: &[ContainerInfo]) {
        let mut published = 0u64;
        let now_ms = zensight_common::current_timestamp_millis();

        for c in containers {
            // Operator-facing and foreign, so it is slugged before it can
            // reach a key — the #843 boundary.
            let slug = zenkey::Chunk::slug(&c.name).to_string();
            let mut labels = HashMap::new();
            labels.insert("container".to_string(), c.name.clone());
            labels.insert("image".to_string(), c.image.reference.clone());
            if let Some(u) = &c.unit {
                labels.insert("unit".to_string(), u.clone());
            }

            let r = &c.resources;
            let mut points: Vec<(String, f64)> = vec![
                (
                    format!("{slug}/running"),
                    if c.is_running() { 1.0 } else { 0.0 },
                ),
                (format!("{slug}/restart_count"), c.restart_count as f64),
            ];
            for (suffix, v) in [
                ("memory_bytes", r.memory_bytes.map(|v| v as f64)),
                ("memory_max_bytes", r.memory_max_bytes.map(|v| v as f64)),
                ("memory_peak_bytes", r.memory_peak_bytes.map(|v| v as f64)),
                ("memory_ratio", c.memory_ratio()),
                ("cpu_usage_usec_total", r.cpu_usage_usec.map(|v| v as f64)),
                (
                    "cpu_throttled_usec_total",
                    r.cpu_throttled_usec.map(|v| v as f64),
                ),
                ("oom_kills_total", r.oom_kills.map(|v| v as f64)),
                (
                    "memory_max_events_total",
                    r.memory_max_events.map(|v| v as f64),
                ),
                ("cpu_pressure_avg10", r.cpu_pressure_avg10),
                ("memory_pressure_avg10", r.memory_pressure_avg10),
                ("io_pressure_avg10", r.io_pressure_avg10),
                ("pids", r.pids.map(|v| v as f64)),
                ("exit_code", c.exit_code.map(|v| v as f64)),
                (
                    "uptime_secs",
                    c.started_at
                        .filter(|_| c.is_running())
                        .map(|s| ((now_ms / 1000) - s).max(0) as f64),
                ),
            ] {
                if let Some(v) = v {
                    points.push((format!("{slug}/{suffix}"), v));
                }
            }
            // Not published when there is no healthcheck or none has ever run:
            // a 0 would say "this service is failing", which is precisely the
            // wrong thing to say about a container whose PROBE is broken.
            match c.health {
                HealthState::Healthy => points.push((format!("{slug}/healthy"), 1.0)),
                HealthState::Unhealthy => points.push((format!("{slug}/healthy"), 0.0)),
                _ => {}
            }
            if c.image.upstream_digest.is_some() {
                points.push((
                    format!("{slug}/image_behind_upstream"),
                    if c.image.is_behind_upstream() {
                        1.0
                    } else {
                        0.0
                    },
                ));
            }

            for (metric, value) in points {
                // `source` is the host running the container, never the
                // container (#883/#884). A container's memory comes from this
                // host's cgroup tree; its name is unique per host, not
                // globally, so filing the series under it made four machines
                // running `zensight-sensor-logs` collide on one identity.
                let p = checked_point(&self.source, &metric, TelemetryValue::Gauge(value))
                    .with_labels(labels.clone());
                if self.publisher.publish(&metric, &p).await.is_ok() {
                    published += 1;
                }
            }

            if let Some(key) = state_key(&["container", &slug])
                && let Err(e) = self.states.publish_serializable(&key, c).await
            {
                tracing::warn!(container = %c.name, error = %e, "container: doc publish failed");
            }

            if let Some(reg) = &self.evidence
                && let Some(claim) = evidence(c)
                && let Some(key) = state_key(&["evidence", "device", &slug])
                && let Err(e) = reg.publish_serializable(&key, &claim).await
            {
                tracing::debug!(container = %c.name, error = %e, "container: evidence publish failed");
            }
        }

        // One `Runs` claim per container, published as the complete current
        // set: `sync` retires whatever it stopped seeing in the same pass, so
        // a removed container leaves the map now rather than in fifteen
        // minutes' time.
        {
            let host_id = zensight_sensor_core::v1::host_id().as_str().to_string();
            let now_ms = zensight_common::current_timestamp_millis();
            let claims: Vec<RelationshipEvidence> = containers
                .iter()
                .filter(|c| c.is_running())
                .map(|c| relation(&host_id, c, now_ms))
                .collect();
            let out = self.relations.sync(&claims).await;
            if out.retired > 0 || out.failed > 0 || out.dropped > 0 {
                tracing::debug!(
                    published = out.published,
                    retired = out.retired,
                    dropped = out.dropped,
                    failed = out.failed,
                    "container: relation evidence sync"
                );
            }
        }

        for (metric, value) in [
            ("containers/total", containers.len() as f64),
            (
                "containers/running",
                containers.iter().filter(|c| c.is_running()).count() as f64,
            ),
            (
                "containers/unhealthy",
                containers
                    .iter()
                    .filter(|c| c.health == HealthState::Unhealthy)
                    .count() as f64,
            ),
        ] {
            let p = checked_point(&self.source, metric, TelemetryValue::Gauge(value));
            if self.publisher.publish(metric, &p).await.is_ok() {
                published += 1;
            }
        }
        self.health.record_metrics_published(published);

        if let Some(reporter) = &self.reporter {
            let age = self.baseline_at.map_or(u64::MAX, |t| t.elapsed().as_secs());
            let firing = alerts::grade(
                &self.cfg.alerts,
                &Observation {
                    source: &self.source,
                    containers,
                    baseline: &self.baseline,
                    baseline_age_secs: age,
                },
            );
            let mut by_rule: HashMap<String, Vec<String>> = HashMap::new();
            for a in &firing {
                by_rule
                    .entry(a.rule.clone())
                    .or_default()
                    .push(a.alert_key());
            }
            for a in firing {
                if let Err(e) = reporter.observe(a, None).await {
                    tracing::warn!(error = %e, "container: alert publish failed");
                }
            }
            for rule in alerts::ALL_RULES {
                let still = by_rule.remove(*rule).unwrap_or_default();
                if let Err(e) = reporter.reconcile(rule, &still).await {
                    tracing::warn!(rule = %rule, error = %e, "container: reconcile failed");
                }
            }
        }

        // The baseline advances only after grading, so the delta rules always
        // compare against the PREVIOUS cycle rather than this one.
        let now = Instant::now();
        let hold = Duration::from_secs(self.cfg.alerts.oom_hold_secs);
        let mut next = HashMap::with_capacity(containers.len());
        for c in containers {
            let kills = c.resources.oom_kills.unwrap_or(0);
            let prev_kills = self.baseline.get(&c.name).map(|(_, k)| *k);
            let burst = self.oom_burst_since.get(&c.name).copied();
            let (held, burst) = next_oom_baseline(prev_kills, kills, burst, now, hold);
            match burst {
                Some(since) => {
                    self.oom_burst_since.insert(c.name.clone(), since);
                }
                None => {
                    self.oom_burst_since.remove(&c.name);
                }
            }
            next.insert(c.name.clone(), (c.restart_count, held));
        }
        self.oom_burst_since
            .retain(|name, _| next.contains_key(name));
        self.baseline = next;
        self.baseline_at = Some(now);
    }
}

fn state_key(chunks: &[&str]) -> Option<String> {
    match zensight_sensor_core::v1::for_producer("container").state_key(chunks) {
        Ok(k) => Some(k.into()),
        Err(e) => {
            tracing::warn!(chunks = ?chunks, error = %e, "container: not a legal state subject");
            None
        }
    }
}

/// A `Runs` claim: this host runs this container (#916).
///
/// `from` is a **self-claim by `host_id`** — the strongest end available, and
/// the reason the catalog can resolve this edge to a real entity rather than
/// an `External` node. `to` is the observed-device slug plus whatever IPs were
/// seen, deliberately the same vocabulary as the `evidence/device/{device}`
/// claim published beside it: a relation claim and an identity claim about one
/// container then join on the catalog's side without a second naming scheme.
///
/// The owning systemd unit rides along as an attr rather than a second edge.
/// It is a property of *this* containment ("podman started it for
/// caddy.service"), not an independent relationship, and modelling it as an
/// edge would double the family's cardinality to say something a tooltip
/// renders.
fn relation(host_id: &str, c: &ContainerInfo, now_ms: i64) -> RelationshipEvidence {
    let mut attrs = BTreeMap::new();
    if let Some(unit) = &c.unit {
        attrs.insert("unit".to_string(), unit.clone());
    }
    RelationshipEvidence {
        sensor: "container".to_string(),
        source: c.name.clone(),
        kind: RelationKind::Runs,
        from: EndpointClaim::host(host_id),
        to: EndpointClaim {
            device: Some(zenkey::Chunk::slug(&c.name).to_string()),
            ips: c.ips.clone(),
            name: Some(c.name.clone()),
            ..Default::default()
        },
        attrs,
        last_updated: now_ms,
    }
}

/// A third-party identity claim about a container.
///
/// The point is the merge: netlink already surfaces the podman bridges'
/// containers as wire-only entities with IPs and nothing else. With this,
/// those rows join the container that owns them instead of floating in the
/// catalog. `host_id` stays `None` — the observed-device precedent; a
/// synthetic hash would carry no merge power and would masquerade as the
/// hashed-machine-id contract.
fn evidence(c: &ContainerInfo) -> Option<HostEvidence> {
    if c.ips.is_empty() {
        return None;
    }
    Some(HostEvidence {
        sensor: "container".to_string(),
        source: c.name.clone(),
        observer: Some("container".to_string()),
        host_id: None,
        boot_id: None,
        hostname: Some(c.name.clone()),
        fqdn: None,
        ips: c.ips.clone(),
        macs: Vec::new(),
        vendor: None,
        platform: Some(c.image.reference.clone()),
        // The container the *reporting process* runs in — not this one. The
        // field is a host-scoped qualifier about the observer, and putting the
        // observed container's id here would make it look like a merge key,
        // which container ids emphatically are not.
        container_id: None,
        cloud: None,
        last_updated: zensight_common::current_timestamp_millis(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zensight_common::container::{ContainerImage, ContainerResources};

    fn c(name: &str, ips: Vec<String>) -> ContainerInfo {
        ContainerInfo {
            id: "abc".into(),
            name: name.into(),
            status: "running".into(),
            image: ContainerImage {
                reference: "img:1".into(),
                digest: None,
                upstream_digest: None,
                signature: SignatureState::NotChecked,
            },
            created_at: None,
            started_at: None,
            restart_count: 0,
            exit_code: None,
            health: HealthState::None,
            health_failing_streak: None,
            unit: None,
            restart_policy: None,
            rootless: false,
            ports: vec![],
            mounts: vec![],
            cgroup_path: None,
            resources: ContainerResources::default(),
            ips,
            observed_at_ms: 0,
        }
    }

    #[test]
    fn evidence_carries_ips_and_never_a_synthetic_host_id() {
        let e = evidence(&c("caddy", vec!["10.89.0.5".into()])).unwrap();
        assert_eq!(e.observer.as_deref(), Some("container"));
        assert_eq!(e.ips, vec!["10.89.0.5"]);
        assert!(e.host_id.is_none());
        assert!(
            e.container_id.is_none(),
            "container ids are per-host and are not merge keys"
        );
    }

    #[test]
    fn a_container_with_no_ip_makes_no_claim() {
        assert!(evidence(&c("caddy", vec![])).is_none());
    }
}

/// The OOM half of the next baseline, and the burst marker to keep.
///
/// Restarts advance every cycle. The OOM baseline is HELD while a burst of
/// new kills is younger than `hold`: a kill is a one-sweep event against a
/// cumulative counter, and an alert with a `for:` window needs to see the
/// condition on more than one sweep. Advancing every cycle made it true for
/// exactly one poll — never long enough — so `container-oom-killed` could not
/// fire at all with the shipped cadences. Once the burst is older than `hold`
/// the baseline catches up and the alert resolves on the next sweep.
fn next_oom_baseline(
    prev: Option<u64>,
    kills: u64,
    burst_since: Option<Instant>,
    now: Instant,
    hold: Duration,
) -> (u64, Option<Instant>) {
    match prev {
        Some(prev) if kills > prev => {
            let since = burst_since.unwrap_or(now);
            if now.duration_since(since) < hold {
                (prev, Some(since))
            } else {
                (kills, None)
            }
        }
        _ => (kills, None),
    }
}

#[cfg(test)]
mod oom_hold_tests {
    use super::*;

    /// With the shipped 30 s poll and 60 s `for_secs`, a kill seen for one
    /// sweep only could never fire. The baseline must stay put for the hold
    /// window and then catch up, so the alert fires AND later resolves.
    #[test]
    fn a_burst_of_oom_kills_holds_the_baseline_for_the_window() {
        let t0 = Instant::now();
        let hold = Duration::from_secs(600);

        // First sweep after the kill: burst begins, baseline held at 3.
        let (held, burst) = next_oom_baseline(Some(3), 4, None, t0, hold);
        assert_eq!(held, 3);
        assert_eq!(burst, Some(t0));

        // Every sweep inside the window: still held, burst start unchanged.
        let t1 = t0 + Duration::from_secs(90);
        assert_eq!(
            next_oom_baseline(Some(3), 4, burst, t1, hold),
            (3, Some(t0))
        );
        // A second kill inside the window extends nothing; the delta grows.
        assert_eq!(
            next_oom_baseline(Some(3), 5, burst, t1, hold),
            (3, Some(t0))
        );

        // Past the window: the baseline catches up and the burst is over.
        let t2 = t0 + hold;
        assert_eq!(next_oom_baseline(Some(3), 5, burst, t2, hold), (5, None));

        // No kill, or no previous baseline: nothing is held.
        assert_eq!(next_oom_baseline(Some(5), 5, None, t2, hold), (5, None));
        assert_eq!(next_oom_baseline(None, 7, None, t2, hold), (7, None));

        // A zero hold is the old behaviour: one sweep, then caught up.
        assert_eq!(
            next_oom_baseline(Some(3), 4, None, t0, Duration::ZERO),
            (4, None)
        );
    }
}
