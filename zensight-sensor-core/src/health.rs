//! Sensor health monitoring and metrics.
//!
//! This module provides:
//! - [`SensorHealth`] for tracking overall sensor health metrics
//! - [`DeviceLiveness`] for tracking per-device availability
//! - [`SensorError`] for unified error reporting

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::liveliness::LivelinessManager;
use crate::publisher::Publisher;

/// Rolling window error counter with 1-minute buckets over the last hour.
struct RollingErrorCounter {
    /// 60 buckets, one per minute.
    buckets: Mutex<[u64; 60]>,
    /// Current bucket index (0-59).
    current_bucket: AtomicUsize,
    /// Unix timestamp (seconds) of the last rotation.
    last_rotation: AtomicI64,
}

impl RollingErrorCounter {
    fn new() -> Self {
        Self {
            buckets: Mutex::new([0; 60]),
            current_bucket: AtomicUsize::new(0),
            last_rotation: AtomicI64::new(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0),
            ),
        }
    }

    fn increment(&self) {
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        self.rotate_locked(&mut buckets);
        let idx = self.current_bucket.load(Ordering::SeqCst);
        buckets[idx] += 1;
    }

    fn count(&self) -> u64 {
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        self.rotate_locked(&mut buckets);
        buckets.iter().sum()
    }

    /// Advance the window to now. Runs under the buckets lock (#1080): the
    /// rotation used to read `last_rotation`, compute, and only then take
    /// the lock, so two callers could both rotate and an increment could
    /// land in a bucket a concurrent rotation had just cleared. And it
    /// stored `now` rather than `last + elapsed`, so each bucket covered
    /// 60–119 s and "the last hour" drifted long.
    fn rotate_locked(&self, buckets: &mut [u64; 60]) {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let last = self.last_rotation.load(Ordering::SeqCst);
        let elapsed_minutes = ((now_secs - last) / 60).max(0) as usize;

        if elapsed_minutes == 0 {
            return;
        }

        let current = self.current_bucket.load(Ordering::SeqCst);

        // Zero out expired buckets
        let to_clear = elapsed_minutes.min(60);
        for i in 1..=to_clear {
            buckets[(current + i) % 60] = 0;
        }

        let new_bucket = (current + elapsed_minutes) % 60;
        self.current_bucket.store(new_bucket, Ordering::SeqCst);
        self.last_rotation
            .store(last + elapsed_minutes as i64 * 60, Ordering::SeqCst);
    }
}

impl std::fmt::Debug for RollingErrorCounter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RollingErrorCounter")
            .field("count", &self.count())
            .finish()
    }
}

/// Sensor health metrics.
///
/// Tracks overall sensor health including device counts, error rates,
/// and performance metrics.
#[derive(Debug)]
pub struct SensorHealth {
    /// Sensor name.
    sensor_name: String,
    /// Start time for uptime calculation.
    start_time: Instant,
    /// Total devices configured.
    devices_total: AtomicU64,
    /// Devices currently responding.
    devices_responding: AtomicU64,
    /// Devices currently failed.
    devices_failed: AtomicU64,
    /// Total metrics published.
    metrics_published: AtomicU64,
    /// Errors in the last hour (rolling window with 1-minute buckets).
    errors_last_hour: RollingErrorCounter,
    /// Epoch ms of the last success (a device answering, a batch landing, a
    /// poll completing); 0 = never (#1080).
    last_success_ms: AtomicU64,
    /// Epoch ms of the last recorded error; 0 = never (#1080).
    last_error_ms: AtomicU64,
    /// Errors since the last success (#1080) — the device-less analogue of
    /// a device's `consecutive_failures`.
    consecutive_errors: AtomicU64,
    /// The most recent error message (#1080).
    last_error: RwLock<Option<String>>,
    /// Last poll duration in milliseconds.
    last_poll_duration_ms: AtomicU64,
    /// Per-device liveness tracking.
    device_liveness: Arc<RwLock<HashMap<String, DeviceState>>>,
    /// Hashed machine-id stamped onto snapshots (identity envelope, #301).
    host_id: RwLock<Option<String>>,
    /// The instance's `<source>` key segment. When set, control-plane keys are
    /// host-scoped (`{prefix}/{source}/@/…`); when unset, the legacy
    /// protocol-scoped shape (`{prefix}/@/…`) is used.
    source: Option<String>,
    /// Publisher for health metrics.
    publisher: Option<Publisher>,
    /// Liveliness manager for Zenoh presence tokens.
    liveliness_manager: Option<Arc<LivelinessManager>>,
    /// Declared memory budget, bytes (#811); 0 = undeclared. Carried into
    /// `self_stats.budget_bytes` and graded by the runner's budget rule —
    /// never enforced here (#812 is the enforcement).
    budget_bytes: AtomicU64,
    /// The baseline tier's publish counters (#811), shared with the
    /// [`Publisher`]'s registry; the sensor may feed dropped/evicted totals
    /// into the same accounting.
    publish_counters: Option<Arc<zensight_common::PublishCounters>>,
    /// Self CPU sampler (#811) — diffed on the health tick.
    cpu_sampler: Mutex<crate::procutil::SelfCpuSampler>,
    /// Per-table occupancy providers (#811), registered by the sensor and
    /// pulled only on the health tick — nothing on hot paths.
    table_providers: TableStatsProviders,
}

