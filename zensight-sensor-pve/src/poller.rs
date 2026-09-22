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

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use zensight_common::pve::{
    AllocationSource, PveBackupJob, PveBackupSchedule, PveBackupSummary, PveBackupVolume,
    PveCephStatus, PveClusterHealth, PveGuest, PveNode, PveStoragePool,
};
use zensight_common::registry::pve::Subject;
use zensight_common::{HostEvidence, QosClass, TelemetryPoint, TelemetryValue};
use zensight_sensor_core::{
    AdvancedPublisherRegistry, AlertReporter, Publisher, SensorHealth, SweepOpts,
};

use crate::alerts::{self, Observation};
use crate::api::{PveClient, build_guest, build_pool};
use crate::config::PveConfig;

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
    /// Whole-job vzdump runs, one per node that has any (#880).
    pub backup_jobs: Vec<PveBackupJob>,
    pub cluster: Option<PveClusterHealth>,
    /// The hypervisors themselves (#1141) — what this sensor did not look at
    /// while it reported every guest running on them.
    pub nodes: Vec<PveNode>,
    /// The scheduled vzdump jobs (#1141), so "a backup that should have run at
    /// 03:00 did not run at all" is expressible.
    pub schedules: Vec<PveBackupSchedule>,
    /// Ceph's own verdict, on a cluster that runs Ceph (#1141).
    pub ceph: Option<PveCephStatus>,
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
    /// The four counters the `/cluster/resources` row already carried and this
    /// sensor parsed away (#1141). Cumulative since the guest booted.
    pub netin: Option<u64>,
    pub netout: Option<u64>,
    pub diskread: Option<u64>,
    pub diskwrite: Option<u64>,
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
    backup_jobs: Vec<PveBackupJob>,
    /// The `Hosts` claims published last sweep (#916). Retiring a migrated
    /// guest matters more here than anywhere else: without it the guest shows
    /// on the old node *and* the new one for the family's TTL, and a migration
    /// is exactly when someone looks at the map.
    relations: zensight_sensor_core::relation::RelationSet,
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
        relations: zensight_sensor_core::relation::RelationSet,
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
            backup_jobs: Vec::new(),
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
        //
        // Two kinds of pool come out of `/cluster/resources`, and they must
        // not be treated alike. A SHARED pool (NFS, Ceph, PBS) is one pool
        // listed once per node; every row describes the same bytes, so it is
        // asked once and kept once. A NON-shared pool (`local`, `local-lvm`
        // — the ones every PVE node has) is a different pool on every node
        // that happens to carry the same name; each has its own capacity,
        // its own volumes and its own over-commitment. Collapsing on the
        // name alone — which this did for a while — kept one `local-lvm` of
        // a three-node cluster and dropped the other two, and then summed
        // every node's guest disks into the survivor (see
        // `derive_allocated`) for an allocated total roughly N× too high and
        // a false `pool-overcommitted`.
        let mut pools = Vec::new();
        let mut shared_seen: std::collections::HashSet<&str> = Default::default();
        for rt in &pool_runtimes {
            if rt.shared && !shared_seen.insert(rt.storage.as_str()) {
                continue; // the same shared pool, seen through another node
            }
            let reported = match self.client.storage_allocated(&rt.node, &rt.storage).await {
                Ok(a) => a,
                Err(e) => {
                    tracing::warn!(
                        storage = %rt.storage, node = %rt.node, error = %e,
                        "pve: content listing failed — allocated will be derived or absent"
                    );
                    None
                }
            };
            // #881: PVE surfaces per-volume sizes for LVM-thin and ZFS and
            // nothing for a `dir` storage, so the headline finding this
            // sensor exists for — "990 GB provisioned on a 937 GB pool" —
            // was unreportable on the storage type the reference deployment
            // actually runs. Every input for the answer is already in hand:
            // each guest disk names its storage and its declared size, and
            // the guests are joined above, before this loop.
            let allocated = match reported {
                Some(bytes) => Some((bytes, AllocationSource::Reported)),
                None => derive_allocated(&guests, &rt.storage, (!rt.shared).then_some(&rt.node))
                    .map(|bytes| (bytes, AllocationSource::DerivedFromGuests)),
            };
            pools.push(build_pool(rt, allocated));
        }
        pools.sort_by(|a, b| (&a.storage, &a.node).cmp(&(&b.storage, &b.node)));

        // ── Backups, on the slowest cadence ─────────────────────────────────
        let refresh_backups = self
            .last_backup_poll
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(self.cfg.backup_interval_secs));
        if refresh_backups {
            let mut nodes: Vec<&str> = pool_runtimes.iter().map(|p| p.node.as_str()).collect();
            nodes.sort_unstable();
            nodes.dedup();
            let (summaries, jobs) = self.collect_backups(&pools, &nodes).await;
            self.backups = summaries;
            self.backup_jobs = jobs;
            self.last_backup_poll = Some(Instant::now());
        }

        // ── Cluster ─────────────────────────────────────────────────────────
        let cluster = self.collect_cluster(&guests, &runtimes).await;

        // ── The hypervisors, the job schedules and Ceph (#1141) ─────────────
        //
        // Every one best-effort and independently: a node that does not
        // answer, a release with no `/cluster/backup`, a cluster with no Ceph
        // — each yields nothing, and nothing is a missing reading rather than
        // a failed sweep. Grading the whole cycle on any of them would make a
        // non-Ceph cluster look broken.
        let mut nodes = Vec::new();
        for node in self.client.nodes().await.unwrap_or_default() {
            if !wanted(&node) {
                continue;
            }
            match self.client.node_status(&node).await {
                Ok(Some(n)) => nodes.push(n),
                Ok(None) => {}
                Err(e) => {
                    tracing::debug!(node = %node, error = %e, "pve: node status read failed")
                }
            }
        }
        let schedules = self.client.backup_jobs().await.unwrap_or_default();
        let ceph = self.client.ceph_status().await.unwrap_or_default();

        let metrics = runtimes
            .iter()
            .map(|rt| GuestMetrics {
                vmid: rt.vmid,
                cpu: rt.cpu,
                mem: rt.mem,
                maxmem: rt.maxmem,
                disk: rt.disk,
                maxdisk: rt.maxdisk,
                netin: rt.netin,
                netout: rt.netout,
                diskread: rt.diskread,
                diskwrite: rt.diskwrite,
            })
            .collect();

        self.health.record_device_success(&self.source);
        self.health.set_devices_total(guests.len() as u64 + 1);
        Ok(Sweep {
            guests,
            metrics,
            pools,
            // CLONED, not taken (#880). Taking emptied the cache on every
            // sweep while it was refilled only every `backup_interval_secs`,
            // so with the shipped 60 s / 900 s cadences fourteen sweeps in
            // fifteen published no backup document and graded no backup rule
            // — which `reconcile` reads as "the condition cleared". Every
            // backup alert therefore resolved and re-fired on a 15-minute
            // cycle. The e2e missed it because it swept two *different*
            // pollers; a second sweep of the same one now covers it.
            backups: self.backups.clone(),
            backup_jobs: self.backup_jobs.clone(),
            cluster,
            nodes,
            schedules,
            ceph,
        })
    }

    /// Everything known about backups this cycle: one summary per guest, plus
    /// the whole-job runs that name no guest at all (#880).
    ///
    /// The **stored volumes are the evidence**; the tasks are corroboration.
    /// That order matters, because a task window is a fixed number of rows
    /// with no time bound: before this, the newest task *tagged with a vmid*
    /// could be an arbitrarily old one-off, and on the reference fleet a
    /// failure from six weeks earlier became "the last backup" of six guests
    /// permanently, firing a critical about a backup that had in fact
    /// succeeded at 03:00 that morning.
    /// `nodes` is every node the runtime rows named — not the nodes of the
    /// (deduplicated) pool list, which keeps a shared pool through one node
    /// only: vzdump runs on the node that hosts the guest, and a job on any
    /// other node was invisible.
    async fn collect_backups(
        &self,
        pools: &[PveStoragePool],
        nodes: &[&str],
    ) -> (Vec<PveBackupSummary>, Vec<PveBackupJob>) {
        let mut volumes: HashMap<u32, Vec<PveBackupVolume>> = HashMap::new();
        // Whether ANY backup-capable pool was successfully listed. Without
        // this the count is indistinguishable from a refused listing, and a
        // confident `0` is exactly the wrong answer.
        let mut listed_any = false;
        for p in pools.iter().filter(|p| {
            p.enabled && (p.content.is_empty() || p.content.iter().any(|c| c == "backup"))
        }) {
            match self.client.backups(&p.node, &p.storage).await {
                Ok(vols) => {
                    listed_any = true;
                    for v in vols {
                        if let Some(vmid) = vmid_from_volid(&v.volid) {
                            volumes.entry(vmid).or_default().push(v);
                        } else {
                            tracing::warn!(
                                volid = %v.volid,
                                "pve: backup volume names no guest this sensor can read"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        storage = %p.storage, node = %p.node, error = %e,
                        "pve: backup listing failed — this guest's volume count \
                         will be reported as unknown, not as zero"
                    )
                }
            }
        }

        let now = zensight_common::current_timestamp_millis() / 1000;
        let max_age = self.cfg.alerts.backup_task_max_age_secs as i64;

        let mut tasks: HashMap<u32, zensight_common::pve::PveBackupTask> = HashMap::new();
        let mut jobs: Vec<PveBackupJob> = Vec::new();
        for &node in nodes {
            match self.client.vzdump_tasks(node, 200).await {
                Ok(rows) => {
                    for (vmid, task) in rows.per_guest {
                        // A stale task is not evidence about last night. The
                        // window is rows, not time, so without this bound the
                        // oldest surviving one-off wins forever.
                        if max_age > 0 && now - task.started_at > max_age {
                            continue;
                        }
                        // Newest first from the client, so the first wins.
                        tasks.entry(vmid).or_insert(task);
                    }
                    let last_task = rows
                        .jobs
                        .into_iter()
                        .next()
                        .filter(|t| max_age <= 0 || now - t.started_at <= max_age);
                    if let Some(t) = &last_task {
                        jobs.push(PveBackupJob {
                            node: node.to_string(),
                            age_secs: Some((now - t.started_at).max(0) as u64),
                            last_task,
                            observed_at_ms: zensight_common::current_timestamp_millis(),
                        });
                    }
                }
                Err(e) => tracing::warn!(node = %node, error = %e, "pve: task listing failed"),
            }
        }

        let mut vmids: Vec<u32> = volumes
            .keys()
            .copied()
            .chain(tasks.keys().copied())
            .collect();
        vmids.sort_unstable();
        vmids.dedup();
        let summaries = vmids
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
                    volumes: listed_any.then_some(vols.len() as u32),
                    latest,
                    previous,
                    size_change_pct,
                    observed_at_ms: zensight_common::current_timestamp_millis(),
                }
            })
            .collect();
        (summaries, jobs)
    }

    async fn collect_cluster(
        &self,
        guests: &[PveGuest],
        runtimes: &[crate::api::GuestRuntime],
    ) -> Option<PveClusterHealth> {
        let (name, quorate, nodes) = match self.client.cluster_status().await {
            Ok(Some(t)) => t,
            // A standalone node: there is no quorum to have or lose.
            Ok(None) => (None, None, Vec::new()),
            Err(e) => {
                // NOT the same thing (#1132). `quorate: None` is what says
                // "standalone", and standalone is what makes every guest
                // observable — so folding a failed read into it would silently
                // turn the quorum hold OFF exactly when the cluster API is the
                // thing that is unwell. `warn`, not `debug`, for the same
                // reason: this is a degraded sweep, not a quiet detail.
                tracing::warn!(error = %e, "pve: cluster status unreadable; \
                    treating this sweep as non-quorate rather than standalone");
                (None, Some(false), Vec::new())
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
    pub async fn publish(&mut self, sweep: &Sweep) {
        let mut published = 0u64;

        for g in &sweep.guests {
            let labels = guest_labels(g);
            let vmid = g.vmid.to_string();
            let mut points: Vec<(Subject, f64)> = Vec::new();
            if let Some(m) = sweep.metrics.iter().find(|m| m.vmid == g.vmid) {
                // Only what the hypervisor actually reported. A `0` for a
                // plugin that cannot report disk usage would read as "empty",
                // which is a worse answer than no series at all.
                for (subject, value) in [
                    (Subject::guest_cpu_ratio(&vmid), m.cpu),
                    (Subject::guest_mem_bytes(&vmid), m.mem.map(|v| v as f64)),
                    (
                        Subject::guest_mem_max_bytes(&vmid),
                        m.maxmem.map(|v| v as f64),
                    ),
                    (Subject::guest_disk_bytes(&vmid), m.disk.map(|v| v as f64)),
                    (
                        Subject::guest_disk_max_bytes(&vmid),
                        m.maxdisk.map(|v| v as f64),
                    ),
                ] {
                    if let Some(v) = value {
                        points.push((subject, v));
                    }
                }
            }
            // Not published at all for a guest this sweep cannot speak for
            // (#1132): on a node that lost quorum `/cluster/resources` reports
            // the far side as `status: "unknown"`, and a `0` here is this
            // sensor inventing "it stopped" out of "we cannot see it" — the
            // same claim every other metric in this block refuses to make.
            if alerts::guest_is_observable(sweep.cluster.as_ref(), &g.node) {
                points.push((
                    Subject::guest_running(&vmid),
                    if g.is_running() { 1.0 } else { 0.0 },
                ));
            }
            points.push((
                Subject::guest_uptime_secs(&vmid),
                g.uptime_secs.unwrap_or(0) as f64,
            ));
            if let Some(p) = g.provisioned_bytes {
                points.push((Subject::guest_provisioned_bytes(&vmid), p as f64));
            }
            // The four counters the row already carried (#1141). Published as
            // COUNTERS, not gauges: they are monotonic since the guest booted
            // and a stop/start resets them to zero, which `CounterTracker`
            // (#1152) decodes as a reset rather than as a cliff. A gauge would
            // put the raw total on a dashboard, where it means nothing.
            if let Some(m) = sweep.metrics.iter().find(|m| m.vmid == g.vmid) {
                for (subject, value) in [
                    (Subject::guest_net_in_bytes(&vmid), m.netin),
                    (Subject::guest_net_out_bytes(&vmid), m.netout),
                    (Subject::guest_disk_read_bytes(&vmid), m.diskread),
                    (Subject::guest_disk_write_bytes(&vmid), m.diskwrite),
                ] {
                    let Some(v) = value else { continue };
                    let point = TelemetryPoint::for_subject(
                        &self.source,
                        &subject,
                        TelemetryValue::Counter(v),
                    )
                    .with_labels(labels.clone());
                    if let Err(e) = self.publisher.publish_subject(&subject, &point).await {
                        tracing::debug!(error = %e, "pve: counter publish failed");
                    } else {
                        published += 1;
                    }
                }
            }

            for (subject, value) in points {
                // `source` is the host doing the reporting, never the guest
                // being reported on (#883). The vmid is in the key and in the
                // labels; a guest is a facet of this hypervisor, not a
                // separate machine that publishes for itself.
                let point = TelemetryPoint::for_subject(
                    &self.source,
                    &subject,
                    TelemetryValue::Gauge(value),
                )
                .with_labels(labels.clone());
                if let Err(e) = self.publisher.publish_subject(&subject, &point).await {
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

        // One `Hosts` claim per guest, as the complete current set.
        {
            let host_id = zensight_sensor_core::v1::host_id().as_str().to_string();
            let now_ms = zensight_common::current_timestamp_millis();
            let claims: Vec<zensight_common::relation::RelationshipEvidence> = sweep
                .guests
                .iter()
                .map(|g| guest_relation(&host_id, g, now_ms))
                .collect();
            let out = self.relations.sync(&claims).await;
            if out.retired > 0 || out.failed > 0 || out.dropped > 0 {
                tracing::debug!(
                    published = out.published,
                    retired = out.retired,
                    dropped = out.dropped,
                    failed = out.failed,
                    "pve: relation evidence sync"
                );
            }
        }

        // ── The hypervisors themselves (#1141) ──────────────────────────────
        for n in &sweep.nodes {
            // Operator-chosen, so it is slugged before it reaches a key — the
            // same foreign-value boundary #843 established for units.
            let chunk = zensight_sensor_core::key::device_chunk(&n.name)
                .as_str()
                .to_string();
            let labels = [("node".to_string(), n.name.clone())]
                .into_iter()
                .collect::<std::collections::HashMap<_, _>>();
            for (subject, value) in [
                (Subject::node_cpu_ratio(&n.name), n.cpu_ratio),
                (
                    Subject::node_mem_bytes(&n.name),
                    n.mem_bytes.map(|v| v as f64),
                ),
                (
                    Subject::node_mem_total_bytes(&n.name),
                    n.mem_total_bytes.map(|v| v as f64),
                ),
                // Absent on a node with no swap configured, which is a
                // deliberate configuration rather than 0 % used.
                (
                    Subject::node_swap_bytes(&n.name),
                    n.swap_bytes.map(|v| v as f64),
                ),
                (
                    Subject::node_rootfs_bytes(&n.name),
                    n.rootfs_bytes.map(|v| v as f64),
                ),
                (Subject::node_rootfs_used_ratio(&n.name), n.rootfs_ratio()),
                (Subject::node_load1(&n.name), n.load1),
                (Subject::node_load_per_cpu(&n.name), n.load_per_cpu()),
                (
                    Subject::node_uptime_secs(&n.name),
                    n.uptime_secs.map(|v| v as f64),
                ),
            ] {
                // Absent stays absent: a field PVE did not report is a
                // MISSING reading, not a zero, and its shape has moved across
                // releases.
                let Some(v) = value else { continue };
                let point =
                    TelemetryPoint::for_subject(&self.source, &subject, TelemetryValue::Gauge(v))
                        .with_labels(labels.clone());
                if let Err(e) = self.publisher.publish_subject(&subject, &point).await {
                    tracing::debug!(error = %e, "pve: node telemetry publish failed");
                } else {
                    published += 1;
                }
            }
            if let Some(key) = state_key(&["node", &chunk])
                && let Err(e) = self.states.publish_serializable(&key, n).await
            {
                tracing::warn!(node = %n.name, error = %e, "pve: node doc publish failed");
            }
        }

        // ── Scheduled backup jobs (#1141) ───────────────────────────────────
        for j in &sweep.schedules {
            let chunk = zensight_sensor_core::key::device_chunk(&j.id)
                .as_str()
                .to_string();
            if let Some(key) = state_key(&["backup", "job", &chunk, "schedule"])
                && let Err(e) = self.states.publish_serializable(&key, j).await
            {
                tracing::warn!(job = %j.id, error = %e, "pve: backup schedule publish failed");
            }
        }

        // ── Ceph (#1141) ────────────────────────────────────────────────────
        //
        // Nothing at all on a cluster that does not run it — no zeroes, no
        // `healthy: 0`. A cluster with no Ceph is not a cluster with unhealthy
        // Ceph.
        if let Some(c) = &sweep.ceph {
            for (subject, value) in [
                // Ceph's OWN enum, never our reading of the counters below it.
                (
                    Subject::CephHealthy,
                    Some(if c.health == "HEALTH_OK" { 1.0 } else { 0.0 }),
                ),
                (Subject::CephOsdsUp, c.osds_up.map(f64::from)),
                (Subject::CephOsdsTotal, c.osds_total.map(f64::from)),
                (Subject::CephPgsDegraded, c.pgs_degraded.map(f64::from)),
                (
                    Subject::CephUsedRatio,
                    match (c.bytes_used, c.bytes_total) {
                        (Some(u), Some(t)) if t > 0 => Some(u as f64 / t as f64),
                        _ => None,
                    },
                ),
            ] {
                let Some(v) = value else { continue };
                let point =
                    TelemetryPoint::for_subject(&self.source, &subject, TelemetryValue::Gauge(v));
                if let Err(e) = self.publisher.publish_subject(&subject, &point).await {
                    tracing::debug!(error = %e, "pve: ceph telemetry publish failed");
                } else {
                    published += 1;
                }
            }
            if let Some(key) = state_key(&["ceph"])
                && let Err(e) = self.states.publish_serializable(&key, c).await
            {
                tracing::warn!(error = %e, "pve: ceph doc publish failed");
            }
        }

        for p in &sweep.pools {
            // Operator-chosen, so it must be slugged before it can reach a key
            // — the same foreign-value boundary #843 established for units.
            // A non-shared pool gets the node in its key chunk, because
            // `local` and `local-lvm` exist once per node in every cluster and
            // two of them would take turns overwriting one `storage/local`
            // document. A SHARED pool is one thing seen from several nodes, so
            // it keeps the bare chunk.
            //
            // The disambiguator is `shared` — a property of the pool — and not
            // "did this sweep see the name twice" (#1132). It was the latter,
            // which made the KEY depend on which nodes answered: when node B
            // dropped out, node A's `local-lvm` moved from
            // `storage/pve1-local-lvm` to `storage/local-lvm` and the old
            // state document became an LWW ghost nothing would ever overwrite.
            let slug = if p.shared {
                zensight_sensor_core::key::device_chunk(&p.storage)
            } else {
                zensight_sensor_core::key::device_chunk(format!("{}-{}", p.node, p.storage))
            };
            // The builder slugs the raw value itself (#1274); `slug` above is
            // what the state document's key carries.
            let store = if p.shared {
                p.storage.clone()
            } else {
                format!("{}-{}", p.node, p.storage)
            };
            for (subject, value) in [
                (Subject::storage_total_bytes(&store), p.total_bytes as f64),
                (Subject::storage_used_bytes(&store), p.used_bytes as f64),
                (Subject::storage_avail_bytes(&store), p.avail_bytes as f64),
                (Subject::storage_used_ratio(&store), p.used_ratio()),
            ] {
                let point = TelemetryPoint::for_subject(
                    &self.source,
                    &subject,
                    TelemetryValue::Gauge(value),
                )
                .with_label("storage", p.storage.clone())
                .with_label("node", p.node.clone());
                if self
                    .publisher
                    .publish_subject(&subject, &point)
                    .await
                    .is_ok()
                {
                    published += 1;
                }
            }
            // Only when known. Publishing 0 for "could not list the content"
            // would read as "nothing is provisioned", which is the one wrong
            // answer this family can give.
            if let Some(a) = p.allocated_bytes {
                for (subject, value) in [
                    (Subject::storage_allocated_bytes(&store), a as f64),
                    (
                        Subject::storage_overcommit_ratio(&store),
                        p.overcommit_ratio.unwrap_or(0.0),
                    ),
                ] {
                    let mut point = TelemetryPoint::for_subject(
                        &self.source,
                        &subject,
                        TelemetryValue::Gauge(value),
                    )
                    .with_label("storage", p.storage.clone())
                    .with_label("node", p.node.clone());
                    // Reported vs derived rides on the series, not only in the
                    // document: a derived total is a floor, and a dashboard
                    // comparing two pools must be able to see which is which.
                    if let Some(src) = p.allocated_source {
                        point = point.with_label(
                            "allocated_source",
                            match src {
                                AllocationSource::Reported => "reported",
                                AllocationSource::DerivedFromGuests => "derived_from_guests",
                            },
                        );
                    }
                    if self
                        .publisher
                        .publish_subject(&subject, &point)
                        .await
                        .is_ok()
                    {
                        published += 1;
                    }
                }
            }
            if let Some(key) = state_key(&["storage", &slug])
                && let Err(e) = self.states.publish_serializable(&key, p).await
            {
                tracing::warn!(storage = %p.storage, error = %e, "pve: pool doc publish failed");
            }
        }

        for b in &sweep.backups {
            let vmid = b.vmid.to_string();
            let mut points: Vec<(Subject, f64)> = Vec::new();
            if let Some(l) = &b.latest {
                points.push((Subject::backup_size_bytes(&vmid), l.size_bytes as f64));
            }
            if let Some(c) = b.size_change_pct {
                points.push((Subject::backup_size_change_pct(&vmid), c));
            }
            if let Some(a) = b.age_secs {
                points.push((Subject::backup_age_secs(&vmid), a as f64));
            }
            if let Some(t) = &b.last_task {
                points.push((Subject::backup_ok(&vmid), if t.ok { 1.0 } else { 0.0 }));
                if let Some(d) = t.duration_secs {
                    points.push((Subject::backup_duration_secs(&vmid), d as f64));
                }
            }
            for (subject, value) in points {
                // Named its subject for the first time: these points carried
                // no labels at all, so a consumer holding one as a value had
                // no idea which guest it was about (#883).
                let point = TelemetryPoint::for_subject(
                    &self.source,
                    &subject,
                    TelemetryValue::Gauge(value),
                )
                .with_label("vmid", b.vmid.to_string());
                if self
                    .publisher
                    .publish_subject(&subject, &point)
                    .await
                    .is_ok()
                {
                    published += 1;
                }
            }
            if let Some(key) = state_key(&["backup", &b.vmid.to_string()])
                && let Err(e) = self.states.publish_serializable(&key, b).await
            {
                tracing::warn!(vmid = b.vmid, error = %e, "pve: backup doc publish failed");
            }
        }

        for j in &sweep.backup_jobs {
            let slug = zensight_sensor_core::key::device_chunk(&j.node);
            if let Some(t) = &j.last_task {
                for (subject, value) in [
                    (
                        Subject::backup_job_ok(&j.node),
                        if t.ok { 1.0 } else { 0.0 },
                    ),
                    (
                        Subject::backup_job_duration_secs(&j.node),
                        t.duration_secs.unwrap_or(0) as f64,
                    ),
                ] {
                    let point = TelemetryPoint::for_subject(
                        &self.source,
                        &subject,
                        TelemetryValue::Gauge(value),
                    )
                    .with_label("node", j.node.clone());
                    if self
                        .publisher
                        .publish_subject(&subject, &point)
                        .await
                        .is_ok()
                    {
                        published += 1;
                    }
                }
            }
            if let Some(key) = state_key(&["backup", "job", &slug])
                && let Err(e) = self.states.publish_serializable(&key, j).await
            {
                tracing::warn!(node = %j.node, error = %e, "pve: backup job doc publish failed");
            }
        }

        if let Some(c) = &sweep.cluster {
            let mut points = vec![
                (Subject::ClusterGuestsTotal, c.guests_total as f64),
                (Subject::ClusterGuestsRunning, c.guests_running as f64),
                (
                    Subject::ClusterReplicationFailed,
                    c.replication.iter().filter(|j| j.failed).count() as f64,
                ),
            ];
            // Absent, not 0, when `/cluster/status` answered nothing (refused,
            // or failed): a standalone node still lists itself there, so an
            // empty node list is "could not ask", and `nodes_online = 0`
            // would read as "every node is down".
            if !c.nodes.is_empty() {
                points.push((Subject::ClusterNodesOnline, c.nodes_online() as f64));
                points.push((Subject::ClusterNodesTotal, c.nodes.len() as f64));
            }
            // Absent, not 0, on a standalone node: there is no quorum to
            // report, and 0 would read as "lost".
            if let Some(q) = c.quorate {
                points.push((Subject::ClusterQuorate, if q { 1.0 } else { 0.0 }));
            }
            for (subject, value) in points {
                let mut point = TelemetryPoint::for_subject(
                    &self.source,
                    &subject,
                    TelemetryValue::Gauge(value),
                );
                // The node this cluster view was read from. Not the identity
                // of the series — that is the reporting host — but the fact a
                // reader needs when two hypervisors are polled from one place.
                if let Some(local) = c.nodes.iter().find(|n| n.local) {
                    point = point.with_label("node", local.name.clone());
                }
                if self
                    .publisher
                    .publish_subject(&subject, &point)
                    .await
                    .is_ok()
                {
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
                backup_jobs: &sweep.backup_jobs,
                source: &self.source,
                guests: &sweep.guests,
                pools: &sweep.pools,
                backups: &sweep.backups,
                cluster: sweep.cluster.as_ref(),
                nodes: &sweep.nodes,
                schedules: &sweep.schedules,
                ceph: sweep.ceph.as_ref(),
                // Passed in rather than read inside the rules, so `grade`
                // stays pure and a test can place "now" where it needs it —
                // the discipline `age_secs` already follows.
                now_ms: zensight_common::current_timestamp_millis(),
            };
            let firing = alerts::grade(&self.cfg.alerts, &obs);
            let (guest, fleet): (Vec<zensight_common::Alert>, Vec<zensight_common::Alert>) = firing
                .into_iter()
                .partition(|a| alerts::GUEST_RULES.contains(&a.rule.as_str()));

            // Every fleet rule reconciles every sweep, including the ones that
            // fired nothing — otherwise a condition that cleared keeps firing
            // until the sensor restarts.
            if let Err(e) = reporter
                .sweep(alerts::FLEET_RULES, fleet, SweepOpts::default())
                .await
            {
                tracing::warn!(error = %e, "pve: alert sweep failed");
            }

            // The guest rules sweep **per node** (#1132), and a node this sweep
            // cannot speak for — no quorum, or listed offline — is swept with
            // `Answered::No`: `grade` held its guests, and a reconcile that read
            // that hold as "recovered" would resolve a real alert on the far
            // side of a corosync partition. Absence of evidence is the one thing
            // a reconcile must never treat as evidence of absence.
            let mut by_node: HashMap<String, Vec<zensight_common::Alert>> = HashMap::new();
            for a in guest {
                // `base` in `grade` labels every guest alert with its node; an
                // alert without one has no scope and is swept under the empty
                // node name, where a real node can never resolve it by mistake.
                let node = a.labels.get("node").cloned().unwrap_or_default();
                by_node.entry(node).or_default().push(a);
            }
            let mut nodes: BTreeSet<String> = sweep.guests.iter().map(|g| g.node.clone()).collect();
            nodes.extend(by_node.keys().cloned());
            for node in &nodes {
                let for_node = by_node.remove(node).unwrap_or_default();
                let answered = alerts::guest_is_observable(sweep.cluster.as_ref(), node).into();
                if let Err(e) = reporter
                    .sweep(
                        alerts::GUEST_RULES,
                        for_node,
                        SweepOpts {
                            scope: Some(("node", node)),
                            answered,
                            ..Default::default()
                        },
                    )
                    .await
                {
                    tracing::warn!(node = %node, error = %e, "pve: alert sweep failed");
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

/// A `Hosts` claim: this node hosts this guest (#916).
///
/// `from` is a self-claim by `host_id` — the node the sensor runs on, which is
/// the strongest end available and what lets the catalog resolve this edge to
/// a real entity. `to` carries the vmid as the device slug **and the guest's
/// MACs**, which is what makes the far end resolvable at all: a guest running
/// its own sensor reports the same MACs, so the catalog joins the hypervisor's
/// view of the guest to the guest's view of itself instead of leaving two
/// unrelated nodes on the map.
///
/// `bridge` and `vlan` ride as attrs off the first NIC that declares them. The
/// first, not a merge of all: a guest with two NICs on two bridges has two
/// answers and picking one silently is better than inventing a third, while
/// modelling each NIC as its own edge would multiply a 1024-cardinality family
/// by the NIC count to say something the guest document already carries in
/// full.
fn guest_relation(
    host_id: &str,
    g: &PveGuest,
    now_ms: i64,
) -> zensight_common::relation::RelationshipEvidence {
    use zensight_common::relation::{EndpointClaim, RelationKind, RelationshipEvidence};
    let mut attrs = std::collections::BTreeMap::new();
    if let Some(bridge) = g.nics.iter().find_map(|n| n.bridge.as_ref()) {
        attrs.insert("bridge".to_string(), bridge.clone());
    }
    if let Some(vlan) = g.nics.iter().find_map(|n| n.vlan_tag) {
        attrs.insert("vlan".to_string(), vlan.to_string());
    }
    attrs.insert("kind".to_string(), g.kind.to_string());
    RelationshipEvidence {
        sensor: "pve".to_string(),
        source: g.node.clone(),
        kind: RelationKind::Hosts,
        from: EndpointClaim::host(host_id),
        to: EndpointClaim {
            device: Some(g.vmid.to_string()),
            macs: g.nics.iter().filter_map(|n| n.mac.clone()).collect(),
            name: g.name.clone(),
            ..Default::default()
        },
        attrs,
        last_updated: now_ms,
    }
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

/// Sum the declared sizes of every guest disk that lives on `storage`.
///
/// A **floor**, deliberately, and labelled as one on the wire
/// ([`AllocationSource::DerivedFromGuests`]): a `unused<N>` volume still
/// occupies the pool but is not attached to any slot, so `provisioned_bytes`
/// excludes it and so does this; a disk with no `size=` (efidisk, TPM state)
/// contributes nothing. Templates are counted — their disks occupy the pool
/// exactly as a running guest's do.
///
/// `None` when no guest has a sized disk on this pool: that is "we cannot say",
/// not "nothing is provisioned", and the difference is the whole of #881.
///
/// `node` scopes the sum to the guests on one node — for a NON-shared pool,
/// where `local-lvm` on node A and `local-lvm` on node B are two pools with
/// one name, and a guest on B occupies nothing on A. A shared pool passes
/// `None`: every node's guests occupy the same bytes.
fn derive_allocated(guests: &[PveGuest], storage: &str, node: Option<&str>) -> Option<u64> {
    let sizes: Vec<u64> = guests
        .iter()
        .filter(|g| node.is_none_or(|n| g.node == n))
        .flat_map(|g| g.disks.iter())
        .filter(|d| d.storage.as_deref() == Some(storage))
        .filter_map(|d| d.size_bytes)
        .collect();
    (!sizes.is_empty()).then(|| sizes.iter().sum())
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

    fn guest_on(vmid: u32, disks: &[(&str, Option<u64>)]) -> PveGuest {
        PveGuest {
            vmid,
            name: None,
            node: "pve".into(),
            kind: GuestKind::Qemu,
            status: "running".into(),
            uptime_secs: None,
            template: false,
            onboot: true,
            protection: false,
            nics: vec![],
            disks: disks
                .iter()
                .enumerate()
                .map(|(i, (storage, size))| zensight_common::pve::GuestDisk {
                    slot: format!("scsi{i}"),
                    volid: format!("{storage}:vm-{vmid}-disk-{i}"),
                    storage: Some((*storage).to_string()),
                    size_bytes: *size,
                    backup: true,
                })
                .collect(),
            provisioned_bytes: None,
            observed_at_ms: 0,
        }
    }

    /// #881: the reference fleet's real numbers. 790 GiB of guest disks
    /// against a 936 GiB `dir` pool the API reports no `allocated` for — a
    /// ratio of 0.84, which the sensor could not produce at all and reported
    /// as `0` instead.
    #[test]
    fn allocated_is_derived_from_the_guests_that_live_on_the_pool() {
        const G: u64 = 1024 * 1024 * 1024;
        let guests = [
            guest_on(100, &[("local", Some(16 * G))]),
            guest_on(110, &[("local", Some(66 * G))]),
            guest_on(130, &[("local", Some(350 * G))]),
            guest_on(160, &[("local", Some(290 * G))]),
            // Another pool entirely: must not be counted here.
            guest_on(170, &[("fast", Some(500 * G))]),
            // A disk with no declared size (efidisk, TPM state) contributes
            // nothing rather than a zero that looks like a measurement.
            guest_on(180, &[("local", None)]),
        ];
        assert_eq!(derive_allocated(&guests, "local", None), Some(722 * G));
        assert_eq!(derive_allocated(&guests, "fast", None), Some(500 * G));
        // No sized disk anywhere on this pool is "we cannot say", not
        // "nothing is provisioned" — the whole of #881.
        assert_eq!(derive_allocated(&guests, "nvme", None), None);
        assert_eq!(derive_allocated(&[], "local", None), None);
    }

    /// A cluster: `local` on every node is a different pool with one name,
    /// so the derived total for node A's `local` counts node A's guests only.
    /// Summing the cluster-wide list — which this did for a while — put every
    /// node's disks on whichever `local` survived the dedup, roughly N× too
    /// high, and fired `pool-overcommitted` on a pool that was half empty.
    #[test]
    fn derived_allocation_is_scoped_to_the_node_for_a_local_pool() {
        const G: u64 = 1 << 30;
        let mut on_a = guest_on(110, &[("local", Some(100 * G))]);
        on_a.node = "pve-a".into();
        let mut on_b = guest_on(120, &[("local", Some(300 * G))]);
        on_b.node = "pve-b".into();
        let guests = vec![on_a, on_b];
        assert_eq!(
            derive_allocated(&guests, "local", Some("pve-a")),
            Some(100 * G)
        );
        assert_eq!(
            derive_allocated(&guests, "local", Some("pve-b")),
            Some(300 * G)
        );
        // A node with no guest on the pool: "cannot say", not zero.
        assert_eq!(derive_allocated(&guests, "local", Some("pve-c")), None);
        // A shared pool: every node's guests occupy the same bytes.
        assert_eq!(derive_allocated(&guests, "local", None), Some(400 * G));
    }
}
