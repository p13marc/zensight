//! Keep the catalogue live as cameras are plugged and unplugged (#410).
//!
//! Enumeration is scan-once — that is upstream's design and it is right, since
//! the initial catalogue has to exist before the sensor advertises anything.
//! This is the other half: a `udev` monitor on the `video4linux` subsystem,
//! folding add/remove events into the catalogue, the per-stream liveliness
//! token, the health device count and the `camera_disappeared` rule.
//!
//! **What it replaced.** The previous watcher re-enumerated all 64 `/dev/video*`
//! nodes every 30 seconds and compared them against a list snapshotted at
//! startup. That made a *disappearance* visible after up to half a minute, and
//! an *appearance* not visible at all: a camera plugged into a running sensor
//! could never enter a list captured before it existed. The rule it drove is
//! kept; only the polling is gone.
//!
//! **Two upstream behaviours this must absorb rather than treat as bugs.**
//!
//! A `Removed` can name a device that never produced an `Added`. Removal cannot
//! be capability-checked — the device is already gone — so udev reports every
//! `/dev/video*` node that vanished, including the metadata-only second node a
//! UVC camera exposes beside its capture node. `Catalog::remove_v4l2` returning
//! `None` is therefore the ordinary case, not a warning.
//!
//! Device ids are namespaced per backend, so with libcamera folding enabled one
//! physical camera produces two `Added` events. This sensor deliberately does
//! not build libcamera, but the backend is checked anyway: a filter that only
//! works because a feature happens to be off is a filter that breaks when
//! somebody turns the feature on.
//!
//! **What happens to a stream that was open when its camera was pulled.**
//! Nothing here, on purpose. Removing the entry stops anything *new* from
//! opening it — `SessionManager::open` looks the stream up and refuses what it
//! cannot find — while the running pipeline fails on its own next read and
//! tears down through the existing `EgressEnded` path, which already reports
//! why. Forcing a teardown from here would mean sending `CloseStream`, and that
//! is refcount-based: it would decrement a viewer's reference, not end the
//! stream.

use std::sync::Arc;

use parallax::elements::device::{CaptureBackend, DeviceEvent, DeviceMonitor};
use zensight_sensor_core::LivelinessManager;

use crate::alerts::ParallaxAlerts;
use crate::catalog::Catalog;

/// Watch `video4linux` hotplug events until the sensor stops.
///
/// Returns immediately when the udev monitor cannot be created — no udev, or no
/// permission to read its socket. That is a degradation, not a failure: the
/// catalogue keeps whatever startup enumeration found, which is exactly the
/// behaviour every build had before this existed. It is logged at `warn` with
/// the reason, because a sensor silently not doing the thing its docs say it
/// does is the failure mode worth avoiding.
pub async fn run(
    catalog: Arc<Catalog>,
    alerts: Arc<ParallaxAlerts>,
    liveliness: Option<Arc<LivelinessManager>>,
    health: Arc<zensight_sensor_core::SensorHealth>,
) {
    let mut monitor = match DeviceMonitor::new() {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "camera hotplug unavailable; the catalogue stays as startup enumeration \
                 found it and a camera plugged in later needs a restart"
            );
            return;
        }
    };
    tracing::info!(
        cameras = catalog.v4l2_devices().len(),
        "camera hotplug watcher running (udev, video4linux)"
    );

    while let Some(event) = monitor.recv().await {
        match event {
            DeviceEvent::Added(device) => {
                if device.backend != CaptureBackend::V4l2 {
                    tracing::debug!(id = %device.id, backend = ?device.backend,
                        "hotplug: ignoring a non-V4L2 backend's view of a device");
                    continue;
                }
                let description = crate::catalog::device_description(&device.name, &device.model);
                let Some(stream) = catalog.add_v4l2(&device.id, Some(description)) else {
                    // udev re-announces devices that are already present, and a
                    // freed /dev/video* path can be handed to a different
                    // camera. Both land here and both are ordinary.
                    tracing::debug!(id = %device.id,
                        "hotplug: device already advertised, or its stream name is taken");
                    continue;
                };
                if let Some(liveliness) = &liveliness
                    && let Err(e) = liveliness.declare_device_alive(&stream).await
                {
                    tracing::warn!(stream = %stream, error = %e,
                        "hotplug: failed to declare stream liveliness");
                }
                health.set_devices_total(catalog.len() as u64);
                // The rule is resolved as well as fired: a camera that comes
                // back must clear the alert it raised when it left, or an
                // operator who fixed a loose cable keeps the page.
                alerts.camera_present(&stream, &device.id, true).await;
                tracing::info!(stream = %stream, device = %device.id,
                    streams = catalog.len(), "camera appeared");
            }
            DeviceEvent::Removed { id } => {
                let Some(stream) = catalog.remove_v4l2(&id) else {
                    tracing::debug!(device = %id,
                        "hotplug: removal of a device this sensor never advertised");
                    continue;
                };
                alerts.camera_present(&stream, &id, false).await;
                if let Some(liveliness) = &liveliness {
                    liveliness.undeclare_device(&stream).await;
                }
                health.set_devices_total(catalog.len() as u64);
                tracing::warn!(stream = %stream, device = %id,
                    streams = catalog.len(), "camera disappeared");
            }
        }
    }
    tracing::warn!("camera hotplug watcher ended");
}
