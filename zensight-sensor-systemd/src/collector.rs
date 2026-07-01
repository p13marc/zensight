//! systemd D-Bus collector.
//!
//! Talks to `org.freedesktop.systemd1.Manager` on the **system bus** via a
//! hand-rolled [`ManagerProxy`], reads the scalar unit/job counters, enumerates
//! units for state aggregates, and derives boot-performance phase durations from
//! the Manager monotonic timestamps (like `systemd-analyze`).
//!
//! The D-Bus → telemetry mapping is factored into pure free functions
//! ([`unit_aggregates`], [`boot_phases`]) so it is unit-testable without a bus.

use std::sync::Arc;
use std::time::Duration;

use zbus::zvariant::OwnedObjectPath;
use zensight_common::telemetry::{Protocol, TelemetryPoint, TelemetryValue};
use zensight_sensor_core::{Publisher, SensorHealth};

use crate::config::SystemdConfig;

/// The `org.freedesktop.systemd1.Manager` subset we need: scalar counters, the
/// six boot monotonic timestamps, and `ListUnits`.
#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1"
)]
trait Manager {
    /// Number of currently loaded bus names.
    #[zbus(property)]
    fn n_names(&self) -> zbus::Result<u32>;
    /// Number of units currently in a failed state.
    #[zbus(property)]
    fn n_failed_units(&self) -> zbus::Result<u32>;
    /// Number of jobs currently queued.
    #[zbus(property)]
    fn n_jobs(&self) -> zbus::Result<u32>;
    /// Total number of jobs ever scheduled.
    #[zbus(property)]
    fn n_installed_jobs(&self) -> zbus::Result<u32>;

    /// Firmware monotonic timestamp (µs before kernel start, `systemd-analyze`).
    #[zbus(property)]
    fn firmware_timestamp_monotonic(&self) -> zbus::Result<u64>;
    /// Boot-loader monotonic timestamp (µs before kernel start).
    #[zbus(property)]
    fn loader_timestamp_monotonic(&self) -> zbus::Result<u64>;
    /// initrd handoff monotonic timestamp (µs since kernel start; 0 if no initrd).
    #[zbus(property, name = "InitRDTimestampMonotonic")]
    fn initrd_timestamp_monotonic(&self) -> zbus::Result<u64>;
    /// Userspace start monotonic timestamp (µs since kernel start).
    #[zbus(property)]
    fn userspace_timestamp_monotonic(&self) -> zbus::Result<u64>;
    /// Basic-boot-finished monotonic timestamp (µs since kernel start).
    #[zbus(property)]
    fn finish_timestamp_monotonic(&self) -> zbus::Result<u64>;

    /// Enumerate all loaded units. Each tuple is
    /// `(name, description, load_state, active_state, sub_state, following,
    ///   unit_path, job_id, job_type, job_path)`.
    #[allow(clippy::type_complexity)]
    fn list_units(
        &self,
    ) -> zbus::Result<
        Vec<(
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
        )>,
    >;
}

/// The load/active state pair extracted from one `ListUnits` row — the only
/// fields the aggregates need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitEntry {
    pub load_state: String,
    pub active_state: String,
}

/// Unit-state roll-up over the enumerated units.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Aggregates {
    pub total: u64,
    pub active: u64,
    pub failed: u64,
    pub loaded: u64,
    pub inactive: u64,
}

/// Roll up unit load/active states into counts (pure — unit-testable).
pub fn unit_aggregates(units: &[UnitEntry]) -> Aggregates {
    let mut a = Aggregates {
        total: units.len() as u64,
        ..Default::default()
    };
    for u in units {
        match u.active_state.as_str() {
            "active" => a.active += 1,
            "failed" => a.failed += 1,
            "inactive" => a.inactive += 1,
            _ => {}
        }
        if u.load_state == "loaded" {
            a.loaded += 1;
        }
    }
    a
}

