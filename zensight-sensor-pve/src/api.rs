//! The Proxmox VE API client (#818) — read-only, bounded, and forgiving of
//! the parts of the API a `PVEAuditor` token cannot see.
//!
//! Three deliberate choices:
//!
//! - **Every response is deserialized into `serde_json::Value` first**, then
//!   read field by field. Proxmox's `/api2/json` adds and moves fields
//!   between point releases and omits keys whose value is the default (a NIC
//!   line has no `firewall=` at all when the flag is off). A strict struct
//!   would turn a cosmetic upstream change into a sensor that reports nothing,
//!   which is the failure mode this whole release is about.
//! - **A 403 is data, not an error.** `PVEAuditor` legitimately cannot read
//!   some endpoints on some installs, and HA/replication simply do not exist
//!   on a standalone node. Those come back as `Ok(None)` so the caller can
//!   publish "not applicable" instead of grading the host unhealthy.
//! - **Concurrency is bounded by a semaphore**, because the API is a perl
//!   daemon on the machine whose failure is total.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::Semaphore;

use zensight_common::pve::{
    AllocationSource, GuestDisk, GuestKind, GuestNic, PveBackupTask, PveBackupVolume, PveGuest,
    PveHaResource, PveNodeStatus, PveReplicationJob, PveStoragePool, parse_flag, parse_kv_list,
    parse_size,
};

/// What the API could not answer, separated from what it answered with
/// nothing. `Forbidden` and `NotFound` are states of the deployment; only
/// `Transport` and `Malformed` are faults of the poll.
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

/// Runtime facts about one guest, from `/cluster/resources`.
#[derive(Debug, Clone, Default)]
pub struct GuestRuntime {
    pub vmid: u32,
    pub name: Option<String>,
    pub node: String,
    pub kind: Option<GuestKind>,
    pub status: String,
    pub uptime_secs: Option<u64>,
    pub template: bool,
    pub cpu: Option<f64>,
    pub mem: Option<u64>,
    pub maxmem: Option<u64>,
    pub disk: Option<u64>,
    pub maxdisk: Option<u64>,
}

/// One node's vzdump tasks, separated by scope.
#[derive(Debug, Clone, Default)]
pub struct VzdumpTasks {
    /// Tasks that name a single guest, newest first.
    pub per_guest: Vec<(u32, PveBackupTask)>,
    /// Whole-job runs (`all 1`), which carry no guest id, newest first.
    pub jobs: Vec<PveBackupTask>,
}

/// Capacity facts about one pool, from `/cluster/resources` / `/nodes/*/storage`.
#[derive(Debug, Clone, Default)]
pub struct StorageRuntime {
    pub storage: String,
    pub node: String,
    pub kind: Option<String>,
    pub active: bool,
    pub enabled: bool,
    pub shared: bool,
    pub total: u64,
    pub used: u64,
    pub avail: u64,
    pub content: Vec<String>,
}

pub struct PveClient {
    http: reqwest::Client,
    base: String,
    token: String,
    limit: Arc<Semaphore>,
    /// Paths currently answering 403/404/501, so the refusal is logged once
    /// per transition rather than once per poll (#880).
    refused: Arc<Mutex<HashSet<String>>>,
}

