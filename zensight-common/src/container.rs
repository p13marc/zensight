//! Container wire types (#819).
//!
//! Every service on the reference fleet is a Podman Quadlet container, and
//! **no sensor knew what a container was**. sysinfo's cgroups collector can be
//! pointed at explicit paths, is off by default, and does not enumerate. The
//! systemd sensor sees `caddy.service` as a unit — not that the unit is Caddy
//! 2.11.4, nor which image digest it runs, nor that its healthcheck has been
//! failing since the day it was deployed. netlink surfaces the podman bridges'
//! containers as eleven rows of a catalog with IPs and nothing else.
//!
//! Four separate findings of the 2026-08-28 audit are fields in this module:
//!
//! - **garage reported `unhealthy` from the day it was deployed** while serving
//!   traffic perfectly — a distroless image with no `/bin/sh`, so a `CMD-SHELL`
//!   check could never pass. Nobody noticed for weeks. That is why
//!   [`HealthState`] distinguishes *failing* from **never ran**.
//! - **cosign silently signed nothing for eight days** → [`SignatureState`].
//! - **12 pinned images behind upstream**, surfaced by a monthly mail rather
//!   than a live gauge → [`ContainerImage::upstream_digest`].
//! - the 2026-08-17 memory incident was attributed to "the bundle" for eleven
//!   days because nothing reported per-container memory → [`ContainerResources`].
//!
//! Read-only throughout. The sensor mounts the podman socket read-only, reads
//! cgroup files, and has no write surface.

use serde::{Deserialize, Serialize};

use schemars::JsonSchema;

/// A container's healthcheck, with the distinction the garage case turns on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    /// No healthcheck is configured. Nothing to say; not a fault.
    None,
    /// Configured, and it has **never produced a result**. The check is
    /// broken, not the service — a `CMD-SHELL` probe in a distroless image
    /// cannot run at all. Reporting this as `Unhealthy` for weeks is exactly
    /// what happened, and reporting it as `Healthy` would be worse.
    NeverRan,
    /// Configured, running, and currently failing.
    Unhealthy,
    /// Configured and passing.
    Healthy,
    /// In its start period; no verdict yet.
    Starting,
}

impl HealthState {
    pub fn as_str(&self) -> &'static str {
        match self {
            HealthState::None => "none",
            HealthState::NeverRan => "never_ran",
            HealthState::Unhealthy => "unhealthy",
            HealthState::Healthy => "healthy",
            HealthState::Starting => "starting",
        }
    }
}

impl std::fmt::Display for HealthState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Whether a registry-hosted image carries a signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SignatureState {
    /// Not looked for — the check is off, or the image is not registry-hosted.
    /// Never conflate this with "unsigned": silence is not evidence.
    NotChecked,
    Present,
    Absent,
}

/// The image a container runs, and how far it is from upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ContainerImage {
    /// The reference as configured, e.g. `docker.io/library/caddy:2.11.4`.
    pub reference: String,
    /// The digest actually running. **This single field replaces
    /// `image-update-report.sh`**: with it, "12 pinned images behind upstream"
    /// is a live comparison instead of a monthly mail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// The digest the configured tag resolves to upstream *now*. `None` unless
    /// the explicitly-egressing collector is on — it is the only part of this
    /// sensor that talks to the internet, so it is a deliberate choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_digest: Option<String>,
    #[serde(default = "not_checked")]
    pub signature: SignatureState,
}

fn not_checked() -> SignatureState {
    SignatureState::NotChecked
}

impl ContainerImage {
    /// `true` only when both digests are known and differ. Unknown is not
    /// "up to date" and is not "behind"; it is unknown.
    pub fn is_behind_upstream(&self) -> bool {
        match (&self.digest, &self.upstream_digest) {
            (Some(a), Some(b)) => a != b,
            _ => false,
        }
    }
}

/// cgroup-v2 resource facts. **This is what would have named netring** rather
/// than "the sensor bundle" on 2026-08-17: five sensors shared one cgroup and
/// one `MemoryMax`, so per-container memory did not exist as a number.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub struct ContainerResources {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    /// `memory.max`. `None` means `max` — no limit — which is a different fact
    /// from a limit of zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_max_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_peak_bytes: Option<u64>,
    /// Cumulative CPU time, microseconds (`cpu.stat: usage_usec`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_usage_usec: Option<u64>,
    /// Cumulative throttled time, microseconds (`cpu.stat: throttled_usec`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_throttled_usec: Option<u64>,
    /// `memory.events: oom_kill` — cumulative, and the number that names a
    /// victim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oom_kills: Option<u64>,
    /// `memory.events: max` — how often the limit was hit at all, which rises
    /// long before a kill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_max_events: Option<u64>,
    /// PSI `some avg10`, percent, from `<cgroup>/{cpu,memory,io}.pressure`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_pressure_avg10: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_pressure_avg10: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io_pressure_avg10: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids: Option<u64>,
}

/// One published port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PortBinding {
    pub container_port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_ip: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
}

