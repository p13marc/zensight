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
    PveBackupJob, PveBackupSummary, PveClusterHealth, PveGuest, PveStoragePool,
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

/// Grade one sweep. Returns every currently-firing alert; the caller
/// reconciles per rule, so anything absent here resolves.
pub fn grade(cfg: &PveAlertsConfig, obs: &Observation<'_>) -> Vec<Alert> {
    let mut out = Vec::new();
    if !cfg.enabled {
        return out;
    }

    for g in obs.guests {
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
        }
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
        let a = grade(
            &cfg,
            &Observation {
                backup_jobs: &jobs,
                source: "pve",
                guests: &g,
                pools: &pools,
                backups: &b,
                cluster: Some(&c),
            },
        );
        let fired: std::collections::HashSet<&str> = a.iter().map(|x| x.rule.as_str()).collect();
        assert_eq!(fired.len(), ALL_RULES.len(), "fired: {fired:?}");
        for r in &fired {
            assert!(ALL_RULES.contains(r), "{r} is not reconciled");
        }
    }
}