/// A pull callback reporting one or more tables' occupancy (#811). Invoked on
/// the 5s health tick only. **Must not call back into [`SensorHealth`]** (the
/// providers run under its lock).
pub type TableStatsFn = Box<dyn Fn() -> Vec<zensight_common::TableStats> + Send + Sync>;

#[derive(Default)]
struct TableStatsProviders(Mutex<Vec<TableStatsFn>>);

impl std::fmt::Debug for TableStatsProviders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = self.0.lock().map(|v| v.len()).unwrap_or(0);
        write!(f, "TableStatsProviders({n})")
    }
}

/// Device state for liveness tracking.
#[derive(Debug, Clone)]
struct DeviceState {
    /// Device identifier.
    device_id: String,
    /// Current status.
    status: DeviceStatus,
    /// Last successful contact timestamp (millis since epoch).
    last_seen: i64,
    /// Number of consecutive failures.
    consecutive_failures: u32,
    /// Last error message (if any).
    last_error: Option<String>,
}

/// Device availability status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceStatus {
    /// Device is responding normally.
    Online,
    /// Device is not responding.
    Offline,
    /// Device is responding but with errors.
    Degraded,
    /// Device status is unknown (never polled).
    #[default]
    Unknown,
}

impl std::fmt::Display for DeviceStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceStatus::Online => write!(f, "online"),
            DeviceStatus::Offline => write!(f, "offline"),
            DeviceStatus::Degraded => write!(f, "degraded"),
            DeviceStatus::Unknown => write!(f, "unknown"),
        }
    }
}

/// Health snapshot for serialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthSnapshot {
    /// Sensor name.
    pub sensor: String,
    /// Overall health status.
    pub status: zensight_common::HealthStatus,
    /// Uptime in seconds.
    pub uptime_secs: u64,
    /// Total devices configured.
    pub devices_total: u64,
    /// Devices currently responding.
    pub devices_responding: u64,
    /// Devices currently failed.
    pub devices_failed: u64,
    /// Last poll duration in milliseconds.
    pub last_poll_duration_ms: u64,
    /// Errors in the last hour.
    pub errors_last_hour: u64,
    /// Total metrics published.
    pub metrics_published: u64,
    /// Hashed machine-id of the publishing host (identity envelope, #301).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    /// The host id of this sensor instance — the same value
    /// behind the origin scoping its control-plane keys
    /// (`zensight/v1/<origin>/state/<producer>/health`). Optional for
    /// mixed-fleet/persisted payloads predating the host-scoped keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Self-measured resource telemetry (#811) — shape shared with the
    /// consumer copy via [`zensight_common::SelfStats`]. Absent = not
    /// measured (a plain [`SensorHealth::snapshot`], or an older sensor).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_stats: Option<zensight_common::SelfStats>,
    /// When the sensor last did its job successfully, epoch ms (#1080).
    /// Mirrors `zensight_common::HealthSnapshot::last_success_unix_ms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success_unix_ms: Option<i64>,
    /// The most recent error the sensor recorded (#1080).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// Device liveness information for serialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceLiveness {
    /// Device identifier.
    pub device: String,
    /// Current status.
    pub status: DeviceStatus,
    /// Last seen timestamp (millis since epoch).
    pub last_seen: i64,
    /// Consecutive failures count.
    pub consecutive_failures: u32,
    /// Last error message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// Error report for unified error publishing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorReport {
    /// Timestamp (millis since epoch).
    pub timestamp: i64,
    /// Device identifier (if applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// Error type classification.
    pub error_type: ErrorType,
    /// Error message.
    pub message: String,
    /// Whether the error is retryable.
    pub retryable: bool,
}

/// Error type classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorType {
    /// Connection timeout.
    Timeout,
    /// Authentication failed.
    AuthFailed,
    /// Connection refused.
    ConnectionRefused,
    /// Connection reset.
    ConnectionReset,
    /// Parse/decode error.
    ParseError,
    /// Protocol error.
    ProtocolError,
    /// Configuration error.
    ConfigError,
    /// Other/unknown error.
    #[default]
    Other,
}

