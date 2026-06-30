//! Zenoh-free, pure-serde type definitions for the ZenSight **aggregate
//! publishers**.
//!
//! Three sensors can publish a single typed object per host (behind their
//! additive `aggregate-publishers` Cargo feature) so a consumer can
//! `get`/`subscribe` it directly instead of re-assembling the per-metric
//! `TelemetryPoint` stream:
//!
//! | Sensor  | Key                                    | Type                       |
//! |---------|----------------------------------------|----------------------------|
//! | sysinfo | `zensight/sysinfo/<host>/host`         | [`HostInfo`]               |
//! | netlink | `zensight/netlink/<host>/interfaces`   | [`HostInterfaces`]         |
//! | syslog  | `zensight/syslog/<host>/events/<uid>`  | [`LogEvent`]               |
//!
//! This crate intentionally has **no dependency on `zenoh`, `tokio`, or
//! `zensight-common`** — only `serde`. That lets an external consumer (e.g. a
//! supervision HMI in another workspace pinning a different `zenoh`) depend on
//! these wire types by `path`/`git` without inheriting a `zenoh` version
//! conflict. It also keeps the structs and their JSON (snake_case) contract in
//! one place, unit-tested here.
//!
//! Besides the types, this crate carries the small **pure, std-only helpers**
//! used to derive their fields (unit conversions, rate from a counter delta,
//! link-state classification, time-sortable id, socket→PID join), so the
//! contract and its derivations are tested together.

#![forbid(unsafe_code)]

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

// ===========================================================================
// sysinfo — HostInfo
// ===========================================================================

/// A structured, single-object snapshot of one host's vitals.
///
/// Published as JSON (snake_case) to `zensight/sysinfo/<host>/host`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HostInfo {
    /// Host identifier (matches the `<host>` key segment / `TelemetryPoint.source`).
    pub host: String,
    /// Per-core CPU usage, percent in `0..=100` (one entry per logical core).
    pub cpu_cores: Vec<f32>,
    /// Used physical memory, mebibytes.
    pub mem_used_mb: u64,
    /// Total physical memory, mebibytes.
    pub mem_total_mb: u64,
    /// Used disk space across included volumes, gibibytes.
    pub disk_used_gb: f64,
    /// Total disk space across included volumes, gibibytes.
    pub disk_total_gb: f64,
    /// Load average `[1m, 5m, 15m]`.
    pub load_avg: [f64; 3],
    /// Uptime, seconds.
    pub uptime_s: u64,
    /// Aggregate receive throughput across included interfaces, bytes/second.
    pub net_rx_bps: u64,
    /// Aggregate transmit throughput across included interfaces, bytes/second.
    pub net_tx_bps: u64,
}

/// Bytes to mebibytes (integer, truncating).
pub fn bytes_to_mb(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

/// Bytes to gibibytes (floating point).
pub fn bytes_to_gb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0 * 1024.0)
}

/// Per-second rate from a cumulative-counter delta. Saturating on the delta and
/// flooring the interval at 1s so a zero/short interval can never divide-by-zero
/// or wrap on a counter reset.
pub fn rate_bps(prev: u64, cur: u64, interval_secs: u64) -> u64 {
    cur.saturating_sub(prev) / interval_secs.max(1)
}

// ===========================================================================
// netlink — NetIface / HostInterfaces
// ===========================================================================

/// A structured snapshot of one network interface (JSON, snake_case).
///
/// This carries only what ZenSight can *observe* from the kernel. It does NOT
/// carry comm-domain concepts:
/// - no `kind` (FO/RF/WIFI link type), and
/// - no `bound_driver_ids` (comm driver instance ids).
///
/// Those are enriched downstream by the consumer (e.g. the supervision HMI),
/// which correlates `bound_pids` / `name` with its CommPlan. ZenSight exposes
/// only the raw observable ([`bound_pids`](NetIface::bound_pids) etc.).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NetIface {
    /// Interface name (e.g. `eth0`).
    pub name: String,
    /// Link state: `UP`, `DOWN`, or `DEGRADED` (admin-up but no carrier).
    pub state: String,
    /// First IP address bound to the interface, if any.
    pub ip_address: Option<String>,
    /// MTU, if reported.
    pub mtu: Option<u32>,
    /// Receive throughput, bytes/second (derived from the byte counter delta).
    pub rx_bps: u64,
    /// Transmit throughput, bytes/second (derived from the byte counter delta).
    pub tx_bps: u64,
    /// Cumulative receive errors.
    pub rx_errs: u64,
    /// Cumulative transmit errors.
    pub tx_errs: u64,
    /// PIDs owning a socket bound to one of this interface's local IPs, resolved
    /// from sockdiag inode + a `/proc/<pid>/fd` scan (best-effort, deduped and
    /// sorted). This is the raw OS observable — NOT comm driver ids.
    pub bound_pids: Vec<u32>,
}

/// The aggregated per-host interface object.
///
/// Wire shape: `{ "host": "...", "interfaces": [NetIface, ...] }`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HostInterfaces {
    /// Host identifier (matches the `<host>` key segment).
    pub host: String,
    /// One entry per included interface.
    pub interfaces: Vec<NetIface>,
}

