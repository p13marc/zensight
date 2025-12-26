//! SVG icons for the ZenSight UI.
//!
//! All icons are embedded at compile time using `include_bytes!`.
//!
//! Two types of icons are available:
//! - Static icons: Standard SVG icons for general use
//! - Animated icons: SVG icons that animate color on hover (for use in buttons)

use iced::widget::svg::Handle;
use iced::{Element, Length};

// Use iced_anim's animated SVG for hover effects
use iced_anim::widget::svg::Svg as AnimatedSvg;
// Keep standard SVG for static icons
use iced::widget::svg::Svg;

/// Icon size presets.
#[derive(Debug, Clone, Copy, Default)]
pub enum IconSize {
    /// Small icon (12px)
    Small,
    /// Medium icon (16px) - default
    #[default]
    Medium,
    /// Large icon (20px)
    Large,
    /// Extra large icon (24px)
    XLarge,
}

impl IconSize {
    fn pixels(self) -> f32 {
        match self {
            IconSize::Small => 12.0,
            IconSize::Medium => 16.0,
            IconSize::Large => 20.0,
            IconSize::XLarge => 24.0,
        }
    }
}

/// Create an SVG element from raw bytes.
fn svg_icon<Message: 'static>(data: &'static [u8], size: IconSize) -> Element<'static, Message> {
    let handle = Handle::from_memory(data);
    Svg::new(handle)
        .width(Length::Fixed(size.pixels()))
        .height(Length::Fixed(size.pixels()))
        .into()
}

/// Create an animated SVG element from raw bytes.
/// Animated icons smoothly transition colors on hover - ideal for use in buttons.
fn animated_svg_icon<Message: 'static>(
    data: &'static [u8],
    size: IconSize,
) -> Element<'static, Message> {
    let handle = Handle::from_memory(data);
    AnimatedSvg::new(handle)
        .width(Length::Fixed(size.pixels()))
        .height(Length::Fixed(size.pixels()))
        .into()
}

// ============================================================================
// Navigation Icons
// ============================================================================

/// Left arrow (back navigation).
pub fn arrow_left<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("arrow-left.svg"), size)
}

/// Up arrow (trend up - green).
pub fn arrow_up<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("arrow-up.svg"), size)
}

/// Down arrow (trend down - red).
pub fn arrow_down<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("arrow-down.svg"), size)
}

/// Right arrow (collapsed indicator).
pub fn arrow_right<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("arrow-right.svg"), size)
}

/// Stable indicator (horizontal line - gray).
pub fn arrow_stable<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("arrow-stable.svg"), size)
}

// ============================================================================
// Status Icons
// ============================================================================

/// Healthy status indicator (green dot).
pub fn status_healthy<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("status-healthy.svg"), size)
}

/// Warning status indicator (amber dot).
pub fn status_warning<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("status-warning.svg"), size)
}

/// Error status indicator (red dot).
pub fn status_error<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("status-error.svg"), size)
}

// ============================================================================
// Action Icons
// ============================================================================

/// Close/X icon.
pub fn close<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("close.svg"), size)
}

/// Settings/gear icon (animated for smooth hover transitions).
pub fn settings<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    animated_svg_icon(include_bytes!("settings.svg"), size)
}

/// Alert/warning triangle icon (animated for smooth hover transitions).
pub fn alert<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    animated_svg_icon(include_bytes!("alert.svg"), size)
}

/// Info icon (circle with "i").
pub fn info<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("info.svg"), size)
}

/// Chart/graph icon.
pub fn chart<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("chart.svg"), size)
}

/// Export/download icon.
pub fn export<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("export.svg"), size)
}

/// Edit/pencil icon (animated for smooth hover transitions).
pub fn edit<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    animated_svg_icon(include_bytes!("edit.svg"), size)
}

/// Checkmark icon (green, animated for smooth hover transitions).
pub fn check<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    animated_svg_icon(include_bytes!("check.svg"), size)
}

/// Trash/delete icon (animated for smooth hover transitions).
pub fn trash<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    animated_svg_icon(include_bytes!("trash.svg"), size)
}

/// Search/magnifying glass icon.
pub fn search<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("search.svg"), size)
}

// ============================================================================
// Connection Icons
// ============================================================================

/// Connected/wifi icon (green).
pub fn connected<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("connected.svg"), size)
}

/// Disconnected icon (red with slash).
pub fn disconnected<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("disconnected.svg"), size)
}

// ============================================================================
// Protocol Icons
// ============================================================================

/// SNMP protocol icon (blue monitor).
pub fn protocol_snmp<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("protocol-snmp.svg"), size)
}

/// Syslog protocol icon (purple document).
pub fn protocol_syslog<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("protocol-syslog.svg"), size)
}

/// NetFlow protocol icon (cyan flow arrow).
pub fn protocol_netflow<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("protocol-netflow.svg"), size)
}

/// Modbus protocol icon (orange chip).
pub fn protocol_modbus<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("protocol-modbus.svg"), size)
}

/// Sysinfo protocol icon (green computer).
pub fn protocol_sysinfo<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("protocol-sysinfo.svg"), size)
}

/// gNMI protocol icon (pink layers).
pub fn protocol_gnmi<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("protocol-gnmi.svg"), size)
}

/// OPC UA protocol icon (cyan sun).
pub fn protocol_opcua<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("protocol-opcua.svg"), size)
}

/// Generic protocol icon (gray info circle).
pub fn protocol_generic<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("protocol-generic.svg"), size)
}

/// Get protocol icon by protocol type.
pub fn protocol_icon<Message: 'static>(
    protocol: zensight_common::Protocol,
    size: IconSize,
) -> Element<'static, Message> {
    match protocol {
        zensight_common::Protocol::Snmp => protocol_snmp(size),
        zensight_common::Protocol::Syslog => protocol_syslog(size),
        zensight_common::Protocol::Netflow => protocol_netflow(size),
        zensight_common::Protocol::Modbus => protocol_modbus(size),
        zensight_common::Protocol::Sysinfo => protocol_sysinfo(size),
        zensight_common::Protocol::Gnmi => protocol_gnmi(size),
        zensight_common::Protocol::Opcua => protocol_opcua(size),
    }
}

// ============================================================================
// Theme Icons
// ============================================================================

/// Sun icon (light theme indicator, animated for smooth hover transitions).
pub fn sun<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    animated_svg_icon(include_bytes!("sun.svg"), size)
}

/// Moon icon (dark theme indicator, animated for smooth hover transitions).
pub fn moon<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    animated_svg_icon(include_bytes!("moon.svg"), size)
}

// ============================================================================
// Specialized View Icons
// ============================================================================

/// CPU/processor icon.
pub fn cpu<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("cpu.svg"), size)
}

/// Memory/RAM icon.
pub fn memory<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("memory.svg"), size)
}

/// Disk/storage icon.
pub fn disk<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("disk.svg"), size)
}

/// Network/topology icon (animated for smooth hover transitions).
pub fn network<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    animated_svg_icon(include_bytes!("network.svg"), size)
}

/// Log/document icon.
pub fn log<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("log.svg"), size)
}

/// Toggle/switch icon.
pub fn toggle<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("toggle.svg"), size)
}

/// Table/grid icon.
pub fn table<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("table.svg"), size)
}

/// Protocol/globe icon.
pub fn protocol<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("protocol.svg"), size)
}

/// Subscription/rss icon.
pub fn subscription<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("subscription.svg"), size)
}

/// Tree/hierarchy icon.
pub fn tree<Message: 'static>(size: IconSize) -> Element<'static, Message> {
    svg_icon(include_bytes!("tree.svg"), size)
}