impl SensorHealth {
    /// Create a new health tracker.
    pub fn new(sensor_name: impl Into<String>) -> Self {
        Self {
            sensor_name: sensor_name.into(),
            start_time: Instant::now(),
            devices_total: AtomicU64::new(0),
            devices_responding: AtomicU64::new(0),
            devices_failed: AtomicU64::new(0),
            metrics_published: AtomicU64::new(0),
            errors_last_hour: RollingErrorCounter::new(),
            last_success_ms: AtomicU64::new(0),
            last_error_ms: AtomicU64::new(0),
            consecutive_errors: AtomicU64::new(0),
            last_error: RwLock::new(None),
            last_poll_duration_ms: AtomicU64::new(0),
            device_liveness: Arc::new(RwLock::new(HashMap::new())),
            host_id: RwLock::new(None),
            source: None,
            publisher: None,
            liveliness_manager: None,
            budget_bytes: AtomicU64::new(0),
            publish_counters: None,
            cpu_sampler: Mutex::new(crate::procutil::SelfCpuSampler::default()),
            table_providers: TableStatsProviders::default(),
        }
    }

    /// Declare the memory budget carried into `self_stats` (#811). 0 clears.
    pub fn set_budget_bytes(&self, bytes: u64) {
        self.budget_bytes.store(bytes, Ordering::Relaxed);
    }

    /// Attach the publish counters read on each health tick (#811). The
    /// runner wires this from [`Publisher::counters`].
    pub fn with_publish_counters(
        mut self,
        counters: Arc<zensight_common::PublishCounters>,
    ) -> Self {
        self.publish_counters = Some(counters);
        self
    }

    /// Register a table-occupancy provider (#811): a cheap pull callback the
    /// health tick invokes (every 5s, never per sample). Register one per
    /// bounded structure worth naming — this is the field that turns "the
    /// sensor is big" into "the flow table is 280 MB of it".
    pub fn register_table_stats(&self, provider: TableStatsFn) {
        self.table_providers
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(provider);
    }

    /// Stamp the host identity onto future snapshots (identity envelope, #301).
    pub fn set_host_id(&self, host_id: Option<String>) {
        *self.host_id.write().expect("host_id lock poisoned") = host_id;
    }

    /// Set the instance's host id, stamped onto snapshots; the published
    /// control-plane keys (`state/<producer>/health` etc.) are origin-scoped
    /// so two hosts running the same protocol never collide. The runner always sets this.
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// Set the publisher for health metrics.
    pub fn with_publisher(mut self, publisher: Publisher) -> Self {
        self.publisher = Some(publisher);
        self
    }

    /// Set the liveliness manager for Zenoh presence tokens.
    ///
    /// When set, device success/failure will automatically declare/undeclare
    /// liveliness tokens for instant presence detection by the frontend.
    pub fn with_liveliness(mut self, liveliness: Arc<LivelinessManager>) -> Self {
        self.liveliness_manager = Some(liveliness);
        self
    }

    /// Set the total number of devices.
    pub fn set_devices_total(&self, count: u64) {
        self.devices_total.store(count, Ordering::SeqCst);
    }

    /// Record that a device poll succeeded.
    ///
    /// This is the synchronous version. Use [`record_device_success_async`] if you
    /// have a liveliness manager configured and want to declare the device token.
    pub fn record_device_success(&self, device_id: &str) {
        let now = chrono::Utc::now().timestamp_millis();

        let mut devices = self
            .device_liveness
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let state = devices
            .entry(device_id.to_string())
            .or_insert_with(|| DeviceState {
                device_id: device_id.to_string(),
                status: DeviceStatus::Unknown,
                last_seen: 0,
                consecutive_failures: 0,
                last_error: None,
            });

        state.status = DeviceStatus::Online;
        state.last_seen = now;
        state.consecutive_failures = 0;
        state.last_error = None;

        // Update counters
        drop(devices);
        self.update_device_counters();
        self.record_success();
    }

    /// Record that a device poll succeeded (async version).
    ///
    /// If a liveliness manager is configured, this will also declare
    /// the device's liveliness token for instant presence detection.
    pub async fn record_device_success_async(&self, device_id: &str) {
        // Update internal state
        self.record_device_success(device_id);

        // Declare liveliness token if configured
        if let Some(ref liveliness) = self.liveliness_manager
            && let Err(e) = liveliness.declare_device_alive(device_id).await
        {
            tracing::warn!(
                device = %device_id,
                error = %e,
                "Failed to declare device liveliness token"
            );
        }
    }

    /// Record that a device poll failed.
    ///
    /// This is the synchronous version. Use [`record_device_failure_async`] if you
    /// have a liveliness manager configured and want to undeclare the device token.
    pub fn record_device_failure(&self, device_id: &str, error: &str) {
        let mut devices = self
            .device_liveness
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let state = devices
            .entry(device_id.to_string())
            .or_insert_with(|| DeviceState {
                device_id: device_id.to_string(),
                status: DeviceStatus::Unknown,
                last_seen: 0,
                consecutive_failures: 0,
                last_error: None,
            });

        state.consecutive_failures += 1;
        state.last_error = Some(error.to_string());

        // Mark as offline after 3 consecutive failures
        if state.consecutive_failures >= 3 {
            state.status = DeviceStatus::Offline;
        } else {
            state.status = DeviceStatus::Degraded;
        }

        // Update counters
        drop(devices);
        self.update_device_counters();
        self.record_error(error);
    }

