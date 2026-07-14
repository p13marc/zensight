use zensight_common::{
    Alert, DeviceLiveness, DeviceStatus, ErrorReport, HealthSnapshot, HostEntity, Protocol,
    SensorInfo, TelemetryPoint,
};

use crate::view::alerts::{ComparisonOp, Severity};
use crate::view::chart::TimeWindow;
use crate::view::settings::ZenohMode;

/// One telemetry sample, plus **who published it** (#474).
///
/// The origin is chunk 3 of the sample's key. A `TelemetryPoint` has never
/// carried one — it carries a *human* `source` — and the GUI used to throw the
/// key's origin away at decode and then reconstruct it from a `source → origin`
/// side map fed out-of-band by health docs. That map collided on duplicate
/// hostnames, and since the map is what builds every origin-scoped `@rpc` key,
/// a collision misrouted drill-downs to the wrong host.
///
/// So: carry it. It was on every sample all along.
#[derive(Debug, Clone)]
pub struct Reading {
    pub point: TelemetryPoint,
    /// The publishing host's origin (`h-<12hex>`), read from the key.
    pub origin: String,
}

impl Reading {
    pub fn new(point: TelemetryPoint, origin: impl Into<String>) -> Self {
        Self {
            point,
            origin: origin.into(),
        }
    }

    pub fn device_id(&self) -> DeviceId {
        DeviceId::from_telemetry(&self.point, &self.origin)
    }
}

/// Messages for the ZenSight application.
#[derive(Debug, Clone)]
pub enum Message {
    /// Telemetry received from Zenoh subscription.
    TelemetryReceived(Reading),

    /// A burst of telemetry drained in one go (startup history / streaming
    /// spikes) — one iced update instead of one per sample.
    TelemetryBatch(Vec<Reading>),

    /// A periodic off-thread store flush finished. Payload is the number of
    /// downsampled buckets persisted (or `Err` with a message on failure). #22.
    StoreFlushed(Result<usize, String>),

    /// Off-thread history pre-load for a device finished (#22): metric name ->
    /// merged (warm/cold) samples to seed the device detail chart on open.
    DeviceHistoryLoaded(DeviceId, Vec<(String, Vec<crate::store::Sample>)>),

    /// Off-thread log cold-store search-back finished (#107, C9): persisted log
    /// records (newest-first) to merge into the rolling buffer on Logs-view open.
    LogHistoryLoaded(Vec<crate::store::StoredLog>),

    /// On-demand `@rpc/logs/events` fetch finished (#358): per-line log events
    /// pulled from the logs sensors' rings (all repliers concatenated), to
    /// merge into the rolling buffer + persist for search-back.
    LogEventsLoaded(Result<Vec<zensight_common::LogRecord>, String>),

    /// Sensor health snapshot received.
    HealthSnapshotReceived(HealthSnapshot),

    /// Device liveness update received.
    DeviceLivenessReceived(String, DeviceLiveness),

    /// Sensor error report received (with the publishing sensor/protocol name
    /// and, on the host-scoped key shape, the instance's `<source>` segment).
    ErrorReportReceived(String, Option<String>, ErrorReport),

    /// Sensor discovery/info received.
    SensorInfoReceived(SensorInfo),

    /// A sensor-emitted alert was received (firing or resolved). Published on
    /// `state/<producer>/alert/<alert_key>`.
    AlertReceived(Alert),

    /// A sensor alert key was deleted (resolve tombstone).
    AlertCleared {
        protocol: String,
        alert_key: String,
    },

    /// Seed of currently-firing alerts fetched on connect from sensors'
    /// alert-state seed queryables on `state/<producer>/alert/*`
    /// (late-joiner recovery — populates without
    /// toasting, since these aren't newly-fired).
    AlertsSeed(Vec<Alert>),

    /// Connect-time snapshot of the correlator's [`HostEntity`] docs, fetched
    /// from the entity seed (`zensight/v1/@catalog/state/entity/*`) (#306).
    /// Absent correlator ⇒ no
    /// replies ⇒ empty store ⇒ degraded per-source path.
    EntitySeed(Vec<HostEntity>),

