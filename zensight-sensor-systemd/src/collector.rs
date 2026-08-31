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

use zensight_common::telemetry::{TelemetryPoint, TelemetryValue};
use zensight_sensor_core::{Publisher, SensorHealth};

use crate::config::SystemdConfig;
use crate::dbus::{ListedUnit, ManagerProxy};

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
    /// Compiled `watch_units` globs (#273); empty = no per-unit streaming.
    watch: Vec<glob::Pattern>,
    /// Optional event ring (#275): when set, per-kind `events/*_total` counters
    /// are re-emitted each tick.
    events: Option<crate::events::EventState>,
    /// Optional threshold-alert evaluator (#276), driven each tick.
    alerts: Option<crate::alerts::AlertEvaluator>,
    conn: Option<zbus::Connection>,
    /// Per-unit IPAccounting rate baseline (#315): `unit → (ingress, egress, at)`
    /// of the last sample, used to derive `ip_*_bps` from cumulative counters.
    ip_rate: std::collections::HashMap<String, (u64, u64, std::time::Instant)>,
}

impl SystemdCollector {
    pub fn new(
        source: String,
        config: SystemdConfig,
        publisher: Publisher,
        health: Arc<SensorHealth>,
    ) -> Self {
        // Compile watchlist globs once; a bad pattern is logged and skipped.
        let watch = crate::config::compile_watch(&config.watch_units);
        Self {
            source,
            config,
            publisher,
            health,
            watch,
            events: None,
            alerts: None,
            conn: None,
            ip_rate: std::collections::HashMap::new(),
        }
    }

    /// Attach the shared event ring so per-kind `events/*_total` counters are
    /// re-emitted each tick (#275).
    pub fn with_events(mut self, events: crate::events::EventState) -> Self {
        self.events = Some(events);
        self
    }

    /// Attach the threshold-alert evaluator, driven each collect tick (#276).
    pub fn with_alerts(mut self, alerts: crate::alerts::AlertEvaluator) -> Self {
        self.alerts = Some(alerts);
        self
    }

    /// Run the periodic collect loop. Never panics: a bus/connection error records
    /// a device failure (surfaced on `state/systemd/health`) and retries on the next tick.
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
                    // connection so the next tick reconnects, and keep the loop alive.
                    self.conn = None;
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

    /// Ensure a live system-bus connection, (re)connecting on first use or after a
    /// prior failure. Returned by clone (the connection is cheap `Arc`-backed) so
    /// callers hold an owned handle without borrowing `self`.
    async fn ensure_conn(&mut self) -> zbus::Result<zbus::Connection> {
        if self.conn.is_none() {
            self.conn = Some(zbus::Connection::system().await?);
        }
        Ok(self.conn.as_ref().expect("conn just set").clone())
    }

