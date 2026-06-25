//! System metrics collection using sysinfo.

use crate::config::SysinfoConfig;
use crate::map::sanitize_key;
use std::collections::HashMap;
use std::sync::Arc;
use sysinfo::{Disks, Networks, System};
use tracing::{debug, warn};
use zenoh::Session;
use zensight_common::serialization::{Format, encode};
use zensight_common::telemetry::{Protocol, TelemetryPoint, TelemetryValue};

#[cfg(target_os = "linux")]
use crate::linux::LinuxMetrics;

/// Collector for system metrics.
pub struct SystemCollector {
    system: System,
    disks: Disks,
    networks: Networks,
    hostname: String,
    key_prefix: String,
    config: SysinfoConfig,
    session: Arc<Session>,
    format: Format,
    /// Previous network stats for calculating rates
    prev_network: HashMap<String, (u64, u64)>,
    /// Previous RAPL energy readings (zone -> (energy_uj, max_energy_uj)) for
    /// deriving instantaneous watts across ticks (Linux power depth, §G).
    #[cfg(target_os = "linux")]
    prev_rapl: HashMap<String, (u64, Option<u64>)>,
    /// Sensor health, updated each poll for the frontend's Sensors view.
    health: Arc<zensight_sensor_core::SensorHealth>,
    /// Threshold-based alert evaluator (OOM/PSI/disk/FD/thermal/swap), driving an
    /// `AlertReporter` → `@/alerts/*`. `None` when alerting is disabled.
    alerts: Option<crate::alerts::AlertEvaluator>,
    /// Busiest block device `%util` observed in the most recent `collect_disk_io`
    /// pass, fed into the saturation score (avoids a second `/proc/diskstats`
    /// read + delta). `None` until a disk-I/O sample with a previous tick exists.
    last_disk_util_percent: Option<f64>,
    /// Previous `pswpin` counter, kept to derive the swap-in rate for the
    /// saturation score independently of the alert evaluator's own state.
    #[cfg(target_os = "linux")]
    prev_pswpin: Option<u64>,
    /// Linux-specific metrics collector
    #[cfg(target_os = "linux")]
    linux_metrics: LinuxMetrics,
}

impl SystemCollector {
    /// Create a new system collector.
    pub fn new(
        hostname: String,
        config: SysinfoConfig,
        session: Arc<Session>,
        format: Format,
    ) -> Self {
        Self {
            system: System::new_all(),
            disks: Disks::new_with_refreshed_list(),
            networks: Networks::new_with_refreshed_list(),
            key_prefix: config.key_prefix.clone(),
            hostname,
            config,
            session,
            format,
            prev_network: HashMap::new(),
            #[cfg(target_os = "linux")]
            prev_rapl: HashMap::new(),
            health: Arc::new(zensight_sensor_core::SensorHealth::new("sysinfo")),
            alerts: None,
            last_disk_util_percent: None,
            #[cfg(target_os = "linux")]
            prev_pswpin: None,
            #[cfg(target_os = "linux")]
            linux_metrics: LinuxMetrics::new(),
        }
    }

    /// Use the runner's shared health tracker (so updates reach `@/health`).
    pub fn with_health(mut self, health: Arc<zensight_sensor_core::SensorHealth>) -> Self {
        self.health = health;
        self
    }

    /// Attach a threshold-alert evaluator driving the shared `AlertReporter`.
    pub fn with_alerts(mut self, evaluator: crate::alerts::AlertEvaluator) -> Self {
        self.alerts = Some(evaluator);
        self
    }

    /// Run the collection loop.
    pub async fn run(mut self) {
        let interval = std::time::Duration::from_secs(self.config.poll_interval_secs);

        tracing::info!(
            "Starting system collector for '{}' (interval: {}s)",
            self.hostname,
            self.config.poll_interval_secs
        );

        // This sensor monitors one host (itself).
        self.health.set_devices_total(1);

        loop {
            let started = std::time::Instant::now();
            self.collect_and_publish().await;
            self.health
                .record_poll_duration(started.elapsed().as_millis() as u64);
            self.health.record_device_success(&self.hostname);
            tokio::time::sleep(interval).await;
        }
    }

