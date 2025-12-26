//! Bridge health monitoring and metrics.
//!
//! This module provides:
//! - [`BridgeHealth`] for tracking overall bridge health metrics
//! - [`DeviceLiveness`] for tracking per-device availability
//! - [`BridgeError`] for unified error reporting

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::liveliness::LivelinessManager;
use crate::publisher::Publisher;

/// Bridge health metrics.
///
/// Tracks overall bridge health including device counts, error rates,
/// and performance metrics.
#[derive(Debug)]
pub struct BridgeHealth {
    /// Bridge name.
    bridge_name: String,
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
    /// Errors in the last hour (rolling).
    errors_last_hour: AtomicU64,
    /// Last poll duration in milliseconds.
    last_poll_duration_ms: AtomicU64,
    /// Per-device liveness tracking.
    device_liveness: Arc<RwLock<HashMap<String, DeviceState>>>,
    /// Publisher for health metrics.
    publisher: Option<Publisher>,
    /// Liveliness manager for Zenoh presence tokens.
    liveliness_manager: Option<Arc<LivelinessManager>>,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceStatus {
    /// Device is responding normally.
    Online,
    /// Device is not responding.
    Offline,
    /// Device is responding but with errors.
    Degraded,
    /// Device status is unknown (never polled).
    Unknown,
}

impl Default for DeviceStatus {
    fn default() -> Self {
        Self::Unknown
    }
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
    /// Bridge name.
    pub bridge: String,
    /// Overall health status.
    pub status: String,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    Other,
}

impl Default for ErrorType {
    fn default() -> Self {
        Self::Other
    }
}

impl BridgeHealth {
    /// Create a new health tracker.
    pub fn new(bridge_name: impl Into<String>) -> Self {
        Self {
            bridge_name: bridge_name.into(),
            start_time: Instant::now(),
            devices_total: AtomicU64::new(0),
            devices_responding: AtomicU64::new(0),
            devices_failed: AtomicU64::new(0),
            metrics_published: AtomicU64::new(0),
            errors_last_hour: AtomicU64::new(0),
            last_poll_duration_ms: AtomicU64::new(0),
            device_liveness: Arc::new(RwLock::new(HashMap::new())),
            publisher: None,
            liveliness_manager: None,
        }
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

        let mut devices = self.device_liveness.write().unwrap();
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
    }

    /// Record that a device poll succeeded (async version).
    ///
    /// If a liveliness manager is configured, this will also declare
    /// the device's liveliness token for instant presence detection.
    pub async fn record_device_success_async(&self, device_id: &str) {
        // Update internal state
        self.record_device_success(device_id);

        // Declare liveliness token if configured
        if let Some(ref liveliness) = self.liveliness_manager {
            if let Err(e) = liveliness.declare_device_alive(device_id).await {
                tracing::warn!(
                    device = %device_id,
                    error = %e,
                    "Failed to declare device liveliness token"
                );
            }
        }
    }

    /// Record that a device poll failed.
    ///
    /// This is the synchronous version. Use [`record_device_failure_async`] if you
    /// have a liveliness manager configured and want to undeclare the device token.
    pub fn record_device_failure(&self, device_id: &str, error: &str) {
        let mut devices = self.device_liveness.write().unwrap();
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
        self.errors_last_hour.fetch_add(1, Ordering::SeqCst);
    }

    /// Record that a device poll failed (async version).
    ///
    /// If a liveliness manager is configured and the device transitions to
    /// Offline status (3+ consecutive failures), this will undeclare the
    /// device's liveliness token.
    pub async fn record_device_failure_async(&self, device_id: &str, error: &str) {
        // Get old status before update
        let was_online = {
            let devices = self.device_liveness.read().unwrap();
            devices
                .get(device_id)
                .is_some_and(|s| s.status != DeviceStatus::Offline)
        };

        // Update internal state
        self.record_device_failure(device_id, error);

        // Check if device just went offline
        let is_now_offline = {
            let devices = self.device_liveness.read().unwrap();
            devices
                .get(device_id)
                .is_some_and(|s| s.status == DeviceStatus::Offline)
        };

        // Undeclare liveliness token if device just went offline
        if was_online && is_now_offline {
            if let Some(ref liveliness) = self.liveliness_manager {
                liveliness.undeclare_device(device_id).await;
            }
        }
    }

