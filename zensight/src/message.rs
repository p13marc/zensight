use zensight_common::{
    Alert, CorrelationEntry, DeviceLiveness, DeviceStatus, ErrorReport, HealthSnapshot, Protocol,
    SensorInfo, TelemetryPoint,
};

use crate::view::alerts::{ComparisonOp, Severity};
use crate::view::chart::TimeWindow;
use crate::view::settings::ZenohMode;

/// Messages for the ZenSight application.
#[derive(Debug, Clone)]
pub enum Message {
    /// Telemetry received from Zenoh subscription.
    TelemetryReceived(TelemetryPoint),

    /// A periodic off-thread store flush finished. Payload is the number of
    /// downsampled buckets persisted (or `Err` with a message on failure). #22.
    StoreFlushed(Result<usize, String>),

    /// Off-thread history pre-load for a device finished (#22): metric name ->
    /// merged (warm/cold) samples to seed the device detail chart on open.
    DeviceHistoryLoaded(DeviceId, Vec<(String, Vec<crate::store::Sample>)>),

    /// Off-thread log cold-store search-back finished (#107, C9): persisted log
    /// records (newest-first) to merge into the rolling buffer on Logs-view open.
    LogHistoryLoaded(Vec<crate::store::StoredLog>),

    /// Sensor health snapshot received.
    HealthSnapshotReceived(HealthSnapshot),

    /// Device liveness update received.
    DeviceLivenessReceived(String, DeviceLiveness),

    /// Sensor error report received (with the publishing sensor/protocol name).
    ErrorReportReceived(String, ErrorReport),

    /// Sensor discovery/info received.
    SensorInfoReceived(SensorInfo),

    /// Correlation entry received.
    CorrelationReceived(CorrelationEntry),

    /// A sensor-emitted alert was received (firing or resolved). Published on
    /// `zensight/<protocol>/@/alerts/<alert_key>`.
    AlertReceived(Alert),

    /// A sensor alert key was deleted (resolve tombstone).
    AlertCleared {
        protocol: String,
        alert_key: String,
    },

    /// Seed of currently-firing alerts fetched on connect from sensors'
    /// `@/query/alerts` queryables (late-joiner recovery — populates without
    /// toasting, since these aren't newly-fired).
    AlertsSeed(Vec<Alert>),

    /// Zenoh connection attempt started.
    Connecting,

    /// Zenoh connection established. Carries the session handle so the app can
    /// send commands back to sensors (`None` in demo mode — no real session).
    Connected(Option<std::sync::Arc<zenoh::Session>>),

    /// Zenoh connection lost or failed.
    Disconnected(String),

    /// Result of a command sent to a sensor (drives a feedback toast).
    CommandFeedback {
        success: bool,
        message: String,
    },

    // ── Expectations authoring (netlink sentinel, Plan 08) ──────────────────
    /// Open the expectations authoring view.
    OpenExpectations,
    /// Close the expectations view.
    CloseExpectations,
    /// Select the sentinel target being authored (netlink vs systemd) (#278).
    SetExpTarget(crate::view::expectations::ExpTarget),
    /// Set the systemd expectation kind being authored (#278).
    SetSystemdExpKind(crate::view::expectations::SystemdExpKind),
    /// A systemd sentinel status reply (ExpectationsConfig JSON) (#278).
    SystemdExpectationsReceived(String),
    /// Set the kind of expectation being authored.
    SetExpectationKind(crate::view::expectations::ExpKind),
    /// Set the expectation name (socket) or interface (link).
    SetExpectationName(String),
    /// Set the expectation port.
    SetExpectationPort(String),
    /// Set the expectation severity.
    SetExpectationSeverity(crate::view::alerts::Severity),
    /// Set the metric path (metric-threshold expectation).
    SetExpectationMetric(String),
    /// Set the comparison operator (metric-threshold expectation).
    SetExpectationOp(ComparisonOp),
    /// Set the threshold value (metric-threshold expectation).
    SetExpectationValue(String),
    /// Build + push the authored expectation to the sentinel.
    AddExpectation,
    /// Remove an expectation by rule slug.
    RemoveExpectation(String),
    /// Query the sentinel's current expectation set.
    RefreshExpectations,
    /// A sentinel status reply (ExpectationsConfig JSON).
    ExpectationStatusReceived(String),

