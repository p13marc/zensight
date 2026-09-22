//! ZenSight systemd sensor library.
//!
//! Reads systemd unit/service state aggregates and boot-performance timings from
//! the `org.freedesktop.systemd1.Manager` D-Bus interface (system bus) and
//! publishes them as [`zensight_common::TelemetryPoint`]s under
//! `zensight/systemd/<host>/…`.
//!
//! The pure mapping (D-Bus property structs → telemetry) lives in [`collector`]
//! as free functions so it is unit-testable without a live bus.

pub mod action;
pub mod alerts;
pub mod cgroup;
pub mod collector;
pub mod command;
pub mod config;
pub mod dbus;
pub mod events;
pub mod journal;
pub mod map;
pub mod query;
pub mod restart_window;
pub mod sentinel;
pub mod unit;

#[cfg(test)]
mod typed_subjects {
    use zensight_common::registry::systemd::Subject;
    use zensight_common::subject::TelemetrySubject;

    /// The generated subjects render the tails this sensor published by hand
    /// (#1274): byte-identical keys, so every consumer's series carries over
    /// — and a unit name is slugged by the builder exactly as `sanitize_unit`
    /// slugs it (its pinned table in `map.rs` still holds).
    #[test]
    fn the_registered_families_render_their_tails() {
        for (subject, tail) in [
            (
                Subject::unit_active("sshd.service"),
                "unit/sshd.service/active",
            ),
            (
                Subject::unit_restarts_total("sshd.service"),
                "unit/sshd.service/restarts_total",
            ),
            (
                Subject::unit_n_refused("sshd.socket"),
                "unit/sshd.socket/n_refused",
            ),
            (
                Subject::unit_next_trigger_usec("logrotate.timer"),
                "unit/logrotate.timer/next_trigger_usec",
            ),
            (Subject::ManagerNFailedUnits, "manager/n_failed_units"),
            (Subject::UnitsInactive, "units/inactive"),
            (Subject::boot("total_usec"), "boot/total_usec"),
            (Subject::MountsFailed, "mounts/failed"),
            (
                Subject::JournalDiskAvailableBytes,
                "journal/disk_available_bytes",
            ),
            (Subject::OtherUnitsTotal, "other/units_total"),
            (
                Subject::events("job_removed_total"),
                "events/job_removed_total",
            ),
        ] {
            assert_eq!(subject.tail(), tail);
        }
        assert_eq!(
            Subject::unit_active("NetworkManager.service").tail(),
            format!(
                "unit/{}/active",
                crate::map::sanitize_unit("NetworkManager.service")
            )
        );
    }
}
