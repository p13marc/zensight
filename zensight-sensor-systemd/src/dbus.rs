//! Shared systemd D-Bus proxies.
//!
//! Typed `#[zbus::proxy]` traits for the `Manager`, `Unit`, and `Service`
//! interfaces, plus the `ListUnits` row alias. Centralized here so the collector
//! (#272/#273), query channel (#274), events (#275), alerts (#276) and sentinel
//! (#277) share one definition.

use zbus::zvariant::OwnedObjectPath;

/// One `ListUnits` row: `(name, description, load_state, active_state, sub_state,
/// following, unit_path, job_id, job_type, job_path)`.
pub type ListedUnit = (
    String,
    String,
    String,
    String,
    String,
    String,
    OwnedObjectPath,
    u32,
    String,
    OwnedObjectPath,
);

/// One `ListUnitFiles` row: `(unit_file_path, state)`, where state is
/// `enabled`/`disabled`/`static`/`masked`/`generated`/…
pub type UnitFileEntry = (String, String);

/// One symlink change from `EnableUnitFiles`/`DisableUnitFiles`:
/// `(change_type, symlink_path, destination)`. `change_type` is `symlink` or
/// `unlink`; `destination` is empty for an unlink.
pub type UnitFileChangeTuple = (String, String, String);

/// The `org.freedesktop.systemd1.Manager` subset we use: scalar counters, the six
/// boot monotonic timestamps, `ListUnits`, `LoadUnit`, and `Subscribe` + signals.
#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1"
)]
pub trait Manager {
    #[zbus(property)]
    fn n_names(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn n_failed_units(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn n_jobs(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn n_installed_jobs(&self) -> zbus::Result<u32>;
    /// Overall system state: `initializing`/`running`/`degraded`/`maintenance`/…
    #[zbus(property)]
    fn system_state(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn firmware_timestamp_monotonic(&self) -> zbus::Result<u64>;
    #[zbus(property)]
    fn loader_timestamp_monotonic(&self) -> zbus::Result<u64>;
    #[zbus(property, name = "InitRDTimestampMonotonic")]
    fn initrd_timestamp_monotonic(&self) -> zbus::Result<u64>;
    #[zbus(property)]
    fn userspace_timestamp_monotonic(&self) -> zbus::Result<u64>;
    #[zbus(property)]
    fn finish_timestamp_monotonic(&self) -> zbus::Result<u64>;

    fn list_units(&self) -> zbus::Result<Vec<ListedUnit>>;
    /// Every *installed* unit file and its enablement state — one call for the
    /// whole host, unlike `GetUnitFileState`, which is per unit.
    fn list_unit_files(&self) -> zbus::Result<Vec<UnitFileEntry>>;
    /// Resolve (loading if needed) a unit name to its object path.
    fn load_unit(&self, name: &str) -> zbus::Result<OwnedObjectPath>;

    // ── Gated service control (#283). `mode` is typically `replace`. Each returns
    // the enqueued job object path, tracked to completion via `JobRemoved`. ──
    fn start_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
    fn stop_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
    fn restart_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
    fn reload_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;

    // ── Unit-file and manager control. These enqueue **no job**, so there is no
    // `JobRemoved` to await: the call returning *is* the outcome. They also need
    // different polkit actions than the four above (`manage-unit-files` and
    // `reload-daemon` rather than `manage-units`), which is why they sit behind
    // their own config switches. ──
    /// `(files, runtime, force) -> (carries_install_info, changes)`. `runtime`
    /// false writes symlinks under `/etc` (persistent across reboots).
    fn enable_unit_files(
        &self,
        files: &[&str],
        runtime: bool,
        force: bool,
    ) -> zbus::Result<(bool, Vec<UnitFileChangeTuple>)>;
    /// `(files, runtime) -> changes`.
    fn disable_unit_files(
        &self,
        files: &[&str],
        runtime: bool,
    ) -> zbus::Result<Vec<UnitFileChangeTuple>>;
    /// daemon-reload: re-read every unit file from disk. Manager-wide, so it
    /// takes no unit and cannot be scoped by the unit allowlist.
    fn reload(&self) -> zbus::Result<()>;

    /// Enable emission of `UnitNew`/`UnitRemoved`/`JobNew`/`JobRemoved` signals.
    fn subscribe(&self) -> zbus::Result<()>;

    #[zbus(signal)]
    fn unit_new(&self, id: String, unit: OwnedObjectPath) -> zbus::Result<()>;
    #[zbus(signal)]
    fn unit_removed(&self, id: String, unit: OwnedObjectPath) -> zbus::Result<()>;
    #[zbus(signal)]
    fn job_new(&self, id: u32, job: OwnedObjectPath, unit: String) -> zbus::Result<()>;
    #[zbus(signal)]
    fn job_removed(
        &self,
        id: u32,
        job: OwnedObjectPath,
        unit: String,
        result: String,
    ) -> zbus::Result<()>;
}

/// The `org.freedesktop.systemd1.Unit` interface subset we read per unit.
#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Unit",
    default_service = "org.freedesktop.systemd1"
)]
pub trait Unit {
    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn description(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn load_state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn active_state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn sub_state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn fragment_path(&self) -> zbus::Result<String>;
    /// Override files layered over `FragmentPath`, in systemd's own order.
    #[zbus(property)]
    fn drop_in_paths(&self) -> zbus::Result<Vec<String>>;
    /// Wall-clock µs of the last active-enter transition (0 if never).
    #[zbus(property)]
    fn active_enter_timestamp(&self) -> zbus::Result<u64>;
    #[zbus(property)]
    fn requires(&self) -> zbus::Result<Vec<String>>;
    #[zbus(property)]
    fn wants(&self) -> zbus::Result<Vec<String>>;
    #[zbus(property)]
    fn after(&self) -> zbus::Result<Vec<String>>;
    #[zbus(property)]
    fn before(&self) -> zbus::Result<Vec<String>>;
    /// Durable per-run identity (16 bytes; all-zero/empty when not running).
    /// Solves "same unit, restarted" the way `start_time` solves PID reuse —
    /// and joins journald lines via `_SYSTEMD_INVOCATION_ID` (#303).
    #[zbus(property, name = "InvocationID")]
    fn invocation_id(&self) -> zbus::Result<Vec<u8>>;
}

/// The `org.freedesktop.systemd1.Service` interface subset — present only on
/// `.service` units; reads fail (→ skipped) on other unit types.
#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Service",
    default_service = "org.freedesktop.systemd1"
)]
pub trait Service {
    #[zbus(property, name = "NRestarts")]
    fn n_restarts(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn memory_current(&self) -> zbus::Result<u64>;
    #[zbus(property, name = "CPUUsageNSec")]
    fn cpu_usage_nsec(&self) -> zbus::Result<u64>;
    #[zbus(property)]
    fn tasks_current(&self) -> zbus::Result<u64>;
    #[zbus(property)]
    fn exec_main_status(&self) -> zbus::Result<i32>;
    /// Outcome of the last completed run: `success`, `exit-code`, `signal`,
    /// `timeout`, `core-dump`, … — the field `ExecMainStatus` alone cannot
    /// replace, because a oneshot's failure mode may not be an exit code
    /// (#824).
    #[zbus(property)]
    fn result(&self) -> zbus::Result<String>;
    #[zbus(property, name = "IPIngressBytes")]
    fn ip_ingress_bytes(&self) -> zbus::Result<u64>;
    #[zbus(property, name = "IPEgressBytes")]
    fn ip_egress_bytes(&self) -> zbus::Result<u64>;
    #[zbus(property, name = "IOReadBytes")]
    fn io_read_bytes(&self) -> zbus::Result<u64>;
    #[zbus(property, name = "IOWriteBytes")]
    fn io_write_bytes(&self) -> zbus::Result<u64>;
    /// Main service PID (0 when not running). Identity is the
    /// `(pid, start_time)` pair — see `main_pid_start_time` on `UnitDetail`.
    #[zbus(property, name = "MainPID")]
    fn main_pid(&self) -> zbus::Result<u32>;
    /// The unit's cgroup path — **the cross-sensor join key**
    /// (`unit.control_group == process.cgroup`, #303).
    #[zbus(property)]
    fn control_group(&self) -> zbus::Result<String>;
}

/// The `org.freedesktop.systemd1.Timer` interface subset (#276 timer-overdue,
/// #279 timer telemetry).
#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Timer",
    default_service = "org.freedesktop.systemd1"
)]
pub trait Timer {
    /// Wall-clock µs of the last trigger (0 if never fired).
    #[zbus(property, name = "LastTriggerUSec")]
    fn last_trigger_usec(&self) -> zbus::Result<u64>;
    /// Wall-clock µs of the next scheduled elapse (0/`u64::MAX` if none).
    ///
    /// **Only calendar timers populate this.** A timer defined with
    /// `OnBootSec=` / `OnUnitActiveSec=` reports 0 here and schedules on
    /// `CLOCK_MONOTONIC` instead — see [`Self::next_elapse_usec_monotonic`].
    #[zbus(property, name = "NextElapseUSecRealtime")]
    fn next_elapse_usec_realtime(&self) -> zbus::Result<u64>;
    /// `CLOCK_MONOTONIC` µs of the next scheduled elapse (0/`u64::MAX` if
    /// none), for the monotonic timers `OnBootSec=` / `OnUnitActiveSec=`
    /// define (#1084).
    ///
    /// Without this bound, every such timer reported a next elapse of 0 and
    /// was skipped by both the overdue rule and the `@rpc` timer listing — so
    /// `systemd-timer-overdue` could never fire for one, however far past its
    /// schedule it was. Verify against a real unit with
    /// `systemctl show -p NextElapseUSecMonotonic systemd-tmpfiles-clean.timer`.
    #[zbus(property, name = "NextElapseUSecMonotonic")]
    fn next_elapse_usec_monotonic(&self) -> zbus::Result<u64>;
    /// The unit this timer activates (`Unit=`, default `<name>.service`) —
    /// the thing whose *outcome* the `succeeded_within_secs` expectation
    /// judges: a timer can fire on schedule for a week while its service
    /// fails every single run (#824).
    #[zbus(property, name = "Unit")]
    fn unit(&self) -> zbus::Result<String>;
}