    /// One collection pass: read the Manager, build points, publish. Returns the
    /// number of points published.
    async fn collect_and_publish(&mut self) -> zbus::Result<usize> {
        let collect = self.config.collect.clone();
        let conn = self.ensure_conn().await?;
        let proxy = ManagerProxy::new(&conn).await?;

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

        // Enumerate units once if the aggregates, the watchlist, or the mount
        // roll-up need it.
        let need_units = collect.list_units || collect.mounts || !self.watch.is_empty();
        let listed = if need_units {
            proxy.list_units().await?
        } else {
            Vec::new()
        };
        let aggregates = collect.list_units.then(|| {
            let units: Vec<UnitEntry> = listed
                .iter()
                .map(|u| UnitEntry {
                    load_state: u.2.clone(),
                    active_state: u.3.clone(),
                })
                .collect();
            unit_aggregates(&units)
        });

        let mut points = build_points(&self.source, &counts, boot.as_ref(), aggregates.as_ref());

        // Per-unit watchlist streaming (#273): match names, cap at watch_max
        // with exact-name priority (#865), and fold the rest into the
        // `other/*` bucket. The sampled units + timers are also fed to the
        // threshold-alert evaluator (#276).
        let mut samples: Vec<crate::unit::UnitSample> = Vec::new();
        let mut timers: Vec<crate::alerts::TimerSample> = Vec::new();
        if !self.watch.is_empty() {
            let cap = self.config.watch_max;
            let sel = select_watched(&listed, &self.watch, cap);
            if !sel.dropped_exact.is_empty() {
                // Only possible when the exact-named patterns alone exceed
                // the cap — explicit config is being ignored; say which.
                tracing::warn!(
                    dropped = ?sel.dropped_exact,
                    watch_max = cap,
                    "watch_max dropped exact-named watch_units entries; raise watch_max"
                );
            }
            if !sel.dropped_wildcard.is_empty() {
                let sample: Vec<&str> = sel
                    .dropped_wildcard
                    .iter()
                    .take(10)
                    .map(String::as_str)
                    .collect();
                tracing::warn!(
                    matched = sel.kept.len() + sel.dropped_exact.len() + sel.dropped_wildcard.len(),
                    watch_max = cap,
                    dropped = sel.dropped_wildcard.len(),
                    sample = ?sample,
                    "watch_units matched more units than watch_max; dropping wildcard matches (folded into other/*)"
                );
                tracing::debug!(
                    dropped = ?sel.dropped_wildcard,
                    "watch_max full wildcard drop list"
                );
            }
            let streamed = sel.kept.len();
            for u in &sel.kept {
                match crate::unit::sample_unit(
                    &conn,
                    &u.6,
                    u.0.clone(),
                    self.config.ip_io_accounting,
                )
                .await
                {
                    Ok(sample) => {
                        points.extend(crate::map::unit_points(&self.source, &sample));
                        // Per-unit IP bandwidth rate (#315): derive `ip_*_bps` from
                        // successive cumulative counters, and surface an explicit
                        // "accounting off" state for active units (not a silent 0).
                        if self.config.ip_io_accounting {
                            let now = std::time::Instant::now();
                            let (ing_bps, egr_bps) = match (
                                sample.ip_ingress_bytes,
                                sample.ip_egress_bytes,
                                self.ip_rate.get(&sample.name).copied(),
                            ) {
                                (Some(ci), Some(ce), Some((pi, pe, at))) => {
                                    let dt = now.duration_since(at).as_secs_f64();
                                    (
                                        crate::map::counter_bps(ci, pi, dt),
                                        crate::map::counter_bps(ce, pe, dt),
                                    )
                                }
                                _ => (None, None),
                            };
                            let accounting_off =
                                sample.is_active() && sample.ip_ingress_bytes.is_none();
                            points.extend(crate::map::ip_rate_points(
                                &self.source,
                                &sample.name,
                                ing_bps,
                                egr_bps,
                                accounting_off,
                            ));
                            if let (Some(ci), Some(ce)) =
                                (sample.ip_ingress_bytes, sample.ip_egress_bytes)
                            {
                                self.ip_rate.insert(sample.name.clone(), (ci, ce, now));
                            }
                        }
                        samples.push(sample);
                    }
                    Err(e) => {
                        tracing::warn!(unit = %u.0, error = %e, "failed to sample watched unit")
                    }
                }
                // Read the timer schedule for watched `.timer` units (#276 alert
                // input + #279 telemetry).
                if u.0.ends_with(".timer")
                    && let Ok(builder) = crate::dbus::TimerProxy::builder(&conn).path(u.6.clone())
                    && let Ok(timer) = builder
                        .cache_properties(zbus::proxy::CacheProperties::No)
                        .build()
                        .await
                {
                    let next = timer.next_elapse_usec_realtime().await.unwrap_or(0);
                    let last = timer.last_trigger_usec().await.unwrap_or(0);
                    points.extend(crate::map::timer_points(&self.source, &u.0, last, next));
                    timers.push(crate::alerts::TimerSample {
                        name: u.0.clone(),
                        next_elapse_usec_realtime: next,
                    });
                }
                // Read socket counters for watched `.socket` units (#279).
                if u.0.ends_with(".socket")
                    && let Ok(builder) = crate::dbus::SocketProxy::builder(&conn).path(u.6.clone())
                    && let Ok(sock) = builder
                        .cache_properties(zbus::proxy::CacheProperties::No)
                        .build()
                        .await
                {
                    let na = sock.n_accepted().await.unwrap_or(0);
                    let nc = sock.n_connections().await.unwrap_or(0);
                    let nr = sock.n_refused().await.unwrap_or(0);
                    points.extend(crate::map::socket_points(&self.source, &u.0, na, nc, nr));
                }
            }
            let unwatched = (listed.len().saturating_sub(streamed)) as u64;
            points.extend(crate::map::other_points(&self.source, unwatched));
        }

        // Mount/automount state aggregates (#279, opt-in).
        if collect.mounts {
            let states = listed
                .iter()
                .filter(|u| u.0.ends_with(".mount") || u.0.ends_with(".automount"))
                .map(|u| u.3.as_str());
            points.extend(crate::map::mount_points(&self.source, states));
        }

        // Journal store health (#279, opt-in): usage walk + statvfs free space.
        if collect.journal {
            let paths = crate::journal::DEFAULT_JOURNAL_PATHS;
            let usage = crate::journal::usage_bytes(&paths);
            let available =
                crate::journal::primary_store(&paths).and_then(crate::journal::available_bytes);
            points.extend(crate::map::journal_points(&self.source, usage, available));
        }

        // Optional streamed control-plane event counters (#275).
        if let Some(events) = &self.events {
            points.extend(events.counter_points(&self.source));
        }

        let n = points.len();
        for point in &points {
            let suffix = point.metric.clone();
            if let Err(e) = self.publisher.publish(&suffix, point).await {
                tracing::warn!(error = %e, metric = %point.metric, "publish failed");
            } else {
                self.health.record_metrics_published(1);
            }
        }

        // Threshold alerts (#276): evaluate + reconcile from the freshly-read
        // state. `system_state` is read here so the degraded rule works even with
        // no watchlist.
        if let Some(ev) = &mut self.alerts {
            let system_state = proxy.system_state().await.unwrap_or_default();
            let now_usec = chrono::Utc::now().timestamp_micros().max(0) as u64;
            ev.tick(
                system_state,
                counts.n_failed_units,
                samples,
                timers,
                now_usec,
                std::time::Instant::now(),
            )
            .await;
        }
        Ok(n)
    }
}

