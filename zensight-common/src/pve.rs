//! Proxmox VE wire types (#818).
//!
//! The hypervisor is the one machine whose failure is total, and ZenSight saw
//! it as a Linux box: three native binaries reporting CPU, memory, disks,
//! units and the journal. Everything that made it a *hypervisor* was
//! invisible, and the 2026-08-28 audit of the reference fleet found three
//! things by hand that a sensor could have been asserting continuously — a
//! guest with `onboot=0` that would not have survived a host reboot, a NIC
//! with `firewall=0` that made an entire firewall file inert, and 990 GB
//! provisioned on a 937 GB pool.
//!
//! None of those is a metric that spikes. They are **configuration facts that
//! stopped matching intent**, so they belong in state documents with real
//! schemas rather than in a gauge — which is also what the #815 gate requires
//! of a state-class payload, and why these types live here rather than in the
//! sensor crate (the hostspec precedent, #816).
//!
//! Everything here is *observation*. The sensor has no write surface at all:
//! a monitor that can stop a VM is a different threat model and would be a
//! separate, deliberate decision.

use serde::{Deserialize, Serialize};

use schemars::JsonSchema;

/// Which guest technology a VM is. Proxmox's own words (`qemu` / `lxc`), kept
/// verbatim because they are what every other PVE tool prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum GuestKind {
    Qemu,
    Lxc,
}

impl GuestKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            GuestKind::Qemu => "qemu",
            GuestKind::Lxc => "lxc",
        }
    }
}

impl std::fmt::Display for GuestKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// One virtual NIC as the guest *config* declares it.
///
/// `firewall` is the field the audit was about: Proxmox's per-guest firewall
/// rules live in `/etc/pve/firewall/<vmid>.fw`, but they are only applied to
/// interfaces whose config line carries `firewall=1`. With the flag off the
/// rules file is inert — present, readable, reviewed, and doing nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GuestNic {
    /// Config slot (`net0`, `net1`, …).
    pub slot: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge: Option<String>,
    /// `firewall=1` on this interface. **False means the guest's `.fw` file
    /// does not apply to it**, whatever that file says.
    pub firewall: bool,
    /// The configured MAC. Identity evidence: it is what lets the hypervisor's
    /// view of a guest fuse with that guest's own sensors in the catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vlan_tag: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// One virtual disk as the guest *config* declares it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GuestDisk {
    /// Config slot (`scsi0`, `virtio1`, `rootfs`, `mp0`, …).
    pub slot: String,
    /// `<storage>:<volume>`, verbatim.
    pub volid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<String>,
    /// Declared (provisioned) size. Thin volumes consume less than this today
    /// and up to this eventually, which is the whole over-commitment question.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    /// `backup=0` excludes this disk from vzdump. A new disk that should have
    /// been excluded and was not shows up here before the pool notices.
    pub backup: bool,
}

/// A guest, as the hypervisor knows it: runtime status joined with the
/// configuration facts that decide what happens at the *next* reboot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PveGuest {
    pub vmid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub node: String,
    pub kind: GuestKind,
    /// `running` / `stopped` / `paused`, Proxmox's own vocabulary.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime_secs: Option<u64>,
    /// A template is not a runnable guest; every assertion below skips them.
    #[serde(default)]
    pub template: bool,
    /// `onboot=1`. **The 2026-08-28 finding**: a guest with this off does not
    /// come back after a host reboot, and nobody learns that until they look.
    pub onboot: bool,
    /// `protection=1` — refuses destroy/remove operations.
    #[serde(default)]
    pub protection: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nics: Vec<GuestNic>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disks: Vec<GuestDisk>,
    /// Sum of `disks[].size_bytes` — what this guest has been promised.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provisioned_bytes: Option<u64>,
    pub observed_at_ms: i64,
}

impl PveGuest {
    /// Every NIC whose firewall flag is off, by slot. Empty is the good case.
    pub fn nics_without_firewall(&self) -> Vec<&str> {
        self.nics
            .iter()
            .filter(|n| !n.firewall)
            .map(|n| n.slot.as_str())
            .collect()
    }

    /// Disks vzdump will skip, by slot.
    pub fn disks_excluded_from_backup(&self) -> Vec<&str> {
        self.disks
            .iter()
            .filter(|d| !d.backup)
            .map(|d| d.slot.as_str())
            .collect()
    }

