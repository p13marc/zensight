//! Specialized protocol-specific views for ZenSight.
//!
//! Each protocol gets a tailored interface that highlights the most relevant
//! metrics and provides domain-appropriate visualizations.

pub mod attribution;
pub mod fetch;
pub mod gnmi;
pub mod modbus;
pub mod netflow;
pub mod netlink;
pub mod netlink_detail;
pub mod netring;
pub mod netring_detail;
pub mod parallax;
pub mod parallax_detail;
pub mod parallax_h264;
pub mod parallax_health;
pub mod parallax_receiver;
pub mod parallax_tier;
pub mod snmp;
pub mod sysinfo;
pub mod syslog;
pub mod systemd;
pub mod systemd_detail;

use iced::{Element, Length};

/// How a finished write reads in a toast (#1261): the producer's own
/// phrasing when its view has one (systemd's job results, snmp's outlet
/// outcome), else the generic reading of the reply's `accepted`, `error`,
/// `reason` and `result` fields.
pub fn write_outcome(
    producer: &str,
    armed: &crate::call::Armed,
    result: &Result<crate::call::Reply, crate::call::WriteFailure>,
) -> (crate::view::toast::ToastSeverity, String) {
    match producer {
        "systemd" => systemd::write_outcome(armed, result),
        "snmp" => snmp::write_outcome(armed, result),
        _ => generic_write_outcome(armed, result),
    }
}

/// The reply fields every write outcome may carry, read without a type.
fn generic_write_outcome(
    armed: &crate::call::Armed,
    result: &Result<crate::call::Reply, crate::call::WriteFailure>,
) -> (crate::view::toast::ToastSeverity, String) {
    use crate::view::toast::ToastSeverity;
    let label = &armed.label;
    match result {
        Ok(reply) => {
            let detail = ["error", "reason", "result"]
                .iter()
                .find_map(|k| reply.value.get(*k).and_then(|v| v.as_str()))
                .map(str::to_string);
            match reply.value.get("accepted").and_then(|v| v.as_bool()) {
                Some(false) => (
                    ToastSeverity::Error,
                    format!(
                        "{label} refused: {}",
                        detail.unwrap_or_else(|| "no reason given".into())
                    ),
                ),
                _ => (
                    ToastSeverity::Success,
                    format!("{label}: {}", detail.unwrap_or_else(|| "done".into())),
                ),
            }
        }
        Err(failure) => (
            if failure.is_warning() {
                ToastSeverity::Warning
            } else {
                ToastSeverity::Error
            },
            format!("{label}: {}", failure.sentence()),
        ),
    }
}

/// What to re-call after a write (#1261): the procedures whose answer the
/// write may have moved, as the producer's view knows them. Refreshing
/// immediately rather than sleeping first: a value one poll stale resolves
/// visibly, and a sleep here is the anti-pattern this replaced.
pub fn after_write(
    producer: &str,
    state: &DeviceDetailState,
    armed: &crate::call::Armed,
    result: &Result<crate::call::Reply, crate::call::WriteFailure>,
) -> Vec<(String, String)> {
    match producer {
        "systemd" => systemd::after_write(state, armed, result),
        _ => Vec::new(),
    }
}

use zensight_common::Protocol;

use crate::message::Message;
use crate::view::components::Sparkline;
use crate::view::device::DeviceDetailState;
use crate::view::tokens::font;

pub use syslog::{
    LogExport, SyslogFilterState, SyslogMessage, log_bundle_kind_from_filter, logs_view,
    syslog_event_view, syslog_message_from_point,
};

/// The active tab of a tabbed specialized view (#243, epic #257). Currently
/// carries the netring tab set (the netlink redesign #270 will extend this).
/// Stored per device in [`DeviceDetailState`] so each sensor screen remembers
/// the last tab you looked at. Shared across the netring and netlink tabbed
/// views (#257, #270); `Overview` is common, the rest are per-protocol. A view
/// falls back to `Overview` if the remembered tab isn't one of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SpecializedTab {
    #[default]
    Overview,
    // netring
    Flows,
    TalkersMatrix,
    Dns,
    HttpTls,
    Bandwidth,
    Assets,
    Security,
    Capture,
    // netlink
    Interfaces,
    Sockets,
    RoutingNeighbors,
    Qos,
    FirewallIpsec,
    Events,
    WireGuard,
    // systemd (#281)
    Units,
    Timers,
    Sentinel,
    Cgroups,
    /// Service-control audit timeline (#283). Only rendered on a host that
    /// advertises service control, so it is never a dead tab.
    Actions,
}

/// Number of trailing history samples to render in an inline sparkline (#44).
const SPARKLINE_SAMPLES: usize = 60;