    // Netring detection-tuning (#121): runtime allowlist + per-detector mute /
    // threshold, pushed to the netring sensor's command channel.
    /// Fetch the netring detector config (status queryable).
    RefreshDetectorConfig,
    /// A netring detector-status reply (AnomalyConfig JSON), or an error.
    DetectorConfigReceived(Result<String, String>),
    /// Mute/unmute a netring detector by name (flips current state).
    ToggleNetringDetector(String),
    /// Edit a detector's threshold input field (not yet applied).
    SetNetringThresholdInput {
        detector: String,
        value: String,
    },
    /// Apply the edited threshold for a detector to the sensor.
    ApplyNetringThreshold(String),
    /// Edit the new-allowlist-entry input field.
    SetNetringAllowlistInput(String),
    /// Add the typed allowlist entry to the netring allowlist.
    AddNetringAllowlist,
    /// Remove an allowlist entry from the netring allowlist.
    RemoveNetringAllowlist(String),
    /// Add a specific host/SLD to the netring allowlist (#120) — used by the
    /// inventory fingerprint explorer's per-row allowlist action.
    AddNetringAllowlistEntry(String),

    // Netring capture-focus (#225/#228): hot-swap the reloadable packet-tier
    // BPF filter live, narrowing capture attention during an incident.
    /// Edit the capture-focus filter expression input (not yet applied).
    SetPacketFilterInput(String),
    /// Apply the typed capture-focus filter to the netring sensor.
    ApplyPacketFilter,
    /// Clear the capture-focus filter back to the configured base.
    ClearPacketFilter,
    /// A capture-filter status reply (`CaptureFilterStatus` JSON), or an error.
    CaptureFilterStatusReceived(Result<String, String>),

    /// Open the unified Incidents triage view (#129).
    OpenIncidents,
    /// Expand/collapse an incident by id (`None` collapses) (#129).
    SelectIncident(Option<String>),

    /// Open the first-class inventory view and (re)fetch assets + fingerprints (#120).
    OpenInventory,
    /// Combined inventory fetch outcome (assets + TLS/QUIC/SSH fingerprints).
    InventoryLoaded(Result<crate::view::inventory::InventoryData, String>),
    /// Set the inventory asset-table sort order.
    SetInventoryAssetSort(crate::view::inventory::AssetSort),
    /// Set the fingerprint-explorer kind filter (`None` = all kinds).
    SetInventoryFpFilter(Option<crate::view::inventory::FpKind>),

    /// Fetch an on-demand systemd detail channel (units/timers/events/cgroups) (#281).
    FetchSystemdDetail(crate::view::specialized::systemd_detail::SystemdDetailTopic),
    /// A systemd detail reply for a topic: the decoded payload, or an error message.
    SystemdDetailReceived(
        crate::view::specialized::systemd_detail::SystemdDetailTopic,
        Result<crate::view::specialized::systemd_detail::SystemdDetailData, String>,
    ),
    /// Units table (#281): set the active-state filter (`None` = all).
    SystemdSetUnitFilter(Option<String>),

    /// Fetch an on-demand netlink detail table (sockets/routes/neighbors).
    FetchNetlinkDetail(crate::view::specialized::netlink_detail::NetlinkDetailTopic),
    /// A netlink detail reply for a topic: the decoded table, or an error message.
    NetlinkDetailReceived(
        crate::view::specialized::netlink_detail::NetlinkDetailTopic,
        Result<crate::view::specialized::netlink_detail::NetlinkDetailData, String>,
    ),
    /// Socket explorer (#112): set the TCP-state filter (`None` = all states).
    SetNetlinkSocketStateFilter(Option<String>),
    /// Socket explorer (#112): set the port substring filter.
    SetNetlinkSocketPortFilter(String),
    /// Socket explorer (#112): set the sort order.
    SetNetlinkSocketSort(crate::view::specialized::netlink_detail::SocketSort),
    /// Socket explorer (#261): reveal another page of socket rows (replaces the
    /// old silent `.take(200)` cutoff).
    NetlinkSocketsMore,
    /// Netlink detail-table (#244): toggle the sort column.
    NetlinkTableSort(
        crate::view::specialized::netlink_detail::NetlinkTable,
        usize,
    ),
    /// Netlink detail-table: set the substring filter.
    NetlinkTableFilter(
        crate::view::specialized::netlink_detail::NetlinkTable,
        String,
    ),
    /// Netlink detail-table: reveal another page of rows.
    NetlinkTableMore(crate::view::specialized::netlink_detail::NetlinkTable),

    /// Select the active tab of a tabbed specialized view (#243). Remembered
    /// per device in `DeviceDetailState`.
    SelectSpecializedTab(DeviceId, crate::view::specialized::SpecializedTab),

