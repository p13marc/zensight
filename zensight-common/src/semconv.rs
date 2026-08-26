//! OpenTelemetry host-metrics semantic-convention mapping (#100, keyspace v2).
//!
//! ZenSight's internal sysinfo metric keys are ad-hoc (`memory/used`,
//! `network/<if>/rx_bytes`, …) and don't map cleanly to the OTel host-metrics
//! semantic conventions (`system.memory.usage{state}`,
//! `system.network.io{direction}`, …). This module is the **single** table both
//! the OTel and Prometheus exporters consult, so exported host metrics are
//! portable and dashboard-compatible with the wider OTel ecosystem.
//!
//! Scope: the **core** USE host metrics (cpu, memory, swap/paging, network,
//! disk I/O, filesystem) plus systemd per-unit series. Keys with no standard
//! equivalent (EDAC, md-raid, schedstat, conntrack, …) return `None`; callers
//! fall back to the raw name. Values pass through unchanged — this maps the
//! metric *identity* (name + attributes), not units, so utilization stays the
//! sensor's 0–100 percent rather than semconv's 0–1 ratio.
//!
//! # Keyed on the registry pattern, not on the metric string (#765)
//!
//! The table used to be keyed on `(Protocol, &str metric)` and each arm
//! **re-split the metric string** to compute its attribute *values* — so
//! `disk/sda/io/read_bytes` produced `device = "sda"` here, while the sysinfo
//! sensor produced `device = "sda"` as a point label. Two producers of one
//! value, and the exporters emitted both:
//!
//! ```text
//! zensight_system_disk_io{device="sda",device="sda",direction="read",…}
//! ```
//!
//! which is an invalid series (#753). The systemd half of the table already
//! carried a workaround for exactly this — its attributes were left empty with
//! a comment saying the point "already carries the `unit` label, and the
//! exporters would otherwise emit it twice". That patched one symptom by
//! omission and left the sysinfo one live.
//!
//! Now an entry is keyed on `(producer, registry pattern)` and only ever
//! **names** an attribute:
//!
//! - `constants` — values the pattern cannot supply (`state`, `direction`, `type`)
//! - `var_renames` — registry variable → semconv attribute name
//!   (`{mount}` → `device`, `{iface}` → `device`, `{core}` → `cpu`)
//!
//! Values come from [`AnySubject::vars`], which is the registry's own parse.
//! There is exactly one producer of each value, so the duplicate is
//! unrepresentable at the source rather than merely caught downstream — and the
//! second parser of a keyspace the registry already parses is gone, which is
//! the thing #475 asked consumers to stop doing.

use crate::registry::AnySubject;
use crate::telemetry::Protocol;
use zenkey::grammar::Class;

/// One table entry, keyed by `(producer, registry pattern)`.
///
/// Deliberately private: an entry is not useful on its own, only materialised
/// against a parsed subject's variables.
struct Entry {
    /// The OTel semconv metric name, dotted (e.g. `system.memory.usage`).
    name: &'static str,
    /// Attributes the pattern cannot supply, as literal values.
    constants: &'static [(&'static str, &'static str)],
    /// Registry variable name → semconv attribute name. The *value* always
    /// comes from the parsed subject, never from re-splitting a string here.
    var_renames: &'static [(&'static str, &'static str)],
}

/// A materialised semconv mapping for one telemetry point: the semconv metric
/// name plus its factored attributes. Attribute keys are static; values owned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemConv {
    /// The OTel semconv metric name, dotted (e.g. `system.memory.usage`).
    pub name: &'static str,
    /// Factored semconv attributes, e.g. `[("state", "used")]`.
    pub attributes: Vec<(&'static str, String)>,
}

