//! Parallax alert rules on `state/parallax/alert/*` (mirrors the other sensors'
//! firing/resolved lifecycle via [`AlertReporter`]):
//!
//! - `camera_disappeared` — a V4L2 device the catalogue advertises vanished
//!   from re-enumeration (unplugged / claimed by another driver).
//! - `rtsp_connect_failed` — an `open_stream` couldn't reach the camera.
//! - `encoder_overrun` — p95 encode time above the per-frame budget (the
//!   encoder cannot keep up with the live source).
//! - `stream_degraded` — the graph is shedding at the `AppSink`: more of what
//!   was produced was thrown away than the interval budget allows.
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
const RULE_DEGRADED: &str = "stream_degraded";

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

    /// An RTSP camera is unreachable (fires) or is reachable again (resolves).
    ///
    /// Two callers, one meaning — *this camera is not delivering*:
    ///
    /// - the initial `connect()` failed, so the stream never opened;
    /// - a stream that had opened dropped and the source's reconnect ladder ran
    ///   out (#731). Since parallax 0.8 the RTSP source retries a dropped
    ///   stream itself, with exponential backoff and jitter, so a single drop
    ///   no longer reaches here — only a *sustained* one does, which is what
    ///   this rule was always named for.
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
    /// each interval).
    ///
    /// **Judged on the tail, not the mean** (#729). Overrun is a tail
    /// phenomenon — a stream whose *average* frame fits the budget while its
    /// p95 does not is precisely the one that stutters — and "overrun" is what
    /// this rule is named for. `p95_ms` is `None` for a path with no
    /// `EncoderStatsHandle` to ask (the JPEG previews, which `TimedElement`
    /// times but parallax does not histogram); there the interval mean is the
    /// only figure there is and the caller judges on it instead.
    ///
    /// The histogram behind `p95_ms` is all-time for the encoder incarnation,
    /// so the rule clears more slowly than a windowed one would: a bad patch
    /// stays in the distribution until later frames dilute it, or until the
    /// tier is torn down and rebuilt.
    pub async fn encoder_overrun(
        &self,
        stream: &str,
        p95_ms: Option<f64>,
        mean_ms: f64,
        budget_ms: f64,
        firing: bool,
    ) {
        let summary = match p95_ms {
            Some(p95) => format!(
                "stream {stream}: p95 encode {p95:.1} ms/frame exceeds the \
                 {budget_ms:.1} ms budget (interval mean {mean_ms:.1} ms)"
            ),
            None => format!(
                "stream {stream}: encoding {mean_ms:.1} ms/frame exceeds the {budget_ms:.1} ms budget"
            ),
        };
        let alert = Alert::new(
            &self.source,
            Protocol::Parallax,
            AlertKind::SensorHealth,
            RULE_OVERRUN,
            AlertSeverity::Warning,
            summary,
        )
        .with_label("stream", stream);
        self.set(RULE_OVERRUN, stream, alert, firing).await;
    }

    /// The graph is shedding: buffers reached a profile's `AppSink` faster
    /// than the egress task pulled them, so `drop_on_full` discarded them
    /// (#692).
    ///
    /// `proportion` is parallax's own QoS quantity,
    /// `(processed + dropped) / processed` — computed from `AppSinkStats`
    /// rather than received as an `Event::Qos`, because no graph this sensor
    /// builds can originate one (`docs/qos-and-latency.md`).
    ///
    /// **Windowed** over one stats interval, unlike `encoder_overrun`'s
    /// all-time tail. That is not a preference: a cumulative ratio could never
    /// clear — one bad thirty seconds at open would hold the alert firing for
    /// the rest of the stream's life — and resolve-on-recovery is this
    /// reporter's whole contract. A tail needs history; a rate has a natural
    /// window.
    ///
    /// Sits *beside* `encoder_overrun`, not instead of it. They catch
    /// different failures and point at different fixes: overrun says the
    /// encoder cannot finish a frame inside its budget (drop a tier, lower the
    /// resolution) and fires **before** anything is lost; this says frames
    /// were produced fine and then thrown away downstream (the host is
    /// starved, the publisher is stalled, too many profiles share one
    /// runtime). Either can fire alone — a slow encoder that is also a slow
    /// *source* queues nothing and sheds nothing.
    pub async fn stream_degraded(
        &self,
        stream: &str,
        proportion: f64,
        shed: u64,
        published: u64,
        firing: bool,
    ) {
        let summary = format!(
            "stream {stream}: the pipeline shed {shed} of {} frames this interval \
             (QoS proportion {proportion:.2}) — egress cannot keep up with the graph",
            shed + published
        );
        let alert = Alert::new(
            &self.source,
            Protocol::Parallax,
            AlertKind::SensorHealth,
            RULE_DEGRADED,
            AlertSeverity::Warning,
            summary,
        )
        .with_label("stream", stream);
        self.set(RULE_DEGRADED, stream, alert, firing).await;
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
