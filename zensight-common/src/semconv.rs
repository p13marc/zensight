//! OpenTelemetry host-metrics semantic-convention mapping (#100, keyspace v2).
//!
//! ZenSight's internal sysinfo metric keys are ad-hoc (`memory/used`,
//! `network/<if>/rx_bytes`, …) and don't map cleanly to the OTel host-metrics
//! semantic conventions (`system.memory.usage{state}`,
//! `system.network.io{direction}`, …). Before this, each exporter hand-mapped
//! every key. This module is the **single** key↔semconv table both the OTel and
//! Prometheus exporters consult, so exported host metrics are portable and
//! dashboard-compatible with the wider OTel ecosystem.
//!
//! Scope: the **core** USE host metrics (cpu, memory, swap/paging, network, disk
//! I/O, filesystem) are mapped with their factored `state` / `direction` /
//! `device` / `cpu` attributes. Keys with no standard semconv equivalent (EDAC,
//! md-raid, schedstat, conntrack, …) return `None`; callers fall back to the raw
//! `zensight.<protocol>.<metric>` name. Values pass through unchanged — this maps
//! the metric *identity* (name + attributes), not units (so utilization stays the
//! sensor's 0–100 percent rather than a 0–1 ratio).

use crate::telemetry::Protocol;

/// An OTel semantic-convention mapping for one internal metric key: the semconv
/// metric name plus the factored attributes (`state` / `direction` / `device` /
/// `cpu` / `type`). The attribute keys are static; values are owned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemConv {
    /// The OTel semconv metric name, dotted (e.g. `system.memory.usage`).
    pub name: &'static str,
    /// Factored semconv attributes, e.g. `[("state", "used")]`.
    pub attributes: Vec<(&'static str, String)>,
}

impl SemConv {
    fn new(name: &'static str, attributes: Vec<(&'static str, String)>) -> Option<SemConv> {
        Some(SemConv { name, attributes })
    }
}

/// Map a telemetry `(protocol, metric)` to OTel semconv, or `None` when the key
/// has no standard equivalent (caller uses the raw name). Sysinfo host metrics
/// (factored `state`/`direction`/`device`) and systemd per-unit series (clean
/// name + `unit` label) are mapped; the entry point is protocol-keyed so other
/// sensors can join the table later without touching the exporters.
pub fn metric_semconv(protocol: Protocol, metric: &str) -> Option<SemConv> {
    match protocol {
        Protocol::Sysinfo => sysinfo_semconv(metric),
        Protocol::Systemd => systemd_semconv(metric),
        _ => None,
    }
}

/// The systemd key → clean-name table (#282). Per-unit series (`unit/<u>/<field>`)
/// map to a single metric name with the unit carried as the point's existing
/// `unit` **label** — so exporters don't bake the unit name into the metric name
/// (Prometheus cardinality anti-pattern). Attributes are intentionally empty: the
/// telemetry point already carries the `unit` label, and the exporters would
/// otherwise emit it twice. Aggregate keys (`units/*`, `manager/*`, `boot/*`,
/// `mounts/*`, `journal/*`) have no per-entity segment, so they fall through to
/// the raw `zensight.systemd.<metric>` name.
pub fn systemd_semconv(metric: &str) -> Option<SemConv> {
    let parts: Vec<&str> = metric.split('/').collect();
    let name = match parts.as_slice() {
        ["unit", _, "active"] => "systemd.unit.active",
        ["unit", _, "state"] => "systemd.unit.state",
        ["unit", _, "restarts_total"] => "systemd.unit.restarts",
        ["unit", _, "active_since_usec"] => "systemd.unit.active_since_usec",
        ["unit", _, "mem_bytes"] => "systemd.unit.memory_bytes",
        ["unit", _, "cpu_usec"] => "systemd.unit.cpu_usec",
        ["unit", _, "tasks"] => "systemd.unit.tasks",
        ["unit", _, "exit_code"] => "systemd.unit.exit_code",
        ["unit", _, "ip_ingress_bytes"] => "systemd.unit.ip_ingress_bytes",
        ["unit", _, "ip_egress_bytes"] => "systemd.unit.ip_egress_bytes",
        ["unit", _, "io_read_bytes"] => "systemd.unit.io_read_bytes",
        ["unit", _, "io_write_bytes"] => "systemd.unit.io_write_bytes",
        ["unit", _, "n_accepted"] => "systemd.socket.accepted",
        ["unit", _, "n_connections"] => "systemd.socket.connections",
        ["unit", _, "n_refused"] => "systemd.socket.refused",
        ["unit", _, "last_trigger_usec"] => "systemd.timer.last_trigger_usec",
        ["unit", _, "next_trigger_usec"] => "systemd.timer.next_trigger_usec",
        _ => return None,
    };
    SemConv::new(name, vec![])
}

