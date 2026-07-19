//! Every metric this sensor can emit is a registered subject (RFC 08 §5, issue #468).
//!
//! Before #468, `sysinfo.toml` registered its whole telemetry tree as one
//! `{metric...}` catch-all, so the registry's lint — *every published key is
//! buildable from a registry entry* — was **vacuously true**: any subject
//! whatsoever was buildable, and `introspect` could say nothing about what the
//! sensor actually emits. This test is the other half of the deal. It drives
//! every mapper with a fully-populated sample and asserts each metric name it
//! produces parses back out of the registry.
//!
//! Add a metric without registering it and this fails. That is the point.
//!
//! The publish path is guarded too (`zensight_common::metric_guard`), which
//! catches the metric names built inline in `collector.rs` when a sensor
//! actually runs; this test is the CI-time half that needs no Zenoh session.

use zensight_common::registry::is_registered_telemetry;
use zensight_sensor_sysinfo::map::*;

#[track_caller]
fn assert_all_registered(what: &str, metrics: &[Metric]) {
    assert!(
        !metrics.is_empty(),
        "{what}: mapper produced nothing — the fixture is not exercising it, \
         so this test would pass vacuously"
    );
    for m in metrics {
        assert!(
            is_registered_telemetry("sysinfo", &m.metric),
            "{what}: metric {:?} is not a registered sysinfo subject — \
             add it to zensight-common/registry/sysinfo.toml (RFC 08 §5)",
            m.metric
        );
    }
}

fn pressure() -> PressureSample {
    PressureSample {
        avg10: 1.5,
        avg60: 0.5,
        avg300: 0.1,
        total_us: 12_345,
    }
}

#[test]
fn pressure_metrics_are_registered() {
    // Every resource × scope, so the fixture covers the whole family.
    let psi = PsiSample {
        cpu_some: Some(pressure()),
        memory_some: Some(pressure()),
        memory_full: Some(pressure()),
        io_some: Some(pressure()),
        io_full: Some(pressure()),
    };
    assert_all_registered("pressure", &map_pressure(&psi));
}

#[test]
fn vmstat_and_kernel_metrics_are_registered() {
    let vm = VmStat {
        oom_kill: Some(1),
        pgmajfault: Some(2),
        pgfault: Some(3),
        pswpin: Some(4),
        pswpout: Some(5),
        pgpgin: Some(6),
        pgpgout: Some(7),
    };
    assert_all_registered("vmstat", &map_vmstat(&vm));

    let k = KernelDerivatives {
        context_switches: 100,
        forks: 20,
        procs_running: Some(3),
        procs_blocked: Some(1),
    };
    assert_all_registered("kernel derivatives", &map_kernel_derivatives(&k));
}

#[test]
fn fd_and_inode_metrics_are_registered() {
    assert_all_registered("fd", &map_fd(&FdStat { used: 1, max: 2 }));

    let inodes = [InodeStat {
        mount: "/var/log".into(),
        fs_type: "ext4".into(),
        total: 100,
        free: 40,
        used: 60,
    }];
    assert_all_registered("inodes", &map_inodes(&inodes));
}

#[test]
fn net_dev_metrics_are_registered() {
    let stats = [NetDevStat {
        iface: "eth0".into(),
        rx_dropped: 1,
        rx_fifo: 2,
        rx_frame: 3,
        multicast: 4,
        tx_dropped: 5,
        tx_fifo: 6,
        tx_colls: 7,
        tx_carrier: 8,
    }];
    assert_all_registered("net_dev", &map_net_dev(&stats));
}

#[test]
fn cgroup_metrics_are_registered() {
    let c = CgroupSample {
        path: "/system.slice/foo.service".into(),
        cpu_nr_throttled: Some(1),
        cpu_throttled_usec: Some(2),
        memory_current: Some(3),
        memory_max: Some(4),
        memory_oom_kills: Some(5),
        memory_oom: Some(6),
        cpu_pressure_some: Some(pressure()),
        memory_pressure_some: Some(pressure()),
        memory_pressure_full: Some(pressure()),
        io_pressure_some: Some(pressure()),
        io_pressure_full: Some(pressure()),
    };
    assert_all_registered("cgroup", &map_cgroup(&c));
}