    /// Collect all metrics and publish to Zenoh.
    async fn collect_and_publish(&mut self) {
        let timestamp = chrono::Utc::now().timestamp_millis();
        let mut count = 0;

        if self.config.collect.system {
            count += self.collect_system(timestamp).await;
        }

        if self.config.collect.cpu {
            count += self.collect_cpu(timestamp).await;
        }

        // Linux-specific: CPU time breakdown
        #[cfg(target_os = "linux")]
        if self.config.collect.cpu_times {
            count += self.collect_cpu_times(timestamp).await;
        }

        if self.config.collect.memory {
            count += self.collect_memory(timestamp).await;
        }

        if self.config.collect.disk {
            count += self.collect_disk(timestamp).await;
        }

        // Linux-specific: Disk I/O stats
        #[cfg(target_os = "linux")]
        if self.config.collect.disk_io {
            count += self.collect_disk_io(timestamp).await;
        }

        if self.config.collect.network {
            count += self.collect_network(timestamp).await;
        }

        // Linux-specific: Temperature sensors
        #[cfg(target_os = "linux")]
        if self.config.collect.temperatures {
            count += self.collect_temperatures(timestamp).await;
        }

        // Linux-specific: TCP connection states
        #[cfg(target_os = "linux")]
        if self.config.collect.tcp_states {
            count += self.collect_tcp_states(timestamp).await;
        }

        if self.config.collect.processes {
            count += self.collect_processes(timestamp).await;
        }

        // Linux-specific saturation/error collectors (Wave 1).
        #[cfg(target_os = "linux")]
        {
            if self.config.collect.pressure {
                count += self.collect_pressure(timestamp).await;
            }
            if self.config.collect.vmstat {
                count += self.collect_vmstat(timestamp).await;
            }
            if self.config.collect.fd_inode {
                count += self.collect_fd_inode(timestamp).await;
            }
            if self.config.collect.net_dev_extended {
                count += self.collect_net_dev_extended(timestamp).await;
            }
            if self.config.collect.cgroups {
                count += self.collect_cgroups(timestamp).await;
            }
            if self.config.collect.power {
                count += self.collect_power(timestamp).await;
            }
            // USE-completeness collectors (#98): network/CPU/memory/disk
            // saturation+error holes. All cheap unprivileged /proc|/sys reads.
            if self.config.collect.netstat {
                count += self.collect_netstat(timestamp).await;
            }
            if self.config.collect.softnet {
                count += self.collect_softnet(timestamp).await;
            }
            if self.config.collect.schedstat {
                count += self.collect_schedstat(timestamp).await;
            }
            if self.config.collect.conntrack {
                count += self.collect_conntrack(timestamp).await;
            }
            if self.config.collect.edac {
                count += self.collect_edac(timestamp).await;
            }
            if self.config.collect.mdadm {
                count += self.collect_mdadm(timestamp).await;
            }
        }

        // Threshold alerting: evaluate the already-collected saturation data and
        // drive the firing/resolved lifecycle. Additive — never touches the
        // telemetry path above.
        if self.alerts.is_some() {
            let raw = self.gather_alert_inputs();
            let interval = self.config.poll_interval_secs as f64;
            if let Some(ev) = self.alerts.as_mut() {
                ev.tick(raw, interval).await;
            }
        }

        // Derived host saturation score + coarse health state (P6): one number the
        // dashboard / topology tint / alerting can all key off, blended from the USE
        // saturation signals already collected above. Additive and cheap.
        if self.config.collect.saturation_score {
            count += self.collect_saturation_score(timestamp).await;
        }

        debug!("Published {} metrics for '{}'", count, self.hostname);
    }

    /// Gather the per-tick alert inputs from the same `/proc`/`/sys` + sysinfo
    /// sources the telemetry collectors read. Disk-space occupancy is computed
    /// cross-platform from the (already-refreshed) `Disks` list; the saturation
    /// counters (PSI, vmstat, FD, inodes, temperatures) are Linux-only and left
    /// empty elsewhere.
    fn gather_alert_inputs(&self) -> crate::alerts::RawInputs {
        let mut raw = crate::alerts::RawInputs::default();

        // Disk-space usage% per included mount (cross-platform via sysinfo).
        for disk in self.disks.list() {
            let mount = disk.mount_point().to_string_lossy().to_string();
            let fs_type = disk.file_system().to_string_lossy().to_string();
            if !self.config.disk.should_include(&mount, &fs_type) {
                continue;
            }
            let total = disk.total_space();
            let used = total.saturating_sub(disk.available_space());
            let used_percent = if total > 0 {
                (used as f64 / total as f64) * 100.0
            } else {
                0.0
            };
            raw.disks.push(crate::alerts::DiskUsageInput {
                mount,
                fs_type,
                used_percent,
            });
        }

        #[cfg(target_os = "linux")]
        {
            if let Some(vm) = crate::linux::collect_vmstat() {
                raw.oom_kill_total = vm.oom_kill;
                raw.pswpin_total = vm.pswpin;
                raw.pswpout_total = vm.pswpout;
            }
            if let Some(psi) = crate::linux::collect_psi() {
                raw.psi_cpu_avg10 = psi.cpu_some.as_ref().map(|p| p.avg10);
                raw.psi_memory_avg10 = psi.memory_some.as_ref().map(|p| p.avg10);
                raw.psi_io_avg10 = psi.io_some.as_ref().map(|p| p.avg10);
            }
            if let Some(fd) = crate::linux::collect_fd() {
                raw.fd_used_percent = Some(if fd.max > 0 {
                    (fd.used as f64 / fd.max as f64) * 100.0
                } else {
                    0.0
                });
            }
            // Inode occupancy per included mount (same mount list as fd_inode).
            let mounts: Vec<(String, String)> = self
                .disks
                .list()
                .iter()
                .map(|d| {
                    (
                        d.mount_point().to_string_lossy().to_string(),
                        d.file_system().to_string_lossy().to_string(),
                    )
                })
                .filter(|(mount, fs_type)| self.config.disk.should_include(mount, fs_type))
                .collect();
            for s in crate::linux::collect_inodes(&mounts) {
                let used_percent = if s.total > 0 {
                    (s.used as f64 / s.total as f64) * 100.0
                } else {
                    0.0
                };
                raw.inodes.push(crate::alerts::DiskUsageInput {
                    mount: s.mount,
                    fs_type: s.fs_type,
                    used_percent,
                });
            }
            // Thermal trip points (only when temperature collection is enabled).
            if self.config.collect.temperatures {
                for t in LinuxMetrics::collect_temperatures() {
                    raw.temps.push(crate::alerts::ThermalInput {
                        chip: t.chip,
                        label: t.label,
                        temp_celsius: t.temp_celsius,
                        critical_celsius: t.critical,
                    });
                }
            }
        }

        raw
    }

