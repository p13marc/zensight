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
    /// Resolve (loading if needed) a unit name to its object path.
    fn load_unit(&self, name: &str) -> zbus::Result<OwnedObjectPath>;

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
}

/// The `org.freedesktop.systemd1.Timer` interface subset (#276 timer-overdue).
#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Timer",
    default_service = "org.freedesktop.systemd1"
)]
pub trait Timer {
    /// Wall-clock µs of the last trigger (0 if never fired).
    #[zbus(property, name = "LastTriggerUSec")]
    fn last_trigger_usec(&self) -> zbus::Result<u64>;
}
