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
pub mod syslog;

use iced::{Element, Length};

use zensight_common::Protocol;

use crate::message::Message;
use crate::view::components::Sparkline;
use crate::view::device::DeviceDetailState;

pub use syslog::SyslogFilterState;

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

/// Render the syslog specialized view with filter state.
pub fn syslog_view<'a>(
    state: &'a DeviceDetailState,
    filter_state: &'a SyslogFilterState,
) -> Element<'a, Message> {
    syslog::syslog_event_view(state, filter_state)
}

/// Check if a protocol has a specialized view available.
pub fn has_specialized_view(protocol: Protocol) -> bool {
    !matches!(protocol, Protocol::Opcua)
}
