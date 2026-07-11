//! Parallax alert rules on `@/alerts/*` (mirrors the other sensors'
//! firing/resolved lifecycle via [`AlertReporter`]):
//!
//! - `camera_disappeared` — a V4L2 device the catalogue advertises vanished
//!   from re-enumeration (unplugged / claimed by another driver).
//! - `rtsp_connect_failed` — an `open_stream` couldn't reach the camera.
//! - `encoder_overrun` — average encode time above the per-frame budget
//!   (the encoder cannot keep up with the live source).
//!
//! Every rule resolves automatically on recovery (`reconcile` semantics:
//! the reporter tombstones alerts whose key is no longer firing).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zensight_common::Protocol;
use zensight_common::alert::{Alert, AlertKind, AlertSeverity};
use zensight_sensor_core::AlertReporter;

use crate::catalog::{Catalog, SourceKind};

/// Per-stream rule bookkeeping over an [`AlertReporter`].
pub struct ParallaxAlerts {
    reporter: Arc<AlertReporter>,
    source: String,
    /// rule → (stream → firing alert key), for reconcile.
    firing: Mutex<HashMap<&'static str, HashMap<String, String>>>,
}

const RULE_CAMERA: &str = "camera_disappeared";
const RULE_RTSP: &str = "rtsp_connect_failed";
const RULE_OVERRUN: &str = "encoder_overrun";

impl ParallaxAlerts {
    pub fn new(reporter: Arc<AlertReporter>, source: String) -> Self {
        Self {
            reporter,
            source,
            firing: Mutex::new(HashMap::new()),
        }
    }

    /// A known V4L2 camera vanished from (or returned to) enumeration.
    pub async fn camera_present(&self, stream: &str, device: &str, present: bool) {
        let alert = Alert::new(
            &self.source,
            Protocol::Parallax,
            AlertKind::SensorHealth,
            RULE_CAMERA,
            AlertSeverity::Warning,
            format!("camera {device} (stream {stream}) disappeared"),
        )
        .with_label("stream", stream)
        .with_label("device", device);
        self.set(RULE_CAMERA, stream, alert, !present).await;
    }

    /// An RTSP open failed (fires) or later succeeded (resolves).
    pub async fn rtsp_connect(&self, stream: &str, error: Option<&str>) {
        let summary = match error {
            Some(e) => format!("rtsp connect for stream {stream} failed: {e}"),
            None => String::new(),
        };
        let alert = Alert::new(
            &self.source,
            Protocol::Parallax,
            AlertKind::SensorHealth,
            RULE_RTSP,
            AlertSeverity::Warning,
            summary,
        )
        .with_label("stream", stream);
        self.set(RULE_RTSP, stream, alert, error.is_some()).await;
    }

    /// Encoder overrun evaluation for one stream (called by the stats ticker
    /// each interval with the current average encode time and budget).
    pub async fn encoder_overrun(
        &self,
        stream: &str,
        encode_ms: f64,
        budget_ms: f64,
        firing: bool,
    ) {
        let alert = Alert::new(
            &self.source,
            Protocol::Parallax,
            AlertKind::SensorHealth,
            RULE_OVERRUN,
            AlertSeverity::Warning,
            format!(
                "stream {stream}: encoding {encode_ms:.1} ms/frame exceeds the {budget_ms:.1} ms budget"
            ),
        )
        .with_label("stream", stream);
        self.set(RULE_OVERRUN, stream, alert, firing).await;
    }

    /// Update one (rule, stream) firing state and reconcile the rule so
    /// cleared streams auto-resolve.
    async fn set(&self, rule: &'static str, stream: &str, alert: Alert, firing: bool) {
        let (observe, keys, changed) = {
            let mut map = self.firing.lock().unwrap_or_else(|e| e.into_inner());
            let entry = map.entry(rule).or_default();
            let changed = if firing {
                entry
                    .insert(stream.to_string(), alert.alert_key())
                    .is_none()
            } else {
                entry.remove(stream).is_some()
            };
            let keys: Vec<String> = entry.values().cloned().collect();
            (firing, keys, changed)
        };
        if !changed && !observe {
            return; // nothing was firing, nothing to resolve
        }
        if observe {
            // Re-observe every tick while firing (refreshes the debounce /
            // timestamp); reconcile below keeps the set consistent.
            if let Err(e) = self.reporter.observe(alert, None).await {
                tracing::warn!(error = %e, rule = %rule, "failed to publish alert");
            }
        }
        if let Err(e) = self.reporter.reconcile(rule, &keys).await {
            tracing::warn!(error = %e, rule = %rule, "failed to reconcile alerts");
        }
    }
}

/// Watch the catalogue's V4L2 cameras: re-enumerate every `interval` and
/// drive the `camera_disappeared` rule. Exits immediately when the catalogue
/// advertises no local cameras.
pub async fn watch_cameras(catalog: Arc<Catalog>, alerts: Arc<ParallaxAlerts>, interval: Duration) {
    let known: Vec<(String, String)> = catalog
        .entries()
        .iter()
        .filter_map(|e| match &e.kind {
            SourceKind::V4l2 { device } => Some((e.name.clone(), device.clone())),
            _ => None,
        })
        .collect();
    if known.is_empty() {
        return;
    }
    tracing::info!(cameras = known.len(), "camera-presence watcher running");

    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        let present: HashSet<String> = match parallax::elements::device::enumerate_video_devices() {
            Ok(devices) => devices.into_iter().map(|d| d.id).collect(),
            Err(e) => {
                tracing::warn!(error = %e, "camera re-enumeration failed; skipping tick");
                continue;
            }
        };
        for (stream, device) in &known {
            alerts
                .camera_present(stream, device, present.contains(device))
                .await;
        }
    }
}