impl PveClient {
    pub fn new(
        base: String,
        token: String,
        timeout: Duration,
        accept_invalid_certs: bool,
        max_concurrent: usize,
    ) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .danger_accept_invalid_certs(accept_invalid_certs)
            .user_agent(concat!("zensight-sensor-pve/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            http,
            base,
            token,
            limit: Arc::new(Semaphore::new(max_concurrent.max(1))),
            refused: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    /// GET a path under `/api2/json`, returning its `data` member.
    ///
    /// `Ok(None)` for 403/404/501: the endpoint exists in the API but not in
    /// this deployment (HA on a standalone node) or not for this token. That
    /// is a fact about the install, and grading it as a failure would make a
    /// correctly-scoped read-only token look like a broken sensor.
    ///
    /// It is still **said out loud**, once per transition (#880). Until this,
    /// a token that could not read a content listing produced `volumes: 0` and
    /// no `allocated` with no log line at any level — a sensor reporting a
    /// confident zero it had never been allowed to measure.
    pub async fn get(&self, path: &str) -> Result<Option<Value>> {
        let _permit = self
            .limit
            .acquire()
            .await
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let url = format!("{}{}", self.base, path);
        let resp = self
            .http
            .get(&url)
            .header("Authorization", &self.token)
            .send()
            .await
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let status = resp.status();
        if matches!(status.as_u16(), 403 | 404 | 501) {
            if self.refused.lock().unwrap().insert(path.to_string()) {
                tracing::warn!(
                    path = %path,
                    status = status.as_u16(),
                    "pve: the API refused this endpoint — the facts it carries will be \
                     reported as unknown, not as zero. A 403 usually means the token's \
                     role is narrower than PVEAuditor"
                );
            }
            return Ok(None);
        }
        if self.refused.lock().unwrap().remove(path) {
            tracing::info!(path = %path, "pve: endpoint readable again");
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status {
                code: status.as_u16(),
                // Never echo an unbounded body into a log line.
                body: body.chars().take(200).collect(),
            });
        }
        let body: Value = resp
            .json()
            .await
            .map_err(|e| ApiError::Malformed(e.to_string()))?;
        Ok(Some(body.get("data").cloned().unwrap_or(Value::Null)))
    }

    async fn get_array(&self, path: &str) -> Result<Option<Vec<Value>>> {
        Ok(self.get(path).await?.and_then(|d| match d {
            Value::Array(a) => Some(a),
            _ => None,
        }))
    }

    /// `/cluster/resources` — one call for every guest and pool on the
    /// cluster. Cheap enough to be the steady-state poll.
    pub async fn resources(&self) -> Result<(Vec<GuestRuntime>, Vec<StorageRuntime>)> {
        let rows = self
            .get_array("/cluster/resources")
            .await?
            .unwrap_or_default();
        let mut guests = Vec::new();
        let mut pools = Vec::new();
        for r in rows {
            match r.get("type").and_then(Value::as_str) {
                Some(t @ ("qemu" | "lxc")) => {
                    let Some(vmid) = num(&r, "vmid").map(|v| v as u32) else {
                        continue;
                    };
                    guests.push(GuestRuntime {
                        vmid,
                        name: text(&r, "name"),
                        node: text(&r, "node").unwrap_or_default(),
                        kind: Some(if t == "qemu" {
                            GuestKind::Qemu
                        } else {
                            GuestKind::Lxc
                        }),
                        status: text(&r, "status").unwrap_or_else(|| "unknown".into()),
                        uptime_secs: num(&r, "uptime").map(|v| v as u64),
                        template: flag(&r, "template"),
                        cpu: num(&r, "cpu"),
                        mem: num(&r, "mem").map(|v| v as u64),
                        maxmem: num(&r, "maxmem").map(|v| v as u64),
                        disk: num(&r, "disk").map(|v| v as u64),
                        maxdisk: num(&r, "maxdisk").map(|v| v as u64),
                    });
                }
                Some("storage") => {
                    let Some(storage) = text(&r, "storage") else {
                        continue;
                    };
                    pools.push(StorageRuntime {
                        storage,
                        node: text(&r, "node").unwrap_or_default(),
                        kind: text(&r, "plugintype").or_else(|| text(&r, "type")),
                        // `status` is "available" on a healthy pool; older
                        // releases report it only through `active`.
                        active: flag(&r, "active")
                            || text(&r, "status").as_deref() == Some("available"),
                        enabled: !r.get("disabled").is_some_and(flag_value),
                        shared: flag(&r, "shared"),
                        total: num(&r, "maxdisk").map(|v| v as u64).unwrap_or(0),
                        used: num(&r, "disk").map(|v| v as u64).unwrap_or(0),
                        avail: 0,
                        content: text(&r, "content")
                            .map(|c| c.split(',').map(str::to_string).collect())
                            .unwrap_or_default(),
                    });
                }
                _ => {}
            }
        }
        for p in &mut pools {
            p.avail = p.total.saturating_sub(p.used);
        }
        Ok((guests, pools))
    }

    /// The nodes this API knows about.
    pub async fn nodes(&self) -> Result<Vec<String>> {
        Ok(self
            .get_array("/nodes")
            .await?
            .unwrap_or_default()
            .iter()
            .filter_map(|n| text(n, "node"))
            .collect())
    }

    /// One guest's configuration — the half that decides the next reboot.
    pub async fn guest_config(&self, rt: &GuestRuntime) -> Result<PveGuest> {
        let kind = rt.kind.unwrap_or(GuestKind::Qemu);
        let path = format!("/nodes/{}/{}/{}/config", rt.node, kind.as_str(), rt.vmid);
        let cfg = self.get(&path).await?.unwrap_or(Value::Null);
        Ok(build_guest(rt, &cfg))
    }

    /// Every volume on one pool, so the sum of declared sizes is knowable.
    ///
    /// This is the *provisioned* number — invisible in `used`, unchanged by
    /// any configuration edit, and the reason a thin pool fills.
    ///
    /// `None` means **not measured**, and it is now returned for one more case
    /// than before (#881): a listing that came back with rows, none of which
    /// are volumes that occupy capacity. That is what a `dir` storage looks
    /// like — PVE surfaces per-volume sizes for LVM-thin and ZFS, not for a
    /// directory — and summing the empty set gave `Some(0)`, i.e. "nothing is
    /// provisioned", on the storage type this sensor's headline finding was
    /// written for. `Some(0)` now means only what it should: a listing that
    /// came back genuinely empty, on a pool that holds nothing.
    pub async fn storage_allocated(&self, node: &str, storage: &str) -> Result<Option<u64>> {
        let path = format!("/nodes/{node}/storage/{storage}/content");
        let Some(rows) = self.get_array(&path).await? else {
            return Ok(None);
        };
        if rows.is_empty() {
            return Ok(Some(0));
        }
        let sizes: Vec<u64> = rows
            .iter()
            .filter(|v| {
                // Only volumes that occupy the pool's capacity. Backups and
                // ISOs are counted in `used`, not promised.
                matches!(
                    text(v, "content").as_deref(),
                    Some("images") | Some("rootdir") | None
                )
            })
            .filter_map(|v| num(v, "size").map(|s| s as u64))
            .collect();
        if sizes.is_empty() {
            return Ok(None);
        }
        Ok(Some(sizes.iter().sum()))
    }

    /// Stored backup volumes, newest first, per guest.
    pub async fn backups(&self, node: &str, storage: &str) -> Result<Vec<PveBackupVolume>> {
        let path = format!("/nodes/{node}/storage/{storage}/content?content=backup");
        let rows = self.get_array(&path).await?.unwrap_or_default();
        let mut out: Vec<PveBackupVolume> = rows
            .iter()
            .filter_map(|v| {
                Some(PveBackupVolume {
                    volid: text(v, "volid")?,
                    storage: storage.to_string(),
                    size_bytes: num(v, "size").map(|s| s as u64).unwrap_or(0),
                    created_at: num(v, "ctime").map(|c| c as i64).unwrap_or(0),
                    protected: v.get("protected").map(flag_value),
                })
            })
            .collect();
        out.sort_by_key(|v| std::cmp::Reverse(v.created_at));
        Ok(out)
    }

    /// Recent vzdump task results on one node, split by what they are evidence
    /// *about*.
    ///
    /// A vzdump task names its guest in `id` — unless the job covers every
    /// guest (`all 1` in `/etc/pve/jobs.cfg`), in which case PVE returns
    /// `id: ""` and the per-guest outcomes exist only inside the task log.
    /// Those rows used to be dropped on the floor (#880): `text()` refuses an
    /// empty string, so `task_vmid` returned `None` and every nightly task
    /// vanished. What survived the window was whatever one-off per-guest task
    /// happened to be in it — on the reference fleet, a failure from six weeks
    /// earlier, which became "the last backup" of six guests permanently.
    ///
    /// So a whole-job task is now kept as what it is: **one job-scoped fact**,
    /// graded once. Attributing it per guest would mean parsing the task log's
    /// free text, which this sensor deliberately does not do — the stored
    /// volumes answer "was this guest backed up last night?" without guessing
    /// at a log format.
    pub async fn vzdump_tasks(&self, node: &str, limit: u32) -> Result<VzdumpTasks> {
        let path = format!("/nodes/{node}/tasks?typefilter=vzdump&limit={limit}");
        let rows = self.get_array(&path).await?.unwrap_or_default();
        let mut out = VzdumpTasks::default();
        for r in rows {
            // A task still running has no endtime and no verdict yet; it is
            // not evidence either way, so it is skipped rather than counted
            // as a failure.
            let Some(end) = num(&r, "endtime").map(|v| v as i64) else {
                continue;
            };
            let start = num(&r, "starttime").map(|v| v as i64).unwrap_or(end);
            let exit = text(&r, "exitstatus");
            let task = PveBackupTask {
                upid: text(&r, "upid").unwrap_or_default(),
                node: text(&r, "node").unwrap_or_else(|| node.to_string()),
                ok: exit.as_deref() == Some("OK"),
                exit_status: exit,
                started_at: start,
                duration_secs: Some((end - start).max(0) as u64),
            };
            match task_vmid(&r) {
                Some(vmid) => out.per_guest.push((vmid, task)),
                None => out.jobs.push(task),
            }
        }
        // Newest first, so the caller can take the first per vmid.
        out.per_guest
            .sort_by_key(|(_, t)| std::cmp::Reverse(t.started_at));
        out.jobs.sort_by_key(|t| std::cmp::Reverse(t.started_at));
        Ok(out)
    }

    /// Cluster membership and quorum. `Ok(None)` on a standalone node.
    pub async fn cluster_status(
        &self,
    ) -> Result<Option<(Option<String>, Option<bool>, Vec<PveNodeStatus>)>> {
        let Some(rows) = self.get_array("/cluster/status").await? else {
            return Ok(None);
        };
        let mut name = None;
        let mut quorate = None;
        let mut nodes = Vec::new();
        for r in rows {
            match r.get("type").and_then(Value::as_str) {
                Some("cluster") => {
                    name = text(&r, "name");
                    quorate = r.get("quorate").map(flag_value);
                }
                Some("node") => nodes.push(PveNodeStatus {
                    name: text(&r, "name").unwrap_or_default(),
                    online: r.get("online").map(flag_value).unwrap_or(true),
                    local: flag(&r, "local"),
                    ip: text(&r, "ip"),
                }),
                _ => {}
            }
        }
        Ok(Some((name, quorate, nodes)))
    }

    /// HA resources. Empty on an install without HA — not a fault.
    pub async fn ha_status(&self) -> Result<Vec<PveHaResource>> {
        Ok(self
            .get_array("/cluster/ha/status/current")
            .await?
            .unwrap_or_default()
            .iter()
            .filter_map(|r| {
                Some(PveHaResource {
                    id: text(r, "id")?,
                    node: text(r, "node"),
                    state: text(r, "state"),
                    status: text(r, "status"),
                })
            })
            .collect())
    }

    /// Replication jobs' last results.
    pub async fn replication(&self, node: &str) -> Result<Vec<PveReplicationJob>> {
        let path = format!("/nodes/{node}/replication");
        Ok(self
            .get_array(&path)
            .await?
            .unwrap_or_default()
            .iter()
            .filter_map(|r| {
                let error = text(r, "error").filter(|e| !e.is_empty());
                Some(PveReplicationJob {
                    id: text(r, "id")?,
                    guest: num(r, "guest").map(|g| g as u32),
                    target: text(r, "target"),
                    failed: error.is_some() || num(r, "fail_count").is_some_and(|c| c > 0.0),
                    last_sync: num(r, "last_sync").map(|t| t as i64),
                    error,
                })
            })
            .collect())
    }
}

// ── Value helpers ────────────────────────────────────────────────────────────
//
// Proxmox is inconsistent about whether a scalar arrives as a number or as a
// string, and about whether a false flag is `0` or simply absent. Reading
// through these helpers rather than through serde is what keeps a cosmetic
// upstream change from silencing the sensor.

fn text(v: &Value, key: &str) -> Option<String> {
    match v.get(key)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn num(v: &Value, key: &str) -> Option<f64> {
    match v.get(key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn flag_value(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => parse_flag(s),
        _ => false,
    }
}

fn flag(v: &Value, key: &str) -> bool {
    v.get(key).is_some_and(flag_value)
}

/// A vzdump task names its guest in `id`. On some releases that is the bare
/// vmid, on others a `<type>/<vmid>`-shaped string.
///
/// `None` means the task is **not about one guest** — an empty `id`, which is
/// what a whole-job (`all 1`) run returns, or a shape we cannot read. Either
/// way the answer is "this row is not per-guest evidence", never "pick some
/// other guest's row instead" (#880).
fn task_vmid(r: &Value) -> Option<u32> {
    let id = match r.get("id") {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        _ => return None,
    };
    if id.is_empty() {
        return None;
    }
    id.rsplit(['/', ':'])
        .next()
        .and_then(|s| s.trim().parse().ok())
}

/// Join a guest's runtime row with its config document.
///
/// Split out from the request so the parsing — which is where the audit's
/// findings actually live — is testable against a config document without a
/// server.
pub fn build_guest(rt: &GuestRuntime, cfg: &Value) -> PveGuest {
    let mut nics = Vec::new();
    let mut disks = Vec::new();
    if let Some(map) = cfg.as_object() {
        // Sorted, so `net0` precedes `net10` and the doc is stable between
        // polls — an unstable ordering makes every publish look like a change.
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        for key in keys {
            let Some(raw) = map[key].as_str() else {
                continue;
            };
            if key.starts_with("net") && key[3..].chars().all(|c| c.is_ascii_digit()) {
                let (head, rest) = parse_kv_list(raw);
                let kv: std::collections::HashMap<_, _> = rest.into_iter().collect();
                nics.push(GuestNic {
                    slot: key.clone(),
                    bridge: kv.get("bridge").map(|s| s.to_string()),
                    // ABSENT means off: Proxmox omits `firewall=0`. Treating
                    // absence as "on" would have hidden the 2026-08-28 finding.
                    firewall: kv.get("firewall").copied().is_some_and(parse_flag),
                    // A QEMU line leads with `<model>=<mac>`; an LXC line
                    // leads with `name=eth0` and carries the MAC in `hwaddr`.
                    // Reading the head positionally gives a container's NIC a
                    // MAC of "ETH0", which merges nothing and would put
                    // nonsense in the identity catalog — so the head counts
                    // only when it actually looks like a MAC.
                    mac: kv
                        .get("hwaddr")
                        .filter(|v| looks_like_mac(v))
                        .map(|v| v.to_ascii_uppercase())
                        .or_else(|| {
                            head.filter(|(k, v)| !k.is_empty() && looks_like_mac(v))
                                .map(|(_, v)| v.to_ascii_uppercase())
                        }),
                    vlan_tag: kv.get("tag").and_then(|t| t.parse().ok()),
                    model: head
                        .filter(|(_, v)| looks_like_mac(v))
                        .map(|(k, _)| k.to_string())
                        .filter(|k| !k.is_empty()),
                });
            } else if is_disk_slot(key) {
                let (head, rest) = parse_kv_list(raw);
                let kv: std::collections::HashMap<_, _> = rest.into_iter().collect();
                let volid = head.map(|(_, v)| v.to_string()).unwrap_or_default();
                if volid.is_empty() || kv.get("media").copied() == Some("cdrom") {
                    continue;
                }
                disks.push(GuestDisk {
                    storage: volid.split_once(':').map(|(s, _)| s.to_string()),
                    slot: key.clone(),
                    volid,
                    size_bytes: kv.get("size").and_then(|s| parse_size(s)),
                    // Here absence means ON: vzdump includes a disk unless
                    // told otherwise, so the default is the opposite of the
                    // firewall flag's. Getting this backwards would report
                    // every disk as excluded.
                    backup: kv.get("backup").copied().is_none_or(parse_flag),
                });
            }
        }
    }
    let provisioned: u64 = disks.iter().filter_map(|d| d.size_bytes).sum();
    PveGuest {
        vmid: rt.vmid,
        name: rt
            .name
            .clone()
            .or_else(|| cfg.get("name").and_then(Value::as_str).map(str::to_string))
            .or_else(|| {
                cfg.get("hostname")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }),
        node: rt.node.clone(),
        kind: rt.kind.unwrap_or(GuestKind::Qemu),
        status: rt.status.clone(),
        uptime_secs: rt.uptime_secs,
        template: rt.template || flag(cfg, "template"),
        onboot: flag(cfg, "onboot"),
        protection: flag(cfg, "protection"),
        nics,
        disks,
        provisioned_bytes: (provisioned > 0).then_some(provisioned),
        observed_at_ms: zensight_common::current_timestamp_millis(),
    }
}

/// Six colon-separated hex pairs. Cheap, and enough to tell a MAC from an
/// interface name in a config line whose shape depends on the guest type.
fn looks_like_mac(v: &str) -> bool {
    let mut pairs = 0;
    for part in v.split(':') {
        if part.len() != 2 || !part.chars().all(|c| c.is_ascii_hexdigit()) {
            return false;
        }
        pairs += 1;
    }
    pairs == 6
}

/// Config keys that name a disk. `unused<N>` is deliberately excluded: it is a
/// volume no longer attached, and counting it would inflate every guest's
/// provisioned total with storage nothing is using.
fn is_disk_slot(key: &str) -> bool {
    const PREFIXES: [&str; 6] = ["scsi", "virtio", "sata", "ide", "mp", "efidisk"];
    if key == "rootfs" {
        return true;
    }
    PREFIXES.iter().any(|p| {
        key.strip_prefix(p)
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
    })
}

/// Fold a pool's runtime row and its allocated total into the state document.
///
/// `allocated` carries its own provenance: the API's own number where the
/// storage plugin reports one, otherwise the sum of the guest disks that live
/// on this pool. The two are never conflated — see [`AllocationSource`].
pub fn build_pool(
    rt: &StorageRuntime,
    allocated: Option<(u64, AllocationSource)>,
) -> PveStoragePool {
    let (allocated_bytes, allocated_source) = match allocated {
        Some((bytes, src)) => (Some(bytes), Some(src)),
        None => (None, None),
    };
    PveStoragePool {
        storage: rt.storage.clone(),
        node: rt.node.clone(),
        kind: rt.kind.clone(),
        active: rt.active,
        enabled: rt.enabled,
        shared: rt.shared,
        total_bytes: rt.total,
        used_bytes: rt.used,
        avail_bytes: rt.avail,
        allocated_bytes,
        allocated_source,
        overcommit_ratio: allocated_bytes
            .filter(|_| rt.total > 0)
            .map(|a| a as f64 / rt.total as f64),
        content: rt.content.clone(),
        observed_at_ms: zensight_common::current_timestamp_millis(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rt() -> GuestRuntime {
        GuestRuntime {
            vmid: 140,
            name: Some("vm-apps".into()),
            node: "pve".into(),
            kind: Some(GuestKind::Qemu),
            status: "running".into(),
            uptime_secs: Some(86400),
            ..Default::default()
        }
    }

    /// The whole reason this sensor exists, in one assertion: VM 140's config
    /// as the audit found it — `onboot` absent (so 0) and a NIC with no
    /// `firewall=` key (so the `.fw` file is inert).
    #[test]
    fn the_2026_08_28_finding_is_readable_from_a_config_document() {
        let cfg = json!({
            "name": "vm-apps",
            "net0": "virtio=AA:BB:CC:DD:EE:FF,bridge=vmbr1,tag=30",
            "scsi0": "local-lvm:vm-140-disk-0,size=32G",
        });
        let g = build_guest(&rt(), &cfg);
        assert!(!g.onboot, "onboot is absent, which means 0");
        assert_eq!(g.nics_without_firewall(), vec!["net0"]);
        assert_eq!(g.nics[0].mac.as_deref(), Some("AA:BB:CC:DD:EE:FF"));
        assert_eq!(g.nics[0].vlan_tag, Some(30));
        assert_eq!(g.provisioned_bytes, Some(32 * 1024 * 1024 * 1024));
    }

    /// The two flags default opposite ways, and getting either backwards is a
    /// silent, total misreport.
    #[test]
    fn firewall_defaults_off_and_backup_defaults_on() {
        let cfg = json!({
            "onboot": 1,
            "net0": "virtio=AA:BB:CC:DD:EE:01,bridge=vmbr0,firewall=1",
            "scsi0": "local-lvm:vm-1-disk-0,size=8G",
            "scsi1": "local-lvm:vm-1-disk-1,size=100G,backup=0",
        });
        let g = build_guest(&rt(), &cfg);
        assert!(g.onboot);
        assert!(g.nics_without_firewall().is_empty());
        assert_eq!(g.disks_excluded_from_backup(), vec!["scsi1"]);
        assert_eq!(g.provisioned_bytes, Some(108 * 1024 * 1024 * 1024));
    }

    /// A CD-ROM is not storage this guest was promised, and `unusedN` is a
    /// volume nothing is attached to. Counting either inflates every
    /// over-commitment figure the sensor publishes.
    #[test]
    fn cdroms_and_unused_volumes_are_not_provisioned_storage() {
        let cfg = json!({
            "ide2": "local:iso/debian.iso,media=cdrom",
            "unused0": "local-lvm:vm-9-disk-3",
            "scsi0": "local-lvm:vm-9-disk-0,size=10G",
        });
        let g = build_guest(&rt(), &cfg);
        assert_eq!(g.disks.len(), 1, "{:?}", g.disks);
        assert_eq!(g.disks[0].slot, "scsi0");
        assert_eq!(g.provisioned_bytes, Some(10 * 1024 * 1024 * 1024));
    }

    /// An LXC guest names things differently — `rootfs`/`mp0` and a NIC line
    /// with no leading `<model>=<mac>` — and must parse just as well.
    #[test]
    fn a_container_config_parses_too() {
        let mut r = rt();
        r.kind = Some(GuestKind::Lxc);
        let cfg = json!({
            "hostname": "ct-registry",
            "onboot": 1,
            "net0": "name=eth0,bridge=vmbr0,firewall=1,hwaddr=BC:24:11:00:00:01,ip=dhcp",
            "rootfs": "local-lvm:subvol-201-disk-0,size=8G",
            "mp0": "local-lvm:subvol-201-disk-1,mp=/data,size=50G,backup=0",
        });
        let g = build_guest(&r, &cfg);
        assert_eq!(g.name.as_deref(), Some("vm-apps"), "runtime name wins");
        assert!(g.nics_without_firewall().is_empty());
        assert_eq!(g.nics[0].mac.as_deref(), Some("BC:24:11:00:00:01"));
        assert_eq!(g.disks.len(), 2);
        assert_eq!(g.disks_excluded_from_backup(), vec!["mp0"]);
    }

    #[test]
    fn a_pool_over_commits_when_more_is_promised_than_exists() {
        let rt = StorageRuntime {
            storage: "local-lvm".into(),
            node: "pve".into(),
            total: 937,
            used: 400,
            avail: 537,
            active: true,
            enabled: true,
            ..Default::default()
        };
        let p = build_pool(&rt, Some((990, AllocationSource::Reported)));
        let ratio = p.overcommit_ratio.unwrap();
        assert!(
            ratio > 1.0,
            "990 promised on 937 is over-committed: {ratio}"
        );
        assert!(p.used_ratio() < 0.5, "and `used` shows nothing wrong");
    }

    #[test]
    fn a_pool_with_unlistable_content_reports_none_not_zero() {
        let rt = StorageRuntime {
            storage: "pbs".into(),
            total: 100,
            ..Default::default()
        };
        let p = build_pool(&rt, None);
        assert_eq!(
            p.allocated_bytes, None,
            "zero would read as 'nothing promised'"
        );
        assert_eq!(p.overcommit_ratio, None);
    }

    #[test]
    fn task_ids_yield_their_vmid_in_both_shapes() {
        assert_eq!(task_vmid(&json!({"id": "140"})), Some(140));
        assert_eq!(task_vmid(&json!({"id": "qemu/140"})), Some(140));
        assert_eq!(task_vmid(&json!({"id": "nonsense"})), None);
        assert_eq!(task_vmid(&json!({"id": 140})), Some(140));
        // A WHOLE-JOB run (`all 1`) names no guest. This is the row that used
        // to be dropped on the floor, sending the sensor looking for a
        // per-guest task and finding an arbitrarily old one (#880).
        assert_eq!(task_vmid(&json!({"id": ""})), None);
        assert_eq!(task_vmid(&json!({"id": "   "})), None);
        assert_eq!(task_vmid(&json!({})), None);
    }

    /// #881: on a `dir` storage PVE reports no per-volume size, so the sum of
    /// the empty set used to become `Some(0)` — "nothing is provisioned",
    /// which is the opposite of the truth and made the over-commitment rule
    /// unable to fire on the storage type the feature was written for.
    #[test]
    fn an_unlistable_allocated_total_is_none_and_never_zero() {
        let rt = StorageRuntime {
            storage: "local".into(),
            node: "pve".into(),
            total: 1000,
            ..Default::default()
        };
        let unknown = build_pool(&rt, None);
        assert_eq!(unknown.allocated_bytes, None);
        assert_eq!(unknown.allocated_source, None);
        assert_eq!(unknown.overcommit_ratio, None);

        let derived = build_pool(&rt, Some((900, AllocationSource::DerivedFromGuests)));
        assert_eq!(derived.allocated_bytes, Some(900));
        assert_eq!(
            derived.allocated_source,
            Some(AllocationSource::DerivedFromGuests),
            "a derived total is a floor and says so"
        );
        assert_eq!(derived.overcommit_ratio, Some(0.9));
    }

    #[test]
    fn flags_read_from_every_shape_proxmox_writes() {
        assert!(flag_value(&json!(1)));
        assert!(flag_value(&json!("1")));
        assert!(flag_value(&json!(true)));
        assert!(!flag_value(&json!(0)));
        assert!(!flag_value(&json!("")));
        assert!(!flag_value(&Value::Null));
    }
}