#[test]
fn power_metrics_are_registered() {
    let p = PowerSample {
        // `zone` is the powercap directory ("intel-rapl:0"), `name` the label
        // inside it ("package-0") — not the other way round. The colon is the
        // point: it survives `sanitize_key` into the published key.
        rapl_watts: vec![("intel-rapl:0".into(), "package-0".into(), 12.5)],
        fans: vec![FanReading {
            chip: "nct6798".into(),
            label: "fan1".into(),
            rpm: 900.0,
        }],
        batteries: vec![BatteryReading {
            name: "BAT0".into(),
            capacity: Some(80.0),
            status: Some("Discharging".into()),
        }],
        entropy_avail: Some(256),
    };
    assert_all_registered("power", &map_power(&p));
}

#[test]
fn network_stack_metrics_are_registered() {
    assert_all_registered(
        "netstat",
        &map_netstat(&NetstatSample {
            tcp_retrans_segs: Some(1),
            listen_overflows: Some(2),
            listen_drops: Some(3),
        }),
    );
    assert_all_registered(
        "sockstat",
        &map_sockstat(&SockstatSample {
            sockets_used: Some(1),
            tcp_inuse: Some(2),
            tcp_mem_pages: Some(3),
            udp_inuse: Some(4),
        }),
    );
    assert_all_registered(
        "softnet",
        &map_softnet(&SoftnetSample {
            processed: 1,
            dropped: 2,
            squeezed: 3,
        }),
    );
    assert_all_registered(
        "conntrack",
        &map_conntrack(&ConntrackSample {
            count: 10,
            max: Some(100),
        }),
    );
}

#[test]
fn schedstat_metrics_are_registered() {
    // Both the host aggregate (`cpu/schedstat/...`) and the per-CPU rows
    // (`cpu0/schedstat/...`, whose *first* chunk is the variable).
    let s = SchedstatSample {
        per_cpu: vec![(0, 111), (1, 222)],
        total_run_delay_ns: 333,
    };
    let metrics = map_schedstat(&s);
    assert_all_registered("schedstat", &metrics);
    assert!(
        metrics
            .iter()
            .any(|m| m.metric == "cpu0/schedstat/run_delay_ns_total"),
        "the per-CPU (variable-head) family must be exercised, not just the aggregate"
    );
}

#[test]
fn edac_and_mdstat_metrics_are_registered() {
    let edac = [EdacSample {
        controller: "mc0".into(),
        ce: 1,
        ue: 0,
    }];
    assert_all_registered("edac", &map_edac(&edac));

    let arrays = [MdArray {
        name: "md0".into(),
        active: true,
        total_disks: Some(2),
        active_disks: Some(1),
        failed_disks: 1,
        degraded: true,
    }];
    assert_all_registered("mdstat", &map_mdstat(&arrays));
}