    /// A single [`HostEntity`] doc was published/updated on
    /// `zensight/v1/@catalog/state/entity/<entity_id>` (#306).
    EntityReceived(HostEntity),

    /// A [`HostEntity`] doc was tombstoned (Delete). Payload is the `entity_id`
    /// parsed from the last key chunk (#306).
    EntityRemoved(String),

    /// An alias record: an entity id that has been retired, and the id it now
    /// points at (`@catalog/state/alias/<old_id>`, RFC 06 §5.1 step 1).
    ///
    /// A UI **MUST** consume these (#486). The entity doc for a retired id is
    /// tombstoned, so anything still holding that id — a focused host, a saved
    /// topology layout, a link an operator was sent — resolves to nothing unless
    /// the alias re-points it. Without this, an operator's merge is invisible in
    /// the product, which is the same as not having merged.
    AliasReceived(zensight_common::AliasRecord),

    /// An alias was retired (Delete) — e.g. an `unlink` undoing a merge.
    AliasRemoved(String),

    /// Resolve passive-DNS names for an IP the entity store doesn't claim
    /// (#314): GET the catalog's `@rpc/names?ip=` procedure from the global
    /// search panel.
    LookupNamesForIp(String),
    /// The names-lookup reply for `ip` (#314): observed names with provenance,
    /// or an error (no correlator on the bus).
    NamesLookupReceived(String, Result<Vec<zensight_common::NameVal>, String>),

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

    // Netring threat-intel (IOC / YARA) hot-reload (#328): swap the live matchers
    // without a capture restart via `@rpc/netring/threat_intel/set`.
    /// Edit the IOC paste box (indicators, one per line).
    SetThreatIocInput(String),
    /// Apply the pasted IOC indicators to the netring sensor's live set.
    ApplyThreatIoc,
    /// Re-read the sensor's configured indicator files and re-apply.
    ReloadThreatIocFiles,
    /// Clear all live IOC indicators.
    ClearThreatIoc,
    /// Edit the YARA rules paste box.
    SetThreatYaraInput(String),
    /// Compile + apply the pasted YARA rules (needs the sensor's `yara` feature).
    ApplyThreatYara,
    /// A threat-intel status reply (`ThreatIntelStatus` JSON), or an error.
    ThreatIntelStatusReceived(Result<String, String>),

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
    /// Filter the passive-asset inventory by role (`None` = all roles, #329).
    SetInventoryAssetRole(Option<String>),
    /// Set the fingerprint-explorer kind filter (`None` = all kinds).
    SetInventoryFpFilter(Option<crate::view::inventory::FpKind>),

    /// Open the bandwidth live-monitor view (#319, epic #320) and fetch per-process rows.
    OpenBandwidth,
    /// Re-fetch the per-process bandwidth table (`@rpc/netlink/bandwidth`).
    RefreshBandwidth,
    /// Per-process bandwidth fetch outcome.
    BandwidthLoaded(Result<Vec<zensight_common::BandwidthRecord>, String>),
    /// Switch the bandwidth monitor between Processes and Services modes.
    SetBandwidthMode(crate::view::bandwidth::BandwidthMode),
    /// Sort the bandwidth table by column index.
    BandwidthTableSort(usize),
    /// Filter the bandwidth table by name substring.
    BandwidthTableFilter(String),

    /// Open the fleet-capabilities view (#469) and fan `introspect` out.
    OpenFleet,
    /// Re-ask the fleet what it serves (`@rpc/<producer>/introspect`).
    RefreshFleet,
    /// The fan-out's replies: one raw registry slice per (origin, producer).
    FleetLoaded(Result<Vec<crate::view::fleet::FleetReply>, String>),
    /// Expand/collapse one row's registry findings.
    ToggleFleetFindings(String),
    /// Sort the fleet table by column index.
    FleetTableSort(usize),
    /// Filter the fleet table by host/producer substring.
    FleetTableFilter(String),

