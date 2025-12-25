use zensight_common::{Protocol, TelemetryPoint};

use crate::view::chart::TimeWindow;
use crate::view::settings::ZenohMode;

/// Messages for the Zensight application.
#[derive(Debug, Clone)]
pub enum Message {
    /// Telemetry received from Zenoh subscription.
    TelemetryReceived(TelemetryPoint),

    /// Zenoh connection established.
    Connected,

    /// Zenoh connection lost or failed.
    Disconnected(String),

    /// User selected a device from the dashboard.
    SelectDevice(DeviceId),

    /// User cleared device selection (back to dashboard).
    ClearSelection,

    /// User toggled protocol filter.
    ToggleProtocolFilter(Protocol),

    /// User selected a metric to graph.
    SelectMetricForChart(String),

    /// User cleared the chart selection.
    ClearChartSelection,

    /// User changed the chart time window.
    SetChartTimeWindow(TimeWindow),

    /// Tick for periodic UI updates (e.g., relative timestamps).
    Tick,

    // Settings messages
    /// Open the settings view.
    OpenSettings,

    /// Close the settings view.
    CloseSettings,

    /// Set Zenoh connection mode.
    SetZenohMode(ZenohMode),

    /// Set Zenoh connect endpoints.
    SetZenohConnect(String),

    /// Set Zenoh listen endpoints.
    SetZenohListen(String),

    /// Set stale threshold.
    SetStaleThreshold(String),

    /// Save settings.
    SaveSettings,

    /// Reset settings to defaults.
    ResetSettings,
}

/// Unique identifier for a device (protocol + source name).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceId {
    pub protocol: Protocol,
    pub source: String,
}

impl DeviceId {
    pub fn new(protocol: Protocol, source: impl Into<String>) -> Self {
        Self {
            protocol,
            source: source.into(),
        }
    }

    pub fn from_telemetry(point: &TelemetryPoint) -> Self {
        Self {
            protocol: point.protocol,
            source: point.source.clone(),
        }
    }
}

impl std::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.protocol, self.source)
    }
}