    /// Record that a device poll failed (async version).
    ///
    /// If a liveliness manager is configured and the device transitions to
    /// Offline status (3+ consecutive failures), this will undeclare the
    /// device's liveliness token.
    pub async fn record_device_failure_async(&self, device_id: &str, error: &str) {
        // Get old status before update
        let was_online = {
            let devices = self
                .device_liveness
                .read()
                .unwrap_or_else(|e| e.into_inner());
            devices
                .get(device_id)
                .is_some_and(|s| s.status != DeviceStatus::Offline)
        };

        // Update internal state
        self.record_device_failure(device_id, error);

        // Check if device just went offline
        let is_now_offline = {
            let devices = self
                .device_liveness
                .read()
                .unwrap_or_else(|e| e.into_inner());
            devices
                .get(device_id)
                .is_some_and(|s| s.status == DeviceStatus::Offline)
        };

        // Undeclare liveliness token if device just went offline
        if was_online
            && is_now_offline
            && let Some(ref liveliness) = self.liveliness_manager
        {
            liveliness.undeclare_device(device_id).await;
        }
    }

    /// Update device responding/failed counters based on liveness states.
    fn update_device_counters(&self) {
        let devices = self
            .device_liveness
            .read()
            .unwrap_or_else(|e| e.into_inner());
        let mut responding = 0u64;
        let mut failed = 0u64;

        for state in devices.values() {
            match state.status {
                DeviceStatus::Online | DeviceStatus::Degraded => responding += 1,
                DeviceStatus::Offline => failed += 1,
                DeviceStatus::Unknown => {}
            }
        }

        self.devices_responding.store(responding, Ordering::SeqCst);
        self.devices_failed.store(failed, Ordering::SeqCst);
    }

    /// Record that the sensor did its job (#1080): a poll completed, a batch
    /// landed, a collector tick finished. For a sensor with a device census
    /// [`record_device_success`](Self::record_device_success) calls this;
    /// a device-less collector (logs, netflow, gnmi, modbus, hostspec) calls
    /// it directly, or its `status` can never say anything but `Healthy`.
    pub fn record_success(&self) {
        self.last_success_ms.store(now_unix_ms(), Ordering::SeqCst);
        self.consecutive_errors.store(0, Ordering::SeqCst);
    }

    /// Record an error that is not tied to a device (#1080). Counts into
    /// `errors_last_hour`, keeps the message, and — until the next
    /// [`record_success`](Self::record_success) — degrades `status`, the
    /// way a device's `consecutive_failures` does for a proxy sensor.
    pub fn record_error(&self, message: &str) {
        self.errors_last_hour.increment();
        self.last_error_ms.store(now_unix_ms(), Ordering::SeqCst);
        self.consecutive_errors.fetch_add(1, Ordering::SeqCst);
        *self.last_error.write().unwrap_or_else(|e| e.into_inner()) = Some(message.to_string());
    }

    /// Record that metrics were published.
    pub fn record_metrics_published(&self, count: u64) {
        self.metrics_published.fetch_add(count, Ordering::SeqCst);
    }

    /// Record poll duration.
    pub fn record_poll_duration(&self, duration_ms: u64) {
        self.last_poll_duration_ms
            .store(duration_ms, Ordering::SeqCst);
    }

    /// Get a snapshot of current health metrics.
    pub fn snapshot(&self) -> HealthSnapshot {
        let uptime = self.start_time.elapsed().as_secs();
        let devices_total = self.devices_total.load(Ordering::SeqCst);
        let devices_responding = self.devices_responding.load(Ordering::SeqCst);
        let devices_failed = self.devices_failed.load(Ordering::SeqCst);

        let census = if devices_failed == 0 && devices_responding == devices_total {
            zensight_common::HealthStatus::Healthy
        } else if devices_failed > 0 && devices_responding > 0 {
            zensight_common::HealthStatus::Degraded
        } else if devices_responding == 0 && devices_total > 0 {
            zensight_common::HealthStatus::Error
        } else {
            zensight_common::HealthStatus::Healthy
        };
        let last_success_ms = self.last_success_ms.load(Ordering::SeqCst);
        let last_error_ms = self.last_error_ms.load(Ordering::SeqCst);
        let status = error_status(
            census,
            last_error_ms > last_success_ms,
            self.consecutive_errors.load(Ordering::SeqCst),
        );

        HealthSnapshot {
            sensor: self.sensor_name.clone(),
            status,
            uptime_secs: uptime,
            devices_total,
            devices_responding,
            devices_failed,
            last_poll_duration_ms: self.last_poll_duration_ms.load(Ordering::SeqCst),
            errors_last_hour: self.errors_last_hour.count(),
            metrics_published: self.metrics_published.load(Ordering::SeqCst),
            host_id: self.host_id.read().expect("host_id lock poisoned").clone(),
            source: self.source.clone(),
            // Self-measurement is the health *tick*'s job
            // ([`snapshot_with_self`]) — the plain snapshot stays cheap for
            // callers that only want the counters.
            self_stats: None,
            last_success_unix_ms: (last_success_ms > 0).then_some(last_success_ms as i64),
            last_error: self
                .last_error
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        }
    }