/// The `org.freedesktop.systemd1.Socket` interface subset (#279 socket telemetry).
/// Present only on `.socket` units.
#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Socket",
    default_service = "org.freedesktop.systemd1"
)]
pub trait Socket {
    #[zbus(property, name = "NAccepted")]
    fn n_accepted(&self) -> zbus::Result<u32>;
    #[zbus(property, name = "NConnections")]
    fn n_connections(&self) -> zbus::Result<u32>;
    #[zbus(property, name = "NRefused")]
    fn n_refused(&self) -> zbus::Result<u32>;
}

/// A next-elapse timestamp systemd did not populate: never scheduled, or
/// scheduled on the other clock.
fn unset(usec: u64) -> bool {
    usec == 0 || usec == u64::MAX
}

/// `CLOCK_MONOTONIC` now, in µs. `None` if the clock cannot be read.
///
/// **`CLOCK_MONOTONIC`, not `CLOCK_BOOTTIME`** — systemd schedules monotonic
/// timers on the former, which excludes suspended time, and mixing the pair
/// would make every timer on a laptop look overdue by the length of its last
/// suspend. `zensight-sensor-netlink`'s eBPF anchor takes the same care for
/// the same reason.
pub fn monotonic_now_usec() -> Option<u64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: CLOCK_MONOTONIC with a valid timespec out-pointer.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return None;
    }
    Some((ts.tv_sec as u64) * 1_000_000 + (ts.tv_nsec as u64) / 1_000)
}