    /// Sort a netring data-table by column index (toggles direction) (#244).
    NetringTableSort(
        crate::view::specialized::netring_detail::NetringTable,
        usize,
    ),
    /// Set a netring data-table's substring filter (#244).
    NetringTableFilter(
        crate::view::specialized::netring_detail::NetringTable,
        String,
    ),
    /// Reveal another page of rows in a netring data-table (#244).
    NetringTableMore(crate::view::specialized::netring_detail::NetringTable),

    /// Drill-down pivot (#246): jump to the Flows tab filtered to an endpoint
    /// (talker → flows, asset → flows, matrix cell → flows). Reuses the Flows
    /// data-table filter; fetches flows if not already loaded.
    NetringPivotToFlows(DeviceId, String),

    /// Asset → topology pivot (#252): open the topology view with the node for
    /// this asset selected (resolved via hostname, then the ip→node map). Falls
    /// back to an info toast when the asset has no topology node.
    NetringAssetToTopology {
        ip: String,
        hostname: Option<String>,
    },

    /// Fetch the on-demand netring flow detail (recent flows).
    FetchNetringFlows,
    /// A netring flow-detail reply: the decoded flows, or an error message.
    NetringFlowsReceived(Result<Vec<zensight_common::FlowRecord>, String>),
    /// Netring flows fetched for deriving real topology edges (#25). Distinct
    /// from NetringFlowsReceived so it doesn't disturb the device flow panel.
    TopologyFlowsReceived(Result<Vec<zensight_common::FlowRecord>, String>),
    /// Netlink neighbor (ARP/NDP) table fetched for deriving adjacency edges
    /// (#49). Merged with flow edges so directly-attached gateways/peers appear
    /// even without observed traffic; `is_router` entries classify Router nodes.
    TopologyNeighborsReceived(Result<Vec<zensight_common::NeighborRecord>, String>),
    /// Fetch the on-demand netring TLS asset inventory.
    FetchNetringTls,
    /// A netring TLS-inventory reply: the decoded records, or an error message.
    NetringTlsReceived(Result<Vec<zensight_common::TlsRecord>, String>),
    /// Fetch the on-demand netring QUIC SNI/ALPN inventory (#72).
    FetchNetringQuic,
    /// A netring QUIC-inventory reply: the decoded records, or an error message.
    NetringQuicReceived(Result<Vec<zensight_common::QuicRecord>, String>),
    /// Fetch the on-demand netring SSH/HASSH inventory (#72).
    FetchNetringSsh,
    /// A netring SSH-inventory reply: the decoded records, or an error message.
    NetringSshReceived(Result<Vec<zensight_common::SshRecord>, String>),
    /// Fetch the on-demand netring JA4H HTTP-fingerprint inventory (#256).
    /// Manual only — the queryable exists only on `ja4plus` sensor builds.
    FetchNetringJa4h,
    /// A netring JA4H-inventory reply: the decoded records, or an error message.
    NetringJa4hReceived(Result<Vec<zensight_common::Ja4hRecord>, String>),
    /// Fetch the on-demand netring passive asset inventory (#70).
    FetchNetringAssets,
    /// A netring asset-inventory reply: the decoded records, or an error message.
    NetringAssetsReceived(Result<Vec<zensight_common::AssetRecord>, String>),
    /// Fetch the on-demand netring top-talker histogram (#45).
    FetchNetringTalkers,
    /// A netring top-talker reply: the decoded records, or an error message.
    NetringTalkersReceived(Result<Vec<zensight_common::TalkerRecord>, String>),
    /// Fetch the on-demand netring `(src,dst)` traffic matrix / service map (#122).
    FetchNetringMatrix,
    /// A netring traffic-matrix reply: the decoded records, or an error message.
    NetringMatrixReceived(Result<Vec<zensight_common::MatrixRecord>, String>),
    /// Fetch the on-demand netring elephant-flow ring (#45).
    FetchNetringElephants,
    /// A netring elephant-flow reply: the decoded records, or an error message.
    NetringElephantsReceived(Result<Vec<zensight_common::ElephantRecord>, String>),
    /// Fetch the on-demand netring per-SLD DNS detail (#45).
    FetchNetringDns,
    /// A netring DNS-detail reply: the decoded records, or an error message.
    NetringDnsReceived(Result<Vec<zensight_common::DnsRecord>, String>),
    /// Fetch the on-demand netring per-host HTTP detail (#45).
    FetchNetringHttp,
    /// A netring HTTP-detail reply: the decoded records, or an error message.
    NetringHttpReceived(Result<Vec<zensight_common::HttpHostRecord>, String>),
    /// Fetch the on-demand sysinfo process explorer for the selected host,
    /// sorted as requested (#47).
    FetchSysinfoProcesses(crate::view::specialized::sysinfo_detail::ProcessSort),
    /// A sysinfo process-explorer reply: the decoded records, or an error.
    SysinfoProcessesReceived(Result<Vec<zensight_common::ProcessRecord>, String>),
    /// Pivot from a Security anomaly to its netring flows (#119): fetch
    /// `@/query/flows` and filter to the offending `src`. `key` is the anomaly's
    /// `alert_key` so the result renders under the right row.
    FetchAnomalyFlows {
        key: String,
        src: String,
    },
    /// A flow-pivot reply for anomaly `key`: the filtered flows, or an error.
    AnomalyFlowsReceived(String, Result<Vec<zensight_common::FlowRecord>, String>),

