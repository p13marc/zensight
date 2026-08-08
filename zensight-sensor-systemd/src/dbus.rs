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
    #[zbus(property, name = "NextElapseUSecRealtime")]
    fn next_elapse_usec_realtime(&self) -> zbus::Result<u64>;
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