/// One mount.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MountPoint {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub destination: String,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// A container, as the runtime and the kernel jointly know it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ContainerInfo {
    /// The runtime id, full. The *slug* used in keys is the name, which is
    /// stable across recreations; the id is not.
    pub id: String,
    pub name: String,
    /// `running` / `exited` / `created` / `paused`, the runtime's own words.
    pub status: String,
    pub image: ContainerImage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    /// How many times the runtime has restarted it. A container in a restart
    /// loop is a first-class state, not something to infer from the journal.
    #[serde(default)]
    pub restart_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    pub health: HealthState,
    /// Consecutive failing health probes, when the runtime tracks it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_failing_streak: Option<u64>,
    /// The systemd unit that owns this container (`PODMAN_SYSTEMD_UNIT`), so a
    /// container **joins up with** the systemd sensor's view instead of
    /// sitting beside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_policy: Option<String>,
    /// Rootless containers run under a user's session; rootful under the
    /// system. Which one matters for where the cgroup and the socket live.
    #[serde(default)]
    pub rootless: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<PortBinding>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<MountPoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroup_path: Option<String>,
    #[serde(default)]
    pub resources: ContainerResources,
    /// Container IPs, when the runtime reports them — the join key with
    /// netlink's observed podman-bridge entities.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ips: Vec<String>,
    pub observed_at_ms: i64,
}

impl ContainerInfo {
    pub fn is_running(&self) -> bool {
        self.status == "running"
    }

    /// Memory as a fraction of its own limit, when it has one.
    pub fn memory_ratio(&self) -> Option<f64> {
        match (self.resources.memory_bytes, self.resources.memory_max_bytes) {
            (Some(used), Some(max)) if max > 0 => Some(used as f64 / max as f64),
            _ => None,
        }
    }
}

/// Parse a cgroup-v2 flat-keyed file (`cpu.stat`, `memory.events`): one
/// `key value` pair per line.
pub fn parse_flat_keyed(text: &str) -> std::collections::HashMap<&str, u64> {
    text.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(' ')?;
            Some((k, v.trim().parse().ok()?))
        })
        .collect()
}

/// Parse a PSI file's `some avg10=<f>` value.
pub fn parse_pressure_avg10(text: &str) -> Option<f64> {
    text.lines()
        .find(|l| l.starts_with("some "))?
        .split_whitespace()
        .find_map(|f| f.strip_prefix("avg10="))?
        .parse()
        .ok()
}

/// Parse a cgroup single-value file. `max` means "no limit" and comes back as
/// `None` — distinct from a limit of `0`, which would read as "cannot use any
/// memory" and is a fact no container survives.
pub fn parse_limit(text: &str) -> Option<u64> {
    let t = text.trim();
    if t == "max" { None } else { t.parse().ok() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_keyed_files_parse() {
        let m = parse_flat_keyed("usage_usec 12345\nuser_usec 6000\nthrottled_usec 42\n");
        assert_eq!(m["usage_usec"], 12345);
        assert_eq!(m["throttled_usec"], 42);
    }

    #[test]
    fn pressure_reads_the_some_line() {
        let psi = "some avg10=1.25 avg60=0.40 avg300=0.10 total=99\n\
                   full avg10=0.50 avg60=0.20 avg300=0.05 total=44\n";
        assert_eq!(parse_pressure_avg10(psi), Some(1.25));
        assert_eq!(parse_pressure_avg10("nonsense"), None);
    }

    /// `max` is not zero. A limit of 0 would say "this container may use no
    /// memory", which is not a state anything runs in.
    #[test]
    fn an_unlimited_cgroup_reports_none_not_zero() {
        assert_eq!(parse_limit("max\n"), None);
        assert_eq!(parse_limit("0\n"), Some(0));
        assert_eq!(parse_limit("134217728\n"), Some(134217728));
    }

    /// The garage case: a healthcheck that can never run is not a passing one
    /// and not the same fact as a failing one.
    #[test]
    fn never_ran_is_its_own_state() {
        for (s, expected) in [
            (HealthState::None, "none"),
            (HealthState::NeverRan, "never_ran"),
            (HealthState::Unhealthy, "unhealthy"),
            (HealthState::Healthy, "healthy"),
        ] {
            assert_eq!(s.as_str(), expected);
        }
        assert_ne!(HealthState::NeverRan, HealthState::Unhealthy);
        assert_ne!(HealthState::NeverRan, HealthState::None);
    }

    /// Unknown is not "up to date". An image whose upstream was never checked
    /// must not read as current, or the whole point of the field is lost.
    #[test]
    fn behind_upstream_needs_both_digests() {
        let mut img = ContainerImage {
            reference: "docker.io/library/caddy:2.11.4".into(),
            digest: Some("sha256:aaa".into()),
            upstream_digest: None,
            signature: SignatureState::NotChecked,
        };
        assert!(!img.is_behind_upstream(), "unknown is not behind");
        img.upstream_digest = Some("sha256:aaa".into());
        assert!(!img.is_behind_upstream());
        img.upstream_digest = Some("sha256:bbb".into());
        assert!(img.is_behind_upstream());
        img.digest = None;
        assert!(!img.is_behind_upstream(), "unknown is still not behind");
    }
}