    /// Open the security (network anomalies) view.
    OpenSecurity,
    /// Close the security view.
    CloseSecurity,
    /// Toggle hiding Info-severity anomalies in the Security view (#48).
    ToggleSecurityHideInfo,
    /// Expand/collapse an anomaly's evidence drill-down by alert_key (#48).
    SelectAnomaly(Option<String>),

    /// Sensor came online (liveliness token appeared).
    SensorOnline(String),

    /// Sensor went offline (liveliness token disappeared).
    SensorOffline(String),

    /// Device came online (liveliness token appeared).
    DeviceOnline(String, String),

    /// Device went offline (liveliness token disappeared).
    DeviceOffline(String, String),

    /// User selected a device from the dashboard.
    SelectDevice(DeviceId),

    /// Jump from an alert straight to the offending device, pre-selecting the
    /// metric (if known) so its chart opens immediately (#35 triage loop).
    InvestigateAlert {
        device: DeviceId,
        metric: Option<String>,
    },

    /// Navigate to the previous/next device within the current filtered set
    /// (#35 cross-device navigation on the device detail view).
    SelectAdjacentDevice {
        forward: bool,
    },

    /// User cleared device selection (back to dashboard).
    ClearSelection,

    /// User toggled protocol filter.
    ToggleProtocolFilter(Protocol),

    /// Filter the dashboard to a single device status (None = all), driven by
    /// the fleet summary chips (#34). Clicking the active chip clears it.
    SetStatusFilter(Option<DeviceStatus>),

    /// User changed device search filter.
    SetDeviceSearchFilter(String),

    /// Go to next page in dashboard.
    NextPage,

    /// Go to previous page in dashboard.
    PrevPage,

    /// Go to a specific page in dashboard.
    GoToPage(usize),

    /// Toggle dashboard view mode (grid vs table).
    ToggleDashboardViewMode,

    /// User selected a metric to graph (single-series mode).
    SelectMetricForChart(String),

    /// User cleared the chart selection.
    ClearChartSelection,

    /// Promote a metric to an alert rule (#50): seed the rule/expectation form
    /// with this metric + current value and open the authoring view. Netlink
    /// routes to the sentinel expectations; other protocols to local rules.
    PromoteMetricToAlert {
        device: DeviceId,
        metric: String,
        value: f64,
    },

    /// Add a metric to the comparison chart (multi-series mode).
    AddMetricToChart(String),

    /// Remove a metric from the comparison chart.
    RemoveMetricFromChart(String),

    /// Toggle visibility of a metric series in the chart.
    ToggleMetricVisibility(String),

    /// Toggle a metric's favorite/pin state on the selected device (#27).
    ToggleMetricFavorite(String),

    /// User changed the chart time window.
    SetChartTimeWindow(TimeWindow),

    /// User typed a custom relative window (minutes) for the chart (#36).
    SetChartCustomMinutes(String),

    /// User edited the absolute-range `from`/`to` inputs (#36).
    SetChartRangeFrom(String),
    SetChartRangeTo(String),

    /// Apply the absolute `from`/`to` range — pins the window + loads it from the
    /// store (#36).
    ApplyChartRange,

    /// Clear the absolute range, returning to the preset / custom window (#36).
    ClearChartRange,

    /// Toggle the chart panel between default and expanded height (#36).
    ToggleChartExpand,

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

    /// Navigate to the dashboard (clears any device selection). Used by the
    /// persistent nav rail.
    OpenDashboard,

    /// Open the sensors (sensor health) view.
    OpenSensors,

