//! The poll loop (#818): three cadences, one publish path.
//!
//! Runtime status is cheap (one `/cluster/resources` call) and moves; guest
//! *configuration* is one call per guest and moves on a human timescale;
//! backups move once a night. Polling all three at the fastest of those rates
//! would be a monitoring sensor hammering the machine whose failure is total,
//! which is the exact failure mode the SNMP sensor's per-device budget exists
//! to prevent one crate over.
//!
//! Everything published here is an observation. There is no write path.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use zensight_common::pve::{
    PveBackupSummary, PveBackupVolume, PveClusterHealth, PveGuest, PveStoragePool,
};
use zensight_common::{HostEvidence, QosClass, TelemetryValue};
use zensight_sensor_core::{AdvancedPublisherRegistry, AlertReporter, Publisher, SensorHealth};

use crate::alerts::{self, Observation};
use crate::api::{PveClient, build_guest, build_pool};
use crate::config::PveConfig;
use crate::telemetry_guard::checked_point;

/// Everything one sweep learned. Kept whole so the state documents, the
/// gauges and the assertions all describe the *same* observation — a
/// re-read between them would let an alert cite a number nothing published.
#[derive(Default)]
pub struct Sweep {
    pub guests: Vec<PveGuest>,
    /// The fast-moving numbers, kept OUT of the guest document on purpose: a
    /// state doc is LWW and a consumer seeds from it, so folding a CPU
    /// reading in would republish the whole configuration every poll and make
    /// every diff between polls meaningless.
    pub metrics: Vec<GuestMetrics>,
    pub pools: Vec<PveStoragePool>,
    pub backups: Vec<PveBackupSummary>,
    pub cluster: Option<PveClusterHealth>,
}

/// One guest's measurements this cycle.
#[derive(Debug, Clone, Default)]
pub struct GuestMetrics {
    pub vmid: u32,
    pub cpu: Option<f64>,
    pub mem: Option<u64>,
    pub maxmem: Option<u64>,
    pub disk: Option<u64>,
    pub maxdisk: Option<u64>,
}

pub struct Poller {
    client: Arc<PveClient>,
    cfg: PveConfig,
    source: String,
    publisher: Publisher,
    states: Arc<AdvancedPublisherRegistry>,
    evidence: Option<Arc<AdvancedPublisherRegistry>>,
    reporter: Option<Arc<AlertReporter>>,
    health: Arc<SensorHealth>,
    /// Guest configs, refreshed on the slower cadence and reused between.
    configs: HashMap<u32, PveGuest>,
    last_config_poll: Option<Instant>,
    last_backup_poll: Option<Instant>,
    backups: Vec<PveBackupSummary>,
}