    /// Gather the per-tick [`crate::saturation::SaturationInputs`] from the same
    /// sources the saturation telemetry collectors read. The disk `%util` is reused
    /// from the most recent `collect_disk_io` pass (`last_disk_util_percent`); the
    /// PSI / FD / run-queue / swap-in signals come from cheap Linux `/proc` reads
    /// (left empty elsewhere, where the score degrades gracefully to a low value).
    #[allow(unused_mut)]
    fn gather_saturation_inputs(&mut self) -> crate::saturation::SaturationInputs {
        let mut s = crate::saturation::SaturationInputs {
            disk_util_percent: self.last_disk_util_percent,
            ..Default::default()
        };

        #[cfg(target_os = "linux")]
        {
            if let Some(psi) = crate::linux::collect_psi() {
                s.psi_cpu_avg10 = psi.cpu_some.as_ref().map(|p| p.avg10);
                s.psi_memory_avg10 = psi.memory_some.as_ref().map(|p| p.avg10);
                s.psi_io_avg10 = psi.io_some.as_ref().map(|p| p.avg10);
            }
            if let Some(fd) = crate::linux::collect_fd() {
                s.fd_used_percent = Some(if fd.max > 0 {
                    (fd.used as f64 / fd.max as f64) * 100.0
                } else {
                    0.0
                });
            }
            // Run-queue depth relative to CPU count (procs_running / nCPU).
            if let Some(k) = crate::linux::collect_kernel_derivatives()
                && let Some(running) = k.procs_running
            {
                let ncpu = self.cpu_count();
                if ncpu > 0 {
                    s.run_queue_ratio = Some(running as f64 / ncpu as f64);
                }
            }
            // Swap-in rate (pswpin pages/s) derived against the previous tick.
            if let Some(vm) = crate::linux::collect_vmstat() {
                let interval = self.config.poll_interval_secs as f64;
                if let (Some(prev), Some(cur)) = (self.prev_pswpin, vm.pswpin)
                    && cur >= prev
                    && interval > 0.0
                {
                    s.swap_in_pages_per_sec = Some((cur - prev) as f64 / interval);
                }
                // Carry the latest counter forward (don't clobber with a miss).
                self.prev_pswpin = vm.pswpin.or(self.prev_pswpin);
            }
        }

        s
    }

    /// Number of logical CPUs (for run-queue normalization). Uses the already
    /// refreshed `sysinfo` CPU list, falling back to `available_parallelism`.
    #[cfg(target_os = "linux")]
    fn cpu_count(&self) -> usize {
        let n = self.system.cpus().len();
        if n > 0 {
            n
        } else {
            std::thread::available_parallelism()
                .map(|p| p.get())
                .unwrap_or(1)
        }
    }

    /// Compute and publish the derived host saturation score (`0..100`, Gauge) and
    /// coarse health state (`ok`/`warn`/`crit`, Text). Returns the number of points
    /// published (always 2 when enabled).
    async fn collect_saturation_score(&mut self, timestamp: i64) -> usize {
        let inputs = self.gather_saturation_inputs();
        let cfg = &self.config.saturation;
        let score = crate::saturation::saturation_score(&inputs, cfg);
        let state = crate::saturation::health_state(score, cfg);

        self.publish(
            "system/saturation_score",
            TelemetryValue::Gauge(score),
            timestamp,
            HashMap::new(),
        )
        .await;
        self.publish(
            "system/health_state",
            TelemetryValue::Text(state.to_string()),
            timestamp,
            HashMap::new(),
        )
        .await;
        2
    }

    /// Publish a batch of mapped metrics, lifting label pairs into the wire
    /// `HashMap`. Returns the number of points published.
    #[cfg(target_os = "linux")]
    async fn publish_metrics(&self, metrics: Vec<crate::map::Metric>, timestamp: i64) -> usize {
        let count = metrics.len();
        for m in metrics {
            let labels: HashMap<String, String> = m
                .labels
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect();
            self.publish(&m.metric, m.value, timestamp, labels).await;
        }
        count
    }

    /// Collect Pressure Stall Information (Linux-specific, Wave 1 §A).
    #[cfg(target_os = "linux")]
    async fn collect_pressure(&self, timestamp: i64) -> usize {
        match crate::linux::collect_psi() {
            Some(psi) => {
                self.publish_metrics(crate::map::map_pressure(&psi), timestamp)
                    .await
            }
            None => 0,
        }
    }

    /// Collect the vmstat saturation allowlist + `/proc/stat` derivatives
    /// (Linux-specific, Wave 1 §B).
    #[cfg(target_os = "linux")]
    async fn collect_vmstat(&self, timestamp: i64) -> usize {
        let mut count = 0;
        if let Some(vm) = crate::linux::collect_vmstat() {
            count += self
                .publish_metrics(crate::map::map_vmstat(&vm), timestamp)
                .await;
        }
        if let Some(k) = crate::linux::collect_kernel_derivatives() {
            count += self
                .publish_metrics(crate::map::map_kernel_derivatives(&k), timestamp)
                .await;
        }
        count
    }

    /// Collect FD + inode saturation ceilings (Linux-specific, Wave 1 §C).
    #[cfg(target_os = "linux")]
    async fn collect_fd_inode(&self, timestamp: i64) -> usize {
        let mut count = 0;
        if let Some(fd) = crate::linux::collect_fd() {
            count += self
                .publish_metrics(crate::map::map_fd(&fd), timestamp)
                .await;
        }
        // Build the filtered mount list from the (already-discovered) disks.
        let mounts: Vec<(String, String)> = self
            .disks
            .list()
            .iter()
            .map(|d| {
                (
                    d.mount_point().to_string_lossy().to_string(),
                    d.file_system().to_string_lossy().to_string(),
                )
            })
            .filter(|(mount, fs_type)| self.config.disk.should_include(mount, fs_type))
            .collect();
        let inodes = crate::linux::collect_inodes(&mounts);
        count += self
            .publish_metrics(crate::map::map_inodes(&inodes), timestamp)
            .await;
        count
    }