    /// Open the top-level logs view (unified syslog/journald feed).
    OpenLogs,

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

    /// Acknowledge all firing external (sensor-pushed) alerts from one source.
    AcknowledgeExternalSource(String),
    /// Acknowledge all firing external alerts.
    AcknowledgeAllExternal,

    /// Silence (mute) a source for the given duration in ms (#26).
    SilenceSource(String, i64),

    /// Toggle the opt-in desktop-notifications setting (#26) and persist it.
    ToggleDesktopNotifications,
    /// Lift a silence on a source (#26).
    UnsilenceSource(String),

    /// Filter the external-alerts feed by severity (`None` = all) (#27).
    SetAlertSeverityFilter(Option<zensight_common::AlertSeverity>),
    /// Filter the external-alerts feed by source (`None` = all) (#27).
    SetAlertSourceFilter(Option<String>),
    /// Save the current external-alert filter combination as a preset (#27).
    SaveAlertFilterPreset,
    /// Apply a saved external-alert filter preset by index (#27).
    ApplyAlertFilterPreset(usize),
    /// Delete a saved external-alert filter preset by index (#27).
    DeleteAlertFilterPreset(usize),

    /// Toggle the keyboard-shortcuts help overlay (#28).
    ToggleHelp,

    /// Open the command palette (#28).
    OpenCommandPalette,
    /// Close the command palette (#28).
    CloseCommandPalette,
    /// Update the command-palette query (#28).
    SetCommandPaletteQuery(String),
    /// Run the command at the given index into the current filtered list (#28).
    RunPaletteCommand(usize),

    /// Open the global cross-device metric search panel (#27).
    OpenGlobalSearch,
    /// Close the global search panel (#27).
    CloseGlobalSearch,
    /// Update the global search query (#27).
    SetGlobalSearch(String),

    /// Clear all alerts.
    ClearAlerts,

    // Export messages
    /// Export device metrics to CSV.
    ExportToCsv,

    /// Export device metrics to JSON.
    ExportToJson,

    /// Outcome of an export save dialog (#37): `Ok(Some(path))` wrote the file,
    /// `Ok(None)` the user cancelled the dialog, `Err(msg)` the write failed.
    ExportFinished(Result<Option<String>, String>),

    // Debug-report download messages (#197)
    /// Request + download a debug report from the sensor at this key prefix
    /// (e.g. `zensight/netlink`).
    DownloadDebugReport(String),
    /// The report request resolved: a `Ready` state to download, or an error.
    ReportRequested(Result<zensight_common::report::ReportState, String>),
    /// Streaming download progress (chunks received / total).
    ReportProgress {
        /// Chunks received so far.
        got: u64,
        /// Total chunks.
        total: u64,
    },
    /// The bundle finished downloading + verifying to a temp path, or failed.
    ReportDownloaded(Result<std::path::PathBuf, String>),
    /// Outcome of the "Save as…" dialog for a downloaded report.
    ReportSaved(Result<Option<String>, String>),
    /// Pause the in-flight report download (keeps the partial; resumable).
    PauseDownload,
    /// Resume a paused report download.
    ResumeDownload,
    /// Cancel the in-flight report download (discards the partial).
    CancelDownload,

    // Tier-2 directory-snapshot download messages (#199 follow-up)
    /// Discover the directories each connected sensor advertises for snapshot
    /// download (queries every sensor's `@/snapshot/status`).
    LoadSnapshotDirs,
    /// The advertised snapshot directories for one sensor key prefix.
    SnapshotDirsLoaded {
        /// Sensor key prefix, e.g. `zensight/sysinfo`.
        key_prefix: String,
        /// Advertised directory names.
        dirs: Vec<String>,
    },
    /// Download the named directory from the sensor at this key prefix (opens a
    /// destination-folder picker first).
    DownloadSnapshot {
        /// Sensor key prefix, e.g. `zensight/sysinfo`.
        key_prefix: String,
        /// Logical directory name to download.
        dir: String,
    },
    /// The destination-folder picker resolved (`None` = the user cancelled).
    SnapshotDestChosen {
        /// Sensor key prefix.
        key_prefix: String,
        /// Logical directory name.
        dir: String,
        /// Chosen destination folder, or `None` if cancelled.
        dest: Option<std::path::PathBuf>,
    },
    /// The snapshot request resolved: a `Ready` state to download, or an error.
    SnapshotRequested(Result<zensight_common::snapshot::SnapshotState, String>),
    /// Streaming download progress (chunks resolved / total).
    SnapshotProgress {
        /// Chunks resolved so far.
        got: u64,
        /// Total distinct chunks.
        total: u64,
    },
    /// The tree finished reconstructing into a folder, or failed.
    SnapshotDownloaded(Result<std::path::PathBuf, String>),
    /// Pause the in-flight directory download (chunks kept; resumable).
    PauseSnapshot,
    /// Resume a paused directory download.
    ResumeSnapshot,
    /// Cancel the in-flight directory download.
    CancelSnapshot,