    pub fn is_running(&self) -> bool {
        self.status == "running"
    }
}

/// A storage pool: capacity, what is actually used, and what has been
/// *promised* to thin volumes.
///
/// `allocated_bytes` is the number the audit needed and no dashboard showed:
/// 990 GB provisioned on a 937 GB pool is not visible in `used`, does not
/// change when nothing is reconfigured, and fills the pool on its own
/// schedule.
/// Where a pool's `allocated_bytes` came from.
///
/// PVE surfaces a per-volume size for LVM-thin and ZFS, and nothing for a
/// `dir` storage — so on the storage type the reference deployment actually
/// runs, the number that this sensor's headline finding depends on ("990 GB
/// provisioned on a 937 GB pool") simply is not reported (#881). It can still
/// be *derived*, because the sensor already reads every guest's disk lines and
/// every disk names its storage. Labelling which is which is not pedantry: a
/// derived total is a **floor**, because a volume no guest currently attaches
/// (`unused<N>`) still occupies the pool and is deliberately not counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AllocationSource {
    /// The storage plugin's own per-volume sizes, summed.
    Reported,
    /// Summed from the disks of the guests that live on this pool. A floor:
    /// detached `unused<N>` volumes and disks with no `size=` are not counted.
    DerivedFromGuests,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PveStoragePool {
    pub storage: String,
    pub node: String,
    /// Plugin type (`lvmthin`, `zfspool`, `dir`, `pbs`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub active: bool,
    pub enabled: bool,
    #[serde(default)]
    pub shared: bool,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub avail_bytes: u64,
    /// Sum of the declared sizes of the volumes this pool holds. `None` when
    /// the pool's content could not be listed — never zero, which would read
    /// as "nothing provisioned" and is the one wrong answer here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allocated_bytes: Option<u64>,
    /// Where [`allocated_bytes`](Self::allocated_bytes) came from. `None` when
    /// there is no number at all. The two sources are never conflated: a
    /// derived total is a floor, not a measurement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allocated_source: Option<AllocationSource>,
    /// `allocated_bytes / total_bytes`. Above 1.0 the pool is over-committed:
    /// it can fill with no configuration change whatsoever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overcommit_ratio: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<String>,
    pub observed_at_ms: i64,
}

impl PveStoragePool {
    pub fn used_ratio(&self) -> f64 {
        if self.total_bytes == 0 {
            0.0
        } else {
            self.used_bytes as f64 / self.total_bytes as f64
        }
    }
}

/// The outcome of one vzdump task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PveBackupTask {
    pub upid: String,
    pub node: String,
    /// `OK` on success; anything else is the failure text Proxmox recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_status: Option<String>,
    pub ok: bool,
    pub started_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u64>,
}

/// One stored backup volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PveBackupVolume {
    pub volid: String,
    pub storage: String,
    pub size_bytes: u64,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protected: Option<bool>,
}

/// The outcome of one **whole-job** vzdump run — a job configured `all 1`,
/// which covers every guest and therefore names none.
///
/// PVE records such a run as a single task with an empty `id`; the per-guest
/// results exist only inside the task log, as free text. So this sensor grades
/// the job as what it is — one fact, one alert — and answers "was *this guest*
/// backed up?" from the stored volumes instead (#880). Seven false
/// `backup-failed` criticals, one per guest, is the alternative.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PveBackupJob {
    /// The node the job ran on.
    pub node: String,
    /// The newest completed whole-job run. `None` when the task window holds
    /// none — which is a state, not a failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_task: Option<PveBackupTask>,
    /// Age of `last_task`, seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_secs: Option<u64>,
    pub observed_at_ms: i64,
}

/// What is known about one guest's backups.
///
/// "The job exited 0" is what the existing mail notification already says.
/// The field worth having is `size_change_pct`: **a backup that succeeds
/// while shrinking** is the failure mode a green exit code cannot show.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PveBackupSummary {
    pub vmid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_task: Option<PveBackupTask>,
    /// Newest stored volume for this guest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<PveBackupVolume>,
    /// The one before it — the comparison baseline, taken from the store
    /// rather than latched in memory, so a sensor restart does not reset it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<PveBackupVolume>,
    /// `(latest - previous) / previous * 100`. Negative means it shrank.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_change_pct: Option<f64>,
    /// Age of `latest`, seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_secs: Option<u64>,
    /// How many stored volumes this guest has. `None` when no backup-capable
    /// pool could be listed at all — the listing was refused, failed, or there
    /// was nothing to ask. A `0` here says "this guest has no backups", which
    /// is a very different claim from "we could not look", and the reference
    /// deployment saw the second reported as the first (#880).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volumes: Option<u32>,
    pub observed_at_ms: i64,
}