    /// Collect richer per-interface `/proc/net/dev` drop/fifo counters
    /// (Linux-specific, Wave 1 §D).
    #[cfg(target_os = "linux")]
    async fn collect_net_dev_extended(&self, timestamp: i64) -> usize {
        let stats: Vec<_> = crate::linux::collect_net_dev()
            .into_iter()
            .filter(|s| self.config.network.should_include(&s.iface))
            .collect();
        self.publish_metrics(crate::map::map_net_dev(&stats), timestamp)
            .await
    }

    /// Collect cgroup-v2 container-saturation metrics (Linux-specific, §E).
    #[cfg(target_os = "linux")]
    async fn collect_cgroups(&self, timestamp: i64) -> usize {
        let Some(samples) = crate::linux::collect_cgroups(&self.config.collect.cgroup_paths) else {
            return 0;
        };
        let mut count = 0;
        for s in &samples {
            count += self
                .publish_metrics(crate::map::map_cgroup(s), timestamp)
                .await;
        }
        count
    }

    /// Collect thermal/power depth: RAPL watts (rate-derived), fan RPM, battery,
    /// entropy (Linux-specific, §G).
    #[cfg(target_os = "linux")]
    async fn collect_power(&mut self, timestamp: i64) -> usize {
        let interval = self.config.poll_interval_secs as f64;

        // RAPL: derive watts from the energy counter delta vs the prev tick.
        let domains = crate::linux::collect_rapl();
        let mut rapl_watts = Vec::with_capacity(domains.len());
        for d in &domains {
            if let Some((prev_uj, _)) = self.prev_rapl.get(&d.zone)
                && let Some(w) =
                    crate::map::rapl_watts(*prev_uj, d.energy_uj, d.max_energy_uj, interval)
            {
                rapl_watts.push((d.zone.clone(), d.name.clone(), w));
            }
            self.prev_rapl
                .insert(d.zone.clone(), (d.energy_uj, d.max_energy_uj));
        }

        let sample = crate::map::PowerSample {
            rapl_watts,
            fans: crate::linux::collect_fans(),
            batteries: crate::linux::collect_batteries(),
            entropy_avail: crate::linux::collect_entropy(),
        };
        self.publish_metrics(crate::map::map_power(&sample), timestamp)
            .await
    }

    /// Collect TCP retransmit / listen-overflow errors + socket occupancy
    /// (Linux-specific, USE-completeness #98).
    #[cfg(target_os = "linux")]
    async fn collect_netstat(&self, timestamp: i64) -> usize {
        let mut count = 0;
        if let Some(s) = crate::linux::collect_netstat() {
            count += self
                .publish_metrics(crate::map::map_netstat(&s), timestamp)
                .await;
        }
        if let Some(s) = crate::linux::collect_sockstat() {
            count += self
                .publish_metrics(crate::map::map_sockstat(&s), timestamp)
                .await;
        }
        count
    }

    /// Collect softnet backlog drops / time-squeezes (Linux-specific, #98).
    #[cfg(target_os = "linux")]
    async fn collect_softnet(&self, timestamp: i64) -> usize {
        match crate::linux::collect_softnet() {
            Some(s) => {
                self.publish_metrics(crate::map::map_softnet(&s), timestamp)
                    .await
            }
            None => 0,
        }
    }

    /// Collect per-CPU scheduler run-delay (Linux-specific, #98).
    #[cfg(target_os = "linux")]
    async fn collect_schedstat(&self, timestamp: i64) -> usize {
        match crate::linux::collect_schedstat() {
            Some(s) => {
                self.publish_metrics(crate::map::map_schedstat(&s), timestamp)
                    .await
            }
            None => 0,
        }
    }

    /// Collect conntrack table fill (Linux-specific, #98).
    #[cfg(target_os = "linux")]
    async fn collect_conntrack(&self, timestamp: i64) -> usize {
        match crate::linux::collect_conntrack() {
            Some(s) => {
                self.publish_metrics(crate::map::map_conntrack(&s), timestamp)
                    .await
            }
            None => 0,
        }
    }

    /// Collect per-controller ECC memory errors (Linux-specific, #98).
    #[cfg(target_os = "linux")]
    async fn collect_edac(&self, timestamp: i64) -> usize {
        let samples = crate::linux::collect_edac();
        self.publish_metrics(crate::map::map_edac(&samples), timestamp)
            .await
    }

    /// Collect software-RAID degraded/failed state (Linux-specific, #98).
    #[cfg(target_os = "linux")]
    async fn collect_mdadm(&self, timestamp: i64) -> usize {
        let arrays = crate::linux::collect_mdstat();
        self.publish_metrics(crate::map::map_mdstat(&arrays), timestamp)
            .await
    }

    /// Collect system-wide metrics (uptime, load averages).
    async fn collect_system(&mut self, timestamp: i64) -> usize {
        let mut count = 0;

        // Uptime
        let uptime = System::uptime();
        self.publish(
            "system/uptime",
            TelemetryValue::Counter(uptime),
            timestamp,
            HashMap::new(),
        )
        .await;
        count += 1;

        // Load averages
        let load_avg = System::load_average();

        let mut labels = HashMap::new();
        labels.insert("period".to_string(), "1m".to_string());
        self.publish(
            "system/load",
            TelemetryValue::Gauge(load_avg.one),
            timestamp,
            labels,
        )
        .await;
        count += 1;

        let mut labels = HashMap::new();
        labels.insert("period".to_string(), "5m".to_string());
        self.publish(
            "system/load",
            TelemetryValue::Gauge(load_avg.five),
            timestamp,
            labels,
        )
        .await;
        count += 1;

        let mut labels = HashMap::new();
        labels.insert("period".to_string(), "15m".to_string());
        self.publish(
            "system/load",
            TelemetryValue::Gauge(load_avg.fifteen),
            timestamp,
            labels,
        )
        .await;
        count += 1;

        // Boot time
        let boot_time = System::boot_time();
        self.publish(
            "system/boot_time",
            TelemetryValue::Counter(boot_time),
            timestamp,
            HashMap::new(),
        )
        .await;
        count += 1;

        count
    }

