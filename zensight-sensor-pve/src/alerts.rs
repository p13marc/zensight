//! The assertions (#818).
//!
//! Every rule here is a finding the 2026-08-28 audit made **by hand, once,
//! weeks late**. That is the whole design brief: none of these is a metric
//! that spikes, so none of them would ever be caught by a threshold on a
//! dashboard. They are configuration facts that stopped matching intent, and
//! the way to catch those is to assert them on every poll.
//!
//! `grade` is pure — no bus, no HTTP — so the entire rule table is testable
//! against documents.

use std::collections::HashMap;

use zensight_common::pve::{
    PveBackupJob, PveBackupSchedule, PveBackupSummary, PveCephStatus, PveClusterHealth, PveGuest,
    PveNode, PveStoragePool,
};
use zensight_common::{Alert, AlertKind, AlertSeverity, Protocol};

use crate::config::PveAlertsConfig;

pub const RULE_ONBOOT: &str = "guest-onboot-off";
pub const RULE_NOT_RUNNING: &str = "guest-not-running";
pub const RULE_NIC_FIREWALL: &str = "guest-nic-firewall-off";
pub const RULE_POOL_USED: &str = "pool-usage";
pub const RULE_POOL_OVERCOMMIT: &str = "pool-overcommitted";
pub const RULE_BACKUP_FAILED: &str = "backup-failed";
pub const RULE_BACKUP_JOB_FAILED: &str = "backup-job-failed";
pub const RULE_BACKUP_STALE: &str = "backup-stale";
pub const RULE_BACKUP_SHRUNK: &str = "backup-shrunk";
pub const RULE_QUORUM: &str = "cluster-not-quorate";
pub const RULE_REPLICATION: &str = "replication-failed";
/// A node's root filesystem is nearly full (#1141) — invisible in every
/// storage pool's numbers, and it stops PVE writing its own state.
pub const RULE_NODE_ROOTFS: &str = "node-rootfs-full";
/// A node's load average per CPU is above the configured ceiling (#1141).
pub const RULE_NODE_LOAD: &str = "node-load-high";
/// A node has started swapping (#1141) — the reading a guest's own numbers
/// cannot show.
pub const RULE_NODE_SWAP: &str = "node-swapping";
/// An **enabled** backup job's `next-run` is in the past and nothing has run
/// since (#1141). The assertion `backup-stale` cannot make.
pub const RULE_BACKUP_OVERDUE: &str = "backup-job-overdue";
/// Ceph's own health enum is not `HEALTH_OK` (#1141).
pub const RULE_CEPH_HEALTH: &str = "ceph-health";

/// The rules that are about ONE GUEST, and therefore about one node.
///
/// They reconcile per node rather than fleet-wide (#1132), so a node that did
/// not answer this sweep keeps its guests' alerts instead of having them
/// announced as recovered. Every alert these raise carries a `node` label —
/// `base` in `grade` puts it there — which is what makes the scoping possible.
pub const GUEST_RULES: &[&str] = &[RULE_ONBOOT, RULE_NOT_RUNNING, RULE_NIC_FIREWALL];

/// Every rule this sensor can raise. The poller reconciles each one every
/// sweep, so a rule that stops firing resolves — including one whose whole
/// input disappeared (a guest that was deleted).
pub const ALL_RULES: &[&str] = &[
    RULE_ONBOOT,
    RULE_NOT_RUNNING,
    RULE_NIC_FIREWALL,
    RULE_POOL_USED,
    RULE_POOL_OVERCOMMIT,
    RULE_BACKUP_FAILED,
    RULE_BACKUP_JOB_FAILED,
    RULE_BACKUP_STALE,
    RULE_BACKUP_SHRUNK,
    RULE_QUORUM,
    RULE_REPLICATION,
    RULE_NODE_ROOTFS,
    RULE_NODE_LOAD,
    RULE_NODE_SWAP,
    RULE_BACKUP_OVERDUE,
    RULE_CEPH_HEALTH,
];

/// One sweep's inputs.
pub struct Observation<'a> {
    /// The reporting host — the `source` of **every** series and alert this
    /// sensor emits (#883). A guest, a pool and a cluster are facets of this
    /// hypervisor, not separate machines that publish for themselves; the
    /// vmid, the storage name and the node ride in the labels, where a rename
    /// costs nothing and where `alert_key` cannot see them.
    pub source: &'a str,
    pub guests: &'a [PveGuest],
    pub pools: &'a [PveStoragePool],
    pub backups: &'a [PveBackupSummary],
    /// Whole-job vzdump runs — the ones that name no guest (#880).
    pub backup_jobs: &'a [PveBackupJob],
    /// Staleness is graded from each summary's own `age_secs`, computed by
    /// the poller against its clock — there is no second clock here.
    pub cluster: Option<&'a PveClusterHealth>,
    /// The hypervisors themselves (#1141).
    pub nodes: &'a [PveNode],
    /// The scheduled vzdump jobs (#1141), so "due and did not run" is
    /// separable from "merely young".
    pub schedules: &'a [PveBackupSchedule],
    /// Ceph's own verdict, where the cluster runs Ceph (#1141).
    pub ceph: Option<&'a PveCephStatus>,
    /// This sweep's wall clock, in epoch millis — passed in rather than read
    /// here so the rules stay pure and a test can place "now" where it needs
    /// it. The same discipline `age_secs` already follows.
    pub now_ms: i64,
}