/// An inline trend sparkline for `metric` from the device's history (#44), or a
/// fixed-width spacer when there aren't enough points yet (keeps rows aligned).
/// Reused by the netring/netlink/sysinfo specialized views.
pub fn metric_sparkline<'a>(state: &DeviceDetailState, metric: &str) -> Element<'a, Message> {
    let values = state.history_values(metric, SPARKLINE_SAMPLES);
    if values.len() < 2 {
        return iced::widget::container(iced::widget::text(""))
            .width(Length::Fixed(80.0))
            .height(Length::Fixed(20.0))
            .into();
    }
    Sparkline::new(values).with_size(80.0, 20.0).view()
}

/// The current numeric value of `metric`, if it projects to a number. Booleans
/// project to a 0/1 step value (#126) so flap-prone signals are chartable and
/// promotable alongside counters/gauges.
fn numeric_metric(state: &DeviceDetailState, metric: &str) -> Option<f64> {
    use zensight_common::TelemetryValue;
    match state.metrics.get(metric).map(|p| &p.value) {
        Some(TelemetryValue::Counter(v)) => Some(*v as f64),
        Some(TelemetryValue::Gauge(v)) => Some(*v),
        Some(TelemetryValue::Boolean(b)) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// A trend sparkline plus an "alert" button that promotes this metric to a rule
/// (#50) — makes promotion reachable from the netring/netlink/sysinfo
/// specialized views, which have no generic per-row metrics table.
pub fn metric_trend_and_alert<'a>(state: &DeviceDetailState, metric: &str) -> Element<'a, Message> {
    use iced::widget::{button, row, text};
    let spark = metric_sparkline(state, metric);
    let value = numeric_metric(state, metric);
    let mut alert = button(text("alert").size(font::MICRO)).padding([2, 8]);
    if let Some(value) = value {
        alert = alert.on_press(Message::PromoteMetricToAlert {
            device: state.device_id.clone(),
            metric: metric.to_string(),
            value,
        });
    }
    row![spark, alert]
        .spacing(6)
        .align_y(iced::Alignment::Center)
        .into()
}

/// Select and render the appropriate specialized view based on protocol.
///
/// This function examines the device's protocol and delegates to the
/// protocol-specific view implementation. If the specialized view cannot
/// be rendered (e.g., insufficient data), it returns `None` to indicate
/// the caller should fall back to the generic device view.
///
/// `artifact` threads the app's shared artifact state into views with
/// contextual actions (#351) — today only netring's Capture tab consumes it;
/// `None` renders those views without the in-context controls.
/// `entity` is the resolved `HostEntity` for this device when the catalog has
/// one (#1019). Optional for the same reason `artifact` is: the bare path and
/// the tests have neither, and a view that needs one renders its honest
/// fallback rather than inventing a value.
pub fn specialized_view<'a>(
    state: &'a DeviceDetailState,
    artifact: Option<crate::view::artifact_fetch::ArtifactCtx<'a>>,
    entity: Option<&zensight_common::HostEntity>,
) -> Option<Element<'a, Message>> {
    // A producer outside the closed enum has no bespoke view; the generic
    // body renders it (#1256). So does any enum member without an arm below.
    let protocol = state.device_id.protocol()?;
    match protocol {
        Protocol::Snmp => Some(snmp::snmp_device_view(state)),
        Protocol::Sysinfo => Some(sysinfo::sysinfo_host_view(state, entity)),
        Protocol::Logs => None, // Syslog needs filter state, handled separately
        Protocol::Modbus => Some(modbus::modbus_plc_view(state)),
        Protocol::Netflow => Some(netflow::netflow_traffic_view(state)),
        Protocol::Gnmi => Some(gnmi::gnmi_streaming_view(state)),
        Protocol::Opcua => None, // No specialized view yet, use generic
        Protocol::Netlink => Some(netlink::netlink_host_view(state)),
        Protocol::Netring => Some(netring::netring_sensor_view(state, artifact)),
        Protocol::Systemd => Some(systemd::systemd_host_view(state)),
        Protocol::Parallax => Some(parallax::parallax_view(state)),
        // Everything else renders through `generic_device_view` (#1256): the
        // producer's `views.toml` when one exists (#1259 — bmc, pve and probe
        // are documents since #1260, and their Rust views are gone), the
        // default family renderer otherwise (#1258). A producer that earns a
        // bespoke Rust view takes an arm above; the rest do not need one.
        _ => None,
    }
}

/// Render the syslog specialized view with filter state. `host_logs` is the
/// app's rolling log buffer filtered to this device's host.
pub fn syslog_view<'a>(
    state: &'a DeviceDetailState,
    filter_state: &'a SyslogFilterState,
    host_logs: &[SyslogMessage],
) -> Element<'a, Message> {
    syslog::syslog_event_view(state, filter_state, host_logs)
}