/// Link state from the admin (`up`) flag and the carrier signal.
///
/// `DEGRADED` = administratively up but carrier down (cable out / no link);
/// `UP` = admin up with carrier present or unknown; `DOWN` = admin down.
pub fn iface_state(up: bool, carrier: Option<bool>) -> &'static str {
    match (up, carrier) {
        (true, Some(false)) => "DEGRADED",
        (true, _) => "UP",
        (false, _) => "DOWN",
    }
}

/// Parse a `/proc/<pid>/fd/*` symlink target of the form `socket:[12345]` into
/// its inode number. Returns `None` for any non-socket fd.
pub fn parse_socket_inode(link: &str) -> Option<u64> {
    link.strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

/// Join the three observables into a `ifname -> sorted, deduped PIDs` map.
///
/// Pure (no `/proc`, no netlink) so the correlation logic is unit-testable.
/// - `ip_to_iface`: local IP string → interface name (from the address dump).
/// - `sockets`: `(local IP string, socket inode)` per socket (from sockdiag).
/// - `inode_pids`: socket inode → owning PIDs (resolved from `/proc`).
///
/// Sockets whose local IP is not bound to a known interface (e.g. wildcard
/// `0.0.0.0` / `::` listeners) are skipped — they cannot be attributed to one
/// interface.
pub fn bound_pids_by_iface(
    ip_to_iface: &HashMap<String, String>,
    sockets: &[(String, u64)],
    inode_pids: &HashMap<u64, Vec<u32>>,
) -> HashMap<String, Vec<u32>> {
    let mut out: HashMap<String, BTreeSet<u32>> = HashMap::new();
    for (ip, inode) in sockets {
        let Some(iface) = ip_to_iface.get(ip) else {
            continue;
        };
        if let Some(pids) = inode_pids.get(inode) {
            let set = out.entry(iface.clone()).or_default();
            for p in pids {
                set.insert(*p);
            }
        }
    }
    out.into_iter()
        .map(|(iface, pids)| (iface, pids.into_iter().collect()))
        .collect()
}

// ===========================================================================
// syslog — LogEvent
// ===========================================================================

/// A structured, single log event (JSON, snake_case).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogEvent {
    /// Time-sortable unique id (`<epoch_ms>-<counter>`, zero-padded so the
    /// lexicographic order matches chronological order).
    pub uid: String,
    /// Event time, epoch milliseconds.
    pub timestamp: i64,
    /// Severity keyword (`emerg`/`alert`/`crit`/`err`/`warning`/`notice`/
    /// `info`/`debug`).
    pub severity: String,
    /// Facility keyword (`kern`/`user`/`daemon`/`auth`/...).
    pub facility: String,
    /// Application / process tag, if present.
    pub app: Option<String>,
    /// Process id, if present and numeric.
    pub pid: Option<u32>,
    /// Message content.
    pub message: String,
    /// Known-event category (e.g. `coredump`, `unit-failed`) when the message
    /// matches a recognized systemd catalog id; otherwise `None`.
    pub category: Option<String>,
}