    /// Collect CPU metrics.
    async fn collect_cpu(&mut self, timestamp: i64) -> usize {
        self.system.refresh_cpu_usage();
        let mut count = 0;

        // Global CPU usage
        let global_usage = self.system.global_cpu_usage();
        self.publish(
            "cpu/usage",
            TelemetryValue::Gauge(global_usage as f64),
            timestamp,
            HashMap::new(),
        )
        .await;
        count += 1;

        // Per-core CPU usage
        for (i, cpu) in self.system.cpus().iter().enumerate() {
            let mut labels = HashMap::new();
            labels.insert("core".to_string(), i.to_string());
            labels.insert("name".to_string(), cpu.name().to_string());

            self.publish(
                &format!("cpu/{}/usage", i),
                TelemetryValue::Gauge(cpu.cpu_usage() as f64),
                timestamp,
                labels.clone(),
            )
            .await;
            count += 1;

            // CPU frequency
            let freq = cpu.frequency();
            if freq > 0 {
                labels.insert("unit".to_string(), "MHz".to_string());
                self.publish(
                    &format!("cpu/{}/frequency", i),
                    TelemetryValue::Gauge(freq as f64),
                    timestamp,
                    labels,
                )
                .await;
                count += 1;
            }
        }

        count
    }

    /// Collect memory metrics.
    async fn collect_memory(&mut self, timestamp: i64) -> usize {
        self.system.refresh_memory();
        let mut count = 0;

        // Total memory
        let total = self.system.total_memory();
        let mut labels = HashMap::new();
        labels.insert("unit".to_string(), "bytes".to_string());
        self.publish(
            "memory/total",
            TelemetryValue::Counter(total),
            timestamp,
            labels.clone(),
        )
        .await;
        count += 1;

        // Used memory
        let used = self.system.used_memory();
        self.publish(
            "memory/used",
            TelemetryValue::Counter(used),
            timestamp,
            labels.clone(),
        )
        .await;
        count += 1;

        // Available memory
        let available = self.system.available_memory();
        self.publish(
            "memory/available",
            TelemetryValue::Counter(available),
            timestamp,
            labels.clone(),
        )
        .await;
        count += 1;

        // Memory usage percentage. Derived from MemAvailable rather than `used`
        // so reclaimable page cache is not counted as pressure (behavioral
        // change vs. the previous `used/total` figure).
        let usage_pct = crate::map::mem_usage_percent(total, available);
        self.publish(
            "memory/usage_percent",
            TelemetryValue::Gauge(usage_pct),
            timestamp,
            HashMap::new(),
        )
        .await;
        count += 1;

        // Memory composition (Linux /proc/meminfo): cached/buffers/slab/dirty/
        // writeback bytes, to attribute usage to reclaimable cache vs. real use.
        #[cfg(target_os = "linux")]
        if let Some(mc) = crate::linux::collect_mem_composition() {
            let mut clabels = HashMap::new();
            clabels.insert("unit".to_string(), "bytes".to_string());
            for (metric, value) in [
                ("memory/cached", mc.cached),
                ("memory/buffers", mc.buffers),
                ("memory/slab", mc.slab),
                ("memory/dirty", mc.dirty),
                ("memory/writeback", mc.writeback),
            ] {
                self.publish(
                    metric,
                    TelemetryValue::Gauge(value as f64),
                    timestamp,
                    clabels.clone(),
                )
                .await;
                count += 1;
            }
        }

        // Swap
        let swap_total = self.system.total_swap();
        let swap_used = self.system.used_swap();

        if swap_total > 0 {
            self.publish(
                "memory/swap_total",
                TelemetryValue::Counter(swap_total),
                timestamp,
                labels.clone(),
            )
            .await;
            count += 1;

            self.publish(
                "memory/swap_used",
                TelemetryValue::Counter(swap_used),
                timestamp,
                labels,
            )
            .await;
            count += 1;

            let swap_pct = (swap_used as f64 / swap_total as f64) * 100.0;
            self.publish(
                "memory/swap_percent",
                TelemetryValue::Gauge(swap_pct),
                timestamp,
                HashMap::new(),
            )
            .await;
            count += 1;
        }

        count
    }