    /// Fetch an on-demand systemd detail channel (units/timers/events/cgroups) (#281).
    FetchSystemdDetail(crate::view::specialized::systemd_detail::SystemdDetailTopic),
    /// A systemd detail reply for a topic: the decoded payload, or an error message.
    SystemdDetailReceived(
        crate::view::specialized::systemd_detail::SystemdDetailTopic,
        Result<crate::view::specialized::systemd_detail::SystemdDetailData, String>,
    ),
    /// Units table (#281): set the active-state filter (`None` = all).
    SystemdSetUnitFilter(Option<String>),
    /// Arm a unit action (#283): the row's start/stop/restart buttons swap to an
    /// inline confirm/cancel pair until resolved.
    SystemdUnitActionArm {
        verb: String,
        unit: String,
    },
    /// Cancel the armed unit action (#283).
    SystemdUnitActionCancel,
    /// Send the armed unit action as `{verb, unit}` via
    /// `@rpc/systemd/action/set` (#283), then poll `@rpc/systemd/action`
    /// for the job outcome. The sensor refuses unless `actions.enabled` is set.
    SystemdUnitActionConfirm,

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
    /// One topology data-refresh reply set (#440): flows (#25) + neighbors
    /// (#49) + matrix + assets (#391), fetched concurrently and landed as a
    /// single message so the edge set rebuilds once per batch instead of
    /// four times back-to-back. `None` = that queryable didn't answer.
    TopologyBatchReceived(crate::view::topology::TopologyBatch),
    /// Switch the topology presentation lens (#392).
    TopologySetLens(crate::view::topology::Lens),
    /// Switch what topology edge labels show (#392).
    TopologySetEdgeLabel(crate::view::topology::EdgeLabelMode),
    /// Switch the topology grouping mode (#392).
    TopologySetGrouping(crate::view::topology::GroupingMode),
    /// Expand a collapsed topology group (clicking its meta-node, #392).
    TopologyExpandGroup(String),
    /// Re-collapse all expanded topology groups (#392).
    TopologyRegroup,
    /// Enter topology focus mode on a node (#392).
    TopologyFocusNode(String),
    /// Change the topology focus radius (#392).
    TopologySetFocusHops(u8),
    /// Leave topology focus mode (#392).
    TopologyExitFocus,
    /// Toggle the topology idle-edge filter (#392).
    TopologyToggleHideIdle,
    /// Toggle the topology passive-node filter (#392).
    TopologyToggleHidePassive,
    /// Toggle the topology external-aggregate filter (#392).
    TopologyToggleHideExternal,
    /// Cap the number of topology flow edges shown (0 = unlimited, #392).
    TopologySetTopN(usize),
    /// Listen sockets fetched for the selected topology node (#393). Carries
    /// the node id so stale replies (selection moved on) are dropped.
    TopologyListenSocketsReceived(String, Result<Vec<zensight_common::SocketRecord>, String>),
    /// Recent flows fetched for the selected topology edge (#393). Carries
    /// the edge index for the same staleness guard.
    TopologyEdgeFlowsReceived(usize, Result<Vec<zensight_common::FlowRecord>, String>),
    /// Copy a string (community_id etc.) to the clipboard (#393).
    TopologyCopyText(String),
    /// Pivot from the topology to the netring flow table (#393).
    TopologyOpenFlows,
    /// Switch the topology layout mode (#394).
    TopologySetLayout(crate::view::topology::LayoutMode),
    /// Toggle a topology node's pin (#394).
    TopologyTogglePin(String),
    /// Apply a canvas-computed zoom-to-fit (#394).
    TopologyFitApplied {
        zoom: f32,
        pan: (f32, f32),
    },
    /// Hover moved onto (or off) a topology node (#394); emitted on change
    /// only.
    TopologyHover(Option<String>),
    /// Advance the topology flow-dash animation (#394); gated subscription.
    TopologyAnimTick,
    /// ~30 fps layout tick (#441): advances the force simulation while it's
    /// unstable. The subscription is gated (view open, Force mode, auto
    /// layout, not stable) so a settled graph burns no frames.
    TopologyLayoutFrame,
    /// Toggle the topology lens legend (#394).
    TopologyToggleLegend,
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
    /// Fetch the passive encrypted-DNS (DoT/DoQ/DoH) destination inventory (#326).
    FetchNetringEncryptedDns,
    /// An encrypted-DNS inventory reply: the decoded records, or an error message.
    NetringEncryptedDnsReceived(Result<Vec<zensight_common::EncryptedDnsRecord>, String>),
    /// Fetch the on-demand netring per-host HTTP detail (#45).
    FetchNetringHttp,
    /// A netring HTTP-detail reply: the decoded records, or an error message.
    NetringHttpReceived(Result<Vec<zensight_common::HttpHostRecord>, String>),
    /// Fetch the on-demand netring capture-file index (#327).
    FetchNetringCaptures,
    /// A netring capture-index reply: the decoded records, or an error message.
    NetringCapturesReceived(Result<Vec<zensight_common::CaptureRecord>, String>),
    /// Manual capture trigger (#327): `capture_now` on `@rpc/netring/capture_disk/set`
    /// (fires the pre-trigger ring in triggered mode, rotates the spool in
    /// rotating mode).
    NetringCaptureNow,
    /// Hot-switch the capture-to-disk mode (#327): `set_capture` on
    /// `@rpc/netring/capture_disk/set` (`"off"` / `"rotating"` / `"triggered"`).
    NetringSetCaptureDiskMode(String),
    /// Download a finished triggered capture by its blob id (#327). Unlike
    /// `StartArtifact` there is no request/produce phase — the file is already
    /// registered on the sensor's `@blob/artifact` server.
    DownloadCaptureBlob {
        producer: String,
        artifact_id: String,
        filename: String,
    },
    /// Fetch the on-demand sysinfo process explorer for the selected host,
    /// sorted as requested (#47).
    FetchSysinfoProcesses(crate::view::specialized::sysinfo_detail::ProcessSort),
    /// A sysinfo process-explorer reply: the decoded records, or an error.
    SysinfoProcessesReceived(Result<Vec<zensight_common::ProcessRecord>, String>),
    /// Fetch the recent-flow ring (`@rpc/netflow/flows`) for the selected exporter (#469).
    FetchNetflowFlows,
    /// A netflow flow-ring reply: the decoded records, or an error message.
    NetflowFlowsReceived(Result<Vec<zensight_common::NetflowRecord>, String>),
    /// Sort the netflow flow table by column index.
    NetflowTableSort(usize),
    /// Filter the netflow flow table.
    NetflowTableFilter(String),
    /// Show more rows of the netflow flow table.
    NetflowTableMore,
    /// Fetch the eBPF saturation histograms (`@rpc/sysinfo/latency`) for the
    /// selected host (#99).
    FetchSysinfoLatency,
    /// A sysinfo latency reply: run-queue + block-I/O histograms, or an error.
    SysinfoLatencyReceived(Result<zensight_common::LatencyReport, String>),
    /// Fetch the parallax stream catalogue (`@rpc/parallax/streams`) for the
    /// selected host (#408).
    FetchParallaxStreams,
    /// A parallax catalogue reply: the advertised streams, or an error.
    ParallaxStreamsReceived(Result<Vec<zensight_common::StreamDescriptor>, String>),
    /// Open a live JPEG preview tile: sends `open_stream` (codec `mjpeg`) and
    /// spawns the abortable per-tile subscriber task (#408).
    ParallaxOpenTile {
        stream: String,
    },
    /// Close a preview tile: aborts its subscriber task and sends
    /// `close_stream`.
    ParallaxCloseTile {
        stream: String,
    },
    /// A decoded preview frame from a tile's subscriber task. `generation`
    /// identifies the tile incarnation the task was opened for (frames from
    /// a replaced task are dropped); stale `seq`s within an incarnation are
    /// dropped too (latest frame wins).
    ParallaxFrame {
        stream: String,
        generation: u64,
        seq: u64,
        handle: iced::widget::image::Handle,
    },
    /// A tile's subscriber task finished (session closed or subscribe
    /// error). Carries the tile incarnation so a replaced task's late end
    /// report cannot clear the new tile's abort handle.
    ParallaxTileEnded {
        stream: String,
        generation: u64,
        error: Option<String>,
    },
    /// A parallax `StreamStatus` transition from `state/parallax/stream/<stream>` (arrives
    /// on the host-scoped control-plane subscriber): a definitive
    /// `open: false` marks a still-waiting tile as failed.
    ParallaxStreamStatus {
        source: String,
        status: zensight_common::stream::StreamStatus,
    },
    /// Open a live H.264 video tile (#409): sends `open_stream` (codec
    /// `h264`) and spawns the decoding subscriber task. Only functional on
    /// builds with the `h264` feature; otherwise it toasts the build hint.
    ParallaxOpenVideoTile {
        stream: String,
    },
    /// Ask the sensor for a fresh IDR (`request_keyframe`) — fired by the
    /// H.264 tile decoder on a sequence discontinuity (#409).
    ParallaxRequestKeyframe {
        stream: String,
    },
    /// Expand a tile to the near-fullscreen overlay (#436). A preview tile
    /// is upgraded to the H.264 video profile when the build and the stream
    /// support it (same refcount-balanced switch as the Video button).
    ParallaxExpandTile {
        stream: String,
    },
    /// Dismiss the expanded-tile overlay (Esc / backdrop click / Close),
    /// restoring the tile's pre-expand profile.
    ParallaxCollapseTile,