/// The five Manager monotonic timestamps (microseconds) driving boot phases.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BootTimestamps {
    /// `FirmwareTimestampMonotonic` — µs before kernel start (0 in containers).
    pub firmware: u64,
    /// `LoaderTimestampMonotonic` — µs before kernel start (0 in containers).
    pub loader: u64,
    /// `InitRDTimestampMonotonic` — µs since kernel start (0 if no initrd).
    pub initrd: u64,
    /// `UserspaceTimestampMonotonic` — µs since kernel start.
    pub userspace: u64,
    /// `FinishTimestampMonotonic` — µs since kernel start.
    pub finish: u64,
}

/// Derive boot-phase durations (microseconds) from the Manager timestamps, using
/// the same arithmetic as `systemd-analyze`. All subtractions saturate so a
/// container (firmware/loader/initrd all 0) yields zeros instead of underflowing.
///
/// Returns `(phase, usec)` pairs for firmware / loader / kernel / initrd /
/// userspace / total.
pub fn boot_phases(ts: BootTimestamps) -> Vec<(&'static str, u64)> {
    let firmware = ts.firmware.saturating_sub(ts.loader);
    let loader = ts.loader;
    // Kernel phase runs from kernel start to the initrd handoff (or, with no
    // initrd, straight to userspace).
    let kernel = if ts.initrd > 0 {
        ts.initrd
    } else {
        ts.userspace
    };
    let initrd = if ts.initrd > 0 {
        ts.userspace.saturating_sub(ts.initrd)
    } else {
        0
    };
    let userspace = ts.finish.saturating_sub(ts.userspace);
    // Total = time-before-kernel (firmware) + time-since-kernel (finish).
    let total = ts.firmware.saturating_add(ts.finish);
    vec![
        ("firmware", firmware),
        ("loader", loader),
        ("kernel", kernel),
        ("initrd", initrd),
        ("userspace", userspace),
        ("total", total),
    ]
}

/// Cheap scalar Manager counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ManagerCounts {
    pub n_names: u32,
    pub n_failed_units: u32,
    pub n_jobs: u32,
    pub n_installed_jobs: u32,
}

/// The systemd collector: owns the config/publisher/health and a lazily
/// (re)established D-Bus connection to the system Manager.
pub struct SystemdCollector {
    source: String,
    config: SystemdConfig,
    publisher: Publisher,
    health: Arc<SensorHealth>,
    proxy: Option<ManagerProxy<'static>>,
}

impl SystemdCollector {
    pub fn new(
        source: String,
        config: SystemdConfig,
        publisher: Publisher,
        health: Arc<SensorHealth>,
    ) -> Self {
        Self {
            source,
            config,
            publisher,
            health,
            proxy: None,
        }
    }

    /// Run the periodic collect loop. Never panics: a bus/connection error records
    /// a device failure (surfaced on `@/health`) and retries on the next tick.
    pub async fn run(mut self) {
        let interval = Duration::from_secs(self.config.poll_interval_secs.max(1));
        self.health.set_devices_total(1);
        tracing::info!(
            "Starting systemd collector for '{}' (interval: {}s)",
            self.source,
            self.config.poll_interval_secs
        );
        loop {
            let started = std::time::Instant::now();
            match self.collect_and_publish().await {
                Ok(n) => {
                    self.health.record_device_success(&self.source);
                    tracing::debug!("published {n} systemd points");
                }
                Err(e) => {
                    // Non-systemd host / bus unavailable: report unhealthy, drop the
                    // proxy so the next tick reconnects, and keep the loop alive.
                    self.proxy = None;
                    self.health
                        .record_device_failure(&self.source, &e.to_string());
                    tracing::warn!(error = %e, "systemd collect failed");
                }
            }
            self.health
                .record_poll_duration(started.elapsed().as_millis() as u64);
            tokio::time::sleep(interval).await;
        }
    }