    /// Collect disk metrics.
    async fn collect_disk(&mut self, timestamp: i64) -> usize {
        self.disks.refresh(true);
        let mut count = 0;

        for disk in self.disks.list() {
            let mount_point = disk.mount_point().to_string_lossy().to_string();
            let fs_type = disk.file_system().to_string_lossy().to_string();

            if !self.config.disk.should_include(&mount_point, &fs_type) {
                continue;
            }

            // Sanitize mount point for key expression (replace / with _)
            let mount_key = sanitize_key(&mount_point);

            let mut labels = HashMap::new();
            labels.insert("mount".to_string(), mount_point.clone());
            labels.insert("fs_type".to_string(), fs_type);
            labels.insert(
                "name".to_string(),
                disk.name().to_string_lossy().to_string(),
            );
            labels.insert("unit".to_string(), "bytes".to_string());

            let total = disk.total_space();
            let available = disk.available_space();
            let used = total.saturating_sub(available);

            self.publish(
                &format!("disk/{}/total", mount_key),
                TelemetryValue::Counter(total),
                timestamp,
                labels.clone(),
            )
            .await;
            count += 1;

            self.publish(
                &format!("disk/{}/used", mount_key),
                TelemetryValue::Counter(used),
                timestamp,
                labels.clone(),
            )
            .await;
            count += 1;

            self.publish(
                &format!("disk/{}/available", mount_key),
                TelemetryValue::Counter(available),
                timestamp,
                labels.clone(),
            )
            .await;
            count += 1;

            // Usage percentage
            let usage_pct = if total > 0 {
                (used as f64 / total as f64) * 100.0
            } else {
                0.0
            };
            labels.remove("unit");
            self.publish(
                &format!("disk/{}/usage_percent", mount_key),
                TelemetryValue::Gauge(usage_pct),
                timestamp,
                labels,
            )
            .await;
            count += 1;
        }

        count
    }

    /// Collect network metrics.
    async fn collect_network(&mut self, timestamp: i64) -> usize {
        self.networks.refresh(true);
        let mut count = 0;

        for (name, data) in self.networks.list() {
            if !self.config.network.should_include(name) {
                continue;
            }

            let iface_key = sanitize_key(name);

            let mut labels = HashMap::new();
            labels.insert("interface".to_string(), name.clone());

            // Bytes received/transmitted (counters)
            let rx_bytes = data.total_received();
            let tx_bytes = data.total_transmitted();

            labels.insert("unit".to_string(), "bytes".to_string());
            self.publish(
                &format!("network/{}/rx_bytes", iface_key),
                TelemetryValue::Counter(rx_bytes),
                timestamp,
                labels.clone(),
            )
            .await;
            count += 1;

            self.publish(
                &format!("network/{}/tx_bytes", iface_key),
                TelemetryValue::Counter(tx_bytes),
                timestamp,
                labels.clone(),
            )
            .await;
            count += 1;

            // Packets received/transmitted
            labels.insert("unit".to_string(), "packets".to_string());
            self.publish(
                &format!("network/{}/rx_packets", iface_key),
                TelemetryValue::Counter(data.total_packets_received()),
                timestamp,
                labels.clone(),
            )
            .await;
            count += 1;

            self.publish(
                &format!("network/{}/tx_packets", iface_key),
                TelemetryValue::Counter(data.total_packets_transmitted()),
                timestamp,
                labels.clone(),
            )
            .await;
            count += 1;

            // Errors
            labels.remove("unit");
            self.publish(
                &format!("network/{}/rx_errors", iface_key),
                TelemetryValue::Counter(data.total_errors_on_received()),
                timestamp,
                labels.clone(),
            )
            .await;
            count += 1;

            self.publish(
                &format!("network/{}/tx_errors", iface_key),
                TelemetryValue::Counter(data.total_errors_on_transmitted()),
                timestamp,
                labels.clone(),
            )
            .await;
            count += 1;

            // Calculate rates if we have previous data
            if let Some((prev_rx, prev_tx)) = self.prev_network.get(name) {
                let interval = self.config.poll_interval_secs as f64;
                if interval > 0.0 {
                    let rx_rate = (rx_bytes.saturating_sub(*prev_rx)) as f64 / interval;
                    let tx_rate = (tx_bytes.saturating_sub(*prev_tx)) as f64 / interval;

                    labels.insert("unit".to_string(), "bytes/s".to_string());
                    self.publish(
                        &format!("network/{}/rx_rate", iface_key),
                        TelemetryValue::Gauge(rx_rate),
                        timestamp,
                        labels.clone(),
                    )
                    .await;
                    count += 1;

                    self.publish(
                        &format!("network/{}/tx_rate", iface_key),
                        TelemetryValue::Gauge(tx_rate),
                        timestamp,
                        labels,
                    )
                    .await;
                    count += 1;
                }
            }

            // Store current values for next iteration
            self.prev_network.insert(name.clone(), (rx_bytes, tx_bytes));
        }

        count
    }