    // Theme messages
    /// Toggle between light and dark theme.
    ToggleTheme,

    // Keyboard shortcut messages
    /// Focus the search input (Ctrl+F).
    FocusSearch,

    /// Escape key pressed - close dialogs, clear selection, etc.
    EscapePressed,

    // Group management messages
    /// Open the groups management panel.
    OpenGroupsPanel,

    /// Close the groups management panel.
    CloseGroupsPanel,

    /// Set the group filter (None = show all).
    SetGroupFilter(Option<u32>),

    /// Set new group name in form.
    SetNewGroupName(String),

    /// Set new group color in form.
    SetNewGroupColor(usize),

    /// Add a new group from the form.
    AddGroup,

    /// Start editing a group.
    EditGroup(u32),

    /// Set edit group name.
    SetEditGroupName(String),

    /// Set edit group color.
    SetEditGroupColor(usize),

    /// Save group edit.
    SaveGroupEdit,

    /// Cancel group edit.
    CancelGroupEdit,

    /// Delete a group.
    DeleteGroup(u32),

    /// Toggle device assignment to a group.
    ToggleDeviceGroup(DeviceId, u32),

    // Overview messages
    /// Select a protocol for the overview section.
    SelectOverviewProtocol(Protocol),

    /// Toggle overview section expanded/collapsed.
    ToggleOverviewExpanded,

    // Topology messages
    /// Open the topology view.
    OpenTopology,

    /// Close the topology view.
    CloseTopology,

    /// Select a node in the topology.
    TopologySelectNode(String),

    /// Navigate to device detail for a topology node.
    TopologyViewDeviceDetail(String),

    /// Select an edge in the topology.
    TopologySelectEdge(usize),

    /// Clear topology selection.
    TopologyClearSelection,

    /// Start dragging a node.
    TopologyDragNodeStart(String, f32, f32),

    /// Update node position during drag.
    TopologyDragNodeUpdate(String, f32, f32),

    /// End node drag.
    TopologyDragNodeEnd(String),

    /// Update pan offset.
    TopologyPanUpdate(f32, f32),

    /// Zoom in on topology.
    TopologyZoomIn,

    /// Zoom out on topology.
    TopologyZoomOut,

    /// Reset topology zoom.
    TopologyZoomReset,

    /// Toggle auto-layout.
    TopologyToggleAutoLayout,

    /// Set topology search query.
    TopologySetSearch(String),

    // Syslog filter messages
    /// Toggle syslog filter panel visibility.
    ToggleSyslogFilterPanel,

    /// Set minimum severity filter (None = all severities).
    SetSyslogMinSeverity(Option<u8>),

    /// Toggle inclusion of a facility in the filter.
    ToggleSyslogFacility(String),

    /// Toggle inclusion of a systemd unit in the filter (journald lens, #64).
    ToggleSyslogUnit(String),

    /// Toggle inclusion of a journald boot in the filter (boot lens, #93).
    ToggleSyslogBoot(String),

    /// Toggle the structured drill-down for a log row, keyed by content (#93).
    ToggleLogRow(String),

    /// Toggle live-tail follow/pause on the log stream (#93).
    ToggleLogFollow,

    /// Resume live tail — jump the log stream back to now (#93).
    LogsJumpToNow,

    /// Set syslog app name filter pattern.
    SetSyslogAppFilter(String),

    /// Set syslog message content filter pattern.
    SetSyslogMessageFilter(String),

    /// Apply syslog filters (send to sensor).
    ApplySyslogFilters,

    /// Clear all syslog filters.
    ClearSyslogFilters,

    /// Syslog filter status received from sensor.
    SyslogFilterStatusReceived(SyslogFilterStatus),

    /// Dismiss a toast notification.
    DismissToast(u64),
}

/// Syslog filter status from sensor.
#[derive(Debug, Clone)]
pub struct SyslogFilterStatus {
    pub messages_received: u64,
    pub messages_passed: u64,
    pub messages_filtered: u64,
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