    /// [`snapshot`](Self::snapshot) plus the self-measured `self_stats`
    /// (#811): `/proc/self` memory + CPU, the publish counters, the declared
    /// budget, registered table providers, and cgroup context. Called from
    /// the health tick — measurement happens every 5s, never per sample.
    pub fn snapshot_with_self(&self) -> HealthSnapshot {
        let mut snap = self.snapshot();
        snap.self_stats = Some(self.collect_self_stats());
        snap
    }

    fn collect_self_stats(&self) -> zensight_common::SelfStats {
        let mem = crate::procutil::self_memory();
        let cpu_percent = self
            .cpu_sampler
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sample();
        let budget = self.budget_bytes.load(Ordering::Relaxed);
        let counters = self.publish_counters.as_ref();
        let tables = {
            let providers = self
                .table_providers
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            providers.iter().flat_map(|p| p()).collect()
        };
        zensight_common::SelfStats {
            rss_bytes: mem.map(|(rss, _)| rss),
            vsz_bytes: mem.map(|(_, vsz)| vsz),
            cpu_percent,
            budget_bytes: (budget > 0).then_some(budget),
            published_total: counters.map(|c| c.published_total()),
            published_bytes_total: counters.map(|c| c.published_bytes_total()),
            // Dropped/evicted are sensor-fed: report them only once something
            // was wired, so an unwired sensor reads as *not measured*.
            dropped_total: counters.map(|c| c.dropped_total()).filter(|&n| n > 0),
            evicted_total: counters.map(|c| c.evicted_total()).filter(|&n| n > 0),
            tables,
            cgroup: crate::procutil::self_cgroup(),
            // Stamped by the runner's governed tick (#812), not measured here.
            ladder: None,
        }
    }

    /// Get liveness info for a specific device.
    pub fn device_liveness(&self, device_id: &str) -> Option<DeviceLiveness> {
        let devices = self
            .device_liveness
            .read()
            .unwrap_or_else(|e| e.into_inner());
        devices.get(device_id).map(|state| DeviceLiveness {
            device: state.device_id.clone(),
            status: state.status,
            last_seen: state.last_seen,
            consecutive_failures: state.consecutive_failures,
            last_error: state.last_error.clone(),
        })
    }

    /// Get liveness info for all devices.
    pub fn all_device_liveness(&self) -> Vec<DeviceLiveness> {
        let devices = self
            .device_liveness
            .read()
            .unwrap_or_else(|e| e.into_inner());
        devices
            .values()
            .map(|state| DeviceLiveness {
                device: state.device_id.clone(),
                status: state.status,
                last_seen: state.last_seen,
                consecutive_failures: state.consecutive_failures,
                last_error: state.last_error.clone(),
            })
            .collect()
    }

    /// Publish health metrics to Zenoh — including `self_stats` (#811): the
    /// health tick is where self-measurement happens.
    pub async fn publish_health(&self) -> Result<()> {
        let snapshot = self.snapshot_with_self();
        self.publish_snapshot(&snapshot).await
    }

    /// Publish an already-taken snapshot — split from
    /// [`publish_health`](Self::publish_health) so the runner's budget rule
    /// can grade the *same* measurement it publishes (sampling twice would
    /// corrupt the CPU diff).
    pub async fn publish_snapshot(&self, snapshot: &HealthSnapshot) -> Result<()> {
        let Some(ref publisher) = self.publisher else {
            return Ok(());
        };
        let key = publisher.v1().health_key();
        publisher
            .publish_json(&key, snapshot, zensight_common::QosClass::HealthLiveness)
            .await
    }

    /// Publish an error report to Zenoh.
    pub async fn publish_error(&self, report: &ErrorReport) -> Result<()> {
        let Some(ref publisher) = self.publisher else {
            return Ok(());
        };

        // An error report is an error (#1080): before this, a sensor that
        // published a report on `state/<producer>/errors` every second still
        // counted zero in `errors_last_hour` and stayed `Healthy`.
        self.record_error(&report.message);
        let key = publisher.v1().errors_key();
        publisher
            .publish_json(&key, report, zensight_common::QosClass::HealthLiveness)
            .await
    }
}