    /// Collect top process metrics.
    async fn collect_processes(&mut self, timestamp: i64) -> usize {
        self.system.refresh_all();
        let mut count = 0;

        // Small bounded aggregates always stream (the per-pid firehose is served
        // on demand via the @/query/processes channel, not streamed — §F / P2).
        let total = self.system.processes().len() as u64;
        let zombie = self
            .system
            .processes()
            .values()
            .filter(|p| matches!(p.status(), sysinfo::ProcessStatus::Zombie))
            .count() as u64;
        self.publish(
            "system/processes_total",
            TelemetryValue::Gauge(total as f64),
            timestamp,
            HashMap::new(),
        )
        .await;
        count += 1;
        self.publish(
            "system/processes_zombie",
            TelemetryValue::Gauge(zombie as f64),
            timestamp,
            HashMap::new(),
        )
        .await;
        count += 1;

        let top_n = self.config.collect.top_processes;

        // Get processes sorted by CPU usage
        let mut processes: Vec<_> = self.system.processes().values().collect();
        processes.sort_by(|a, b| {
            b.cpu_usage()
                .partial_cmp(&a.cpu_usage())
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        for (rank, proc) in processes.iter().take(top_n).enumerate() {
            let mut labels = HashMap::new();
            labels.insert("pid".to_string(), proc.pid().to_string());
            labels.insert(
                "name".to_string(),
                proc.name().to_string_lossy().to_string(),
            );
            labels.insert("rank".to_string(), (rank + 1).to_string());

            self.publish(
                &format!("process/{}/cpu", rank + 1),
                TelemetryValue::Gauge(proc.cpu_usage() as f64),
                timestamp,
                labels.clone(),
            )
            .await;
            count += 1;

            labels.insert("unit".to_string(), "bytes".to_string());
            self.publish(
                &format!("process/{}/memory", rank + 1),
                TelemetryValue::Counter(proc.memory()),
                timestamp,
                labels,
            )
            .await;
            count += 1;
        }

        count
    }

    /// Collect CPU time breakdown (Linux-specific).
    #[cfg(target_os = "linux")]
    async fn collect_cpu_times(&mut self, timestamp: i64) -> usize {
        let mut count = 0;

        let cpu_times = self.linux_metrics.collect_cpu_times();

        for (cpu_name, times) in cpu_times {
            let is_total = cpu_name == "cpu";
            let prefix = if is_total {
                "cpu/times".to_string()
            } else {
                format!("{}/times", cpu_name)
            };

            let mut labels = HashMap::new();
            if !is_total {
                labels.insert(
                    "core".to_string(),
                    cpu_name
                        .strip_prefix("cpu")
                        .unwrap_or(&cpu_name)
                        .to_string(),
                );
            }

            // Publish each CPU time component
            self.publish(
                &format!("{}/user", prefix),
                TelemetryValue::Gauge(times.user),
                timestamp,
                labels.clone(),
            )
            .await;
            count += 1;

            self.publish(
                &format!("{}/nice", prefix),
                TelemetryValue::Gauge(times.nice),
                timestamp,
                labels.clone(),
            )
            .await;
            count += 1;

            self.publish(
                &format!("{}/system", prefix),
                TelemetryValue::Gauge(times.system),
                timestamp,
                labels.clone(),
            )
            .await;
            count += 1;

            self.publish(
                &format!("{}/idle", prefix),
                TelemetryValue::Gauge(times.idle),
                timestamp,
                labels.clone(),
            )
            .await;
            count += 1;

            self.publish(
                &format!("{}/iowait", prefix),
                TelemetryValue::Gauge(times.iowait),
                timestamp,
                labels.clone(),
            )
            .await;
            count += 1;

            self.publish(
                &format!("{}/irq", prefix),
                TelemetryValue::Gauge(times.irq),
                timestamp,
                labels.clone(),
            )
            .await;
            count += 1;

            self.publish(
                &format!("{}/softirq", prefix),
                TelemetryValue::Gauge(times.softirq),
                timestamp,
                labels.clone(),
            )
            .await;
            count += 1;

            self.publish(
                &format!("{}/steal", prefix),
                TelemetryValue::Gauge(times.steal),
                timestamp,
                labels,
            )
            .await;
            count += 1;
        }

        count
    }

    /// Collect disk I/O statistics (Linux-specific).
    #[cfg(target_os = "linux")]
    async fn collect_disk_io(&mut self, timestamp: i64) -> usize {
        let mut count = 0;
        let interval = self.config.poll_interval_secs as f64;

        let disk_io = self.linux_metrics.collect_disk_io(interval);

        // Track the busiest device's %util this pass to feed the saturation score
        // (so we don't re-read /proc/diskstats just for the score).
        let mut max_util: Option<f64> = None;

        for (device, (stats, rates, saturation)) in disk_io {
            let mut labels = HashMap::new();
            labels.insert("device".to_string(), device.clone());

            // Cumulative counters
            labels.insert("unit".to_string(), "bytes".to_string());
            self.publish(
                &format!("disk/{}/io/read_bytes", device),
                TelemetryValue::Counter(stats.read_bytes),
                timestamp,
                labels.clone(),
            )
            .await;
            count += 1;

            self.publish(
                &format!("disk/{}/io/write_bytes", device),
                TelemetryValue::Counter(stats.write_bytes),
                timestamp,
                labels.clone(),
            )
            .await;
            count += 1;

            labels.insert("unit".to_string(), "ops".to_string());
            self.publish(
                &format!("disk/{}/io/read_ops", device),
                TelemetryValue::Counter(stats.read_ios),
                timestamp,
                labels.clone(),
            )
            .await;
            count += 1;

            self.publish(
                &format!("disk/{}/io/write_ops", device),
                TelemetryValue::Counter(stats.write_ios),
                timestamp,
                labels.clone(),
            )
            .await;
            count += 1;

            labels.insert("unit".to_string(), "ms".to_string());
            self.publish(
                &format!("disk/{}/io/time_ms", device),
                TelemetryValue::Counter(stats.io_time_ms),
                timestamp,
                labels.clone(),
            )
            .await;
            count += 1;

            // Derived saturation gauges (need a previous sample to delta).
            if let Some(sat) = saturation {
                max_util = Some(max_util.map_or(sat.util_percent, |m| m.max(sat.util_percent)));
                labels.insert("unit".to_string(), "percent".to_string());
                self.publish(
                    &format!("disk/{}/io/util_percent", device),
                    TelemetryValue::Gauge(sat.util_percent),
                    timestamp,
                    labels.clone(),
                )
                .await;
                count += 1;

                labels.insert("unit".to_string(), "requests".to_string());
                self.publish(
                    &format!("disk/{}/io/queue_depth", device),
                    TelemetryValue::Gauge(sat.queue_depth),
                    timestamp,
                    labels.clone(),
                )
                .await;
                count += 1;
            }

            // Rates (if available)
            if let Some(rates) = rates {
                labels.insert("unit".to_string(), "bytes/s".to_string());
                self.publish(
                    &format!("disk/{}/io/read_rate", device),
                    TelemetryValue::Gauge(rates.read_bytes as f64),
                    timestamp,
                    labels.clone(),
                )
                .await;
                count += 1;

                self.publish(
                    &format!("disk/{}/io/write_rate", device),
                    TelemetryValue::Gauge(rates.write_bytes as f64),
                    timestamp,
                    labels.clone(),
                )
                .await;
                count += 1;

                labels.insert("unit".to_string(), "iops".to_string());
                self.publish(
                    &format!("disk/{}/io/read_iops", device),
                    TelemetryValue::Gauge(rates.read_ios as f64),
                    timestamp,
                    labels.clone(),
                )
                .await;
                count += 1;

                self.publish(
                    &format!("disk/{}/io/write_iops", device),
                    TelemetryValue::Gauge(rates.write_ios as f64),
                    timestamp,
                    labels,
                )
                .await;
                count += 1;
            }
        }

        // Latch the busiest %util for this tick's saturation score (None on the
        // first pass, when no device has a previous sample to delta against).
        if max_util.is_some() {
            self.last_disk_util_percent = max_util;
        }

        count
    }

    /// Collect temperature sensor readings (Linux-specific).
    #[cfg(target_os = "linux")]
    async fn collect_temperatures(&mut self, timestamp: i64) -> usize {
        let mut count = 0;

        let temps = LinuxMetrics::collect_temperatures();

        for temp in temps {
            let chip_key = sanitize_key(&temp.chip);
            let label_key = sanitize_key(&temp.label);

            let mut labels = HashMap::new();
            labels.insert("chip".to_string(), temp.chip.clone());
            labels.insert("label".to_string(), temp.label.clone());
            labels.insert("unit".to_string(), "celsius".to_string());

            self.publish(
                &format!("sensors/{}/{}/temp", chip_key, label_key),
                TelemetryValue::Gauge(temp.temp_celsius),
                timestamp,
                labels.clone(),
            )
            .await;
            count += 1;

            if let Some(critical) = temp.critical {
                self.publish(
                    &format!("sensors/{}/{}/critical", chip_key, label_key),
                    TelemetryValue::Gauge(critical),
                    timestamp,
                    labels.clone(),
                )
                .await;
                count += 1;
            }

            if let Some(max) = temp.max {
                self.publish(
                    &format!("sensors/{}/{}/max", chip_key, label_key),
                    TelemetryValue::Gauge(max),
                    timestamp,
                    labels,
                )
                .await;
                count += 1;
            }
        }

        count
    }

    /// Collect TCP connection state counts (Linux-specific).
    #[cfg(target_os = "linux")]
    async fn collect_tcp_states(&mut self, timestamp: i64) -> usize {
        let mut count = 0;

        let states = LinuxMetrics::collect_tcp_states();

        let state_values = [
            ("established", states.established),
            ("syn_sent", states.syn_sent),
            ("syn_recv", states.syn_recv),
            ("fin_wait1", states.fin_wait1),
            ("fin_wait2", states.fin_wait2),
            ("time_wait", states.time_wait),
            ("close", states.close),
            ("close_wait", states.close_wait),
            ("last_ack", states.last_ack),
            ("listen", states.listen),
            ("closing", states.closing),
        ];

        for (state_name, value) in state_values {
            let mut labels = HashMap::new();
            labels.insert("state".to_string(), state_name.to_string());

            self.publish(
                &format!("tcp/{}", state_name),
                TelemetryValue::Counter(value),
                timestamp,
                labels,
            )
            .await;
            count += 1;
        }

        // Also publish total connections
        let total: u64 = state_values.iter().map(|(_, v)| v).sum();
        self.publish(
            "tcp/total",
            TelemetryValue::Counter(total),
            timestamp,
            HashMap::new(),
        )
        .await;
        count += 1;

        count
    }

    /// Publish a telemetry point to Zenoh.
    async fn publish(
        &self,
        metric: &str,
        value: TelemetryValue,
        timestamp: i64,
        labels: HashMap<String, String>,
    ) {
        self.health.record_metrics_published(1);
        let key = format!("{}/{}/{}", self.key_prefix, self.hostname, metric);

        let point = TelemetryPoint {
            timestamp,
            source: self.hostname.clone(),
            protocol: Protocol::Sysinfo,
            metric: metric.to_string(),
            value,
            labels,
        };

        match encode(&point, self.format) {
            Ok(payload) => {
                if let Err(e) = self.session.put(&key, payload).await {
                    warn!("Failed to publish '{}': {}", key, e);
                }
            }
            Err(e) => {
                warn!("Failed to encode metric '{}': {}", metric, e);
            }
        }
    }
}

/// Build a key expression for a sysinfo metric.
pub fn build_key_expr(prefix: &str, hostname: &str, metric: &str) -> String {
    format!("{}/{}/{}", prefix, hostname, metric)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DiskConfig, NetworkConfig};

    #[test]
    fn test_build_key_expr() {
        assert_eq!(
            build_key_expr("zensight/sysinfo", "server01", "cpu/usage"),
            "zensight/sysinfo/server01/cpu/usage"
        );
    }

    #[test]
    fn test_network_filter_defaults() {
        // Default config has exclude_loopback: false (from Default trait)
        let config = NetworkConfig::default();
        assert!(config.should_include("eth0"));
        assert!(config.should_include("enp0s3"));
        // With default, loopback is included (exclude_loopback defaults to false)
        assert!(config.should_include("lo"));
    }

    #[test]
    fn test_disk_filter_defaults() {
        // Default config has exclude_pseudo: false (from Default trait)
        let config = DiskConfig::default();
        assert!(config.should_include("/", "ext4"));
        assert!(config.should_include("/home", "xfs"));
        // With default, tmpfs is included (exclude_pseudo defaults to false)
        assert!(config.should_include("/run", "tmpfs"));
    }
}