    // ── Cross-view identity pivots (#313) — host-local joins over already-
    // published data; every pivot is a query-time read, no new bus traffic.
    /// Open (`Some(unit)`) or close (`None`) the systemd unit drill-down: fetch
    /// `@rpc/systemd/unit?name=` and render the identity panel in the Units tab.
    SystemdSelectUnit(Option<String>),
    /// The single-unit detail reply for the drill-down panel.
    SystemdUnitDetailReceived(Result<zensight_common::UnitDetail, String>),
    /// Pivot to the systemd device for `host` with `unit`'s drill-down loading
    /// (process cgroup chip, journald unit-run chip). Toast fallback when no
    /// systemd device exists for the host.
    PivotToUnit {
        host: String,
        unit: String,
    },
    /// Pivot to the sysinfo device for `host` with the process explorer
    /// filtered to `pid`. `start_time` (the `(pid, start_time)` identity pair)
    /// arms the stale-generation guard: a reused pid renders as "exited", never
    /// as the wrong process. Toast fallback when no sysinfo device exists.
    PivotToProcess {
        host: String,
        pid: i32,
        start_time: Option<u64>,
    },
    /// Clear the process explorer's pivot pid filter.
    ClearSysinfoPidFilter,
    /// Pivot to the Logs view pre-filtered to one unit *run* (journald
    /// `_SYSTEMD_INVOCATION_ID`), from the unit drill-down's "logs for this run".
    OpenLogsForInvocation {
        unit: String,
        invocation_id: String,
    },
    /// Clear the Logs view's unit-run (invocation id) filter.
    ClearLogsInvocationFilter,
    /// Pivot from a Security anomaly to its netring flows (#119): fetch
    /// `@rpc/netring/flows` and filter to the offending `src`. `key` is the anomaly's
    /// `alert_key` so the result renders under the right row.
    FetchAnomalyFlows {
        key: String,
        src: String,
    },
    /// A flow-pivot reply for anomaly `key`: the filtered flows, or an error.
    AnomalyFlowsReceived(String, Result<Vec<zensight_common::FlowRecord>, String>),
    /// Flow ↔ process display join (#309): fetch the endpoint hosts' sockets
    /// (`@rpc/netlink/sockets?ip=`, all replies) and match the flow's 5-tuple to its
    /// owning process. `key` identifies the flow row the result renders under.
    FetchFlowAttribution {
        target: AttributionTarget,
        key: String,
        src: String,
        dst: String,
    },
    /// The flow↔process join outcome for flow `key` (#309): the matched owning
    /// process, `Ok(None)` when no socket matched (unattributed), or `Err` when
    /// no netlink sensor replied at all.
    FlowAttributionReceived {
        target: AttributionTarget,
        key: String,
        result: Result<Option<crate::view::specialized::attribution::AttributedProcess>, String>,
    },
    /// The capture-to-disk index fetched for the Security drill-down (#327), so
    /// an expanded anomaly can offer its matching triggered capture.
    AnomalyCapturesReceived(Result<Vec<zensight_common::CaptureRecord>, String>),

