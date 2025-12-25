use zensight_common::{Protocol, TelemetryPoint};

use crate::view::alerts::{ComparisonOp, Severity};
use crate::view::chart::TimeWindow;
use crate::view::settings::ZenohMode;

/// Messages for the ZenSight application.
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

    /// User changed device search filter.
    SetDeviceSearchFilter(String),

    /// Go to next page in dashboard.
    NextPage,

    /// Go to previous page in dashboard.
    PrevPage,

    /// Go to a specific page in dashboard.
    GoToPage(usize),

    /// User selected a metric to graph (single-series mode).
    SelectMetricForChart(String),

    /// User cleared the chart selection.
    ClearChartSelection,

    /// Add a metric to the comparison chart (multi-series mode).
    AddMetricToChart(String),

    /// Remove a metric from the comparison chart.
    RemoveMetricFromChart(String),

    /// Toggle visibility of a metric series in the chart.
    ToggleMetricVisibility(String),

    /// User changed the chart time window.
    SetChartTimeWindow(TimeWindow),

    /// Zoom in on the chart.
    ChartZoomIn,

    /// Zoom out on the chart.
    ChartZoomOut,

    /// Reset chart zoom to 100%.
    ChartZoomReset,

    /// Pan chart left (back in time).
    ChartPanLeft,

    /// Pan chart right (forward in time).
    ChartPanRight,

    /// Reset chart pan to view current time.
    ChartPanReset,

    /// Start chart drag at position.
    ChartDragStart(f32),

    /// Update chart drag to position.
    ChartDragUpdate(f32, f32),

    /// End chart drag.
    ChartDragEnd,

    /// User changed the metric search filter.
    SetMetricFilter(String),

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

    /// Set max metric history per device.
    SetMaxHistory(String),

    /// Set max alerts to keep.
    SetMaxAlerts(String),

    /// Save settings.
    SaveSettings,

    /// Reset settings to defaults.
    ResetSettings,

    // Alert messages
    /// Open the alerts view.
    OpenAlerts,

    /// Close the alerts view.
    CloseAlerts,

    /// Set new rule name.
    SetAlertRuleName(String),

    /// Set new rule metric pattern.
    SetAlertRuleMetric(String),

    /// Set new rule threshold.
    SetAlertRuleThreshold(String),

    /// Set new rule operator.
    SetAlertRuleOperator(ComparisonOp),

    /// Set new rule severity.
    SetAlertRuleSeverity(Severity),

    /// Add a new alert rule.
    AddAlertRule,

    /// Test the current rule form against existing metrics.
    TestAlertRule,

    /// Remove an alert rule.
    RemoveAlertRule(u32),

    /// Toggle an alert rule's enabled state.
    ToggleAlertRule(u32),

    /// Acknowledge an alert.
    AcknowledgeAlert(u64),

    /// Acknowledge all alerts.
    AcknowledgeAllAlerts,

    /// Clear all alerts.
    ClearAlerts,

    // Export messages
    /// Export device metrics to CSV.
    ExportToCsv,

    /// Export device metrics to JSON.
    ExportToJson,

    // Theme messages
    /// Toggle between light and dark theme.
    ToggleTheme,

    // Keyboard shortcut messages
    /// Focus the search input (Ctrl+F).
    FocusSearch,

    /// Escape key pressed - close dialogs, clear selection, etc.
    EscapePressed,
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