/// The `sensor-budget` rule slug (#811).
pub const SENSOR_BUDGET_RULE: &str = "sensor-budget";

/// Grade RSS against the declared budget (#811): Warning at ≥ 80 %,
/// Critical at ≥ 95 %, and — once firing — the alert holds until usage drops
/// under 75 % (hysteresis, so a sensor oscillating around the threshold
/// updates one alert instead of flapping). Pure; the runner drives it.
pub fn budget_level(
    currently_firing: bool,
    rss_bytes: u64,
    budget_bytes: u64,
) -> Option<zensight_common::AlertSeverity> {
    if budget_bytes == 0 {
        return None;
    }
    let ratio = rss_bytes as f64 / budget_bytes as f64;
    if ratio >= 0.95 {
        Some(zensight_common::AlertSeverity::Critical)
    } else if ratio >= 0.80 || (currently_firing && ratio >= 0.75) {
        Some(zensight_common::AlertSeverity::Warning)
    } else {
        None
    }
}

/// The health status the error record implies (#1080), folded over the
/// device census. The census alone made every device-less sensor `Healthy`
/// unconditionally — `errors_last_hour` was published beside `status` and
/// never consulted, so a sensor logging three thousand errors an hour kept a
/// green card. Same rule the census applies to one device: an error not yet
/// followed by a success is `Degraded`; three in a row with no success
/// between is `Error`. A census verdict is only ever upgraded toward worse.
pub fn error_status(
    census: zensight_common::HealthStatus,
    unrecovered: bool,
    consecutive_errors: u64,
) -> zensight_common::HealthStatus {
    use zensight_common::HealthStatus::*;
    // A census that already says Degraded is one device among several
    // failing, and three failures of that one device are still one device:
    // the census's own verdict stands. The escalation is for the sensors
    // the census cannot see at all.
    match census {
        Healthy | Starting if unrecovered && consecutive_errors >= 3 => Error,
        Healthy | Starting if unrecovered => Degraded,
        other => other,
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The health status the shed ladder implies (#812): `Degraded` from step 2
/// (optional work actually stopped — which is exactly what Degraded means to
/// zenwatch and the fleet card), never from step 1 (LRU eviction inside plan
/// is normal operation; flipping the card amber for it trains operators to
/// ignore amber). The device-census verdict is only ever *upgraded* toward
/// Degraded — an existing `Error`/`Unhealthy` stands.
pub fn ladder_status(
    census: zensight_common::HealthStatus,
    ladder_step: u8,
) -> zensight_common::HealthStatus {
    use zensight_common::HealthStatus::*;
    if ladder_step >= 2 && matches!(census, Healthy | Starting) {
        Degraded
    } else {
        census
    }
}

/// The `sensor-budget` alert message (#811): an alert that says "this sensor
/// is large" is a page; one that names the table that is growing is a fix.
pub fn budget_summary(stats: &zensight_common::SelfStats) -> String {
    let (rss, budget) = (
        stats.rss_bytes.unwrap_or(0),
        stats.budget_bytes.unwrap_or(0),
    );
    let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
    let pct = if budget > 0 {
        (rss as f64 / budget as f64 * 100.0).round()
    } else {
        0.0
    };
    let mut out = format!(
        "rss {:.0} MiB at {pct:.0}% of {:.0} MiB budget",
        mib(rss),
        mib(budget)
    );
    // Name the largest table — by bytes when known, else by entries.
    let largest = stats
        .tables
        .iter()
        .max_by_key(|t| (t.bytes.unwrap_or(0), t.entries));
    if let Some(t) = largest {
        match t.bytes {
            Some(b) => {
                out.push_str(&format!(
                    "; largest table {}: {:.0} MiB ({} entries)",
                    t.name,
                    mib(b),
                    t.entries
                ));
            }
            None => out.push_str(&format!(
                "; largest table {}: {} entries",
                t.name, t.entries
            )),
        }
    }
    out
}

impl ErrorReport {
    /// Create a new error report.
    pub fn new(error_type: ErrorType, message: impl Into<String>) -> Self {
        Self {
            timestamp: chrono::Utc::now().timestamp_millis(),
            device: None,
            error_type,
            message: message.into(),
            retryable: true,
        }
    }

    /// Set the device this error relates to.
    pub fn with_device(mut self, device: impl Into<String>) -> Self {
        self.device = Some(device.into());
        self
    }

    /// Mark as non-retryable.
    pub fn non_retryable(mut self) -> Self {
        self.retryable = false;
        self
    }

    /// Create a timeout error.
    pub fn timeout(device: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(ErrorType::Timeout, message).with_device(device)
    }

    /// Create a connection refused error.
    pub fn connection_refused(device: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(ErrorType::ConnectionRefused, message).with_device(device)
    }

    /// Create an auth failed error.
    pub fn auth_failed(device: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(ErrorType::AuthFailed, message)
            .with_device(device)
            .non_retryable()
    }

    /// Create a parse error.
    pub fn parse_error(message: impl Into<String>) -> Self {
        Self::new(ErrorType::ParseError, message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_health_new() {
        let health = SensorHealth::new("test");
        assert_eq!(health.sensor_name, "test");

        let snapshot = health.snapshot();
        assert_eq!(snapshot.sensor, "test");
        assert_eq!(snapshot.status, zensight_common::HealthStatus::Healthy);
        assert_eq!(snapshot.devices_total, 0);
        // The plain snapshot never self-measures (#811).
        assert!(snapshot.self_stats.is_none());
    }

    /// #811: the health tick's snapshot measures the process itself.
    #[test]
    #[cfg(target_os = "linux")]
    fn snapshot_with_self_measures_this_process() {
        let health = SensorHealth::new("test");
        let snap = health.snapshot_with_self();
        let stats = snap.self_stats.expect("measured");
        assert!(stats.rss_bytes.unwrap_or(0) > 0, "a running test has RSS");
        assert!(stats.vsz_bytes.unwrap_or(0) > 0);
        // First sample: nothing to diff CPU against — None, not zero.
        assert_eq!(stats.cpu_percent, None);
        // No budget declared, nothing wired: absent, never zero.
        assert_eq!(stats.budget_bytes, None);
        assert_eq!(stats.dropped_total, None);
        assert!(stats.tables.is_empty());

        health.set_budget_bytes(64 * 1024 * 1024);
        health.register_table_stats(Box::new(|| {
            vec![zensight_common::TableStats {
                name: "demo".into(),
                entries: 7,
                bytes: None,
                capacity_entries: Some(16),
                capacity_bytes: None,
            }]
        }));
        let stats = health.snapshot_with_self().self_stats.expect("measured");
        assert_eq!(stats.budget_bytes, Some(64 * 1024 * 1024));
        assert_eq!(stats.tables.len(), 1);
        assert_eq!(stats.tables[0].entries, 7);
    }

    /// #811 budget grading: 80/95 thresholds with 75% release hysteresis.
    #[test]
    fn budget_level_thresholds_and_hysteresis() {
        use zensight_common::AlertSeverity::{Critical, Warning};
        let gib = 100u64;
        // No budget → never grades.
        assert_eq!(budget_level(false, 90, 0), None);
        // Below 80%: quiet.
        assert_eq!(budget_level(false, 79, gib), None);
        // 80% fires Warning; 95% escalates Critical.
        assert_eq!(budget_level(false, 80, gib), Some(Warning));
        assert_eq!(budget_level(false, 95, gib), Some(Critical));
        // Hysteresis: once firing, 76% still holds; 74% releases.
        assert_eq!(budget_level(true, 76, gib), Some(Warning));
        assert_eq!(budget_level(true, 74, gib), None);
        // Not firing at 76%: stays quiet (no premature fire).
        assert_eq!(budget_level(false, 76, gib), None);
    }

    /// #811: the alert message names the table that is growing — the
    /// difference between a page and a fix.
    #[test]
    fn budget_summary_names_the_largest_table() {
        let mib = |n: u64| n * 1024 * 1024;
        let stats = zensight_common::SelfStats {
            rss_bytes: Some(mib(355)),
            budget_bytes: Some(mib(400)),
            tables: vec![
                zensight_common::TableStats {
                    name: "small".into(),
                    entries: 10,
                    bytes: Some(mib(2)),
                    ..Default::default()
                },
                zensight_common::TableStats {
                    name: "flow_inventory".into(),
                    entries: 1_200_000,
                    bytes: Some(mib(280)),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let s = budget_summary(&stats);
        assert!(s.contains("355 MiB"), "{s}");
        assert!(s.contains("89%"), "{s}");
        assert!(s.contains("flow_inventory"), "{s}");
        assert!(s.contains("280 MiB"), "{s}");
    }

    #[test]
    fn test_device_success() {
        let health = SensorHealth::new("test");
        health.set_devices_total(2);

        health.record_device_success("device1");

        let liveness = health.device_liveness("device1").unwrap();
        assert_eq!(liveness.status, DeviceStatus::Online);
        assert_eq!(liveness.consecutive_failures, 0);
        assert!(liveness.last_error.is_none());
    }

    #[test]
    fn test_device_failure() {
        let health = SensorHealth::new("test");
        health.set_devices_total(1);

        // First failure - degraded
        health.record_device_failure("device1", "timeout");
        let liveness = health.device_liveness("device1").unwrap();
        assert_eq!(liveness.status, DeviceStatus::Degraded);
        assert_eq!(liveness.consecutive_failures, 1);

        // Second failure - still degraded
        health.record_device_failure("device1", "timeout");
        let liveness = health.device_liveness("device1").unwrap();
        assert_eq!(liveness.status, DeviceStatus::Degraded);

        // Third failure - offline
        health.record_device_failure("device1", "timeout");
        let liveness = health.device_liveness("device1").unwrap();
        assert_eq!(liveness.status, DeviceStatus::Offline);
        assert_eq!(liveness.consecutive_failures, 3);
    }

    #[test]
    fn test_recovery() {
        let health = SensorHealth::new("test");

        // Fail device
        health.record_device_failure("device1", "error");
        health.record_device_failure("device1", "error");
        health.record_device_failure("device1", "error");

        let liveness = health.device_liveness("device1").unwrap();
        assert_eq!(liveness.status, DeviceStatus::Offline);

        // Recover
        health.record_device_success("device1");

        let liveness = health.device_liveness("device1").unwrap();
        assert_eq!(liveness.status, DeviceStatus::Online);
        assert_eq!(liveness.consecutive_failures, 0);
    }

    #[test]
    fn test_health_status() {
        let health = SensorHealth::new("test");
        health.set_devices_total(2);

        // All healthy
        health.record_device_success("d1");
        health.record_device_success("d2");
        assert_eq!(
            health.snapshot().status,
            zensight_common::HealthStatus::Healthy
        );

        // One failed - degraded
        health.record_device_failure("d1", "error");
        health.record_device_failure("d1", "error");
        health.record_device_failure("d1", "error");
        assert_eq!(
            health.snapshot().status,
            zensight_common::HealthStatus::Degraded
        );
    }

    /// A device-less sensor's status reads its errors (#1080). Before this,
    /// `devices_total == 0` took the census's final `else` and was `Healthy`
    /// with any number of errors recorded.
    #[test]
    fn a_device_less_sensor_with_unrecovered_errors_is_not_healthy() {
        use zensight_common::HealthStatus::*;
        let health = SensorHealth::new("logs");
        assert_eq!(health.snapshot().status, Healthy, "no errors, no devices");
        health.record_error("listener bind failed");
        let snap = health.snapshot();
        assert_eq!(snap.status, Degraded, "one unrecovered error degrades");
        assert_eq!(snap.errors_last_hour, 1);
        assert_eq!(snap.last_error.as_deref(), Some("listener bind failed"));
        health.record_error("again");
        health.record_error("and again");
        assert_eq!(
            health.snapshot().status,
            Error,
            "three in a row is an error"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
        health.record_success();
        let snap = health.snapshot();
        assert_eq!(snap.status, Healthy, "a success recovers");
        assert!(snap.last_success_unix_ms.is_some());
        assert_eq!(
            snap.errors_last_hour, 3,
            "the hour window still counts them"
        );
    }

    #[test]
    fn error_status_never_improves_the_census() {
        use zensight_common::HealthStatus::*;
        assert_eq!(error_status(Error, false, 0), Error);
        assert_eq!(error_status(Offline, true, 5), Offline);
        assert_eq!(error_status(Degraded, true, 1), Degraded);
        assert_eq!(
            error_status(Degraded, true, 3),
            Degraded,
            "one device failing thrice is still one device"
        );
        assert_eq!(error_status(Healthy, false, 0), Healthy);
    }

    #[test]
    fn test_error_report() {
        let report = ErrorReport::timeout("router01", "SNMP request timed out after 5000ms");

        assert_eq!(report.error_type, ErrorType::Timeout);
        assert_eq!(report.device, Some("router01".to_string()));
        assert!(report.retryable);
    }

    #[test]
    fn test_metrics_counter() {
        let health = SensorHealth::new("test");

        health.record_metrics_published(10);
        health.record_metrics_published(5);

        assert_eq!(health.snapshot().metrics_published, 15);
    }

    /// v1 pin: state keys are origin-scoped (`<base>/v1/<origin>/state/
    /// <producer>/…`, RFC 04), so two hosts running the same producer never
    /// collide — the job the legacy `{source}` chunk used to do.
    #[test]
    fn test_state_keys_are_origin_scoped() {
        let ctx = crate::v1::for_producer("sysinfo");
        assert!(ctx.health_key().starts_with("v1/h-"));
        assert!(ctx.health_key().ends_with("/state/sysinfo/health"));
        assert!(ctx.errors_key().ends_with("/state/sysinfo/errors"));
        assert!(
            ctx.device_alive_key("router01")
                .ends_with("/state/sysinfo/device/router01/alive")
        );
    }

    #[test]
    fn test_snapshot_carries_source() {
        let health = SensorHealth::new("sysinfo").with_source("hostA");
        assert_eq!(health.snapshot().source.as_deref(), Some("hostA"));
        assert_eq!(SensorHealth::new("sysinfo").snapshot().source, None);
    }
}