    /// Open the security (network anomalies) view.
    OpenSecurity,
    /// Close the security view.
    CloseSecurity,
    /// Toggle hiding Info-severity anomalies in the Security view (#48).
    ToggleSecurityHideInfo,
    /// Expand/collapse an anomaly's evidence drill-down by alert_key (#48).
    SelectAnomaly(Option<String>),

    /// Sensor came online (liveliness token appeared). Carries the protocol
    /// and, on the host-scoped key shape, the instance's `<source>` segment.
    SensorOnline(String, Option<String>),

    /// Sensor went offline (liveliness token disappeared).
    SensorOffline(String, Option<String>),

    /// Device came online (liveliness token appeared). Carries the publishing
    /// host's origin because that is half of the device's identity (#474) —
    /// without it the handler cannot name the device it is being told about.
    DeviceOnline {
        protocol: String,
        origin: String,
        device: String,
    },

    /// Device went offline (liveliness token disappeared).
    DeviceOffline {
        protocol: String,
        origin: String,
        device: String,
    },

    /// User selected a device from the dashboard.
    SelectDevice(DeviceId),

    /// Select a device the user picked out by *name* — an entity member, a
    /// topology node, a search hit. The origin is not knowable from a name, so
    /// the app resolves it against the devices it has seen
    /// ([`crate::view::dashboard::DashboardState::resolve_device`]) rather than
    /// letting the view fabricate a handle (#474, RFC 06 §6).
    SelectDeviceNamed {
        protocol: Protocol,
        source: String,
    },