/// Monotonic per-process counter making each `uid` unique even within the same
/// millisecond.
static UID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Build a time-sortable, unique event id from a millisecond timestamp.
///
/// Format: `{timestamp_ms:013}-{counter:08}`. Zero padding keeps the
/// lexicographic order aligned with chronological order (13 digits covers epoch
/// ms well past year 2200). Negative timestamps are floored to 0.
pub fn next_uid(timestamp_ms: i64) -> String {
    let counter = UID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:013}-{:08}", timestamp_ms.max(0), counter)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- HostInfo -----

    fn host_info_sample() -> HostInfo {
        HostInfo {
            host: "server01".to_string(),
            cpu_cores: vec![12.5, 0.0, 100.0],
            mem_used_mb: 2048,
            mem_total_mb: 8192,
            disk_used_gb: 40.0,
            disk_total_gb: 120.0,
            load_avg: [0.5, 0.4, 0.3],
            uptime_s: 3600,
            net_rx_bps: 1000,
            net_tx_bps: 500,
        }
    }

    #[test]
    fn host_info_serializes_snake_case_and_round_trips() {
        let original = host_info_sample();
        let json = serde_json::to_value(&original).unwrap();
        for key in [
            "host",
            "cpu_cores",
            "mem_used_mb",
            "mem_total_mb",
            "disk_used_gb",
            "disk_total_gb",
            "load_avg",
            "uptime_s",
            "net_rx_bps",
            "net_tx_bps",
        ] {
            assert!(json.get(key).is_some(), "missing field {key}");
        }
        assert_eq!(json["cpu_cores"], serde_json::json!([12.5, 0.0, 100.0]));
        assert_eq!(json["load_avg"], serde_json::json!([0.5, 0.4, 0.3]));
        let decoded: HostInfo = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn host_info_unit_conversions() {
        assert_eq!(bytes_to_mb(1024 * 1024), 1);
        assert_eq!(bytes_to_mb(10 * 1024 * 1024), 10);
        assert_eq!(bytes_to_mb(0), 0);
        assert!((bytes_to_gb(1024 * 1024 * 1024) - 1.0).abs() < f64::EPSILON);
        assert!((bytes_to_gb(0)).abs() < f64::EPSILON);
    }

    #[test]
    fn rate_handles_counter_delta_and_edges() {
        assert_eq!(rate_bps(1000, 3000, 2), 1000);
        assert_eq!(rate_bps(3000, 1000, 2), 0); // counter reset saturates
        assert_eq!(rate_bps(0, 500, 0), 500); // zero interval floored to 1s
    }

    // ----- NetIface / HostInterfaces -----

    #[test]
    fn host_interfaces_serializes_snake_case_and_round_trips() {
        let host = HostInterfaces {
            host: "usv2_cpu1".to_string(),
            interfaces: vec![NetIface {
                name: "eth0".to_string(),
                state: "UP".to_string(),
                ip_address: Some("10.0.0.1".to_string()),
                mtu: Some(1500),
                rx_bps: 1234,
                tx_bps: 5678,
                rx_errs: 1,
                tx_errs: 0,
                bound_pids: vec![7, 42],
            }],
        };
        let json = serde_json::to_value(&host).unwrap();
        assert_eq!(json["host"], "usv2_cpu1");
        let iface = &json["interfaces"][0];
        for key in [
            "name",
            "state",
            "ip_address",
            "mtu",
            "rx_bps",
            "tx_bps",
            "rx_errs",
            "tx_errs",
            "bound_pids",
        ] {
            assert!(iface.get(key).is_some(), "missing field {key}");
        }
        assert_eq!(iface["bound_pids"], serde_json::json!([7, 42]));
        let decoded: HostInterfaces = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, host);
    }

    #[test]
    fn iface_state_classifies_carrier() {
        assert_eq!(iface_state(true, Some(true)), "UP");
        assert_eq!(iface_state(true, None), "UP");
        assert_eq!(iface_state(true, Some(false)), "DEGRADED");
        assert_eq!(iface_state(false, Some(true)), "DOWN");
        assert_eq!(iface_state(false, None), "DOWN");
    }

    #[test]
    fn parse_socket_inode_only_matches_sockets() {
        assert_eq!(parse_socket_inode("socket:[12345]"), Some(12345));
        assert_eq!(parse_socket_inode("anon_inode:[eventfd]"), None);
        assert_eq!(parse_socket_inode("/dev/null"), None);
        assert_eq!(parse_socket_inode("socket:[notanumber]"), None);
    }

    #[test]
    fn bound_pids_join_dedups_sorts_and_skips_unmapped() {
        let ip_to_iface = HashMap::from([
            ("10.0.0.1".to_string(), "eth0".to_string()),
            ("10.0.0.2".to_string(), "eth1".to_string()),
        ]);
        let inode_pids = HashMap::from([
            (100u64, vec![42u32, 7]),
            (200u64, vec![42u32]), // same pid on another socket → deduped
            (300u64, vec![99u32]),
        ]);
        let sockets = vec![
            ("10.0.0.1".to_string(), 100u64), // eth0 → {42,7}
            ("10.0.0.1".to_string(), 200u64), // eth0 → +42 (dedup)
            ("10.0.0.2".to_string(), 300u64), // eth1 → {99}
            ("0.0.0.0".to_string(), 100u64),  // wildcard → skipped
            ("10.0.0.1".to_string(), 999u64), // inode with no pid → no-op
        ];
        let got = bound_pids_by_iface(&ip_to_iface, &sockets, &inode_pids);
        assert_eq!(got.get("eth0"), Some(&vec![7u32, 42])); // sorted + deduped
        assert_eq!(got.get("eth1"), Some(&vec![99u32]));
        assert_eq!(got.len(), 2); // wildcard produced no interface entry
    }

    // ----- LogEvent -----

    #[test]
    fn log_event_serializes_snake_case_and_round_trips() {
        let event = LogEvent {
            uid: "0001700000000000-00000001".to_string(),
            timestamp: 1_700_000_000_000,
            severity: "err".to_string(),
            facility: "daemon".to_string(),
            app: Some("sshd".to_string()),
            pid: Some(1234),
            message: "Connection from 10.0.0.1".to_string(),
            category: Some("coredump".to_string()),
        };
        let json = serde_json::to_value(&event).unwrap();
        for key in [
            "uid",
            "timestamp",
            "severity",
            "facility",
            "app",
            "pid",
            "message",
            "category",
        ] {
            assert!(json.get(key).is_some(), "missing field {key}");
        }
        assert_eq!(json["timestamp"], 1_700_000_000_000i64);
        assert_eq!(json["pid"], 1234);
        let decoded: LogEvent = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, event);
    }

    #[test]
    fn next_uid_is_time_sortable_and_unique() {
        let a = next_uid(1_700_000_000_000);
        let b = next_uid(1_700_000_000_000);
        assert_ne!(a, b);
        assert!(a < b, "{a} should sort before {b}");
        let earlier = next_uid(1_600_000_000_000);
        let later = next_uid(1_700_000_000_000);
        assert!(earlier < later);
        assert!(next_uid(-5).starts_with("0000000000000-"));
    }
}
