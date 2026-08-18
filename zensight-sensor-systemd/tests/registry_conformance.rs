//! Reverse registry conformance for systemd (#654, RFC 08 §6.1).
//!
//! The forward direction (*published ⊆ registered*) is enforced at runtime by
//! `telemetry_guard::checked_point`, which debug-panics on an unregistered
//! name, and by the mapper unit tests. This asserts the other direction:
//! *registered ⊆ emittable* — a family the registry advertises that no build
//! can publish is a surface `introspect` promises the fleet and nobody serves.
//!
//! Every systemd metric name comes from a pure mapper, so this drives the real
//! ones rather than listing names. Fixtures are populated enough that each
//! conditional branch fires: an under-populated fixture reports a live family
//! as unemitted and sends the next reader hunting a bug that is not there.

use zensight_common::registry::systemd::Subject;
use zensight_common::registry_audit;
use zensight_sensor_systemd::collector::{Aggregates, BootTimestamps, ManagerCounts, build_points};
use zensight_sensor_systemd::events::{EventRecord, EventState};
use zensight_sensor_systemd::map;
use zensight_sensor_systemd::unit::UnitSample;

/// Registered systemd telemetry families this build can never emit, and why.
///
/// Empty. The audit helper keeps it that way: an entry the build *does* emit
/// fails, and so does one the registry no longer declares.
const CONDITIONAL_FAMILIES: &[(&str, &str)] = &[];

/// A unit sample with every optional field present, so the accounting-gated
/// families (`mem_bytes`, `cpu_usec`, `tasks`, `ip_*`, `io_*`) are all emitted.
fn full_unit() -> UnitSample {
    UnitSample {
        name: "nginx.service".to_string(),
        load_state: "loaded".to_string(),
        // `failed` so `exit_code` is emitted — it is meaningful only then.
        active_state: "failed".to_string(),
        sub_state: "failed".to_string(),
        active_enter_usec: 1_700_000_000_000_000,
        n_restarts: 3,
        mem_bytes: Some(64 * 1024 * 1024),
        cpu_usec: Some(12_345_678),
        tasks: Some(17),
        exec_main_status: 1,
        ip_ingress_bytes: Some(4096),
        ip_egress_bytes: Some(2048),
        io_read_bytes: Some(8192),
        io_write_bytes: Some(1024),
    }
}

#[test]
fn every_registered_family_has_an_emitter() {
    let mut emitted: Vec<String> = Vec::new();
    let mut push = |pts: Vec<zensight_common::TelemetryPoint>| {
        emitted.extend(pts.into_iter().map(|p| p.metric));
    };

    push(map::unit_points("h", &full_unit()));

    // Both arms of the IP-rate mapper: rates present, and accounting off — the
    // second emits `ip_accounting=false` rather than a silent zero, so it is a
    // family of its own rather than a missing sample.
    push(map::ip_rate_points(
        "h",
        "nginx.service",
        Some(1024.0),
        Some(512.0),
        false,
    ));
    push(map::ip_rate_points("h", "nginx.service", None, None, true));

    push(map::socket_points("h", "sshd.socket", 10, 2, 1));
    push(map::timer_points(
        "h",
        "logrotate.timer",
        1_700_000_000_000_000,
        1_700_000_600_000_000,
    ));
    push(map::mount_points("h", ["active", "failed", "inactive"]));
    // `disk_available_bytes` is gated on the value being known.
    push(map::journal_points("h", 1_000_000, Some(500_000)));
    push(map::other_points("h", 42));

    push(build_points(
        "h",
        &ManagerCounts {
            n_names: 300,
            n_failed_units: 2,
            n_jobs: 1,
            n_installed_jobs: 9,
        },
        Some(&BootTimestamps {
            firmware: 1_000,
            loader: 2_000,
            initrd: 3_000,
            userspace: 4_000,
            ..Default::default()
        }),
        Some(&Aggregates {
            total: 300,
            active: 250,
            failed: 2,
            loaded: 280,
            inactive: 48,
        }),
    ));

    // D-Bus signal counters: one record is enough to create the `events/{kind}`
    // family, which is variable-headed by kind.
    let events = EventState::new(16);
    events.record(EventRecord {
        ts_unix: 1_700_000_000,
        kind: "job_removed".to_string(),
        unit: Some("nginx.service".to_string()),
        from: Some("active".to_string()),
        to: Some("failed".to_string()),
        job_result: Some("failed".to_string()),
    });
    push(events.counter_points("h"));

    registry_audit::assert_families_covered(
        "systemd",
        &emitted,
        |m| Subject::parse_metric(m).map(|s| s.pattern()),
        CONDITIONAL_FAMILIES,
    );
}