    /// User dismissed a stale facet: drop it from the in-memory device map.
    /// Facets are not persisted, so this is a pure view-model removal — the
    /// facet reappears if its telemetry resumes.
    ForgetDevice(DeviceId),

    /// Jump from an alert straight to the offending device, pre-selecting the
    /// metric (if known) so its chart opens immediately (#35 triage loop).
    ///
    /// Names the device by *human* identity — an alert carries a `source`, not
    /// an origin. The app resolves it against the devices it has seen
    /// ([`DashboardState::resolve_device`]); a view must not invent the handle.
    InvestigateAlert {
        protocol: Protocol,
        source: String,
        metric: Option<String>,
    },

    /// Navigate to the previous/next device within the current filtered set
    /// (#35 cross-device navigation on the device detail view).
    SelectAdjacentDevice {
        forward: bool,
    },

    /// User cleared device selection (back to dashboard).
    ClearSelection,

    /// **Focus this host** (#476): subscribe to one origin instead of the fleet.
    ///
    /// `Some(origin)` narrows every data-plane subscription to
    /// `zensight/v1/<origin>/…`; `None` restores the fleet. Changing this
    /// re-keys the Zenoh subscription, so Iced tears the session down and
    /// re-declares — same mechanism as a settings change.
    SetFocusHost(Option<String>),

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

    /// Toggle "group by host" on the dashboard (#306): merge per-protocol
    /// facets into one host card via correlator entities, or show per-source.
    ToggleGroupByHost,

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

    /// Set the link profile (#364): standard vs. constrained.
    SetLinkProfile(zensight_common::LinkProfile),