/// The sysinfo key → `system.*` semconv table (#100). Pure and exhaustively
/// unit-tested.
pub fn sysinfo_semconv(metric: &str) -> Option<SemConv> {
    let parts: Vec<&str> = metric.split('/').collect();
    let attr = |k: &'static str, v: &str| (k, v.to_string());
    match parts.as_slice() {
        // ── CPU ──
        ["cpu", "usage"] => SemConv::new("system.cpu.utilization", vec![]),
        ["cpu", n, "usage"] => SemConv::new("system.cpu.utilization", vec![attr("cpu", n)]),
        ["load", "1m"] => SemConv::new("system.cpu.load_average.1m", vec![]),
        ["load", "5m"] => SemConv::new("system.cpu.load_average.5m", vec![]),
        ["load", "15m"] => SemConv::new("system.cpu.load_average.15m", vec![]),

        // ── Memory ── (semconv states: used / cached / buffered / free)
        ["memory", "used"] => SemConv::new("system.memory.usage", vec![attr("state", "used")]),
        ["memory", "cached"] => SemConv::new("system.memory.usage", vec![attr("state", "cached")]),
        ["memory", "buffers"] => {
            SemConv::new("system.memory.usage", vec![attr("state", "buffered")])
        }
        ["memory", "available"] => SemConv::new("system.memory.usage", vec![attr("state", "free")]),
        ["memory", "total"] => SemConv::new("system.memory.limit", vec![]),
        ["memory", "usage_percent"] => SemConv::new("system.memory.utilization", vec![]),

        // ── Swap / paging ──
        ["memory", "swap_used"] => SemConv::new("system.paging.usage", vec![attr("state", "used")]),
        ["memory", "swap_percent"] => SemConv::new("system.paging.utilization", vec![]),
        ["memory", "paging_in_total"] => {
            SemConv::new("system.paging.operations", vec![attr("direction", "in")])
        }
        ["memory", "paging_out_total"] => {
            SemConv::new("system.paging.operations", vec![attr("direction", "out")])
        }
        ["memory", "page_faults_major_total"] => {
            SemConv::new("system.paging.faults", vec![attr("type", "major")])
        }

        // ── Network (per interface) ──
        ["network", dev, field] => network_semconv(dev, field),

        // ── Disk I/O (per device) ──
        ["disk", dev, "io", field] => disk_io_semconv(dev, field),

        // ── Filesystem (per mount/device) ──
        ["disk", dev, "used"] => SemConv::new(
            "system.filesystem.usage",
            vec![attr("device", dev), attr("state", "used")],
        ),
        ["disk", dev, "available"] => SemConv::new(
            "system.filesystem.usage",
            vec![attr("device", dev), attr("state", "free")],
        ),
        ["disk", dev, "usage_percent"] => {
            SemConv::new("system.filesystem.utilization", vec![attr("device", dev)])
        }

        _ => None,
    }
}

/// `network/<if>/<field>` → `system.network.*{direction, device}`.
fn network_semconv(dev: &str, field: &str) -> Option<SemConv> {
    let (name, direction) = match field {
        "rx_bytes" => ("system.network.io", "receive"),
        "tx_bytes" => ("system.network.io", "transmit"),
        "rx_packets" => ("system.network.packets", "receive"),
        "tx_packets" => ("system.network.packets", "transmit"),
        "rx_errors" => ("system.network.errors", "receive"),
        "tx_errors" => ("system.network.errors", "transmit"),
        "rx_dropped" => ("system.network.dropped", "receive"),
        "tx_dropped" => ("system.network.dropped", "transmit"),
        _ => return None,
    };
    SemConv::new(
        name,
        vec![
            ("device", dev.to_string()),
            ("direction", direction.to_string()),
        ],
    )
}