fn alert(
    source: &str,
    rule: &str,
    severity: AlertSeverity,
    summary: String,
    labels: &[(&str, String)],
) -> Alert {
    let mut a = Alert::new(
        source,
        Protocol::Pve,
        AlertKind::Expectation,
        rule,
        severity,
        summary,
    );
    let mut map = HashMap::new();
    for (k, v) in labels {
        map.insert((*k).to_string(), v.clone());
    }
    a.labels = map;
    a
}

/// Whether this sweep can speak for a guest at all (#1132).
///
/// `/cluster/resources` on a node that has lost quorum still answers. It
/// reports the guests on the far side of the partition as `status: "unknown"`
/// — and `is_running()` is `status == "running"`, so every one of them looked
/// stopped. A ninety-second corosync blip fired a **critical**
/// `guest-not-running` for every VM in the cluster, none of which had stopped.
///
/// Two ways a guest is unobservable, and both hold every rule rather than
/// grading on what the API happened to say:
///
/// - **The cluster is not quorate.** A node without quorum is not entitled to
///   an opinion about anything but itself, and Proxmox's own tooling refuses
///   to act in that state.
/// - **The guest's node is listed and offline.** Its guests are unreachable,
///   not stopped.
///
/// A standalone node (`cluster: None`, or `quorate: None`) is always
/// observable: there is no quorum to have or lose. This is SNMP's
/// `device_answered` and BMC's `chassis.is_none()` guard, one API over — and
/// the lesson both of those already paid for.
pub fn guest_is_observable(cluster: Option<&PveClusterHealth>, node: &str) -> bool {
    let Some(c) = cluster else {
        return true;
    };
    if c.quorate == Some(false) {
        return false;
    }
    !c.nodes.iter().any(|n| n.name == node && !n.online)
}