    /// Update device responding/failed counters based on liveness states.
    fn update_device_counters(&self) {
        let devices = self.device_liveness.read().unwrap();
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

        let status = if devices_failed == 0 && devices_responding == devices_total {
            "healthy"
        } else if devices_failed > 0 && devices_responding > 0 {
            "degraded"
        } else if devices_responding == 0 && devices_total > 0 {
            "error"
        } else {
            "healthy"
        };

        HealthSnapshot {
            bridge: self.bridge_name.clone(),
            status: status.to_string(),
            uptime_secs: uptime,
            devices_total,
            devices_responding,
            devices_failed,
            last_poll_duration_ms: self.last_poll_duration_ms.load(Ordering::SeqCst),
            errors_last_hour: self.errors_last_hour.load(Ordering::SeqCst),
            metrics_published: self.metrics_published.load(Ordering::SeqCst),
        }
    }

    /// Get liveness info for a specific device.
    pub fn device_liveness(&self, device_id: &str) -> Option<DeviceLiveness> {
        let devices = self.device_liveness.read().unwrap();
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
        let devices = self.device_liveness.read().unwrap();
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

    /// Publish health metrics to Zenoh.
    pub async fn publish_health(&self) -> Result<()> {
        let Some(ref publisher) = self.publisher else {
            return Ok(());
        };

        let snapshot = self.snapshot();
        let key = format!("{}/@/health", publisher.key_prefix());
        publisher.publish_json(&key, &snapshot).await
    }

    /// Publish device liveness to Zenoh.
    pub async fn publish_device_liveness(&self, device_id: &str) -> Result<()> {
        let Some(ref publisher) = self.publisher else {
            return Ok(());
        };

        if let Some(liveness) = self.device_liveness(device_id) {
            let key = format!(
                "{}/@/devices/{}/liveness",
                publisher.key_prefix(),
                device_id
            );
            publisher.publish_json(&key, &liveness).await?;
        }

        Ok(())
    }

    /// Publish an error report to Zenoh.
    pub async fn publish_error(&self, report: &ErrorReport) -> Result<()> {
        let Some(ref publisher) = self.publisher else {
            return Ok(());
        };

        let key = format!("{}/@/errors", publisher.key_prefix());
        publisher.publish_json(&key, report).await
    }
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
        let health = BridgeHealth::new("test");
        assert_eq!(health.bridge_name, "test");

        let snapshot = health.snapshot();
        assert_eq!(snapshot.bridge, "test");
        assert_eq!(snapshot.status, "healthy");
        assert_eq!(snapshot.devices_total, 0);
    }

    #[test]
    fn test_device_success() {
        let health = BridgeHealth::new("test");
        health.set_devices_total(2);

        health.record_device_success("device1");

        let liveness = health.device_liveness("device1").unwrap();
        assert_eq!(liveness.status, DeviceStatus::Online);
        assert_eq!(liveness.consecutive_failures, 0);
        assert!(liveness.last_error.is_none());
    }

    #[test]
    fn test_device_failure() {
        let health = BridgeHealth::new("test");
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
        let health = BridgeHealth::new("test");

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
        let health = BridgeHealth::new("test");
        health.set_devices_total(2);

        // All healthy
        health.record_device_success("d1");
        health.record_device_success("d2");
        assert_eq!(health.snapshot().status, "healthy");

        // One failed - degraded
        health.record_device_failure("d1", "error");
        health.record_device_failure("d1", "error");
        health.record_device_failure("d1", "error");
        assert_eq!(health.snapshot().status, "degraded");
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
        let health = BridgeHealth::new("test");

        health.record_metrics_published(10);
        health.record_metrics_published(5);

        assert_eq!(health.snapshot().metrics_published, 15);
    }
}