/// One node's own resource picture, from `/nodes/{node}/status` (#1141).
///
/// The sensor saw guests and pools and **not the hypervisor** — which is the
/// first thing anyone looks at when a guest is slow. A node swapping, or with
/// a full rootfs, or with a load average four times its core count, was
/// invisible while every guest on it looked merely unhappy.
///
/// Every field is optional because PVE's node status shape has moved across
/// releases and a field this build does not find is a **missing** reading, not
/// a zero.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PveNode {
    /// The node name, and the chunk in the key.
    pub name: String,
    /// Seconds since boot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime_secs: Option<u64>,
    /// CPU utilisation, 0..1 as PVE reports it — **not per-core**, the same
    /// convention `guest/{vmid}/cpu_ratio` already uses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_ratio: Option<f64>,
    /// How many CPUs the node has, so a load average can be read against it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_total_bytes: Option<u64>,
    /// Swap **in use**. A hypervisor that has started swapping is the reading
    /// a guest's own numbers cannot show.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap_total_bytes: Option<u64>,
    /// The node's **root filesystem**, not a storage pool. A full `/` stops
    /// PVE writing its own state and is not visible in any pool's numbers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rootfs_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rootfs_total_bytes: Option<u64>,
    /// 1-, 5- and 15-minute load averages, as the node reports them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load1: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load5: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load15: Option<f64>,
    /// PVE version string, for the fleet-wide "what is out of date" question.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pve_version: Option<String>,
    /// Kernel release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<String>,
    pub observed_at_ms: i64,
}

impl PveNode {
    /// Root filesystem used fraction, 0..1. `None` when either half is
    /// missing — a ratio against a total we do not have is not a ratio.
    pub fn rootfs_ratio(&self) -> Option<f64> {
        let total = self.rootfs_total_bytes?;
        let used = self.rootfs_bytes?;
        (total > 0).then(|| used as f64 / total as f64)
    }

    /// Swap used fraction, 0..1. `None` on a node with no swap configured,
    /// which is a normal and deliberate configuration rather than 0 % used.
    pub fn swap_ratio(&self) -> Option<f64> {
        let total = self.swap_total_bytes?;
        let used = self.swap_bytes?;
        (total > 0).then(|| used as f64 / total as f64)
    }

    /// Load average per CPU, which is the number that means something across
    /// nodes of different sizes.
    pub fn load_per_cpu(&self) -> Option<f64> {
        let cpus = self.cpus?;
        let load = self.load1?;
        (cpus > 0).then(|| load / f64::from(cpus))
    }
}

/// One scheduled vzdump job, from `/cluster/backup` (#1141).
///
/// The sensor could say "the last backup ran N seconds ago" and **not** "a
/// backup that should have run at 03:00 did not run at all" — the two are
/// different questions, and a job that was disabled, or whose schedule was
/// edited away, looks identical to one that is merely young.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PveBackupSchedule {
    /// The job id PVE assigns, and the chunk in the key.
    pub id: String,
    /// Whether the job is enabled. A **disabled** job whose last run is old is
    /// not overdue; it is switched off, which is a different thing to tell an
    /// operator.
    pub enabled: bool,
    /// The systemd calendar spec, verbatim (`"mon..fri 03:00"`). Recorded as
    /// the string PVE holds rather than parsed: a calendar spec this build
    /// evaluated differently from systemd would be a confident wrong answer
    /// about when a backup was due.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
    /// When PVE itself says the job next runs, epoch millis. This is the
    /// authority on the schedule — it is systemd's own evaluation, handed to
    /// us — and `None` on a release that does not report it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run_ms: Option<i64>,
    /// Which node the job is pinned to, when it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// The storage the job writes to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<String>,
    /// The job's comment, which is what an operator named it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// How many guests the job selects, where the API says. `None` is "we
    /// could not tell", never "none" — a job that backs up nothing is a real
    /// and serious state, and must not be confused with an unread field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guests: Option<u32>,
    /// `all: 1` — the job takes every guest rather than a list.
    #[serde(default)]
    pub all_guests: bool,
    pub observed_at_ms: i64,
}