/// One timer's next elapse as **wall-clock µs**, from whichever clock systemd
/// populated (#1084). `0` means "not scheduled", the value both consumers
/// already treat as absent.
///
/// A calendar timer (`OnCalendar=`) fills the realtime property; a monotonic
/// one (`OnBootSec=`, `OnUnitActiveSec=`) fills only the monotonic property
/// and reports 0 for realtime. Reading realtime alone — which is what both
/// call sites did — skipped every monotonic timer silently, so the overdue
/// rule could not fire for one however late it was.
///
/// Pure, so the conversion is testable without a bus or a clock.
pub fn next_elapse_wall_usec(
    realtime_usec: u64,
    monotonic_usec: u64,
    now_wall_usec: u64,
    now_monotonic_usec: Option<u64>,
) -> u64 {
    if !unset(realtime_usec) {
        return realtime_usec;
    }
    let (Some(now_mono), false) = (now_monotonic_usec, unset(monotonic_usec)) else {
        return 0;
    };
    // The monotonic reading is relative to boot; anchor it to wall time. An
    // elapse already in the past yields a wall time in the past, which is
    // exactly what `timer_overdue` needs to see.
    if monotonic_usec >= now_mono {
        now_wall_usec.saturating_add(monotonic_usec - now_mono)
    } else {
        now_wall_usec.saturating_sub(now_mono - monotonic_usec)
    }
}