impl Entry {
    /// Bind this entry's attribute *names* to values from the parsed subject.
    ///
    /// A `var_rename` naming a variable the subject does not have is skipped
    /// rather than emitted empty — `semconv_and_registry_agree` is what stops
    /// that being silent.
    fn materialize(&self, vars: &[(&'static str, String)]) -> SemConv {
        let mut attributes: Vec<(&'static str, String)> =
            Vec::with_capacity(self.constants.len() + self.var_renames.len());
        for (k, v) in self.constants {
            attributes.push((*k, (*v).to_string()));
        }
        for (var, attr) in self.var_renames {
            if let Some((_, value)) = vars.iter().find(|(n, _)| n == var) {
                attributes.push((*attr, value.clone()));
            }
        }
        SemConv {
            name: self.name,
            attributes,
        }
    }
}

/// The whole table. `None` means "no standard equivalent" — the caller falls
/// back to the raw name, which is the honest answer for EDAC, md-raid,
/// schedstat, cgroup and conntrack keys.
fn entry(producer: &str, pattern: &str) -> Option<Entry> {
    let e = |name,
             constants: &'static [(&'static str, &'static str)],
             var_renames: &'static [(&'static str, &'static str)]| {
        Some(Entry {
            name,
            constants,
            var_renames,
        })
    };

    match (producer, pattern) {
        // ── CPU ──
        //
        // Both arms map to one name, distinguished by the `cpu` attribute. That
        // is the *intended* family collision the naming rule allows (#770's
        // INTENDED_COLLISIONS): an aggregate and its per-entity refinement.
        ("sysinfo", "cpu/usage") => e("system.cpu.utilization", &[], &[]),
        ("sysinfo", "cpu/{core}/usage") => e("system.cpu.utilization", &[], &[("core", "cpu")]),

        // ── Memory (semconv states: used / cached / buffered / free) ──
        ("sysinfo", "memory/used") => e("system.memory.usage", &[("state", "used")], &[]),
        ("sysinfo", "memory/cached") => e("system.memory.usage", &[("state", "cached")], &[]),
        ("sysinfo", "memory/buffers") => e("system.memory.usage", &[("state", "buffered")], &[]),
        ("sysinfo", "memory/available") => e("system.memory.usage", &[("state", "free")], &[]),
        ("sysinfo", "memory/total") => e("system.memory.limit", &[], &[]),
        ("sysinfo", "memory/usage_percent") => e("system.memory.utilization", &[], &[]),

        // ── Swap / paging ──
        ("sysinfo", "memory/swap_used") => e("system.paging.usage", &[("state", "used")], &[]),
        ("sysinfo", "memory/swap_percent") => e("system.paging.utilization", &[], &[]),
        ("sysinfo", "memory/paging_in_total") => {
            e("system.paging.operations", &[("direction", "in")], &[])
        }
        ("sysinfo", "memory/paging_out_total") => {
            e("system.paging.operations", &[("direction", "out")], &[])
        }
        ("sysinfo", "memory/page_faults_major_total") => {
            e("system.paging.faults", &[("type", "major")], &[])
        }

        // ── Network (per interface). `{iface}` is semconv's `device`. ──
        ("sysinfo", "network/{iface}/rx_bytes") => e(
            "system.network.io",
            &[("direction", "receive")],
            &[("iface", "device")],
        ),
        ("sysinfo", "network/{iface}/tx_bytes") => e(
            "system.network.io",
            &[("direction", "transmit")],
            &[("iface", "device")],
        ),
        ("sysinfo", "network/{iface}/rx_packets") => e(
            "system.network.packets",
            &[("direction", "receive")],
            &[("iface", "device")],
        ),
        ("sysinfo", "network/{iface}/tx_packets") => e(
            "system.network.packets",
            &[("direction", "transmit")],
            &[("iface", "device")],
        ),
        ("sysinfo", "network/{iface}/rx_errors") => e(
            "system.network.errors",
            &[("direction", "receive")],
            &[("iface", "device")],
        ),
        ("sysinfo", "network/{iface}/tx_errors") => e(
            "system.network.errors",
            &[("direction", "transmit")],
            &[("iface", "device")],
        ),
        ("sysinfo", "network/{iface}/rx_dropped") => e(
            "system.network.dropped",
            &[("direction", "receive")],
            &[("iface", "device")],
        ),
        ("sysinfo", "network/{iface}/tx_dropped") => e(
            "system.network.dropped",
            &[("direction", "transmit")],
            &[("iface", "device")],
        ),

        // ── Disk I/O (per device) ──
        ("sysinfo", "disk/{device}/io/read_bytes") => e(
            "system.disk.io",
            &[("direction", "read")],
            &[("device", "device")],
        ),
        ("sysinfo", "disk/{device}/io/write_bytes") => e(
            "system.disk.io",
            &[("direction", "write")],
            &[("device", "device")],
        ),
        ("sysinfo", "disk/{device}/io/read_ops") => e(
            "system.disk.operations",
            &[("direction", "read")],
            &[("device", "device")],
        ),
        ("sysinfo", "disk/{device}/io/write_ops") => e(
            "system.disk.operations",
            &[("direction", "write")],
            &[("device", "device")],
        ),

        // ── Filesystem. NOTE the registry var is `{mount}`, and semconv's
        //    attribute is `device` — a rename, not an identity. Getting this
        //    wrong is how the old table produced a value the sensor also
        //    produced.
        ("sysinfo", "disk/{mount}/used") => e(
            "system.filesystem.usage",
            &[("state", "used")],
            &[("mount", "device")],
        ),
        ("sysinfo", "disk/{mount}/available") => e(
            "system.filesystem.usage",
            &[("state", "free")],
            &[("mount", "device")],
        ),
        ("sysinfo", "disk/{mount}/usage_percent") => {
            e("system.filesystem.utilization", &[], &[("mount", "device")])
        }

        // ── systemd per-unit series (#282) ──
        //
        // The unit name now rides as a *renamed pattern var*. It used to be
        // omitted entirely, with a comment explaining that the point already
        // carried a `unit` label and the exporters would otherwise emit it
        // twice — a workaround for #753 applied here and nowhere else. With one
        // producer of the value, naming it is safe and the mapping is honest.
        ("systemd", "unit/{unit}/active") => e("systemd.unit.active", &[], &[("unit", "unit")]),
        ("systemd", "unit/{unit}/state") => e("systemd.unit.state", &[], &[("unit", "unit")]),
        ("systemd", "unit/{unit}/restarts_total") => {
            e("systemd.unit.restarts", &[], &[("unit", "unit")])
        }
        ("systemd", "unit/{unit}/active_since_usec") => {
            e("systemd.unit.active_since_usec", &[], &[("unit", "unit")])
        }
        ("systemd", "unit/{unit}/mem_bytes") => {
            e("systemd.unit.memory_bytes", &[], &[("unit", "unit")])
        }
        ("systemd", "unit/{unit}/cpu_usec") => e("systemd.unit.cpu_usec", &[], &[("unit", "unit")]),
        ("systemd", "unit/{unit}/tasks") => e("systemd.unit.tasks", &[], &[("unit", "unit")]),
        ("systemd", "unit/{unit}/exit_code") => {
            e("systemd.unit.exit_code", &[], &[("unit", "unit")])
        }
        ("systemd", "unit/{unit}/ip_ingress_bytes") => {
            e("systemd.unit.ip_ingress_bytes", &[], &[("unit", "unit")])
        }
        ("systemd", "unit/{unit}/ip_egress_bytes") => {
            e("systemd.unit.ip_egress_bytes", &[], &[("unit", "unit")])
        }
        ("systemd", "unit/{unit}/io_read_bytes") => {
            e("systemd.unit.io_read_bytes", &[], &[("unit", "unit")])
        }
        ("systemd", "unit/{unit}/io_write_bytes") => {
            e("systemd.unit.io_write_bytes", &[], &[("unit", "unit")])
        }
        ("systemd", "unit/{unit}/n_accepted") => {
            e("systemd.socket.accepted", &[], &[("unit", "unit")])
        }
        ("systemd", "unit/{unit}/n_connections") => {
            e("systemd.socket.connections", &[], &[("unit", "unit")])
        }
        ("systemd", "unit/{unit}/n_refused") => {
            e("systemd.socket.refused", &[], &[("unit", "unit")])
        }
        ("systemd", "unit/{unit}/last_trigger_usec") => {
            e("systemd.timer.last_trigger_usec", &[], &[("unit", "unit")])
        }
        ("systemd", "unit/{unit}/next_trigger_usec") => {
            e("systemd.timer.next_trigger_usec", &[], &[("unit", "unit")])
        }

        // Deliberately unmapped, and worth writing down:
        //
        // `system/load` carries the window in a `period` LABEL (1m/5m/15m),
        // while OTel semconv has three separate metrics
        // (`system.cpu.load_average.1m` and friends). This table maps one
        // pattern to one name and cannot split a family by label value, so the
        // key falls through to its raw name — which is already aggregatable,
        // because `period` is a label rather than a name chunk.
        //
        // The old table had entries for `load/1m` / `load/5m` / `load/15m`.
        // **No sensor has ever published those keys** — sysinfo emits
        // `system/load` — so those arms mapped nothing, and the only thing
        // referencing them was this module's own unit test. That is precisely
        // the failure `semconv_and_registry_agree` now catches.
        _ => None,
    }
}

/// Map a parsed subject to OTel semconv, or `None` when it has no standard
/// equivalent.
///
/// This is the entry point for callers that already hold an [`AnySubject`] —
/// they have the pattern and the variables, so nothing is re-parsed.
pub fn semconv_of(subject: &AnySubject) -> Option<SemConv> {
    entry(subject.producer_name(), subject.pattern()).map(|e| e.materialize(&subject.vars()))
}

/// Map a telemetry `(protocol, metric)` to OTel semconv.
///
/// Resolves the metric through the registry first, so the pattern and the
/// variable values both come from the generated parser rather than from a
/// second hand-rolled split. `None` when the key is unregistered or has no
/// standard equivalent.
pub fn metric_semconv(protocol: Protocol, metric: &str) -> Option<SemConv> {
    let producer = protocol.as_str();
    let chunks: Vec<&str> = metric.split('/').collect();
    let subject = crate::registry::parse_subject(producer, Class::Telemetry, &chunks)?;
    semconv_of(&subject)
}

/// Every `(producer, pattern)` the table maps, for conformance tests.
///
/// Kept in lockstep with [`entry`] by `semconv_table_is_complete`, which walks
/// the registry and asserts the two agree — so a pattern added here without a
/// registry entry, or a registry rename that orphans an entry, fails the build
/// rather than silently mapping nothing.
pub const MAPPED_PATTERNS: &[(&str, &str)] = &[
    ("sysinfo", "cpu/usage"),
    ("sysinfo", "cpu/{core}/usage"),
    ("sysinfo", "memory/used"),
    ("sysinfo", "memory/cached"),
    ("sysinfo", "memory/buffers"),
    ("sysinfo", "memory/available"),
    ("sysinfo", "memory/total"),
    ("sysinfo", "memory/usage_percent"),
    ("sysinfo", "memory/swap_used"),
    ("sysinfo", "memory/swap_percent"),
    ("sysinfo", "memory/paging_in_total"),
    ("sysinfo", "memory/paging_out_total"),
    ("sysinfo", "memory/page_faults_major_total"),
    ("sysinfo", "network/{iface}/rx_bytes"),
    ("sysinfo", "network/{iface}/tx_bytes"),
    ("sysinfo", "network/{iface}/rx_packets"),
    ("sysinfo", "network/{iface}/tx_packets"),
    ("sysinfo", "network/{iface}/rx_errors"),
    ("sysinfo", "network/{iface}/tx_errors"),
    ("sysinfo", "network/{iface}/rx_dropped"),
    ("sysinfo", "network/{iface}/tx_dropped"),
    ("sysinfo", "disk/{device}/io/read_bytes"),
    ("sysinfo", "disk/{device}/io/write_bytes"),
    ("sysinfo", "disk/{device}/io/read_ops"),
    ("sysinfo", "disk/{device}/io/write_ops"),
    ("sysinfo", "disk/{mount}/used"),
    ("sysinfo", "disk/{mount}/available"),
    ("sysinfo", "disk/{mount}/usage_percent"),
    ("systemd", "unit/{unit}/active"),
    ("systemd", "unit/{unit}/state"),
    ("systemd", "unit/{unit}/restarts_total"),
    ("systemd", "unit/{unit}/active_since_usec"),
    ("systemd", "unit/{unit}/mem_bytes"),
    ("systemd", "unit/{unit}/cpu_usec"),
    ("systemd", "unit/{unit}/tasks"),
    ("systemd", "unit/{unit}/exit_code"),
    ("systemd", "unit/{unit}/ip_ingress_bytes"),
    ("systemd", "unit/{unit}/ip_egress_bytes"),
    ("systemd", "unit/{unit}/io_read_bytes"),
    ("systemd", "unit/{unit}/io_write_bytes"),
    ("systemd", "unit/{unit}/n_accepted"),
    ("systemd", "unit/{unit}/n_connections"),
    ("systemd", "unit/{unit}/n_refused"),
    ("systemd", "unit/{unit}/last_trigger_usec"),
    ("systemd", "unit/{unit}/next_trigger_usec"),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn sc(metric: &str) -> SemConv {
        metric_semconv(Protocol::Sysinfo, metric)
            .unwrap_or_else(|| panic!("no semconv for {metric}"))
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
            sc("memory/available").attributes,
            vec![("state", "free".to_string())]
        );
    }

    #[test]
    fn network_factors_direction_and_device() {
        let n = sc("network/eth0/rx_bytes");
        assert_eq!(n.name, "system.network.io");
        assert_eq!(
            n.attributes,
            vec![
                ("direction", "receive".to_string()),
                ("device", "eth0".to_string()),
            ]
        );
    }

    /// The value comes from the registry parse, not from re-splitting here —
    /// which is what makes the duplicate impossible (#753/#765).
    #[test]
    fn disk_io_factors_direction_and_device() {
        let d = sc("disk/sda/io/read_bytes");
        assert_eq!(d.name, "system.disk.io");
        assert_eq!(
            d.attributes,
            vec![
                ("direction", "read".to_string()),
                ("device", "sda".to_string()),
            ]
        );
    }

    /// Filesystem's semconv `device` comes from the `{mount}` variable. The
    /// rename is the whole point: a table that assumed identity would have to
    /// compute the value itself.
    ///
    /// The mount is slugged into the key by the sensor (`sanitize_key`: `/var`
    /// -> `var`, `/` -> `root`), because a key chunk must begin and end
    /// alphanumeric. The sensor also carries the *unslugged* path in its own
    /// `mount` label, so both survive the merge under different names.
    #[test]
    fn filesystem_renames_mount_to_device() {
        let f = sc("disk/var/used");
        assert_eq!(f.name, "system.filesystem.usage");
        assert_eq!(
            f.attributes,
            vec![("state", "used".to_string()), ("device", "var".to_string())]
        );

        let root = sc("disk/root/usage_percent");
        assert_eq!(root.name, "system.filesystem.utilization");
        assert_eq!(root.attributes, vec![("device", "root".to_string())]);
    }

    /// An aggregate and its per-entity refinement share a family, told apart by
    /// the `cpu` attribute — and `{core}` is renamed to it.
    #[test]
    fn cpu_aggregate_and_per_core_share_a_family() {
        assert_eq!(sc("cpu/usage").name, "system.cpu.utilization");
        assert!(sc("cpu/usage").attributes.is_empty());

        let per_core = sc("cpu/3/usage");
        assert_eq!(per_core.name, "system.cpu.utilization");
        assert_eq!(per_core.attributes, vec![("cpu", "3".to_string())]);
    }

    #[test]
    fn systemd_units_carry_their_unit_name() {
        let u = metric_semconv(Protocol::Systemd, "unit/sshd.service/active")
            .expect("systemd unit mapped");
        assert_eq!(u.name, "systemd.unit.active");
        assert_eq!(u.attributes, vec![("unit", "sshd.service".to_string())]);
    }

    #[test]
    fn only_sysinfo_and_systemd_are_mapped() {
        assert!(metric_semconv(Protocol::Sysinfo, "memory/used").is_some());
        assert!(metric_semconv(Protocol::Systemd, "unit/sshd.service/active").is_some());
        // SNMP is deliberately unmapped (#647): its metric tree is defined by
        // the polled device, so there is nothing stable to map.
        assert!(metric_semconv(Protocol::Snmp, "memory/used").is_none());
        assert!(metric_semconv(Protocol::Netlink, "cpu/usage").is_none());
    }

    /// Unmapped-but-registered keys fall through to the raw name rather than
    /// erroring — EDAC, md-raid, cgroup and conntrack have no semconv analogue.
    #[test]
    fn unmapped_registered_keys_return_none() {
        assert!(metric_semconv(Protocol::Sysinfo, "network/conntrack/count").is_none());
        assert!(metric_semconv(Protocol::Sysinfo, "cgroup/memory/current").is_none());
    }

    /// `load/1m` was in the old table and **no sensor ever published it** —
    /// sysinfo emits `system/load` with a `period` label. The entry mapped
    /// nothing, and this module's own test was the only reference to it.
    #[test]
    fn the_dead_load_entries_are_gone() {
        assert!(
            metric_semconv(Protocol::Sysinfo, "load/1m").is_none(),
            "load/1m is not a key any sensor publishes"
        );
        assert!(
            metric_semconv(Protocol::Sysinfo, "system/load").is_none(),
            "system/load is deliberately unmapped: semconv splits it into three \
             metrics, we carry the window as a `period` label"
        );
    }

    /// Every `(producer, pattern)` the table claims to map must exist in that
    /// producer's registry, and every `var_rename` must name a variable that
    /// pattern actually has.
    ///
    /// This is the guard whose absence let the `load/*` entries rot.
    #[test]
    fn semconv_table_is_complete() {
        for (producer, pattern) in MAPPED_PATTERNS {
            let registered = crate::registry_audit::registered_telemetry_patterns(producer);
            assert!(
                registered.iter().any(|p| p == pattern),
                "semconv maps ({producer}, {pattern:?}) but the registry has no such \
                 telemetry subject — the entry maps nothing"
            );

            let e = entry(producer, pattern).unwrap_or_else(|| {
                panic!("MAPPED_PATTERNS lists ({producer}, {pattern:?}) but `entry` returns None")
            });
            for (var, attr) in e.var_renames {
                assert!(
                    pattern.contains(&format!("{{{var}}}")),
                    "({producer}, {pattern:?}) renames {var:?} -> {attr:?} but the \
                     pattern has no {{{var}}} variable"
                );
            }
        }
    }
}
