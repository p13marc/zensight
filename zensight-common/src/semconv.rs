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
/// has no standard equivalent (caller uses the raw name). Only sysinfo host
/// metrics are mapped today; the entry point is protocol-keyed so other sensors
/// can join the table later without touching the exporters.
pub fn metric_semconv(protocol: Protocol, metric: &str) -> Option<SemConv> {
    match protocol {
        Protocol::Sysinfo => sysinfo_semconv(metric),
        _ => None,
    }
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
    fn only_sysinfo_is_mapped() {
        assert!(metric_semconv(Protocol::Sysinfo, "memory/used").is_some());
        assert!(metric_semconv(Protocol::Snmp, "memory/used").is_none());
        assert!(metric_semconv(Protocol::Netlink, "cpu/usage").is_none());
    }
}