impl Poller {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: Arc<PveClient>,
        cfg: PveConfig,
        source: String,
        publisher: Publisher,
        states: Arc<AdvancedPublisherRegistry>,
        evidence: Option<Arc<AdvancedPublisherRegistry>>,
        reporter: Option<Arc<AlertReporter>>,
        health: Arc<SensorHealth>,
    ) -> Self {
        Self {
            client,
            cfg,
            source,
            publisher,
            states,
            evidence,
            reporter,
            health,
            configs: HashMap::new(),
            last_config_poll: None,
            last_backup_poll: None,
            backups: Vec::new(),
        }
    }

    pub async fn run(mut self) {
        let mut tick = tokio::time::interval(Duration::from_secs(self.cfg.poll_interval_secs));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let started = Instant::now();
            match self.sweep().await {
                Ok(sweep) => {
                    self.publish(&sweep).await;
                    self.health
                        .record_poll_duration(started.elapsed().as_millis() as u64);
                }
                Err(e) => {
                    // One failure is the API's, not each guest's: recording it
                    // per guest would turn a 30-second restart of pvedaemon
                    // into N dead devices in the fleet view.
                    tracing::warn!(error = %e, "pve: poll cycle failed");
                    self.health.record_device_failure(&self.source, &e);
                }
            }
        }
    }

    /// One full observation.
    pub async fn sweep(&mut self) -> Result<Sweep, String> {
        let (runtimes, pool_runtimes) = self.client.resources().await.map_err(|e| e.to_string())?;

        let wanted =
            |node: &str| self.cfg.nodes.is_empty() || self.cfg.nodes.iter().any(|n| n == node);
        let runtimes: Vec<_> = runtimes.into_iter().filter(|g| wanted(&g.node)).collect();
        let pool_runtimes: Vec<_> = pool_runtimes
            .into_iter()
            .filter(|p| wanted(&p.node))
            .collect();

        // ── Guest configuration, on its own slower cadence ──────────────────
        let refresh_configs = self
            .last_config_poll
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(self.cfg.config_interval_secs));
        if refresh_configs {
            let mut next = HashMap::new();
            for rt in &runtimes {
                match self.client.guest_config(rt).await {
                    Ok(g) => {
                        self.health.record_device_success(&rt.vmid.to_string());
                        next.insert(rt.vmid, g);
                    }
                    Err(e) => {
                        self.health
                            .record_device_failure(&rt.vmid.to_string(), &e.to_string());
                        // Keep the previous config rather than dropping the
                        // guest: a config we read five minutes ago is a far
                        // better answer than none, and its absence would
                        // silently resolve the very alerts it raised.
                        if let Some(prev) = self.configs.get(&rt.vmid) {
                            next.insert(rt.vmid, prev.clone());
                        }
                        tracing::debug!(vmid = rt.vmid, error = %e, "pve: guest config read failed");
                    }
                }
            }
            self.configs = next;
            self.last_config_poll = Some(Instant::now());
        }

        // Join: runtime status is always this cycle's; configuration may be
        // up to `config_interval_secs` old, which is stated in the doc's own
        // `observed_at_ms`.
        let guests: Vec<PveGuest> = runtimes
            .iter()
            .map(|rt| match self.configs.get(&rt.vmid) {
                Some(cfg) => PveGuest {
                    status: rt.status.clone(),
                    uptime_secs: rt.uptime_secs,
                    node: rt.node.clone(),
                    ..cfg.clone()
                },
                None => build_guest(rt, &serde_json::Value::Null),
            })
            .collect();

        // ── Pools, with the allocated total the audit needed ────────────────
        let mut pools = Vec::new();
        for rt in &pool_runtimes {
            // A shared pool is listed once per node; asking every node for the
            // same content listing is N times the work for one answer.
            let allocated = match self.client.storage_allocated(&rt.node, &rt.storage).await {
                Ok(a) => a,
                Err(e) => {
                    tracing::debug!(storage = %rt.storage, error = %e, "pve: content listing failed");
                    None
                }
            };
            pools.push(build_pool(rt, allocated));
        }
        pools.sort_by(|a, b| a.storage.cmp(&b.storage));
        pools.dedup_by(|a, b| a.storage == b.storage);

        // ── Backups, on the slowest cadence ─────────────────────────────────
        let refresh_backups = self
            .last_backup_poll
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(self.cfg.backup_interval_secs));
        if refresh_backups {
            self.backups = self.collect_backups(&pools).await;
            self.last_backup_poll = Some(Instant::now());
        }

        // ── Cluster ─────────────────────────────────────────────────────────
        let cluster = self.collect_cluster(&guests, &runtimes).await;

        let metrics = runtimes
            .iter()
            .map(|rt| GuestMetrics {
                vmid: rt.vmid,
                cpu: rt.cpu,
                mem: rt.mem,
                maxmem: rt.maxmem,
                disk: rt.disk,
                maxdisk: rt.maxdisk,
            })
            .collect();

        self.health.record_device_success(&self.source);
        self.health.set_devices_total(guests.len() as u64 + 1);
        Ok(Sweep {
            guests,
            metrics,
            pools,
            backups: std::mem::take(&mut self.backups),
            cluster,
        })
    }

    async fn collect_backups(&self, pools: &[PveStoragePool]) -> Vec<PveBackupSummary> {
        let mut volumes: HashMap<u32, Vec<PveBackupVolume>> = HashMap::new();
        for p in pools.iter().filter(|p| {
            p.enabled && (p.content.is_empty() || p.content.iter().any(|c| c == "backup"))
        }) {
            match self.client.backups(&p.node, &p.storage).await {
                Ok(vols) => {
                    for v in vols {
                        if let Some(vmid) = vmid_from_volid(&v.volid) {
                            volumes.entry(vmid).or_default().push(v);
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!(storage = %p.storage, error = %e, "pve: backup listing failed")
                }
            }
        }

        let mut tasks: HashMap<u32, zensight_common::pve::PveBackupTask> = HashMap::new();
        let mut nodes: Vec<&str> = pools.iter().map(|p| p.node.as_str()).collect();
        nodes.sort_unstable();
        nodes.dedup();
        for node in nodes {
            match self.client.vzdump_tasks(node, 200).await {
                Ok(rows) => {
                    for (vmid, task) in rows {
                        // Newest first from the client, so the first wins.
                        tasks.entry(vmid).or_insert(task);
                    }
                }
                Err(e) => tracing::debug!(node = %node, error = %e, "pve: task listing failed"),
            }
        }

        let now = zensight_common::current_timestamp_millis() / 1000;
        let mut vmids: Vec<u32> = volumes
            .keys()
            .copied()
            .chain(tasks.keys().copied())
            .collect();
        vmids.sort_unstable();
        vmids.dedup();
        vmids
            .into_iter()
            .map(|vmid| {
                let mut vols = volumes.remove(&vmid).unwrap_or_default();
                vols.sort_by_key(|v| std::cmp::Reverse(v.created_at));
                let latest = vols.first().cloned();
                let previous = vols.get(1).cloned();
                // The baseline comes from the STORE, not from memory: a
                // restart must not reset what "the previous backup" was, or
                // a shrunk dump silently becomes the new normal.
                let size_change_pct = match (&latest, &previous) {
                    (Some(l), Some(p)) if p.size_bytes > 0 => Some(
                        (l.size_bytes as f64 - p.size_bytes as f64) / p.size_bytes as f64 * 100.0,
                    ),
                    _ => None,
                };
                PveBackupSummary {
                    vmid,
                    last_task: tasks.remove(&vmid),
                    age_secs: latest
                        .as_ref()
                        .filter(|l| l.created_at > 0)
                        .map(|l| (now - l.created_at).max(0) as u64),
                    volumes: vols.len() as u32,
                    latest,
                    previous,
                    size_change_pct,
                    observed_at_ms: zensight_common::current_timestamp_millis(),
                }
            })
            .collect()
    }

    async fn collect_cluster(
        &self,
        guests: &[PveGuest],
        runtimes: &[crate::api::GuestRuntime],
    ) -> Option<PveClusterHealth> {
        let (name, quorate, nodes) = match self.client.cluster_status().await {
            Ok(Some(t)) => t,
            Ok(None) => (None, None, Vec::new()),
            Err(e) => {
                tracing::debug!(error = %e, "pve: cluster status unavailable");
                (None, None, Vec::new())
            }
        };
        let ha = self.client.ha_status().await.unwrap_or_default();
        let mut replication = Vec::new();
        let mut node_names: Vec<&str> = runtimes.iter().map(|r| r.node.as_str()).collect();
        node_names.sort_unstable();
        node_names.dedup();
        for n in node_names {
            replication.extend(self.client.replication(n).await.unwrap_or_default());
        }
        Some(PveClusterHealth {
            name,
            quorate,
            nodes,
            ha,
            replication,
            guests_total: guests.iter().filter(|g| !g.template).count() as u32,
            guests_running: guests.iter().filter(|g| g.is_running()).count() as u32,
            observed_at_ms: zensight_common::current_timestamp_millis(),
        })
    }

    /// Publish one sweep: gauges, state documents, evidence claims, and the
    /// assertions. Public so the e2e test can drive a single observation
    /// rather than waiting on the interval.
    pub async fn publish(&self, sweep: &Sweep) {
        let mut published = 0u64;

        for g in &sweep.guests {
            let src = alerts::guest_source(g.vmid);
            let labels = guest_labels(g);
            let mut points = Vec::new();
            if let Some(m) = sweep.metrics.iter().find(|m| m.vmid == g.vmid) {
                // Only what the hypervisor actually reported. A `0` for a
                // plugin that cannot report disk usage would read as "empty",
                // which is a worse answer than no series at all.
                for (suffix, value) in [
                    ("cpu_ratio", m.cpu),
                    ("mem_bytes", m.mem.map(|v| v as f64)),
                    ("mem_max_bytes", m.maxmem.map(|v| v as f64)),
                    ("disk_bytes", m.disk.map(|v| v as f64)),
                    ("disk_max_bytes", m.maxdisk.map(|v| v as f64)),
                ] {
                    if let Some(v) = value {
                        points.push((format!("guest/{}/{suffix}", g.vmid), v));
                    }
                }
            }
            points.push((
                format!("guest/{}/running", g.vmid),
                if g.is_running() { 1.0 } else { 0.0 },
            ));
            points.push((
                format!("guest/{}/uptime_secs", g.vmid),
                g.uptime_secs.unwrap_or(0) as f64,
            ));
            if let Some(p) = g.provisioned_bytes {
                points.push((format!("guest/{}/provisioned_bytes", g.vmid), p as f64));
            }
            for (metric, value) in points {
                let mut point = checked_point(&src, &metric, TelemetryValue::Gauge(value));
                point.labels = labels.clone();
                if let Err(e) = self.publisher.publish(&metric, &point).await {
                    tracing::debug!(error = %e, "pve: telemetry publish failed");
                } else {
                    published += 1;
                }
            }

            let key = state_key(&["guest", &g.vmid.to_string()]);
            if let Some(key) = key
                && let Err(e) = self.states.publish_serializable(&key, g).await
            {
                tracing::warn!(vmid = g.vmid, error = %e, "pve: guest doc publish failed");
            }

            if let Some(reg) = &self.evidence
                && let Some(claim) = guest_evidence(g)
                && let Some(key) = state_key(&["evidence", "device", &g.vmid.to_string()])
                && let Err(e) = reg.publish_serializable(&key, &claim).await
            {
                tracing::debug!(vmid = g.vmid, error = %e, "pve: evidence publish failed");
            }
        }

        for p in &sweep.pools {
            // Operator-chosen, so it must be slugged before it can reach a key
            // — the same foreign-value boundary #843 established for units.
            let slug = zenkey::Chunk::slug(&p.storage);
            let stem = format!("storage/{slug}");
            for (suffix, value) in [
                ("total_bytes", p.total_bytes as f64),
                ("used_bytes", p.used_bytes as f64),
                ("avail_bytes", p.avail_bytes as f64),
                ("used_ratio", p.used_ratio()),
            ] {
                let metric = format!("{stem}/{suffix}");
                let mut point = checked_point(&p.storage, &metric, TelemetryValue::Gauge(value));
                point
                    .labels
                    .insert("storage".to_string(), p.storage.clone());
                point.labels.insert("node".to_string(), p.node.clone());
                if self.publisher.publish(&metric, &point).await.is_ok() {
                    published += 1;
                }
            }
            // Only when known. Publishing 0 for "could not list the content"
            // would read as "nothing is provisioned", which is the one wrong
            // answer this family can give.
            if let Some(a) = p.allocated_bytes {
                for (suffix, value) in [
                    ("allocated_bytes", a as f64),
                    ("overcommit_ratio", p.overcommit_ratio.unwrap_or(0.0)),
                ] {
                    let metric = format!("{stem}/{suffix}");
                    let mut point =
                        checked_point(&p.storage, &metric, TelemetryValue::Gauge(value));
                    point
                        .labels
                        .insert("storage".to_string(), p.storage.clone());
                    let _ = self.publisher.publish(&metric, &point).await;
                    published += 1;
                }
            }
            if let Some(key) = state_key(&["storage", &slug])
                && let Err(e) = self.states.publish_serializable(&key, p).await
            {
                tracing::warn!(storage = %p.storage, error = %e, "pve: pool doc publish failed");
            }
        }

        for b in &sweep.backups {
            let src = alerts::guest_source(b.vmid);
            let mut points = Vec::new();
            if let Some(l) = &b.latest {
                points.push((format!("backup/{}/size_bytes", b.vmid), l.size_bytes as f64));
            }
            if let Some(c) = b.size_change_pct {
                points.push((format!("backup/{}/size_change_pct", b.vmid), c));
            }
            if let Some(a) = b.age_secs {
                points.push((format!("backup/{}/age_secs", b.vmid), a as f64));
            }
            if let Some(t) = &b.last_task {
                points.push((
                    format!("backup/{}/ok", b.vmid),
                    if t.ok { 1.0 } else { 0.0 },
                ));
                if let Some(d) = t.duration_secs {
                    points.push((format!("backup/{}/duration_secs", b.vmid), d as f64));
                }
            }
            for (metric, value) in points {
                let point = checked_point(&src, &metric, TelemetryValue::Gauge(value));
                if self.publisher.publish(&metric, &point).await.is_ok() {
                    published += 1;
                }
            }
            if let Some(key) = state_key(&["backup", &b.vmid.to_string()])
                && let Err(e) = self.states.publish_serializable(&key, b).await
            {
                tracing::warn!(vmid = b.vmid, error = %e, "pve: backup doc publish failed");
            }
        }

        if let Some(c) = &sweep.cluster {
            let mut points = vec![
                ("cluster/nodes_online".to_string(), c.nodes_online() as f64),
                ("cluster/nodes_total".to_string(), c.nodes.len() as f64),
                ("cluster/guests_total".to_string(), c.guests_total as f64),
                (
                    "cluster/guests_running".to_string(),
                    c.guests_running as f64,
                ),
                (
                    "cluster/replication_failed".to_string(),
                    c.replication.iter().filter(|j| j.failed).count() as f64,
                ),
            ];
            // Absent, not 0, on a standalone node: there is no quorum to
            // report, and 0 would read as "lost".
            if let Some(q) = c.quorate {
                points.push(("cluster/quorate".to_string(), if q { 1.0 } else { 0.0 }));
            }
            for (metric, value) in points {
                let point = checked_point(&self.source, &metric, TelemetryValue::Gauge(value));
                if self.publisher.publish(&metric, &point).await.is_ok() {
                    published += 1;
                }
            }
            if let Some(key) = state_key(&["cluster"])
                && let Err(e) = self.states.publish_serializable(&key, c).await
            {
                tracing::warn!(error = %e, "pve: cluster doc publish failed");
            }
        }

        self.health.record_metrics_published(published);

        // ── Assertions ──────────────────────────────────────────────────────
        if let Some(reporter) = &self.reporter {
            let obs = Observation {
                source: &self.source,
                guests: &sweep.guests,
                pools: &sweep.pools,
                backups: &sweep.backups,
                cluster: sweep.cluster.as_ref(),
                now_secs: zensight_common::current_timestamp_millis() / 1000,
            };
            let firing = alerts::grade(&self.cfg.alerts, &obs);
            let mut by_rule: HashMap<&str, Vec<String>> = HashMap::new();
            for a in &firing {
                by_rule
                    .entry(
                        alerts::ALL_RULES
                            .iter()
                            .find(|r| **r == a.rule)
                            .copied()
                            .unwrap_or("?"),
                    )
                    .or_default()
                    .push(a.alert_key());
            }
            for a in firing {
                if let Err(e) = reporter.observe(a, None).await {
                    tracing::warn!(error = %e, "pve: alert publish failed");
                }
            }
            // Every rule reconciles every sweep, including the ones that fired
            // nothing — otherwise a condition that cleared (someone set
            // onboot=1) keeps firing until the sensor restarts.
            for rule in alerts::ALL_RULES {
                let still = by_rule.remove(*rule).unwrap_or_default();
                if let Err(e) = reporter.reconcile(rule, &still).await {
                    tracing::warn!(rule = %rule, error = %e, "pve: alert reconcile failed");
                }
            }
        }
    }
}

/// Build a `state/pve/<chunks…>` key, refusing anything the grammar will not
/// mint rather than papering over it.
fn state_key(chunks: &[&str]) -> Option<String> {
    match zensight_sensor_core::v1::for_producer("pve").state_key(chunks) {
        Ok(k) => Some(k.into()),
        Err(e) => {
            tracing::warn!(chunks = ?chunks, error = %e, "pve: not a legal state subject");
            None
        }
    }
}

fn guest_labels(g: &PveGuest) -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("vmid".to_string(), g.vmid.to_string());
    if let Some(n) = &g.name {
        m.insert("name".to_string(), n.clone());
    }
    m.insert("node".to_string(), g.node.clone());
    m.insert("kind".to_string(), g.kind.to_string());
    m
}