    /// Edit the telemetry subscription scope (#364), comma-separated.
    SubscriptionScopeChanged(String),

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

    // Unified artifact download messages (report / snapshot / capture) via the artifact channel.
    /// Discover the artifact kinds each connected sensor produces (queries every
    /// sensor's `artifact/status` read procedure), so the GUI knows which affordances to render.
    LoadArtifactKinds,
    /// The advertised artifact kinds (+ bounds/adverts) for one producer.
    ArtifactKindsLoaded {
        /// Producer name, e.g. `sysinfo`.
        producer: String,
        /// The kinds this sensor produces and their per-kind status.
        kinds: Vec<zensight_common::KindStatus>,
    },
    /// Request + download an artifact of `kind` from the sensor at `producer`
    /// (e.g. `netlink`).
    StartArtifact {
        /// Producer name.
        producer: String,
        /// What to produce (report / snapshot / capture).
        kind: zensight_common::ArtifactKind,
        /// Target one sensor instance (`ArtifactRequest.opts.target_source`).
        /// `None` fans out to every host running this protocol.
        target_source: Option<String>,
    },
    /// The destination-folder picker resolved for a tree artifact (`None` = the
    /// user cancelled). Only tree kinds (snapshots) pick a folder first; blobs go
    /// to a temp dir then a Save-as dialog.
    ArtifactDestChosen {
        /// Producer name.
        producer: String,
        /// What to produce.
        kind: zensight_common::ArtifactKind,
        /// Target one sensor instance, threaded from `StartArtifact`.
        target_source: Option<String>,
        /// Chosen destination folder, or `None` if cancelled.
        dest: Option<std::path::PathBuf>,
    },
    /// The sensor reported production progress (streamed from the status poll
    /// while the request is in flight): an optional human-readable line (e.g.
    /// `"capturing 12s/30s"`) and an optional fraction in `0.0..=1.0`.
    ArtifactGenerating {
        /// Producer-reported progress line, if any.
        detail: Option<String>,
        /// Producer-reported fraction in `0.0..=1.0`, if any.
        progress: Option<f32>,
    },
    /// The artifact request resolved: a `Ready` state to download, or an error.
    ArtifactRequested(Result<zensight_common::ArtifactState, String>),
    /// Streaming download progress (units resolved / total).
    ArtifactProgress {
        /// Units resolved so far.
        got: u64,
        /// Total units.
        total: u64,
    },
    /// The artifact finished downloading (a temp file for a blob, the chosen
    /// folder for a tree), or failed.
    ArtifactDownloaded(Result<std::path::PathBuf, String>),
    /// Outcome of the "Save as…" dialog for a downloaded blob artifact.
    ArtifactSaved(Result<Option<String>, String>),
    /// Pause the in-flight artifact download (keeps the partial; resumable).
    PauseArtifact,
    /// Resume a paused artifact download.
    ResumeArtifact,
    /// Cancel the in-flight artifact download (discards the partial).
    CancelArtifact,
    /// Edit a text field of a sensor's capture form (#333).
    CaptureFormEdited {
        /// Sensor key prefix the form belongs to.
        producer: String,
        /// Which field changed.
        field: crate::view::artifact_fetch::CaptureField,
        /// The new text value.
        value: String,
    },
    /// Toggle a boolean of a sensor's capture form (#333).
    CaptureFormToggled {
        /// Sensor key prefix the form belongs to.
        producer: String,
        /// Which toggle flipped.
        field: crate::view::artifact_fetch::CaptureToggle,
    },

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

    /// Collapse/expand the "Log statistics" block on the logs facet (#350).
    ToggleLogStatsPanel,

    /// Toggle the by-unit list between top-3 and all units (#350).
    ToggleLogStatsAllUnits,

    /// Collapse/expand the host identity details (facts + resolution group) in
    /// the merged host nav bar (#350). Persisted.
    ToggleIdentityDetails,

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

