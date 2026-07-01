//! Per-unit D-Bus readout.
//!
//! Typed proxies for the `org.freedesktop.systemd1.Unit` and `.Service`
//! interfaces and a [`UnitSample`] snapshot combining both. Resource counters
//! (memory/cpu/tasks/ip/io) are only populated when the unit has the matching
//! accounting enabled; systemd reports `u64::MAX` otherwise, which we map to
//! `None`. Shared by the collector (#273 telemetry), the query channel (#274),
//! the threshold alerts (#276) and the sentinel (#277).

use zbus::zvariant::OwnedObjectPath;

/// The `org.freedesktop.systemd1.Unit` interface subset we read per unit.
#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Unit",
    default_service = "org.freedesktop.systemd1"
)]
trait Unit {
    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn load_state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn active_state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn sub_state(&self) -> zbus::Result<String>;
    /// Wall-clock µs of the last active-enter transition (0 if never).
    #[zbus(property)]
    fn active_enter_timestamp(&self) -> zbus::Result<u64>;
}

/// The `org.freedesktop.systemd1.Service` interface subset — present only on
/// `.service` units; reads fail (→ skipped) on other unit types.
#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Service",
    default_service = "org.freedesktop.systemd1"
)]
trait Service {
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

/// A per-unit snapshot combining `Unit` state and (best-effort) `Service`
/// resource accounting. `None` resource fields = accounting disabled / not a
/// service.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UnitSample {
    pub name: String,
    pub load_state: String,
    pub active_state: String,
    pub sub_state: String,
    pub active_enter_usec: u64,
    pub n_restarts: u32,
    pub mem_bytes: Option<u64>,
    pub cpu_usec: Option<u64>,
    pub tasks: Option<u64>,
    /// Exit status of the main process (meaningful when `active_state == failed`).
    pub exec_main_status: i32,
    pub ip_ingress_bytes: Option<u64>,
    pub ip_egress_bytes: Option<u64>,
    pub io_read_bytes: Option<u64>,
    pub io_write_bytes: Option<u64>,
}

impl UnitSample {
    pub fn is_active(&self) -> bool {
        self.active_state == "active"
    }
    pub fn is_failed(&self) -> bool {
        self.active_state == "failed"
    }
}

/// systemd reports unset resource counters as `u64::MAX`; normalize to `None`.
fn accounting(v: u64) -> Option<u64> {
    if v == u64::MAX { None } else { Some(v) }
}

/// Read a [`UnitSample`] for the unit at `path`. Unit-interface properties are
/// required; Service-interface (resource) properties are best-effort — a
/// non-service unit or disabled accounting simply leaves them `None`. When
/// `ip_io` is false the IP/IO accounting reads are skipped entirely.
pub async fn sample_unit(
    conn: &zbus::Connection,
    path: &OwnedObjectPath,
    name: String,
    ip_io: bool,
) -> zbus::Result<UnitSample> {
    let unit = UnitProxy::builder(conn).path(path.clone())?.build().await?;
    let mut s = UnitSample {
        name,
        load_state: unit.load_state().await?,
        active_state: unit.active_state().await?,
        sub_state: unit.sub_state().await?,
        active_enter_usec: unit.active_enter_timestamp().await.unwrap_or(0),
        ..Default::default()
    };
    // Service-interface resource accounting is best-effort.
    if let Ok(svc) = ServiceProxy::builder(conn)
        .path(path.clone())?
        .build()
        .await
    {
        s.n_restarts = svc.n_restarts().await.unwrap_or(0);
        s.exec_main_status = svc.exec_main_status().await.unwrap_or(0);
        s.mem_bytes = svc.memory_current().await.ok().and_then(accounting);
        s.cpu_usec = svc
            .cpu_usage_nsec()
            .await
            .ok()
            .and_then(accounting)
            .map(|ns| ns / 1000);
        s.tasks = svc.tasks_current().await.ok().and_then(accounting);
        if ip_io {
            s.ip_ingress_bytes = svc.ip_ingress_bytes().await.ok().and_then(accounting);
            s.ip_egress_bytes = svc.ip_egress_bytes().await.ok().and_then(accounting);
            s.io_read_bytes = svc.io_read_bytes().await.ok().and_then(accounting);
            s.io_write_bytes = svc.io_write_bytes().await.ok().and_then(accounting);
        }
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounting_maps_u64_max_to_none() {
        assert_eq!(accounting(u64::MAX), None);
        assert_eq!(accounting(0), Some(0));
        assert_eq!(accounting(4096), Some(4096));
    }

    #[test]
    fn sample_state_helpers() {
        let mut s = UnitSample {
            active_state: "active".into(),
            ..Default::default()
        };
        assert!(s.is_active() && !s.is_failed());
        s.active_state = "failed".into();
        assert!(s.is_failed() && !s.is_active());
    }
}