/// Grade one sweep. Returns every currently-firing alert; the caller
/// reconciles per rule, so anything absent here resolves.
///
/// **Absence is not resolution for a guest whose node did not answer.** The
/// caller reconciles the guest rules per node (`reconcile_labeled(rule,
/// "node", …)`) and skips the nodes this function held, so a guest nobody can
/// see keeps its previous state instead of being announced as recovered.
pub fn grade(cfg: &PveAlertsConfig, obs: &Observation<'_>) -> Vec<Alert> {
    let mut out = Vec::new();
    if !cfg.enabled {
        return out;
    }

    for g in obs.guests {
        // Held, not graded (#1132) — see `guest_is_observable`.
        if !guest_is_observable(obs.cluster, &g.node) {
            continue;
        }
        // A template is a stamp, not a guest: it has no business being
        // `onboot`, running, or firewalled.
        if g.template || cfg.exempt_vmids.contains(&g.vmid) {
            continue;
        }
        let name = g.name.clone().unwrap_or_else(|| format!("vm-{}", g.vmid));
        let base = [
            ("vmid", g.vmid.to_string()),
            ("name", name.clone()),
            ("node", g.node.clone()),
            ("kind", g.kind.to_string()),
        ];

        if cfg.guest_onboot && !g.onboot {
            out.push(alert(
                obs.source,
                RULE_ONBOOT,
                AlertSeverity::Warning,
                format!(
                    "{name} ({}) has onboot=0 — it will not come back after a host reboot",
                    g.vmid
                ),
                &base,
            ));
        }
        // Only meaningful for a guest that is *supposed* to be up. A guest
        // deliberately left off is not a fault, and `onboot` is the only
        // machine-readable statement of that intent Proxmox has.
        if cfg.guest_not_running && g.onboot && !g.is_running() {
            out.push(alert(
                obs.source,
                RULE_NOT_RUNNING,
                AlertSeverity::Critical,
                format!(
                    "{name} ({}) is set to start at boot but is {}",
                    g.vmid, g.status
                ),
                &[base.as_slice(), &[("status", g.status.clone())]].concat(),
            ));
        }
        if cfg.nic_firewall {
            for slot in g.nics_without_firewall() {
                out.push(alert(
                    obs.source,
                    RULE_NIC_FIREWALL,
                    AlertSeverity::Warning,
                    format!(
                        "{name} ({}) has {slot} without firewall=1 — the guest's firewall \
                         rules do not apply to it",
                        g.vmid
                    ),
                    &[base.as_slice(), &[("nic", slot.to_string())]].concat(),
                ));
            }
        }
    }

    for p in obs.pools {
        if !p.enabled {
            continue;
        }
        let labels = [("storage", p.storage.clone()), ("node", p.node.clone())];
        if cfg.pool_used_pct > 0.0 && p.total_bytes > 0 {
            let pct = p.used_ratio() * 100.0;
            if pct >= cfg.pool_used_pct {
                out.push(alert(
                    obs.source,
                    RULE_POOL_USED,
                    if pct >= 95.0 {
                        AlertSeverity::Critical
                    } else {
                        AlertSeverity::Warning
                    },
                    format!("pool {} is {pct:.1}% used", p.storage),
                    &[
                        labels.as_slice(),
                        &[
                            ("used_pct", format!("{pct:.1}")),
                            ("threshold_pct", format!("{:.1}", cfg.pool_used_pct)),
                        ],
                    ]
                    .concat(),
                ));
            }
        }
        if cfg.pool_overcommit_ratio > 0.0
            && let Some(ratio) = p.overcommit_ratio
            && ratio >= cfg.pool_overcommit_ratio
        {
            out.push(alert(
                obs.source,
                RULE_POOL_OVERCOMMIT,
                AlertSeverity::Warning,
                format!(
                    "pool {} has {} provisioned against {} of capacity (ratio {ratio:.2}) — \
                     it can fill with no configuration change",
                    p.storage,
                    human_bytes(p.allocated_bytes.unwrap_or(0)),
                    human_bytes(p.total_bytes),
                ),
                &[
                    labels.as_slice(),
                    &[
                        ("ratio", format!("{ratio:.3}")),
                        (
                            "allocated_bytes",
                            p.allocated_bytes.unwrap_or(0).to_string(),
                        ),
                        ("total_bytes", p.total_bytes.to_string()),
                    ],
                ]
                .concat(),
            ));
        }
    }

    for b in obs.backups {
        // A template is a stamp, not a guest — and the reference fleet's job
        // excludes 9000 explicitly, yet it fired `backup-failed` anyway,
        // because this loop had neither guard the guest loop above has had
        // all along (#880). A guest the operator has exempted is exempt here
        // too: "I know, and I have decided" is an answer.
        if obs.guests.iter().any(|g| g.vmid == b.vmid && g.template)
            || cfg.exempt_vmids.contains(&b.vmid)
        {
            continue;
        }
        let base = [("vmid", b.vmid.to_string())];

        // A failed task is evidence about a backup that failed — unless a
        // volume exists that is NEWER than the task, in which case the
        // failure has already been superseded by a run that worked. Volumes
        // are the ground truth; the task says why.
        let superseded = |t: &zensight_common::pve::PveBackupTask| {
            b.latest
                .as_ref()
                .is_some_and(|l| l.created_at > t.started_at)
        };
        if cfg.backup_failed
            && let Some(t) = &b.last_task
            && !t.ok
            && !superseded(t)
        {
            out.push(alert(
                obs.source,
                RULE_BACKUP_FAILED,
                AlertSeverity::Critical,
                format!(
                    "the last vzdump of guest {} failed: {}",
                    b.vmid,
                    t.exit_status.as_deref().unwrap_or("no exit status")
                ),
                &[
                    base.as_slice(),
                    &[
                        ("upid", t.upid.clone()),
                        (
                            "exit_status",
                            t.exit_status.clone().unwrap_or_else(|| "unknown".into()),
                        ),
                    ],
                ]
                .concat(),
            ));
        }
        if cfg.backup_stale_secs > 0
            && let Some(age) = b.age_secs
            && age > cfg.backup_stale_secs
        {
            out.push(alert(
                obs.source,
                RULE_BACKUP_STALE,
                AlertSeverity::Warning,
                format!(
                    "guest {}'s newest backup is {} old (limit {})",
                    b.vmid,
                    human_secs(age),
                    human_secs(cfg.backup_stale_secs)
                ),
                &[
                    base.as_slice(),
                    &[
                        ("age_secs", age.to_string()),
                        ("limit_secs", cfg.backup_stale_secs.to_string()),
                    ],
                ]
                .concat(),
            ));
        }
        // The one a green exit code cannot show. "The job exited 0" is what
        // the existing mail notification already says.
        if cfg.backup_shrink_pct > 0.0
            && let Some(change) = b.size_change_pct
            && change <= -cfg.backup_shrink_pct
        {
            out.push(alert(
                obs.source,
                RULE_BACKUP_SHRUNK,
                AlertSeverity::Critical,
                format!(
                    "guest {}'s newest backup is {:.0}% smaller than the one before it \
                     ({} → {}) — it succeeded and shrank",
                    b.vmid,
                    change.abs(),
                    human_bytes(b.previous.as_ref().map(|p| p.size_bytes).unwrap_or(0)),
                    human_bytes(b.latest.as_ref().map(|p| p.size_bytes).unwrap_or(0)),
                ),
                &[
                    base.as_slice(),
                    &[
                        ("change_pct", format!("{change:.1}")),
                        ("threshold_pct", format!("-{:.1}", cfg.backup_shrink_pct)),
                    ],
                ]
                .concat(),
            ));
        }
    }

    // A whole-job run covers every guest and names none, so it is graded once
    // (#880). Seven per-guest criticals for one job is not seven findings.
    if cfg.backup_job_failed {
        for j in obs.backup_jobs {
            if let Some(t) = &j.last_task
                && !t.ok
            {
                out.push(alert(
                    obs.source,
                    RULE_BACKUP_JOB_FAILED,
                    AlertSeverity::Critical,
                    format!(
                        "the last whole-job vzdump on {} failed: {}",
                        j.node,
                        t.exit_status.as_deref().unwrap_or("no exit status")
                    ),
                    &[
                        ("node", j.node.clone()),
                        ("upid", t.upid.clone()),
                        (
                            "exit_status",
                            t.exit_status.clone().unwrap_or_else(|| "unknown".into()),
                        ),
                    ],
                ));
            }
        }
    }

    if let Some(c) = obs.cluster {
        // `quorate: None` is a standalone node — no quorum to lose, so this
        // rule cannot fire there. Publishing "not quorate" for a single node
        // would be a permanent false positive on the commonest install.
        if cfg.quorum && c.quorate == Some(false) {
            out.push(alert(
                obs.source,
                RULE_QUORUM,
                AlertSeverity::Critical,
                format!(
                    "the cluster is not quorate — {} of {} members online",
                    c.nodes_online(),
                    c.nodes.len()
                ),
                &[
                    ("nodes_online", c.nodes_online().to_string()),
                    ("nodes_total", c.nodes.len().to_string()),
                ],
            ));
        }
        if cfg.replication {
            for job in c.replication.iter().filter(|j| j.failed) {
                out.push(alert(
                    obs.source,
                    RULE_REPLICATION,
                    AlertSeverity::Warning,
                    format!(
                        "replication job {} failed: {}",
                        job.id,
                        job.error.as_deref().unwrap_or("no error text")
                    ),
                    &[
                        ("job", job.id.clone()),
                        (
                            "target",
                            job.target.clone().unwrap_or_else(|| "unknown".into()),
                        ),
                    ],
                ));
            }
        }
    }

    // ── node-rootfs-full / node-load-high / node-swapping (#1141) ───────────
    //
    // The hypervisor itself, which this sensor did not look at while it
    // reported every guest running on it. A node swapping, or with a full
    // rootfs, or with a load average four times its core count, was invisible
    // while every guest on it merely looked unhappy.
    for n in obs.nodes {
        let labels = [("node", n.name.clone())];

        // A full `/` stops PVE writing its own state, and no storage pool's
        // numbers contain it — `pool-usage` cannot see this.
        if cfg.node_rootfs_ratio > 0.0
            && let Some(ratio) = n.rootfs_ratio()
            && ratio >= cfg.node_rootfs_ratio
        {
            out.push(alert(
                obs.source,
                RULE_NODE_ROOTFS,
                AlertSeverity::Critical,
                format!(
                    "node {}: root filesystem {:.0}% full (threshold {:.0}%) — this is `/`, \
                     not a storage pool, and a full one stops PVE writing its own state",
                    n.name,
                    ratio * 100.0,
                    cfg.node_rootfs_ratio * 100.0
                ),
                &labels,
            ));
        }

        // Per CPU, because a raw load average means different things on a
        // 4-core and a 64-core node and one fleet-wide number has to mean one
        // thing.
        if cfg.node_load_per_cpu > 0.0
            && let Some(per_cpu) = n.load_per_cpu()
            && per_cpu >= cfg.node_load_per_cpu
        {
            out.push(alert(
                obs.source,
                RULE_NODE_LOAD,
                AlertSeverity::Warning,
                format!(
                    "node {}: load {:.2} over {} CPU(s) = {:.2} per CPU (threshold {:.2})",
                    n.name,
                    n.load1.unwrap_or_default(),
                    n.cpus.unwrap_or_default(),
                    per_cpu,
                    cfg.node_load_per_cpu
                ),
                &labels,
            ));
        }

        // `swap_ratio` is None on a node with no swap configured, so a
        // deliberate no-swap host never fires here.
        if cfg.node_swap_ratio > 0.0
            && let Some(ratio) = n.swap_ratio()
            && ratio >= cfg.node_swap_ratio
        {
            out.push(alert(
                obs.source,
                RULE_NODE_SWAP,
                AlertSeverity::Warning,
                format!(
                    "node {}: {:.0}% of swap in use (threshold {:.0}%) — a hypervisor that has \
                     started swapping is not visible in any guest's own numbers",
                    n.name,
                    ratio * 100.0,
                    cfg.node_swap_ratio * 100.0
                ),
                &labels,
            ));
        }
    }

    // ── backup-job-overdue (#1141) ──────────────────────────────────────────
    //
    // The assertion `backup-stale` cannot make. Staleness is measured against
    // a fixed age, so a job that was switched OFF, or whose schedule was
    // edited away, looks exactly like one that is merely young. A schedule
    // says when it was due.
    //
    // `next_run_ms` is PVE's own evaluation of the calendar spec — systemd's,
    // handed to us. A spec this build parsed itself would be a confident wrong
    // answer about when a backup was due, so a job whose release does not
    // report `next-run` is not graded at all.
    if cfg.backup_overdue_grace_secs > 0 {
        let grace_ms = (cfg.backup_overdue_grace_secs as i64) * 1000;
        for j in obs.schedules {
            // A DISABLED job is not overdue. It is switched off, which is a
            // different thing to tell an operator, and firing on it would
            // make every deliberately-paused job a standing alert.
            if !j.enabled {
                continue;
            }
            let Some(next) = j.next_run_ms else { continue };
            if obs.now_ms <= next + grace_ms {
                continue;
            }
            // Something ran since it was due? Then it is not overdue,
            // whatever the clock says.
            // `started_at` is epoch SECONDS (PVE's `starttime`); `next_run_ms`
            // is millis. Comparing them raw would put every task in 1970 and
            // make every enabled job permanently overdue.
            let ran_since = obs.backup_jobs.iter().any(|run| {
                run.last_task
                    .as_ref()
                    .is_some_and(|t| t.started_at * 1000 >= next)
            });
            if ran_since {
                continue;
            }
            let late_mins = (obs.now_ms - next) / 60_000;
            out.push(alert(
                obs.source,
                RULE_BACKUP_OVERDUE,
                AlertSeverity::Critical,
                format!(
                    "backup job {}{}: due {} minute(s) ago and nothing has run since{}",
                    j.id,
                    j.comment
                        .as_ref()
                        .map(|c| format!(" ({c})"))
                        .unwrap_or_default(),
                    late_mins,
                    j.schedule
                        .as_ref()
                        .map(|sch| format!(" — schedule `{sch}`"))
                        .unwrap_or_default()
                ),
                &[("job", j.id.clone())],
            ));
        }
    }

    // ── ceph-health (#1141) ─────────────────────────────────────────────────
    //
    // Ceph's OWN enum. Never a verdict derived from the OSD or PG counters
    // beside it — the same rule the BMC sensor follows about somebody else's
    // hardware, and for the same reason: Ceph knows what its numbers mean and
    // we do not.
    if cfg.ceph_health
        && let Some(c) = obs.ceph
        && c.is_faulted()
    {
        out.push(alert(
            obs.source,
            RULE_CEPH_HEALTH,
            if c.health == "HEALTH_ERR" {
                AlertSeverity::Critical
            } else {
                AlertSeverity::Warning
            },
            format!(
                "ceph: {}{}",
                c.health,
                if c.checks.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", c.checks.join(", "))
                }
            ),
            &[],
        ));
    }

    out
}

fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// Coarse, but never so coarse that an age and its limit read alike: "2 d
/// old (limit 2 d)" was the sentence for a 49 h backup against a 48 h limit.
fn human_secs(s: u64) -> String {
    match s {
        0..=3599 => format!("{} min", s / 60),
        3600..=172_799 => format!("{} h", s / 3600),
        _ => {
            let days = s as f64 / 86_400.0;
            if (days - days.round()).abs() < 0.05 {
                format!("{} d", days.round() as u64)
            } else {
                format!("{days:.1} d")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zensight_common::pve::{
        AllocationSource, GuestKind, GuestNic, PveBackupTask, PveBackupVolume, PveNodeStatus,
        PveReplicationJob,
    };

    /// The reporting hypervisor: every alert is filed under it, never under
    /// the guest or pool being reported on (#883).
    const HOST: &str = "sd-189169";

    fn guest(vmid: u32, onboot: bool, running: bool, fw: bool) -> PveGuest {
        PveGuest {
            vmid,
            name: Some(format!("vm-{vmid}")),
            node: "pve".into(),
            kind: GuestKind::Qemu,
            status: if running { "running" } else { "stopped" }.into(),
            uptime_secs: running.then_some(1000),
            template: false,
            onboot,
            protection: false,
            nics: vec![GuestNic {
                slot: "net0".into(),
                bridge: Some("vmbr0".into()),
                firewall: fw,
                mac: None,
                vlan_tag: None,
                model: None,
            }],
            disks: vec![],
            provisioned_bytes: None,
            observed_at_ms: 0,
        }
    }

    fn obs<'a>(guests: &'a [PveGuest]) -> Observation<'a> {
        Observation {
            source: HOST,
            guests,
            backup_jobs: &[],
            pools: &[],
            backups: &[],
            cluster: None,
            nodes: &[],
            schedules: &[],
            ceph: None,
            now_ms: 0,
        }
    }

    fn obs_with_cluster<'a>(
        guests: &'a [PveGuest],
        cluster: &'a PveClusterHealth,
    ) -> Observation<'a> {
        Observation {
            cluster: Some(cluster),
            nodes: &[],
            schedules: &[],
            ceph: None,
            now_ms: 0,
            ..obs(guests)
        }
    }

    fn cluster(quorate: Option<bool>, nodes: Vec<PveNodeStatus>) -> PveClusterHealth {
        PveClusterHealth {
            name: Some("cl".into()),
            quorate,
            nodes,
            ha: vec![],
            replication: vec![],
            guests_total: 1,
            guests_running: 0,
            observed_at_ms: 0,
        }
    }

    fn node(name: &str, online: bool) -> PveNodeStatus {
        PveNodeStatus {
            name: name.into(),
            online,
            local: false,
            ip: None,
        }
    }

    /// **#1132, the acceptance.** `/cluster/resources` on a node that has lost
    /// quorum still answers, and reports the guests on the far side of the
    /// partition as `status: "unknown"` — which `is_running()` reads as "not
    /// running". A ninety-second corosync blip therefore fired a **critical**
    /// `guest-not-running` for every VM in the cluster, none of which had
    /// stopped.
    #[test]
    fn a_non_quorate_sweep_fires_only_the_quorum_rule() {
        // Set to start at boot, and reported as `unknown` — exactly what a
        // guest on the far side of a partition looks like.
        let mut g = guest(140, true, false, true);
        g.status = "unknown".into();
        let guests = [g];
        let c = cluster(Some(false), vec![node("pve", true), node("pve2", true)]);

        assert_eq!(
            rules(&grade(
                &PveAlertsConfig::default(),
                &obs_with_cluster(&guests, &c)
            )),
            vec![RULE_QUORUM],
            "a node without quorum is not entitled to an opinion about a \
             guest it cannot see"
        );
    }

    /// The same guest, the same reply, with quorum: now it IS a fault.
    #[test]
    fn a_quorate_sweep_still_grades_the_guest() {
        let mut g = guest(140, true, false, true);
        g.status = "unknown".into();
        let guests = [g];
        let c = cluster(Some(true), vec![node("pve", true)]);
        assert!(
            rules(&grade(
                &PveAlertsConfig::default(),
                &obs_with_cluster(&guests, &c)
            ))
            .contains(&RULE_NOT_RUNNING),
            "with quorum, `unknown` is the hypervisor's answer and not a \
             partition — the guard must not swallow a real fault"
        );
    }

    /// Quorum is a cluster-wide hold; an offline **node** is a narrower one.
    /// A guest on a node the cluster lists as down is unreachable, not
    /// stopped — and a guest on a node that is up is graded as usual in the
    /// same sweep.
    #[test]
    fn an_offline_node_holds_only_its_own_guests() {
        let mut far = guest(140, true, false, true);
        far.node = "pve2".into();
        far.status = "unknown".into();
        let near = guest(141, false, true, true); // onboot off: a real finding
        let guests = [far, near];
        let c = cluster(Some(true), vec![node("pve", true), node("pve2", false)]);

        let fired = grade(&PveAlertsConfig::default(), &obs_with_cluster(&guests, &c));
        assert_eq!(rules(&fired), vec![RULE_ONBOOT]);
        assert_eq!(
            fired[0].labels["vmid"], "141",
            "the finding must be the one on the node that answered"
        );
    }

    /// A standalone node has no quorum to have or lose, and must not be held
    /// by a guard built for clusters.
    #[test]
    fn a_standalone_node_is_always_observable() {
        assert!(guest_is_observable(None, "pve"));
        let c = cluster(None, vec![]);
        assert!(guest_is_observable(Some(&c), "pve"));
    }

    fn rules(alerts: &[Alert]) -> Vec<&str> {
        let mut r: Vec<&str> = alerts.iter().map(|a| a.rule.as_str()).collect();
        r.sort_unstable();
        r
    }

    /// VM 140 as the audit found it, end to end: onboot off and a NIC whose
    /// firewall file is inert. Two alerts, from one guest, on facts no gauge
    /// would ever have shown.
    #[test]
    fn the_audits_guest_findings_fire() {
        let g = [guest(140, false, true, false)];
        let a = grade(&PveAlertsConfig::default(), &obs(&g));
        assert_eq!(rules(&a), vec![RULE_NIC_FIREWALL, RULE_ONBOOT]);
        let fw = a.iter().find(|x| x.rule == RULE_NIC_FIREWALL).unwrap();
        assert_eq!(fw.labels["nic"], "net0");
        assert_eq!(fw.labels["vmid"], "140");
        assert_eq!(fw.labels["name"], "vm-140");
        assert_eq!(
            fw.source, HOST,
            "the reporting hypervisor is the source; the vmid is a label (#883)"
        );
    }

    #[test]
    fn a_healthy_guest_fires_nothing() {
        let g = [guest(101, true, true, true)];
        assert!(grade(&PveAlertsConfig::default(), &obs(&g)).is_empty());
    }

    /// A guest deliberately left off is not a fault; `onboot` is the only
    /// machine-readable statement of intent Proxmox has, so "should be up"
    /// means "onboot=1".
    #[test]
    fn not_running_needs_onboot_to_mean_anything() {
        let off_on_purpose = [guest(200, false, false, true)];
        assert!(
            !rules(&grade(&PveAlertsConfig::default(), &obs(&off_on_purpose)))
                .contains(&RULE_NOT_RUNNING)
        );
        let should_be_up = [guest(201, true, false, true)];
        assert!(
            rules(&grade(&PveAlertsConfig::default(), &obs(&should_be_up)))
                .contains(&RULE_NOT_RUNNING)
        );
    }

    /// A template is a stamp, not a guest. Asserting onboot on one would fire
    /// on every install that has ever made a template.
    #[test]
    fn templates_are_skipped_entirely() {
        let mut t = guest(9000, false, false, false);
        t.template = true;
        assert!(grade(&PveAlertsConfig::default(), &obs(&[t])).is_empty());
    }

    #[test]
    fn exempt_vmids_are_skipped() {
        let cfg = PveAlertsConfig {
            exempt_vmids: vec![140],
            ..Default::default()
        };
        assert!(grade(&cfg, &obs(&[guest(140, false, true, false)])).is_empty());
    }

    /// 990 GB promised against 937 GB of capacity, with `used` showing nothing
    /// wrong. The pool fills on its own schedule and no edit precedes it.
    #[test]
    fn a_pool_over_commitment_fires_while_usage_looks_fine() {
        let pools = [PveStoragePool {
            storage: "local-lvm".into(),
            node: "pve".into(),
            kind: Some("lvmthin".into()),
            active: true,
            enabled: true,
            shared: false,
            total_bytes: 937 * 1024 * 1024 * 1024,
            used_bytes: 300 * 1024 * 1024 * 1024,
            avail_bytes: 637 * 1024 * 1024 * 1024,
            allocated_bytes: Some(990 * 1024 * 1024 * 1024),
            allocated_source: Some(AllocationSource::Reported),
            overcommit_ratio: Some(990.0 / 937.0),
            content: vec![],
            observed_at_ms: 0,
        }];
        let o = Observation {
            pools: &pools,
            ..obs(&[])
        };
        let a = grade(&PveAlertsConfig::default(), &o);
        assert_eq!(rules(&a), vec![RULE_POOL_OVERCOMMIT], "usage is only 32%");
        assert_eq!(a[0].labels["ratio"], "1.057");
        assert_eq!(a[0].labels["storage"], "local-lvm");
        assert_eq!(
            a[0].source, HOST,
            "the reporting hypervisor is the source; the pool is a label (#883)"
        );
    }

    fn backup(
        vmid: u32,
        ok: bool,
        latest: u64,
        previous: Option<u64>,
        age: u64,
    ) -> PveBackupSummary {
        let change = previous.map(|p| (latest as f64 - p as f64) / p as f64 * 100.0);
        PveBackupSummary {
            vmid,
            last_task: Some(PveBackupTask {
                upid: "UPID:pve:1".into(),
                node: "pve".into(),
                exit_status: Some(if ok { "OK".into() } else { "job failed".into() }),
                ok,
                started_at: 0,
                duration_secs: Some(120),
            }),
            latest: Some(PveBackupVolume {
                volid: "pbs:backup/vm/140".into(),
                storage: "pbs".into(),
                size_bytes: latest,
                created_at: 0,
                protected: None,
            }),
            previous: previous.map(|p| PveBackupVolume {
                volid: "pbs:backup/vm/140-prev".into(),
                storage: "pbs".into(),
                size_bytes: p,
                created_at: 0,
                protected: None,
            }),
            size_change_pct: change,
            age_secs: Some(age),
            volumes: Some(2),
            observed_at_ms: 0,
        }
    }

    /// The whole point of reading backup *sizes*: this job exited 0. The mail
    /// notification said so. It also halved.
    #[test]
    fn a_backup_that_succeeds_while_shrinking_fires() {
        let b = [backup(140, true, 5_000_000, Some(10_000_000), 3600)];
        let o = Observation {
            backups: &b,
            ..obs(&[])
        };
        let a = grade(&PveAlertsConfig::default(), &o);
        assert_eq!(rules(&a), vec![RULE_BACKUP_SHRUNK]);
        assert_eq!(a[0].labels["change_pct"], "-50.0");
        assert!(
            a[0].summary.contains("succeeded and shrank"),
            "{}",
            a[0].summary
        );
    }

    #[test]
    fn a_backup_that_grew_or_held_steady_fires_nothing() {
        for (latest, prev) in [(10_000_000u64, 10_000_000u64), (12_000_000, 10_000_000)] {
            let b = [backup(1, true, latest, Some(prev), 60)];
            let o = Observation {
                backups: &b,
                ..obs(&[])
            };
            assert!(grade(&PveAlertsConfig::default(), &o).is_empty());
        }
    }

    #[test]
    fn a_failed_backup_task_fires_with_its_exit_status() {
        let b = [backup(140, false, 1, None, 60)];
        let o = Observation {
            backups: &b,
            ..obs(&[])
        };
        let a = grade(&PveAlertsConfig::default(), &o);
        assert_eq!(rules(&a), vec![RULE_BACKUP_FAILED]);
        assert_eq!(a[0].labels["exit_status"], "job failed");
    }

    /// Off by default: backup cadence is deployment policy, and a wrong
    /// default here is a nightly false positive.
    #[test]
    fn staleness_fires_only_once_a_limit_is_configured() {
        let b = [backup(140, true, 10, None, 90_000)];
        let o = Observation {
            backups: &b,
            ..obs(&[])
        };
        assert!(grade(&PveAlertsConfig::default(), &o).is_empty());
        let cfg = PveAlertsConfig {
            backup_stale_secs: 86_400,
            ..Default::default()
        };
        assert_eq!(rules(&grade(&cfg, &o)), vec![RULE_BACKUP_STALE]);
    }

    /// The commonest Proxmox install is one node. It has no quorum to lose,
    /// and reporting "not quorate" there would be a permanent false positive.
    #[test]
    fn a_standalone_node_can_never_fire_the_quorum_rule() {
        let c = PveClusterHealth {
            name: None,
            quorate: None,
            nodes: vec![PveNodeStatus {
                name: "pve".into(),
                online: true,
                local: true,
                ip: None,
            }],
            ha: vec![],
            replication: vec![],
            guests_total: 3,
            guests_running: 3,
            observed_at_ms: 0,
        };
        let o = Observation {
            cluster: Some(&c),
            ..obs(&[])
        };
        assert!(grade(&PveAlertsConfig::default(), &o).is_empty());
    }

    #[test]
    fn a_cluster_that_lost_quorum_fires_critical() {
        let c = PveClusterHealth {
            name: Some("prod".into()),
            quorate: Some(false),
            nodes: vec![
                PveNodeStatus {
                    name: "a".into(),
                    online: true,
                    local: true,
                    ip: None,
                },
                PveNodeStatus {
                    name: "b".into(),
                    online: false,
                    local: false,
                    ip: None,
                },
            ],
            ha: vec![],
            replication: vec![PveReplicationJob {
                id: "140-0".into(),
                guest: Some(140),
                target: Some("b".into()),
                failed: true,
                last_sync: None,
                error: Some("connection refused".into()),
            }],
            guests_total: 1,
            guests_running: 1,
            observed_at_ms: 0,
        };
        let o = Observation {
            cluster: Some(&c),
            ..obs(&[])
        };
        let a = grade(&PveAlertsConfig::default(), &o);
        assert_eq!(rules(&a), vec![RULE_QUORUM, RULE_REPLICATION]);
        assert_eq!(
            a.iter().find(|x| x.rule == RULE_QUORUM).unwrap().severity,
            AlertSeverity::Critical
        );
    }

    /// Switching the block off must silence everything, so a deployment that
    /// wants telemetry without opinions can have it.
    #[test]
    fn disabled_grades_nothing() {
        let cfg = PveAlertsConfig {
            enabled: false,
            ..Default::default()
        };
        assert!(grade(&cfg, &obs(&[guest(140, false, false, false)])).is_empty());
    }

    /// Every rule the grader can emit must be in ALL_RULES, or the poller
    /// never reconciles it and a resolved condition keeps firing forever.
    ///
    /// Graded **twice** since #1132: `cluster-not-quorate` needs
    /// `quorate: Some(false)`, and that is exactly the state in which the
    /// guest rules are held — the two can no longer be observed in one sweep,
    /// which is the point of the guard. The union is what this test is about.
    #[test]
    fn every_emitted_rule_is_reconciled() {
        // Two guests, because `onboot=0` and "set to start at boot but
        // stopped" are mutually exclusive on a single one.
        let g = [
            guest(140, false, false, false),
            guest(141, true, false, true),
        ];
        let pools = [PveStoragePool {
            storage: "s".into(),
            node: "pve".into(),
            kind: None,
            active: true,
            enabled: true,
            shared: false,
            total_bytes: 100,
            used_bytes: 99,
            avail_bytes: 1,
            allocated_bytes: Some(200),
            allocated_source: Some(AllocationSource::Reported),
            overcommit_ratio: Some(2.0),
            content: vec![],
            observed_at_ms: 0,
        }];
        let b = [backup(140, false, 1, Some(100), 999_999)];
        let c = PveClusterHealth {
            name: None,
            quorate: Some(false),
            nodes: vec![],
            ha: vec![],
            replication: vec![PveReplicationJob {
                id: "j".into(),
                guest: None,
                target: None,
                failed: true,
                last_sync: None,
                error: Some("x".into()),
            }],
            guests_total: 1,
            guests_running: 0,
            observed_at_ms: 0,
        };
        let cfg = PveAlertsConfig {
            backup_stale_secs: 60,
            ..Default::default()
        };
        // A whole-job run that failed: one job-scoped alert, not one per guest.
        let jobs = [PveBackupJob {
            node: "pve".into(),
            last_task: Some(PveBackupTask {
                upid: "UPID:pve:...:vzdump::root@pam:".into(),
                node: "pve".into(),
                exit_status: Some("job errors".into()),
                ok: false,
                started_at: 0,
                duration_secs: Some(30),
            }),
            age_secs: Some(60),
            observed_at_ms: 0,
        }];
        let quorate = PveClusterHealth {
            quorate: Some(true),
            ..c.clone()
        };

        // The #1141 surfaces, each in the state that makes its rule fire.
        let nodes = [PveNode {
            name: "pve".into(),
            uptime_secs: Some(1000),
            cpu_ratio: Some(0.9),
            cpus: Some(4),
            mem_bytes: Some(90),
            mem_total_bytes: Some(100),
            swap_bytes: Some(90),
            swap_total_bytes: Some(100),
            rootfs_bytes: Some(99),
            rootfs_total_bytes: Some(100),
            load1: Some(40.0),
            load5: None,
            load15: None,
            pve_version: None,
            kernel: None,
            observed_at_ms: 0,
        }];
        // Enabled, due an hour before `now_ms`, with nothing run since.
        let schedules = [PveBackupSchedule {
            id: "backup-0001".into(),
            enabled: true,
            schedule: Some("mon..fri 03:00".into()),
            next_run_ms: Some(1),
            node: None,
            storage: None,
            comment: None,
            guests: None,
            all_guests: true,
            observed_at_ms: 0,
        }];
        let ceph = PveCephStatus {
            health: "HEALTH_ERR".into(),
            checks: vec!["OSD_DOWN".into()],
            osds_total: Some(3),
            osds_up: Some(2),
            osds_in: Some(3),
            monitors_total: Some(3),
            monitors_quorum: Some(3),
            pgs_total: Some(128),
            pgs_degraded: Some(4),
            bytes_used: Some(50),
            bytes_total: Some(100),
            observed_at_ms: 0,
        };

        let mut fired: std::collections::HashSet<String> = std::collections::HashSet::new();
        for cluster in [&c, &quorate] {
            let a = grade(
                &cfg,
                &Observation {
                    backup_jobs: &jobs,
                    source: "pve",
                    guests: &g,
                    pools: &pools,
                    backups: &b,
                    cluster: Some(cluster),
                    nodes: &nodes,
                    schedules: &schedules,
                    ceph: Some(&ceph),
                    // Well past the schedule's `next_run_ms` plus the grace.
                    now_ms: 10_000_000,
                },
            );
            fired.extend(a.iter().map(|x| x.rule.clone()));
        }
        assert_eq!(fired.len(), ALL_RULES.len(), "fired: {fired:?}");
        for r in &fired {
            assert!(ALL_RULES.contains(&r.as_str()), "{r} is not reconciled");
        }
    }
}