/// A third-party identity claim about a guest.
///
/// `host_id` stays `None` — the netlink/snmp observed-device precedent: a
/// synthetic hash would carry no merge power and would masquerade as the
/// hashed-machine-id contract. The MACs are the merge evidence, and they are
/// exactly what the hypervisor knows and the guest's own sensors also report.
fn guest_evidence(g: &PveGuest) -> Option<HostEvidence> {
    let macs: Vec<String> = g.nics.iter().filter_map(|n| n.mac.clone()).collect();
    if macs.is_empty() && g.name.is_none() {
        return None;
    }
    Some(HostEvidence {
        sensor: "pve".to_string(),
        source: g.vmid.to_string(),
        observer: Some("pve".to_string()),
        host_id: None,
        boot_id: None,
        hostname: g.name.clone(),
        fqdn: None,
        ips: Vec::new(),
        macs,
        vendor: None,
        platform: Some(format!("proxmox-{}", g.kind)),
        container_id: None,
        cloud: None,
        last_updated: zensight_common::current_timestamp_millis(),
    })
}

/// `local:backup/vzdump-qemu-140-2026_08_28-02_00_01.vma.zst` → 140.
///
/// Proxmox Backup Server volids look like `pbs:backup/vm/140/2026-…`, so both
/// shapes have to be read rather than one being assumed.
pub fn vmid_from_volid(volid: &str) -> Option<u32> {
    if let Some(rest) = volid.split("vzdump-").nth(1) {
        // vzdump-qemu-140-2026_08_28-…
        let mut parts = rest.split('-');
        let _kind = parts.next()?;
        return parts.next()?.parse().ok();
    }
    // pbs:backup/vm/140/…  or  …/ct/201/…
    let mut chunks = volid.split('/').peekable();
    while let Some(c) = chunks.next() {
        if matches!(c, "vm" | "ct" | "qemu" | "lxc")
            && let Some(next) = chunks.peek()
            && let Ok(n) = next.parse()
        {
            return Some(n);
        }
    }
    None
}