/// `disk/<dev>/io/<field>` → `system.disk.*{direction, device}`.
fn disk_io_semconv(dev: &str, field: &str) -> Option<SemConv> {
    let (name, direction) = match field {
        "read_bytes" => ("system.disk.io", "read"),
        "write_bytes" => ("system.disk.io", "write"),
        "read_ops" => ("system.disk.operations", "read"),
        "write_ops" => ("system.disk.operations", "write"),
        _ => return None,
    };
    SemConv::new(
        name,
        vec![
            ("device", dev.to_string()),
            ("direction", direction.to_string()),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sc(metric: &str) -> SemConv {
        sysinfo_semconv(metric).unwrap_or_else(|| panic!("no semconv for {metric}"))
    }

    #[test]
    fn memory_states_factor_into_one_metric() {
        assert_eq!(sc("memory/used").name, "system.memory.usage");
        assert_eq!(
            sc("memory/used").attributes,
            vec![("state", "used".to_string())]
        );
        assert_eq!(
            sc("memory/cached").attributes,
            vec![("state", "cached".to_string())]
        );
        assert_eq!(
            sc("memory/buffers").attributes,
            vec![("state", "buffered".to_string())]
        );
        assert_eq!(
            sc("memory/available").attributes,
            vec![("state", "free".to_string())]
        );
        assert_eq!(sc("memory/total").name, "system.memory.limit");
        assert_eq!(sc("memory/usage_percent").name, "system.memory.utilization");
    }

    #[test]
    fn network_io_factors_direction_and_device() {
        let rx = sc("network/eth0/rx_bytes");
        assert_eq!(rx.name, "system.network.io");
        assert_eq!(
            rx.attributes,
            vec![
                ("device", "eth0".to_string()),
                ("direction", "receive".to_string())
            ]
        );
        let tx = sc("network/eth0/tx_bytes");
        assert_eq!(tx.attributes[1], ("direction", "transmit".to_string()));
        assert_eq!(sc("network/wg0/tx_packets").name, "system.network.packets");
        assert_eq!(sc("network/eth0/rx_errors").name, "system.network.errors");
        assert_eq!(sc("network/eth0/tx_dropped").name, "system.network.dropped");
    }

    #[test]
    fn disk_io_and_filesystem() {
        assert_eq!(sc("disk/sda/io/read_bytes").name, "system.disk.io");
        assert_eq!(
            sc("disk/sda/io/read_bytes").attributes,
            vec![
                ("device", "sda".to_string()),
                ("direction", "read".to_string())
            ]
        );
        assert_eq!(sc("disk/sda/io/write_ops").name, "system.disk.operations");
        assert_eq!(sc("disk/root/used").name, "system.filesystem.usage");
        assert_eq!(
            sc("disk/root/used").attributes,
            vec![
                ("device", "root".to_string()),
                ("state", "used".to_string())
            ]
        );
        assert_eq!(
            sc("disk/root/usage_percent").name,
            "system.filesystem.utilization"
        );
    }

    #[test]
    fn cpu_and_load() {
        assert_eq!(sc("cpu/usage").name, "system.cpu.utilization");
        assert!(sc("cpu/usage").attributes.is_empty());
        assert_eq!(sc("cpu/3/usage").attributes, vec![("cpu", "3".to_string())]);
        assert_eq!(sc("load/1m").name, "system.cpu.load_average.1m");
        assert_eq!(sc("load/15m").name, "system.cpu.load_average.15m");
    }

    #[test]
    fn paging() {
        assert_eq!(sc("memory/swap_used").name, "system.paging.usage");
        assert_eq!(
            sc("memory/paging_in_total").name,
            "system.paging.operations"
        );
        assert_eq!(
            sc("memory/paging_in_total").attributes,
            vec![("direction", "in".to_string())]
        );
        assert_eq!(
            sc("memory/page_faults_major_total").name,
            "system.paging.faults"
        );
    }

    #[test]
    fn unmapped_keys_return_none() {
        // Exotic / non-semconv keys fall back to the raw name.
        assert!(sysinfo_semconv("memory/edac/mc0/correctable_total").is_none());
        assert!(sysinfo_semconv("disk/md/md0/degraded").is_none());
        assert!(sysinfo_semconv("network/conntrack/count").is_none());
        assert!(sysinfo_semconv("cpu/schedstat/run_delay_ns_total").is_none());
        assert!(sysinfo_semconv("saturation_score").is_none());
    }

    #[test]
    fn only_sysinfo_and_systemd_are_mapped() {
        assert!(metric_semconv(Protocol::Sysinfo, "memory/used").is_some());
        assert!(metric_semconv(Protocol::Systemd, "unit/sshd.service/active").is_some());
        assert!(metric_semconv(Protocol::Snmp, "memory/used").is_none());
        assert!(metric_semconv(Protocol::Netlink, "cpu/usage").is_none());
    }

    #[test]
    fn systemd_per_unit_maps_to_clean_name_no_attrs() {
        // Per-unit series collapse to one metric name; the unit rides as the
        // point's existing label, so semconv adds NO attributes (avoids a
        // duplicate `unit` label in the exporters).
        let sc = systemd_semconv("unit/sshd.service/active").unwrap();
        assert_eq!(sc.name, "systemd.unit.active");
        assert!(sc.attributes.is_empty());
        assert_eq!(
            systemd_semconv("unit/user_1000.service/mem_bytes")
                .unwrap()
                .name,
            "systemd.unit.memory_bytes"
        );
        assert_eq!(
            systemd_semconv("unit/sshd.socket/n_accepted").unwrap().name,
            "systemd.socket.accepted"
        );
        assert_eq!(
            systemd_semconv("unit/logrotate.timer/next_trigger_usec")
                .unwrap()
                .name,
            "systemd.timer.next_trigger_usec"
        );
    }

    #[test]
    fn systemd_aggregates_fall_through_to_raw_name() {
        // Aggregates keep the raw zensight.systemd.<metric> name (no per-entity
        // segment to factor out).
        assert!(systemd_semconv("units/failed").is_none());
        assert!(systemd_semconv("manager/n_failed_units").is_none());
        assert!(systemd_semconv("boot/total_usec").is_none());
        assert!(systemd_semconv("mounts/total").is_none());
        assert!(systemd_semconv("journal/disk_usage_bytes").is_none());
    }
}