/// One tick's watchlist selection (#865): which matched units stream, and
/// which the `watch_max` cap dropped, split by how they matched.
struct WatchSelection<'a> {
    kept: Vec<&'a ListedUnit>,
    dropped_exact: Vec<String>,
    dropped_wildcard: Vec<String>,
}

/// An exact pattern names one unit; anything carrying a glob metacharacter is
/// a wildcard. `glob::Pattern` keeps the source string, so it is authoritative.
fn pattern_is_exact(p: &glob::Pattern) -> bool {
    !p.as_str().contains(['*', '?', '['])
}

/// Apply the watchlist and `watch_max` to one `ListUnits` enumeration (pure —
/// unit-testable). Units matched by an exact pattern always survive the cap
/// (#865): an operator who spelled out `sshd.service` gets `sshd.service`,
/// whatever the cap. Wildcard matches fill whatever room remains. Both
/// partitions are sorted by unit name — deterministic across ticks and hosts,
/// never D-Bus arrival order, which leads with sockets and once cost every
/// named service its slot.
fn select_watched<'a>(
    listed: &'a [ListedUnit],
    watch: &[glob::Pattern],
    cap: usize,
) -> WatchSelection<'a> {
    let (exact, wildcard): (Vec<&glob::Pattern>, Vec<&glob::Pattern>) =
        watch.iter().partition(|p| pattern_is_exact(p));
    let mut kept: Vec<&ListedUnit> = Vec::new();
    let mut wildcard_matched: Vec<&ListedUnit> = Vec::new();
    for u in listed {
        if exact.iter().any(|g| g.matches(&u.0)) {
            kept.push(u);
        } else if wildcard.iter().any(|g| g.matches(&u.0)) {
            wildcard_matched.push(u);
        }
    }
    kept.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    wildcard_matched.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    let dropped_exact: Vec<String> = kept
        .split_off(cap.min(kept.len()))
        .into_iter()
        .map(|u| u.0.clone())
        .collect();
    let room = cap.saturating_sub(kept.len());
    let dropped_wildcard: Vec<String> = wildcard_matched
        .split_off(room.min(wildcard_matched.len()))
        .into_iter()
        .map(|u| u.0.clone())
        .collect();
    kept.append(&mut wildcard_matched);
    WatchSelection {
        kept,
        dropped_exact,
        dropped_wildcard,
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
        crate::telemetry_guard::checked_point(source, metric, TelemetryValue::Gauge(v))
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
    // Points are built by `checked_point`, so the lib no longer names Protocol;
    // the tests still assert on it.
    use zensight_common::telemetry::Protocol;

    /// Minimal `ListUnits` row (same shape as query.rs's test helper).
    fn lu(name: &str) -> ListedUnit {
        (
            name.to_string(),
            format!("{name} desc"),
            "loaded".to_string(),
            "active".to_string(),
            "running".to_string(),
            String::new(),
            zbus::zvariant::OwnedObjectPath::try_from("/org/freedesktop/systemd1/unit/x").unwrap(),
            0,
            String::new(),
            zbus::zvariant::OwnedObjectPath::try_from("/").unwrap(),
        )
    }

    fn pats(ps: &[&str]) -> Vec<glob::Pattern> {
        ps.iter().map(|p| glob::Pattern::new(p).unwrap()).collect()
    }

    fn names(kept: &[&ListedUnit]) -> Vec<String> {
        kept.iter().map(|u| u.0.clone()).collect()
    }

    #[test]
    fn pattern_is_exact_classification() {
        for exact in ["sshd.service", "user@1000.service", "dbus-broker.service"] {
            assert!(
                pattern_is_exact(&glob::Pattern::new(exact).unwrap()),
                "{exact}"
            );
        }
        for wild in [
            "*.timer",
            "user@*.service",
            "foo?.service",
            "foo[ab].service",
        ] {
            assert!(
                !pattern_is_exact(&glob::Pattern::new(wild).unwrap()),
                "{wild}"
            );
        }
    }

    #[test]
    fn select_watched_exact_matches_survive_cap() {
        // The #865 repro in miniature: D-Bus enumeration leads with a wall of
        // sockets, the named service arrives last, and the cap is small.
        let mut listed: Vec<ListedUnit> = (0..8).map(|i| lu(&format!("sock{i}.socket"))).collect();
        listed.push(lu("sshd.service"));
        let sel = select_watched(&listed, &pats(&["*.socket", "sshd.service"]), 4);
        assert_eq!(sel.kept.len(), 4);
        assert_eq!(sel.kept[0].0, "sshd.service", "exact-named unit kept first");
        assert!(sel.dropped_exact.is_empty());
        assert_eq!(sel.dropped_wildcard.len(), 5);
    }

    #[test]
    fn select_watched_wildcard_fill_sorted_by_name() {
        // Shuffled arrival order in; the surviving wildcards are the
        // alphabetically-first ones, deterministically.
        let listed = vec![lu("c.timer"), lu("a.timer"), lu("d.timer"), lu("b.timer")];
        let sel = select_watched(&listed, &pats(&["*.timer"]), 2);
        assert_eq!(names(&sel.kept), ["a.timer", "b.timer"]);
        assert_eq!(sel.dropped_wildcard, ["c.timer", "d.timer"]);
    }

    #[test]
    fn select_watched_exacts_over_cap_dropped_sorted() {
        let listed = vec![
            lu("e.service"),
            lu("c.service"),
            lu("a.service"),
            lu("d.service"),
        ];
        let sel = select_watched(
            &listed,
            &pats(&["a.service", "c.service", "d.service", "e.service"]),
            3,
        );
        assert_eq!(names(&sel.kept), ["a.service", "c.service", "d.service"]);
        assert_eq!(sel.dropped_exact, ["e.service"]);
        assert!(sel.dropped_wildcard.is_empty());
    }

    #[test]
    fn select_watched_under_cap_no_drops() {
        let listed = vec![lu("b.timer"), lu("sshd.service"), lu("a.timer")];
        let sel = select_watched(&listed, &pats(&["sshd.service", "*.timer"]), 50);
        // Kept order is exacts first, then wildcards, each name-sorted —
        // documents that publication order is no longer D-Bus arrival order.
        assert_eq!(names(&sel.kept), ["sshd.service", "a.timer", "b.timer"]);
        assert!(sel.dropped_exact.is_empty() && sel.dropped_wildcard.is_empty());
    }

    #[test]
    fn select_watched_exact_beats_overlapping_wildcard() {
        // A unit matching both an exact and a wildcard pattern counts as
        // exact: it survives while pure-wildcard matches drop.
        let listed = vec![lu("a.service"), lu("b.service"), lu("sshd.service")];
        let sel = select_watched(&listed, &pats(&["sshd.service", "*.service"]), 1);
        assert_eq!(names(&sel.kept), ["sshd.service"]);
        assert_eq!(sel.dropped_wildcard, ["a.service", "b.service"]);
    }

    #[test]
    fn select_watched_no_match_no_watch() {
        let listed = vec![lu("a.service")];
        let sel = select_watched(&listed, &pats(&[]), 10);
        assert!(sel.kept.is_empty());
        assert!(sel.dropped_exact.is_empty() && sel.dropped_wildcard.is_empty());
        let sel = select_watched(&listed, &pats(&["*.timer"]), 10);
        assert!(sel.kept.is_empty());
    }

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