/// The QoS for the state documents. Public so the e2e test can assert the
/// sensor is not quietly publishing hypervisor state on the telemetry class.
pub const STATE_QOS: QosClass = QosClass::HealthLiveness;

#[cfg(test)]
mod tests {
    use super::*;
    use zensight_common::pve::{GuestKind, GuestNic};

    #[test]
    fn a_vzdump_volid_yields_its_vmid_in_both_dialects() {
        assert_eq!(
            vmid_from_volid("local:backup/vzdump-qemu-140-2026_08_28-02_00_01.vma.zst"),
            Some(140)
        );
        assert_eq!(
            vmid_from_volid("local:backup/vzdump-lxc-201-2026_08_28-02_10_00.tar.zst"),
            Some(201)
        );
        assert_eq!(
            vmid_from_volid("pbs:backup/vm/140/2026-08-28T02:00:01Z"),
            Some(140)
        );
        assert_eq!(vmid_from_volid("local:iso/debian.iso"), None);
    }

    #[test]
    fn evidence_carries_macs_and_never_a_synthetic_host_id() {
        let g = PveGuest {
            vmid: 140,
            name: Some("vm-apps".into()),
            node: "pve".into(),
            kind: GuestKind::Qemu,
            status: "running".into(),
            uptime_secs: None,
            template: false,
            onboot: true,
            protection: false,
            nics: vec![GuestNic {
                slot: "net0".into(),
                bridge: None,
                firewall: true,
                mac: Some("AA:BB:CC:DD:EE:FF".into()),
                vlan_tag: None,
                model: None,
            }],
            disks: vec![],
            provisioned_bytes: None,
            observed_at_ms: 0,
        };
        let e = guest_evidence(&g).unwrap();
        assert_eq!(e.observer.as_deref(), Some("pve"));
        assert_eq!(e.macs, vec!["AA:BB:CC:DD:EE:FF"]);
        assert_eq!(e.hostname.as_deref(), Some("vm-apps"));
        assert!(e.host_id.is_none(), "a synthetic hash would merge nothing");
    }

    #[test]
    fn a_guest_with_nothing_identifying_makes_no_claim() {
        let g = PveGuest {
            vmid: 9,
            name: None,
            node: "pve".into(),
            kind: GuestKind::Lxc,
            status: "stopped".into(),
            uptime_secs: None,
            template: false,
            onboot: false,
            protection: false,
            nics: vec![],
            disks: vec![],
            provisioned_bytes: None,
            observed_at_ms: 0,
        };
        assert!(guest_evidence(&g).is_none());
    }
}