/// Whether a next-elapse wall timestamp is in the past by more than `grace`.
///
/// One predicate for the alert rule and the `@rpc` timer listing, which were
/// two independent implementations of the same sentence — and only one of them
/// had a grace window.
pub fn timer_overdue(next_elapse_wall_usec: u64, now_wall_usec: u64, grace_usec: u64) -> bool {
    !unset(next_elapse_wall_usec)
        && now_wall_usec > next_elapse_wall_usec.saturating_add(grace_usec)
}

#[cfg(test)]
mod timer_clock_tests {
    use super::*;

    const NOW_WALL: u64 = 1_700_000_000_000_000;
    // Five hours of uptime. Deliberately not one hour: `NOW_MONO - 1h` would
    // be exactly 0, which systemd's own encoding reserves for "unset", so the
    // fixture would be testing the sentinel rather than the conversion.
    const NOW_MONO: u64 = 18_000_000_000;

    /// A calendar timer keeps using the realtime property, untouched.
    #[test]
    fn a_calendar_timer_uses_the_realtime_clock() {
        let next = NOW_WALL + 60_000_000;
        assert_eq!(
            next_elapse_wall_usec(next, 0, NOW_WALL, Some(NOW_MONO)),
            next
        );
    }

    /// #1084: a monotonic-only timer reports 0 for realtime, and used to be
    /// skipped entirely. An hour past its elapse, it is overdue.
    #[test]
    fn a_monotonic_only_timer_an_hour_late_is_overdue() {
        // Scheduled for one hour ago, on the monotonic clock.
        let mono_next = NOW_MONO - 3_600_000_000;
        let wall = next_elapse_wall_usec(0, mono_next, NOW_WALL, Some(NOW_MONO));
        assert_eq!(wall, NOW_WALL - 3_600_000_000);
        assert!(
            timer_overdue(wall, NOW_WALL, 60_000_000),
            "a monotonic timer an hour past its elapse must be overdue"
        );
        // And the reading it replaced — a bare 0 — never could be.
        assert!(!timer_overdue(0, NOW_WALL, 60_000_000));
    }

    /// A monotonic timer scheduled in the future is not overdue.
    #[test]
    fn a_monotonic_timer_still_to_come_is_not_overdue() {
        let wall = next_elapse_wall_usec(0, NOW_MONO + 600_000_000, NOW_WALL, Some(NOW_MONO));
        assert_eq!(wall, NOW_WALL + 600_000_000);
        assert!(!timer_overdue(wall, NOW_WALL, 60_000_000));
    }

    /// Both sentinels mean "not scheduled", on either clock, and a host whose
    /// monotonic clock cannot be read falls back to "not scheduled" rather
    /// than to a wrong-but-plausible time.
    #[test]
    fn an_unscheduled_timer_stays_unscheduled() {
        for (rt, mono) in [(0u64, 0u64), (u64::MAX, 0), (0, u64::MAX)] {
            assert_eq!(next_elapse_wall_usec(rt, mono, NOW_WALL, Some(NOW_MONO)), 0);
        }
        assert_eq!(next_elapse_wall_usec(0, NOW_MONO, NOW_WALL, None), 0);
    }

    /// The grace window is part of the predicate, so both call sites get it.
    #[test]
    fn the_grace_window_holds_a_just_late_timer() {
        let just_late = NOW_WALL - 30_000_000;
        assert!(!timer_overdue(just_late, NOW_WALL, 60_000_000));
        assert!(timer_overdue(just_late, NOW_WALL, 10_000_000));
    }
}