    /// Ensure a live `ManagerProxy`, connecting to the system bus on first use or
    /// after a prior failure.
    async fn ensure_proxy(&mut self) -> zbus::Result<&ManagerProxy<'static>> {
        if self.proxy.is_none() {
            let conn = zbus::Connection::system().await?;
            let proxy = ManagerProxy::new(&conn).await?;
            self.proxy = Some(proxy);
        }
        Ok(self.proxy.as_ref().expect("proxy just set"))
    }

    /// One collection pass: read the Manager, build points, publish. Returns the
    /// number of points published.
    async fn collect_and_publish(&mut self) -> zbus::Result<usize> {
        // Gather everything from D-Bus first (borrow of `self.proxy`), then publish
        // (borrows `self.publisher`) — keeps the borrows non-overlapping.
        let collect = self.config.collect.clone();
        let (counts, boot, aggregates) = {
            let proxy = self.ensure_proxy().await?;
            let counts = ManagerCounts {
                n_names: proxy.n_names().await?,
                n_failed_units: proxy.n_failed_units().await?,
                n_jobs: proxy.n_jobs().await?,
                n_installed_jobs: proxy.n_installed_jobs().await?,
            };
            let boot = if collect.boot {
                Some(BootTimestamps {
                    firmware: proxy.firmware_timestamp_monotonic().await?,
                    loader: proxy.loader_timestamp_monotonic().await?,
                    initrd: proxy.initrd_timestamp_monotonic().await?,
                    userspace: proxy.userspace_timestamp_monotonic().await?,
                    finish: proxy.finish_timestamp_monotonic().await?,
                })
            } else {
                None
            };
            let aggregates = if collect.list_units {
                let units: Vec<UnitEntry> = proxy
                    .list_units()
                    .await?
                    .into_iter()
                    .map(|u| UnitEntry {
                        load_state: u.2,
                        active_state: u.3,
                    })
                    .collect();
                Some(unit_aggregates(&units))
            } else {
                None
            };
            (counts, boot, aggregates)
        };

        let points = build_points(&self.source, &counts, boot.as_ref(), aggregates.as_ref());
        let n = points.len();
        for point in &points {
            let suffix = format!("{}/{}", point.source, point.metric);
            if let Err(e) = self.publisher.publish(&suffix, point).await {
                tracing::warn!(error = %e, metric = %point.metric, "publish failed");
            } else {
                self.health.record_metrics_published(1);
            }
        }
        Ok(n)
    }
}

