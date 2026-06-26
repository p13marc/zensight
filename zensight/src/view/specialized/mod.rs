//! Specialized protocol-specific views for ZenSight.
//!
//! Each protocol gets a tailored interface that highlights the most relevant
//! metrics and provides domain-appropriate visualizations.

pub mod fetch;
pub mod gnmi;
pub mod modbus;
pub mod netflow;
pub mod netlink;
pub mod netlink_detail;
pub mod netring;
pub mod netring_detail;
pub mod snmp;
pub mod sysinfo;
pub mod sysinfo_detail;
pub mod syslog;

use iced::{Element, Length};

use zensight_common::Protocol;

use crate::message::Message;
use crate::view::components::Sparkline;
use crate::view::device::DeviceDetailState;

pub use syslog::{
    SyslogFilterState, SyslogMessage, logs_view, syslog_event_view, syslog_message_from_point,
};

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
    let mut alert = button(text("alert").size(10)).padding([2, 8]);
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
pub fn specialized_view<'a>(state: &'a DeviceDetailState) -> Option<Element<'a, Message>> {
    match state.device_id.protocol {
        Protocol::Snmp => Some(snmp::snmp_device_view(state)),
        Protocol::Sysinfo => Some(sysinfo::sysinfo_host_view(state)),
        Protocol::Syslog => None, // Syslog needs filter state, handled separately
        Protocol::Modbus => Some(modbus::modbus_plc_view(state)),
        Protocol::Netflow => Some(netflow::netflow_traffic_view(state)),
        Protocol::Gnmi => Some(gnmi::gnmi_streaming_view(state)),
        Protocol::Opcua => None, // No specialized view yet, use generic
        Protocol::Netlink => Some(netlink::netlink_host_view(state)),
        Protocol::Netring => Some(netring::netring_sensor_view(state)),
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

/// Check if a protocol has a specialized view available.
pub fn has_specialized_view(protocol: Protocol) -> bool {
    !matches!(protocol, Protocol::Opcua)
}