/// Ceph's own health verdict, from `/cluster/ceph/status` (#1141).
///
/// Absent on every cluster that does not run Ceph, which is most of them —
/// the endpoint answers 501 or 404 and that is a fact about the cluster, not a
/// failed poll.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PveCephStatus {
    /// `HEALTH_OK` / `HEALTH_WARN` / `HEALTH_ERR`, **Ceph's own enum**, never
    /// a verdict this sensor derived from the counters below.
    pub health: String,
    /// The health check names Ceph is currently raising
    /// (`OSD_DOWN`, `PG_DEGRADED`, …). What an operator acts on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub osds_total: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub osds_up: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub osds_in: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitors_total: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitors_quorum: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pgs_total: Option<u32>,
    /// Placement groups **not** in `active+clean`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pgs_degraded: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_used: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_total: Option<u64>,
    pub observed_at_ms: i64,
}

impl PveCephStatus {
    /// Whether Ceph itself says something is wrong. Never our own reading of
    /// the counters — the same rule the BMC sensor follows.
    pub fn is_faulted(&self) -> bool {
        self.health == "HEALTH_WARN" || self.health == "HEALTH_ERR"
    }
}

/// One cluster member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PveNodeStatus {
    pub name: String,
    pub online: bool,
    #[serde(default)]
    pub local: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
}

/// One HA-managed resource. Empty on a single-node install, which is not a
/// fault — HA is simply not configured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PveHaResource {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// One replication job's last result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PveReplicationJob {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub failed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sync: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Cluster-wide health. On a single-node install `quorate` is `None` and the
/// node list has one entry — that is a complete answer, not a degraded one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PveClusterHealth {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `None` on a standalone node: there is no quorum to have or lose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quorate: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<PveNodeStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ha: Vec<PveHaResource>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replication: Vec<PveReplicationJob>,
    pub guests_total: u32,
    pub guests_running: u32,
    pub observed_at_ms: i64,
}

impl PveClusterHealth {
    pub fn nodes_online(&self) -> u32 {
        self.nodes.iter().filter(|n| n.online).count() as u32
    }
}

/// The head token and the `key=value` tail of one Proxmox config line.
pub type KvList<'a> = (Option<(&'a str, &'a str)>, Vec<(&'a str, &'a str)>);

/// Parse a Proxmox config value list — `virtio=AA:BB:…,bridge=vmbr0,firewall=1`
/// — into its `key=value` pairs plus the leading bare/`k=v` head token.
///
/// Proxmox writes these lines in several shapes (`net0` leads with
/// `<model>=<mac>`, `scsi0` leads with a bare `<storage>:<volume>`), so the
/// head is returned separately and interpreted by the caller rather than
/// guessed at here.
pub fn parse_kv_list(raw: &str) -> KvList<'_> {
    let mut parts = raw.split(',').map(str::trim).filter(|p| !p.is_empty());
    let head = parts.next().map(|h| match h.split_once('=') {
        Some((k, v)) => (k, v),
        // A bare token (`local-lvm:vm-100-disk-0`) — no key.
        None => ("", h),
    });
    let rest = parts.filter_map(|p| p.split_once('=')).collect();
    (head, rest)
}

/// Proxmox size suffixes on config lines (`size=32G`) → bytes.
pub fn parse_size(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    let (num, mult) = match raw.chars().last()? {
        'K' | 'k' => (&raw[..raw.len() - 1], 1024u64),
        'M' | 'm' => (&raw[..raw.len() - 1], 1024 * 1024),
        'G' | 'g' => (&raw[..raw.len() - 1], 1024 * 1024 * 1024),
        'T' | 't' => (&raw[..raw.len() - 1], 1024u64.pow(4)),
        _ => (raw, 1),
    };
    num.trim()
        .parse::<f64>()
        .ok()
        .map(|n| (n * mult as f64) as u64)
}