/// The families built inline in `collector.rs` rather than in a mapper.
///
/// These are plain literals in the collector's `publish(...)` calls, so there
/// is no sample type to drive; pinning one representative per family keeps
/// them honest. A live sensor is covered by the publish-path guard
/// (`zensight_common::metric_guard`), which panics in debug on an unregistered
/// key.
#[test]
fn collector_inline_families_are_registered() {
    for metric in [
        // system/*
        "system/uptime",
        "system/load",
        "system/boot_time",
        "system/processes_total",
        "system/processes_zombie",
        "system/saturation_score",
        "system/health_state",
        // cpu/* — the host aggregate, the per-core family, and host times
        "cpu/usage",
        "cpu/0/usage",
        "cpu/0/frequency",
        "cpu/times/user",
        "cpu/times/steal",
        // memory/*
        "memory/total",
        "memory/used",
        "memory/available",
        "memory/usage_percent",
        "memory/swap_total",
        "memory/swap_used",
        "memory/swap_percent",
        "memory/cached",
        "memory/buffers",
        "memory/slab",
        "memory/dirty",
        "memory/writeback",
        // disk/* — the three shapes that share the `disk` head
        "disk/root/total",
        "disk/root/usage_percent",
        "disk/sda/io/read_bytes",
        "disk/nvme0n1/io/util_percent",
        // network/{iface}/*
        "network/eth0/rx_bytes",
        "network/eth0/tx_rate",
        // tcp/* — the socket-state histogram
        "tcp/established",
        "tcp/total",
        // sensors/*
        "sensors/coretemp/core_0/temp",
        "sensors/coretemp/core_0/critical",
        "sensors/dell_ddv/cpu_fan/rpm",
        // process/{rank}/* — the `collect.processes` top-N stream. Absent from
        // this list until 2026-07-16, which is how it shipped unregistered: the
        // flag defaults off, so nothing exercised it and only the runtime
        // metric_guard ever complained (loudly, once the demo turned it on).
        "process/1/cpu",
        "process/1/memory",
    ] {
        assert!(
            is_registered_telemetry("sysinfo", metric),
            "collector metric {metric:?} is not a registered sysinfo subject"
        );
    }
}

/// Precedence: a literal beats a `{var}` beats a `{var...}` (RFC 08 §1).
///
/// Several families overlap structurally — `cpu/usage` (host) vs
/// `cpu/{core}/usage` (per-core); `network/tcp/*` sitting inside
/// `network/{iface}/*`'s shadow; the three shapes sharing the `disk` head. The
/// generated parser must resolve each to its own entry.
#[test]
fn overlapping_families_resolve_to_the_right_entry() {
    use zensight_common::registry::sysinfo::Subject;

    // Host aggregate vs per-core: same literal head, different arity.
    assert_eq!(Subject::parse_metric("cpu/usage"), Some(Subject::CpuUsage));
    assert_eq!(
        Subject::parse_metric("cpu/3/usage"),
        Some(Subject::CpuCoreUsage { core: "3".into() })
    );

    // `cpu/times/user` (host) must beat `{cpu}/times/{component}` (per-CPU),
    // which is literal-beats-var at position 0.
    assert_eq!(
        Subject::parse_metric("cpu/times/user"),
        Some(Subject::CpuTimes {
            component: "user".into()
        })
    );
    assert_eq!(
        Subject::parse_metric("cpu0/times/user"),
        Some(Subject::PerCpuTimes {
            cpu: "cpu0".into(),
            component: "user".into()
        })
    );

    // `network/tcp/...` is a literal subtree inside `network/{iface}/...`.
    assert_eq!(
        Subject::parse_metric("network/tcp/retrans_segs_total"),
        Some(Subject::NetworkTcpRetransSegsTotal)
    );
    assert_eq!(
        Subject::parse_metric("network/eth0/rx_bytes"),
        Some(Subject::NetworkRxBytes {
            iface: "eth0".into()
        })
    );

    // Three shapes share the `disk` head, disjoint by arity + literal position.
    assert_eq!(
        Subject::parse_metric("disk/root/used"),
        Some(Subject::DiskUsed {
            mount: "root".into()
        })
    );
    assert_eq!(
        Subject::parse_metric("disk/sda/io/read_bytes"),
        Some(Subject::DiskIoReadBytes {
            device: "sda".into()
        })
    );
    assert_eq!(
        Subject::parse_metric("disk/md/md0/degraded"),
        Some(Subject::DiskMdDegraded {
            array: "md0".into()
        })
    );

    // The payoff for #475: the variable is typed and named, not `parts[1]`.
    let s = Subject::parse_metric("sensors/coretemp/core_0/temp").unwrap();
    assert_eq!(
        s.vars(),
        vec![("chip", "coretemp".into()), ("label", "core_0".into())]
    );

    // And the whole point of dropping the catch-all: an unregistered metric
    // returns None now, instead of matching `{metric...}`.
    assert_eq!(Subject::parse_metric("not/a/real/metric"), None);
    assert_eq!(Subject::parse_metric("memory/usedd"), None); // a typo is now caught
}