/// Build the full telemetry point set for one tick (pure — unit-testable).
pub fn build_points(
    source: &str,
    counts: &ManagerCounts,
    boot: Option<&BootTimestamps>,
    aggregates: Option<&Aggregates>,
) -> Vec<TelemetryPoint> {
    let gauge = |metric: &str, v: f64| {
        TelemetryPoint::new(source, Protocol::Systemd, metric, TelemetryValue::Gauge(v))
    };
    let mut points = vec![
        gauge("manager/n_names", counts.n_names as f64),
        gauge("manager/n_failed_units", counts.n_failed_units as f64),
        gauge("manager/n_jobs", counts.n_jobs as f64),
        gauge("manager/n_installed_jobs", counts.n_installed_jobs as f64),
    ];
    if let Some(a) = aggregates {
        points.push(gauge("units/total", a.total as f64));
        points.push(gauge("units/active", a.active as f64));
        points.push(gauge("units/failed", a.failed as f64));
        points.push(gauge("units/loaded", a.loaded as f64));
        points.push(gauge("units/inactive", a.inactive as f64));
    }
    if let Some(ts) = boot {
        for (phase, usec) in boot_phases(*ts) {
            points.push(gauge(&format!("boot/{phase}_usec"), usec as f64));
        }
    }
    points
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregates_count_by_state() {
        let units = vec![
            UnitEntry {
                load_state: "loaded".into(),
                active_state: "active".into(),
            },
            UnitEntry {
                load_state: "loaded".into(),
                active_state: "failed".into(),
            },
            UnitEntry {
                load_state: "not-found".into(),
                active_state: "inactive".into(),
            },
            UnitEntry {
                load_state: "loaded".into(),
                active_state: "active".into(),
            },
        ];
        let a = unit_aggregates(&units);
        assert_eq!(a.total, 4);
        assert_eq!(a.active, 2);
        assert_eq!(a.failed, 1);
        assert_eq!(a.inactive, 1);
        assert_eq!(a.loaded, 3);
    }

    #[test]
    fn boot_phases_typical_host_with_initrd() {
        // firmware > loader (both before kernel); initrd < userspace < finish.
        let ts = BootTimestamps {
            firmware: 7_000_000,
            loader: 2_000_000,
            initrd: 3_000_000,
            userspace: 4_000_000,
            finish: 24_000_000,
        };
        let p: std::collections::HashMap<_, _> = boot_phases(ts).into_iter().collect();
        assert_eq!(p["firmware"], 5_000_000); // 7M - 2M
        assert_eq!(p["loader"], 2_000_000);
        assert_eq!(p["kernel"], 3_000_000); // initrd handoff
        assert_eq!(p["initrd"], 1_000_000); // 4M - 3M
        assert_eq!(p["userspace"], 20_000_000); // 24M - 4M
        assert_eq!(p["total"], 31_000_000); // firmware + finish
    }

    #[test]
    fn boot_phases_no_initrd_uses_userspace_for_kernel() {
        let ts = BootTimestamps {
            firmware: 0,
            loader: 0,
            initrd: 0,
            userspace: 5_000_000,
            finish: 18_000_000,
        };
        let p: std::collections::HashMap<_, _> = boot_phases(ts).into_iter().collect();
        assert_eq!(p["kernel"], 5_000_000); // no initrd → userspace
        assert_eq!(p["initrd"], 0);
        assert_eq!(p["userspace"], 13_000_000);
    }

    #[test]
    fn boot_phases_container_zeros_do_not_underflow() {
        // Container: firmware/loader/initrd all 0, userspace may exceed finish
        // in odd captures — saturation must keep everything at 0, never panic.
        let ts = BootTimestamps {
            firmware: 0,
            loader: 0,
            initrd: 0,
            userspace: 9,
            finish: 4,
        };
        let p: std::collections::HashMap<_, _> = boot_phases(ts).into_iter().collect();
        assert_eq!(p["firmware"], 0);
        assert_eq!(p["userspace"], 0); // saturating: 4 - 9 → 0
        assert_eq!(p["total"], 4);
    }

    #[test]
    fn build_points_shapes_and_gating() {
        let counts = ManagerCounts {
            n_names: 100,
            n_failed_units: 2,
            n_jobs: 0,
            n_installed_jobs: 500,
        };
        let agg = Aggregates {
            total: 300,
            active: 200,
            failed: 2,
            loaded: 280,
            inactive: 98,
        };
        // Full set.
        let pts = build_points(
            "host01",
            &counts,
            Some(&BootTimestamps::default()),
            Some(&agg),
        );
        let by: std::collections::HashMap<_, _> =
            pts.iter().map(|p| (p.metric.as_str(), &p.value)).collect();
        assert_eq!(by["manager/n_failed_units"], &TelemetryValue::Gauge(2.0));
        assert_eq!(by["units/total"], &TelemetryValue::Gauge(300.0));
        assert!(by.contains_key("boot/total_usec"));
        assert_eq!(pts[0].protocol, Protocol::Systemd);
        assert_eq!(pts[0].source, "host01");
        // Gating: no units, no boot → only the 4 manager scalars.
        let scalar_only = build_points("host01", &counts, None, None);
        assert_eq!(scalar_only.len(), 4);
        assert!(scalar_only.iter().all(|p| p.metric.starts_with("manager/")));
    }
}