    /// Open the global bandwidth monitor pre-scoped to one host (#351).
    OpenBandwidthForHost(String),

    /// Clear the bandwidth monitor's host scope (#351).
    ClearBandwidthHostFilter,

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

/// Which surface a flow↔process attribution result renders on (#309): the
/// netring device Flows tab, or the Security anomaly pivot-flows table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttributionTarget {
    Device,
    Security,
    /// The topology edge panel (#393).
    Topology,
}

/// Unique identifier for a device: **who published it**, what protocol, and
/// which device it is *about*.
///
/// # Why the origin is here (#474)
///
/// This used to be `{ protocol, source }`, where `source` is the payload's
/// human label. That collides: two hosts reporting the same hostname
/// (`localhost`, a cloned VM image, two containers named alike) landed on one
/// `DeviceId` and overwrote each other.
///
/// On the old keyspace that only muddled a *display*. On v1 it **misroutes
/// queries** — this id is what every origin-scoped `@rpc`/`@media` key is built
/// from, so the loser's drill-downs and video tiles silently targeted the
/// winner's host. The origin (`h-<12hex>`) is the identity; the hostname never
/// was (RFC 06 §6.1).
///
/// # Why `source` is still here
///
/// It is **not** merely a display label, and dropping it would be a worse bug
/// than the one being fixed. For a *host* sensor (sysinfo, netlink, …) `source`
/// is the publishing host's own hostname, and is indeed decorative — the origin
/// already says which box it is. But for a **proxy** sensor (SNMP, Modbus,
/// gNMI, NetFlow) `source` is the *polled device*: `origin` is the poller's
/// host, `source` is `router01`. Keying on the origin alone would collapse
/// every device a poller polls into one.
///
/// So the triple is exactly RFC 06 §3's model — *observed devices are subjects,
/// not origins*: the origin says who is talking, the source says who they are
/// talking about. Two collectors polling one router yield two ids, and merging
/// them is the catalog's job, not this struct's.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceId {
    pub protocol: Protocol,
    /// The publishing host's v1 origin (`h-<12hex>`) — chunk 3 of every key it
    /// publishes. Read from the key, never from the payload.
    pub origin: String,
    /// The payload's `source`: the publisher's own hostname for a host sensor,
    /// the polled device's name for a proxy sensor.
    pub source: String,
}

/// The placeholder origin worn by every [`DeviceId::fixture`].
pub const FIXTURE_ORIGIN: &str = "h-fixture0000";

impl DeviceId {
    pub fn new(protocol: Protocol, origin: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            protocol,
            origin: origin.into(),
            source: source.into(),
        }
    }

    /// A device from an unspecified host — **fixtures only** (tests, mock data,
    /// the demo simulator's static environment). Every such device gets the same
    /// placeholder origin, so `(protocol, source)` still tells them apart.
    ///
    /// Real code never calls this: the origin arrives on the key, and inventing
    /// one is the bug this type exists to prevent (#474). The placeholder is
    /// deliberately not a valid minted id, so if one ever escapes onto the wire
    /// it fails the grammar rather than quietly addressing a host that isn't
    /// there.
    pub fn fixture(protocol: Protocol, source: impl Into<String>) -> Self {
        Self::new(protocol, FIXTURE_ORIGIN, source)
    }

    /// The origin comes from the **key** (chunk 3), not the payload — a
    /// `TelemetryPoint` has never carried one, which is exactly why the GUI used
    /// to need a `source -> origin` side map fed out-of-band from health docs.
    /// It is on every sample; it was simply being thrown away at decode.
    pub fn from_telemetry(point: &TelemetryPoint, origin: impl Into<String>) -> Self {
        Self {
            protocol: point.protocol,
            origin: origin.into(),
            source: point.source.clone(),
        }
    }
}

impl std::fmt::Display for DeviceId {
    /// Human-facing: the hostname is what an operator recognises, so it stays
    /// the label. The origin is an *address*, not a name (RFC 06 §1).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.protocol, self.source)
    }
}
