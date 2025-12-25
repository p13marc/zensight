//! System metrics collection using sysinfo.

use crate::config::SysinfoConfig;
use std::collections::HashMap;
use std::sync::Arc;
use sysinfo::{Disks, Networks, System};
use tracing::{debug, warn};
use zenoh::Session;
use zensight_common::serialization::{Format, encode};
use zensight_common::telemetry::{Protocol, TelemetryPoint, TelemetryValue};

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
        }
    }

    /// Run the collection loop.
    pub async fn run(mut self) {
        let interval = std::time::Duration::from_secs(self.config.poll_interval_secs);

        tracing::info!(
            "Starting system collector for '{}' (interval: {}s)",
            self.hostname,
            self.config.poll_interval_secs
        );

        loop {
            self.collect_and_publish().await;
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

        if self.config.collect.memory {
            count += self.collect_memory(timestamp).await;
        }

        if self.config.collect.disk {
            count += self.collect_disk(timestamp).await;
        }

        if self.config.collect.network {
            count += self.collect_network(timestamp).await;
        }

        if self.config.collect.processes {
            count += self.collect_processes(timestamp).await;
        }

        debug!("Published {} metrics for '{}'", count, self.hostname);
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

        // Memory usage percentage
        let usage_pct = if total > 0 {
            (used as f64 / total as f64) * 100.0
        } else {
            0.0
        };
        self.publish(
            "memory/usage_percent",
            TelemetryValue::Gauge(usage_pct),
            timestamp,
            HashMap::new(),
        )
        .await;
        count += 1;

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
            labels.remove(&"unit".to_string());
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
            labels.remove(&"unit".to_string());
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

    /// Publish a telemetry point to Zenoh.
    async fn publish(
        &self,
        metric: &str,
        value: TelemetryValue,
        timestamp: i64,
        labels: HashMap<String, String>,
    ) {
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

/// Sanitize a string for use in key expressions.
/// Replaces problematic characters with underscores.
fn sanitize_key(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '/' | ' ' | '#' | '?' | '*' => {
                if !result.ends_with('_') && !result.is_empty() {
                    result.push('_');
                }
            }
            _ => result.push(c),
        }
    }
    // Remove leading/trailing underscores
    result.trim_matches('_').to_string()
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
    fn test_sanitize_key() {
        assert_eq!(sanitize_key("/"), "");
        assert_eq!(sanitize_key("/home"), "home");
        assert_eq!(sanitize_key("/home/user"), "home_user");
        assert_eq!(sanitize_key("eth0"), "eth0");
        assert_eq!(sanitize_key("my interface"), "my_interface");
    }

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