/// `1`/`0`/`yes`/`true` → bool, the way Proxmox writes flags.
pub fn parse_flag(raw: &str) -> bool {
    matches!(raw.trim(), "1" | "yes" | "true" | "on")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nic_line_yields_mac_bridge_and_the_firewall_flag() {
        let (head, rest) = parse_kv_list("virtio=AA:BB:CC:DD:EE:FF,bridge=vmbr1,firewall=1,tag=42");
        assert_eq!(head, Some(("virtio", "AA:BB:CC:DD:EE:FF")));
        let map: std::collections::HashMap<_, _> = rest.into_iter().collect();
        assert_eq!(map.get("bridge"), Some(&"vmbr1"));
        assert!(parse_flag(map["firewall"]));
        assert_eq!(map.get("tag"), Some(&"42"));
    }

    /// The finding this sensor exists for: a NIC with the flag ABSENT is a NIC
    /// the guest's firewall file does not apply to. Absent and `firewall=0`
    /// must read identically — Proxmox omits the key when it is 0.
    #[test]
    fn an_absent_firewall_key_is_not_a_firewalled_nic() {
        let (_, rest) = parse_kv_list("virtio=AA:BB:CC:DD:EE:FF,bridge=vmbr0");
        let map: std::collections::HashMap<_, _> = rest.into_iter().collect();
        assert!(!map.contains_key("firewall"));
        assert!(!parse_flag(map.get("firewall").copied().unwrap_or("0")));
    }

    #[test]
    fn a_disk_line_leads_with_a_bare_volid() {
        let (head, rest) = parse_kv_list("local-lvm:vm-140-disk-0,size=32G,backup=0,ssd=1");
        assert_eq!(head, Some(("", "local-lvm:vm-140-disk-0")));
        let map: std::collections::HashMap<_, _> = rest.into_iter().collect();
        assert_eq!(parse_size(map["size"]), Some(32 * 1024 * 1024 * 1024));
        assert!(!parse_flag(map["backup"]));
    }

    #[test]
    fn sizes_carry_their_suffix() {
        assert_eq!(parse_size("512"), Some(512));
        assert_eq!(parse_size("8G"), Some(8 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("1.5T"), Some(1_649_267_441_664));
        assert_eq!(parse_size("nonsense"), None);
    }

    #[test]
    fn a_guest_reports_its_unfirewalled_nics_and_unbacked_disks() {
        let g = PveGuest {
            vmid: 140,
            name: Some("vm-apps".into()),
            node: "pve".into(),
            kind: GuestKind::Qemu,
            status: "running".into(),
            uptime_secs: Some(3600),
            template: false,
            onboot: false,
            protection: false,
            nics: vec![
                GuestNic {
                    slot: "net0".into(),
                    bridge: Some("vmbr0".into()),
                    firewall: false,
                    mac: Some("AA:BB:CC:DD:EE:FF".into()),
                    vlan_tag: None,
                    model: Some("virtio".into()),
                },
                GuestNic {
                    slot: "net1".into(),
                    bridge: Some("vmbr1".into()),
                    firewall: true,
                    mac: None,
                    vlan_tag: None,
                    model: None,
                },
            ],
            disks: vec![GuestDisk {
                slot: "scsi1".into(),
                volid: "local-lvm:vm-140-disk-1".into(),
                storage: Some("local-lvm".into()),
                size_bytes: Some(1024),
                backup: false,
            }],
            provisioned_bytes: Some(1024),
            observed_at_ms: 0,
        };
        assert_eq!(g.nics_without_firewall(), vec!["net0"]);
        assert_eq!(g.disks_excluded_from_backup(), vec!["scsi1"]);
        assert!(g.is_running());
    }

    #[test]
    fn a_pool_reports_its_used_ratio_without_dividing_by_zero() {
        let mut p = PveStoragePool {
            storage: "local-lvm".into(),
            node: "pve".into(),
            kind: Some("lvmthin".into()),
            active: true,
            enabled: true,
            shared: false,
            total_bytes: 0,
            used_bytes: 0,
            avail_bytes: 0,
            allocated_bytes: None,
            allocated_source: None,
            overcommit_ratio: None,
            content: vec![],
            observed_at_ms: 0,
        };
        assert_eq!(p.used_ratio(), 0.0);
        p.total_bytes = 1000;
        p.used_bytes = 250;
        assert_eq!(p.used_ratio(), 0.25);
    }
}
