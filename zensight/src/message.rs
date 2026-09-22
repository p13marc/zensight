use zensight_common::{
    Alert, DeviceLiveness, DeviceStatus, ErrorReport, HealthSnapshot, HostEntity, Protocol,
    SensorInfo, TelemetryPoint,
};

use crate::view::alerts::ComparisonOp;
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
    /// The producer base name — chunk 4 of the key with its instance suffix
    /// stripped (`keyexpr::producer_name`). Since #1255 the point no longer
    /// repeats it, and it is a **name**, not an enum: a producer this build
    /// was not compiled with still has one.
    pub producer: String,
    /// The key's subject tail, `/`-joined — everything after the producer
    /// chunk.
    ///
    /// Carried for the same reason as `origin`, and it is not derivable from
    /// the payload either: for a proxy producer the wire subject is
    /// `{device}/{metric...}` while [`TelemetryPoint::metric`] is only the
    /// `{metric...}` half. `(origin, producer, subject)` is what names a
    /// series to the store and to the fleet historian (#904), so a cache that
    /// rebuilt it from the payload would name the same series differently from
    /// the service it is a cache of.
    pub subject: String,
}

impl Reading {
    pub fn new(
        point: TelemetryPoint,
        origin: impl Into<String>,
        producer: impl Into<String>,
        subject: impl Into<String>,
    ) -> Self {
        Self {
            point,
            origin: origin.into(),
            producer: producer.into(),
            subject: subject.into(),
        }
    }

    /// The device this reading belongs to, off the key: producer (chunk 4),
    /// origin (chunk 3), and the payload's `source`.
    pub fn device_id(&self) -> DeviceId {
        DeviceId::from_reading(self)
    }
}

/// One fleet-wide answer to `@rpc/logs/events/page` (#1147).
///
/// `partial` is the whole reason this is not a bare `Vec`. The GUI used to
/// infer "the store had nothing more" from a **short page**, which is wrong in
/// exactly the case the paging exists for: a search truncated by the sensor's
/// scan cap returns *zero* rows, zero is shorter than the cap, and the operator
/// was told the walk had finished. `?pattern=OOM;from=<7d>` over a large store
/// with the last OOM far back read as "no OOM this week", permanently.
///
/// Fleet fan-in (RFC 05 §2.1): every logs sensor answers, so the rows are
/// concatenated and `partial` is the **disjunction** — one sensor with more to
/// give means the fleet walk is not done, whatever the others said.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LogPage {
    pub records: Vec<zensight_common::LogRecord>,
    /// At least one sensor stopped before finishing its walk.
    pub partial: bool,
}

impl LogPage {
    /// A complete walk that returned these rows.
    #[must_use]
    pub fn complete(records: Vec<zensight_common::LogRecord>) -> Self {
        Self {
            records,
            partial: false,
        }
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

    /// The window manager asked the window to close (#1119).
    ///
    /// Not the same as the window having closed: this arrives *before*, and it
    /// is the only chance to flush. Up to fifteen seconds of buckets, logs and
    /// events sat in `MetricStore`'s pending buffers at any moment — the
    /// fifteen seconds an operator was watching when they decided to quit and
    /// go look — and closing discarded them. The same gap cost every parallax
    /// tile its `close_stream`, so the sensor carried the viewer refcount
    /// until its idle reaper fired.
    CloseRequested(iced::window::Id),

    /// Off-thread history pre-load for a device finished (#22): metric name ->
    /// merged (warm/cold) samples to seed the device detail chart on open.
    /// Seeded history for a device, and whether the reply was capped.
    ///
    /// Truncation rides with the data rather than being inferred: only the
    /// replier knows whether it stopped at its limit, and a chart drawing a
    /// partial window without saying so is a claim about a period it was not
    /// given (#910).
    DeviceHistoryLoaded(DeviceId, Vec<(String, Vec<zensight_store::Sample>)>, bool),

    /// Off-thread log cold-store search-back finished (#107, C9): persisted log
    /// records (newest-first) to merge into the rolling buffer on Logs-view open.
    LogHistoryLoaded(Vec<zensight_store::StoredLog>),

    /// On-demand `@rpc/logs/events` fetch finished (#358): per-line log events
    /// pulled from the logs sensors' rings (all repliers concatenated), to
    /// merge into the rolling buffer + persist for search-back.
    LogEventsLoaded(Result<LogPage, String>),

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
    /// `state/<producer>/alert/<alert_key>`. `origin` is the publishing
    /// host's origin chunk read from the key (`None` for the demo feed) — the
    /// one identifier a later Delete tombstone carries, since the tombstone
    /// has no payload and so no `source`.
    AlertReceived {
        origin: Option<String>,
        alert: Alert,
    },

    /// A sensor alert key was deleted (resolve tombstone).
    AlertCleared {
        protocol: String,
        origin: String,
        alert_key: String,
    },

    /// Seed of currently-firing alerts fetched on connect from sensors'
    /// alert-state seed queryables on `state/<producer>/alert/*`
    /// (late-joiner recovery — populates without
    /// toasting, since these aren't newly-fired).
    AlertsSeed(Vec<(Option<String>, Alert)>),

    /// The catalog's ack / silence / incident sets, as **one snapshot** each
    /// (#1116).
    ///
    /// These used to arrive as a stream of `AckReceived`/`SilenceReceived`/
    /// `IncidentReceived`, one per reply — additive, so a document retired
    /// while the GUI was disconnected stayed in the projection for the life of
    /// the process. Its tombstone went to a subscriber that no longer existed,
    /// and the seed that followed the reconnect could only *add*.
    ///
    /// A seed is a statement about a whole class, so it replaces one.
    CatalogSeed(Box<CatalogSnapshot>),

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

    /// Connect-time snapshot of the catalog's resolved topology graph, from
    /// the edge seed (`zensight/v1/@catalog/state/edge/*`) (#919).
    ///
    /// Without it the map is blank until something in the fleet's topology
    /// *changes* — and the catalog's change gate means that may be a long
    /// time, deliberately. Absent catalog ⇒ no replies ⇒ flow-only graph,
    /// which is the documented degraded path.
    EdgeSeed(Vec<zensight_common::relation::Edge>),

    /// A single resolved edge was published/updated on
    /// `zensight/v1/@catalog/state/edge/<edge_id>` (#919).
    EdgeReceived(zensight_common::relation::Edge),

    /// An edge was tombstoned (Delete). Payload is the `edge_id` parsed from
    /// the last key chunk (#919).
    EdgeRemoved(String),

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

    /// The outcome of forwarding one tile's receiver report (#718).
    ///
    /// Its own variant rather than [`Self::CommandFeedback`] because its
    /// cadence is different in kind: a report goes out every few seconds per
    /// open tile, forever, so a producer that refuses them would otherwise
    /// toast on a 3-second loop for as long as the tile is open. This one
    /// toasts the first refusal and then goes quiet until reports work again.
    ParallaxReportOutcome {
        success: bool,
        message: String,
    },
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
    /// The operator chose which host's sentinel the pane addresses (#1114).
    SetExpectationHost(crate::view::expectations::ExpHost),
    /// Set the systemd expectation kind being authored (#278).
    SetSystemdExpKind(crate::view::expectations::SystemdExpKind),
    /// Pick the hostspec assertion kind being authored (#821).
    SetHostspecExpKind(crate::view::expectations::HostspecExpKind),
    /// The hostspec sentinel's current assertion set (raw JSON reply).
    HostspecExpectationsReceived(String),
    /// The hostspec sensor's `@rpc/hostspec/spec` answer — "what is this host
    /// being held to" (raw JSON reply, #867). Kept verbatim: an empty set is a
    /// *state*, and the sensor's own words for it are more trustworthy than
    /// the GUI's guess at why a pane is blank.
    HostspecSpecReceived(String),
    /// A systemd sentinel status reply (ExpectationsConfig JSON) (#278).
    SystemdExpectationsReceived(String),
    /// A producer's `@rpc/<producer>/thresholds` reply — the operator rule set
    /// it is currently evaluating (raw JSON, #933).
    ThresholdsReceived(String),
    /// The producer's `state/<producer>/applied/thresholds` marker (#933):
    /// which writer — file, desired or rpc — is actually in force.
    ThresholdsAppliedReceived(String),
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
    /// Edit a detector's threshold input field (not yet applied).
    SetNetringThresholdInput {
        detector: String,
        value: String,
    },
    /// Edit the new-allowlist-entry input field.
    SetNetringAllowlistInput(String),

    // Netring capture-focus (#225/#228): hot-swap the reloadable packet-tier
    // BPF filter live, narrowing capture attention during an incident.
    /// Edit the capture-focus filter expression input (not yet applied).
    SetPacketFilterInput(String),

    // Netring threat-intel (IOC / YARA) hot-reload (#328): swap the live matchers
    // without a capture restart via `@rpc/netring/threat_intel/set`.
    /// Edit the IOC paste box (indicators, one per line).
    SetThreatIocInput(String),
    /// Edit the YARA rules paste box.
    SetThreatYaraInput(String),

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
    /// The sweep's outcome: one raw registry slice per (origin, producer), plus
    /// what the fan-in's reply bound refused (#745).
    FleetLoaded(Result<crate::view::fleet::FleetSweep, String>),

    /// The fleet's `views` replies, one definition per producer (#1259):
    /// what each producer says about how its family model is best shown.
    /// Fetched after every sweep for the producers that declare the
    /// procedure; a producer-served definition wins over the bundled one.
    ViewsLoaded(Vec<(String, zensight_common::views::ViewSet)>),

    /// The fleet's `describe` replies, one schema set per producer (#1256) —
    /// the schema half of the runtime registry, fetched after every sweep
    /// for the producers not yet described.
    SchemasLoaded(Vec<(String, zensight_common::schema::SchemaSet)>),

    /// A state document the compiled registry has no type for (#1256): a
    /// producer this build never heard of, or a registered producer's
    /// subject the GUI maps to nothing. Wire facts only — the origin and
    /// producer off the key, the subject tail, the value decoded
    /// structurally. Judged at fold time against the runtime registry.
    Document {
        origin: String,
        producer: String,
        subject: String,
        value: serde_json::Value,
    },

    /// An events-class record with no typed arm (#1256) — same shape as
    /// [`Message::Document`], held in a bounded ring.
    Event {
        origin: String,
        producer: String,
        subject: String,
        value: serde_json::Value,
    },
    /// Expand/collapse one row's registry findings.
    ToggleFleetFindings(String),
    /// Sort the fleet table by column index.
    FleetTableSort(usize),
    /// Filter the fleet table by host/producer substring.
    FleetTableFilter(String),

    /// Open the bus-explorer view (#748) and start its monitor if none runs.
    OpenExplorer,
    /// The explorer pump is up; the handle sends it watch/inspect/shutdown
    /// commands.
    ExplorerStarted(crate::view::explorer::pump::ExplorerCtl),
    /// One stats tick's snapshot (~4/s regardless of bus rate): the key
    /// tree, presence, the QoS ledger, and the four distinct loss counters.
    ExplorerTick(std::sync::Arc<crate::view::explorer::core::ExplorerSnapshot>),
    /// The watch-selector input changed.
    ExplorerWatchInput(String),
    /// Declare the typed selector as a data-plane watch.
    ExplorerWatchSubmit,
    /// Release one watch.
    ExplorerUnwatch(zenkey_fleet::WatchId),
    /// Expand/collapse one tree node.
    ExplorerToggleNode(String),
    /// Select (or clear) the key the inspector shows.
    ExplorerSelectKey(Option<String>),
    /// Ask the pump for an acknowledged teardown.
    ExplorerStop,
    /// The pump ended (after `ExplorerStop`, a disconnect, or a failure).
    /// The last snapshot stays readable.
    ExplorerStopped,
    /// A pump-side failure worth showing (watch refused, monitor failed).
    ExplorerError(String),

    /// Arm a write procedure on the selected device (#1261): what will be
    /// sent, how it reads, how it is confirmed. The row swaps to its
    /// confirmation; nothing goes on the wire until [`Message::Confirm`].
    Arm(crate::call::Armed),
    /// Disarm the armed write.
    Disarm,
    /// What the operator has typed into a typed confirmation.
    ConfirmText(String),
    /// Send the armed write — only when its confirmation holds, checked
    /// again in the app so a message arriving any other way cannot skip it.
    Confirm,
    /// A write procedure answered, or did not (#1261). Carries the surface,
    /// the device and the request it answers, so a stale outcome is dropped.
    Written {
        surface: crate::call::CallSurface,
        device: Option<DeviceId>,
        procedure: String,
        request: serde_json::Value,
        result: Result<crate::call::Reply, crate::call::WriteFailure>,
    },
    /// Choose the host the Security pane's netring tuning reads from and
    /// writes to (#1261) — one host's sensor, never the fleet.
    SetSecurityHost(crate::view::expectations::ExpHost),
    /// Forget a procedure's answer on the selected device (#1261), so its
    /// panel offers the call again — a unit file hidden, a table dismissed.
    ForgetCall {
        procedure: String,
    },
    /// Fetch this host's advertised service-control gate (#283) so the Units tab
    /// can render what it will actually accept.
    // ── Gated PDU outlet control (#956) ─────────────────────────────────

    /// Set one of a device view's own filter controls (#1261) — the socket
    /// explorer's state chip, port substring or sort — kept in
    /// `DeviceDetailState::filters` as `<table>/<key>`; `""` clears it. The
    /// table's page resets, so a narrowed filter never hides matches.
    SetDetailFilter {
        table: String,
        key: String,
        value: String,
    },

    /// Select the active tab of a tabbed specialized view (#243). Remembered
    /// per device in `DeviceDetailState`.
    SelectSpecializedTab(DeviceId, crate::view::specialized::SpecializedTab),

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
    /// Download a finished triggered capture by its blob id (#327). Unlike
    /// `StartArtifact` there is no request/produce phase — the file is already
    /// registered on the sensor's `@blob/artifact` server.
    DownloadCaptureBlob {
        producer: String,
        artifact_id: String,
        /// The **concrete** `@blob/artifact` prefix of the host holding the
        /// file, straight off the capture record. A bulk fetch must name a
        /// literal origin (RFC 07 §3); this is where the GUI learns which one
        /// instead of wildcarding because it does not know.
        blob_prefix: String,
        /// BLAKE3 root to pin the transfer to (RFC 07 §2.1), when the sensor
        /// served one — already hex-validated at the wire (`ContentHash`).
        root: Option<zenkey::ContentHash>,
        filename: String,
    },
    /// Call a read procedure (#1261, design §5.5):
    /// `@rpc/<producer>/<procedure>?<params>` on the selected device's
    /// origin, or fleet-wide when the request names another producer. The
    /// one message every on-demand panel asks with; the answer lands as
    /// [`Message::Reply`] in the surface's [`crate::call::Calls`].
    Call(crate::call::Request),
    /// A read procedure answered (or did not). Carries the surface, the
    /// device and the params it answers, so a reply to a superseded call —
    /// or to a device no longer selected — is dropped, not shown.
    Reply {
        surface: crate::call::CallSurface,
        /// The device that asked, on the device surface.
        device: Option<DeviceId>,
        /// What the answer is filed under — see [`crate::call::Request::key`].
        key: String,
        procedure: String,
        params: String,
        result: Result<crate::call::Reply, String>,
    },
    /// Several messages from one press (#1261): a join that asks two calls
    /// at once. Folded in order, each as if sent alone.
    Batch(Vec<Message>),
    /// Sort a device view's on-demand table by column index (#1261). The
    /// table is named by the view (`flows`), the state lives in
    /// `DeviceDetailState::tables`.
    DetailTableSort {
        table: String,
        column: usize,
    },
    /// Filter a device view's on-demand table.
    DetailTableFilter {
        table: String,
        query: String,
    },
    /// Show more rows of a device view's on-demand table.
    DetailTableMore {
        table: String,
    },
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
    /// A tile's periodic receiver report (#718, RFC 07 §1.1): how the stream
    /// is arriving, measured by the tile itself. The app forwards it to that
    /// tile's own producer as an `@rpc/parallax/stream/report` write — never
    /// to the fleet selector, because a report is about one key on one host.
    ///
    /// Carries the tile incarnation for the same reason frames do: a report
    /// from a replaced subscriber describes a subscription that no longer
    /// exists. Boxed because the report is the largest thing any `Message`
    /// carries and every other variant would pay for it.
    ParallaxReceiverReport {
        stream: String,
        generation: u64,
        report: Box<zensight_common::stream::MediaReceiverReport>,
    },
    /// A parallax `StreamStatus` transition from `state/parallax/stream/<stream>` (arrives
    /// on the host-scoped control-plane subscriber): a definitive
    /// `open: false` marks a still-waiting tile as failed.
    ParallaxStreamStatus {
        source: String,
        status: zensight_common::stream::StreamStatus,
    },
    /// The cold store's event rows at boot (#578, #1261): the feed survives a
    /// GUI restart without a bus-side storage. Folded into the ring like a
    /// live `Event`, and not written back.
    EventHistory(Vec<zensight_store::StoredEvent>),
    /// Expand/collapse the trap feed's filter row and full listing (#578).
    ToggleSnmpEventFilters,
    /// Trap-feed facets (#578). `None` clears that facet.
    SetSnmpEventDevice(Option<String>),
    SetSnmpEventSeverity(Option<zensight_common::AlertSeverity>),
    SetSnmpEventKind(Option<String>),
    SetSnmpEventTimeRange(crate::view::time_range::TimeRange),
    /// Free-text search over the trap feed (kind/summary/source/fields).
    SetSnmpEventSearch(String),
    /// Reset every trap-feed facet (#578).
    ClearSnmpEventFilters,
    /// An older-page fetch finished (#601). Kept separate from
    /// `LogEventsLoaded` so a page merge never advances the live-tail
    /// watermark — an older page must not make the tail skip forward.
    LogOlderPageLoaded(Result<LogPage, String>),
    /// Open the Alerts view scoped to one device (#578): the trap-feed row's
    /// pivot, and still the honest link for a record that drove no alert
    /// transition, or one written before #651.
    OpenAlertsForSource(String),
    /// Open the Alerts view focused on the one alert a trap raised or cleared
    /// (#651). `source` + `alert_key` are the in-GUI external identity
    /// `<source>/<alert_key>`; scoping the view by source alone would land the
    /// operator in a list when several alerts fire on one device.
    OpenAlertForKey {
        source: String,
        alert_key: String,
    },
    /// Drop the focused-alert highlight (#651).
    ClearAlertFocus,
    /// Subnet-discovery report (#579) off `state/snmp/discovery` — LWW per
    /// publishing sensor origin; proposals only, nothing auto-adds (#541).
    SnmpDiscoveryReport {
        /// The **origin** that published this report — the sensor's host, not
        /// the evidence source. Named `source` until #940, which is the exact
        /// confusion #1007 was filed about, and the field #940's Adopt needs:
        /// a target set is written to one host's sensor by name.
        origin: String,
        report: zensight_common::DiscoveryReport,
    },
    /// Toggle the SNMP overview's discovery card between the one-line count
    /// and the expanded proposal list (#579).
    ToggleSnmpDiscovery,
    /// Copy a text snippet (e.g. a proposed `devices[]` entry) to the
    /// clipboard (#579).
    CopyText(String),
    /// Open a live H.264 video tile (#409) on a specific `tier`: sends
    /// `open_stream` (codec `h264`, that tier) and spawns the decoding
    /// subscriber on the exact tier key. Fired by the per-tier buttons — each
    /// offered tier is its own button (#494/#502). Opening a different tier for
    /// a stream replaces its single tile. Only functional on builds with the
    /// `h264` feature; otherwise it toasts the build hint.
    ParallaxOpenVideoTile {
        stream: String,
        tier: String,
    },
    /// Hand tier selection back to the controller (#720).
    ///
    /// The counterpart of a manual tier click, which pins the stream. There is
    /// no "turn adaptation off" — a pin *is* off, for the one stream the
    /// operator pinned, and it is expressed by the thing they already did.
    ParallaxAutoTier {
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
    /// Clear the selected device's pivot (#313): the process explorer's
    /// pid filter, or whatever a later pivot carries.
    ClearPivot,
    /// Pivot to the Logs view pre-filtered to one unit *run* (journald
    /// `_SYSTEMD_INVOCATION_ID`), from the unit drill-down's "logs for this run".
    OpenLogsForInvocation {
        unit: String,
        invocation_id: String,
    },
    /// Clear the Logs view's unit-run (invocation id) filter.
    ClearLogsInvocationFilter,
    /// Pivot from a log-sourced alert to the Logs feed pre-filtered to its
    /// context (#558): unit and/or a message pattern (template/sample), optional
    /// min severity, with a breadcrumb naming the source rule.
    PivotToLogsFromAlert {
        rule: String,
        unit: Option<String>,
        pattern: Option<String>,
        severity_min: Option<u8>,
        /// When the alert fired (epoch ms), so the pivot can scope the log
        /// history around it (#603). Without a window the pivot asks only the
        /// sensors' hot rings, so an alert from an hour ago opened an empty
        /// feed.
        at_ms: Option<i64>,
    },
    /// Clear the "filtered from alert <rule>" breadcrumb on the Logs view (#558).
    ClearLogsAlertPivot,
    /// Route a global-search query into the Logs view pre-filtered to that
    /// message pattern (#554) — the "search logs for …" pivot.
    SearchLogsFor(String),
    /// Pivot from a Security anomaly to its netring flows (#119): fetch
    /// `@rpc/netring/flows` and filter to the offending `src`. `key` is the anomaly's
    /// `alert_key` so the result renders under the right row.
    FetchAnomalyFlows {
        key: String,
        src: String,
    },
    /// A flow-pivot reply for anomaly `key`: the filtered flows, or an error.
    AnomalyFlowsReceived(String, Result<Vec<zensight_common::FlowRecord>, String>),
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
        producer: String,
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
        producer: String,
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

    // ── The time cursor (#910) ──────────────────────────────────────────────
    /// The scrubber moved to this instant (epoch ms). Fires per pixel of
    /// travel, so it only records the position — the query is debounced.
    ScrubTo(i64),
    /// The debounce elapsed for this generation: query if it is still current.
    ///
    /// A generation rather than a timestamp, because what has to be compared
    /// is "is this still the gesture in progress", and two scrubs to the same
    /// instant are two gestures.
    ScrubCommit(crate::history::ScrubGeneration),
    /// Return to following the feed.
    ScrubLive,
    /// Timeline markers for the scrubbed window, tagged with the generation
    /// that asked — a reply for an abandoned cursor position is dropped.
    ScrubMarkersLoaded(
        crate::history::ScrubGeneration,
        Vec<zensight_common::history::TimelineEntry>,
    ),

    /// User toggled protocol filter.
    ToggleProducerFilter(String),

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

    /// One chart interaction on the selected device (#1306): the widgets and
    /// the canvas emit `chart::Action`s, `DeviceDetailState::apply_chart`
    /// applies them, and the app acts on the `chart::Effect` it hands back.
    Chart(crate::view::chart::Action),

    /// Promote a metric to an alert rule (#50): seed the rule/expectation form
    /// with this metric + current value and open the authoring view. Netlink
    /// routes to the sentinel expectations; other protocols to local rules.
    PromoteMetricToAlert {
        device: DeviceId,
        metric: String,
        value: f64,
    },

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
    /// Set the live-video frame-age deadline in milliseconds (#716); "0" is off.
    SetMaxLiveLatency(String),

    /// Save settings.
    SaveSettings,

    /// Reset settings to defaults.
    ResetSettings,

    // Alert messages
    /// Open the alerts view.
    OpenAlerts,

    /// Close the alerts view.
    CloseAlerts,

    /// The catalog's liveliness token appeared or vanished (#925).
    ///
    /// The catalog is the only writer of acks and silences, so its absence is
    /// what disables those buttons — with a reason on them, rather than a
    /// button that quietly does nothing.
    CatalogAlive(bool),
    /// `@desired/state/alive` appeared or vanished (#939, routed since #1031).
    ///
    /// The policy controller is the only writer that makes an adoption
    /// **durable**, so its absence is what tells the discovery card to warn
    /// rather than promise (#940). Subscribed by name for the same reason the
    /// catalog's is: `*` cannot match a verbatim `@` chunk.
    DesiredAlive(bool),
    /// Adopt a discovered SNMP device into the monitored set (#940).
    ///
    /// The sweep proposes (#541, propose-only, never auto-adds); this is what
    /// accepts. `origin` is the host whose sensor found it — the target set is
    /// written to that one sensor, and the key is the only thing that says
    /// which. `durable` is whether the policy controller was alive when the
    /// button was drawn: with it, the adoption becomes an override the
    /// controller keeps; without it, an `@rpc` write that lasts until that
    /// sensor restarts.
    AdoptDiscovered {
        origin: String,
        device: Box<zensight_common::DiscoveredDevice>,
        durable: bool,
    },
    /// The current SNMP target set for one origin, in reply to the GET that
    /// an adopt issues first (#940).
    ///
    /// A target set is replaced **wholesale**, so adopting means "the set the
    /// sensor last reported, plus this one" — the same shape #933 uses for
    /// thresholds. Adding one device to a set this build has not seen would
    /// delete every other.
    SnmpTargetsForAdopt {
        origin: String,
        device: Box<zensight_common::DiscoveredDevice>,
        durable: bool,
        current: String,
    },
    /// The `state/snmp/applied/targets` marker for one origin (#936).
    SnmpTargetsApplied {
        origin: String,
        json: String,
    },
    /// One `@catalog/state/ack/*` document arrived (#925).
    AckReceived(Box<zensight_common::ack::AlertAck>),
    /// An ack was tombstoned by the catalog.
    AckRetired(zensight_common::alert::AlertRef),
    /// One `@catalog/state/silence/*` document arrived (#925).
    SilenceReceived(Box<zensight_common::silence::Silence>),
    /// A silence was tombstoned (its window closed, or it was lifted).
    SilenceRetired(String),
    /// One `@catalog/state/incident/*` document arrived (#925).
    ///
    /// The catalog's grouping, which is keyed by **entity** — a host that
    /// publishes under three origins is one incident. The GUI's own
    /// `group_incidents` stays as the offline fallback, keyed by source,
    /// because a GUI with no catalog must still show what is on fire.
    IncidentReceived(Box<zensight_common::incident::Incident>),
    /// An incident was tombstoned — no member is firing any more.
    IncidentRetired(String),
    /// Acknowledge all firing external (sensor-pushed) alerts from one source.
    AcknowledgeExternalSource(String),
    /// Acknowledge all firing external alerts.
    AcknowledgeAllExternal,

    /// Silence (mute) a source for the given duration in ms (#26).
    SilenceSource(String, i64),

    /// The identity panel's "merge into" field changed (#1129).
    MergeTargetChanged(String),
    /// `@catalog` `link?old=…;new=…` (#1129): fuse the host at origin `old`
    /// into the one at `new`. Both are `h-<12hex>` origins, never entity ids.
    LinkHosts {
        old: String,
        new: String,
    },
    /// `@catalog` `unlink?old=…;new=…` (#1129): retract the link that fuses
    /// `old` into `new`, so the catalog stops merging them.
    UnlinkHosts {
        old: String,
        new: String,
    },

    /// Toggle the opt-in desktop-notifications setting (#26) and persist it.
    ToggleDesktopNotifications,
    /// Lift a silence on a source (#26).
    UnsilenceSource(String),

    /// Filter the external-alerts feed by severity (`None` = all) (#27).
    SetAlertSeverityFilter(Option<zensight_common::AlertSeverity>),
    /// Filter the external-alerts feed by source (`None` = all) (#27).
    SetAlertSourceFilter(Option<String>),
    /// Filter the external-alerts feed to one protocol (#582). `None` = all.
    SetAlertProtocolFilter(Option<zensight_common::Protocol>),
    /// Open the Alerts view pre-filtered to a protocol — the overview tiles'
    /// click-through (#582).
    OpenAlertsForProtocol(zensight_common::Protocol),
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
    /// A Ready tree artifact was verified pre-download (root-fetched index +
    /// holder probe) — or the verification failed, before any folder picker
    /// opened or any chunk moved.
    ArtifactTreeVerified(Result<crate::view::artifact_fetch::TreeVerify, String>),
    /// The operator confirmed the verified tree — open the folder picker.
    ArtifactTreeConfirmed,
    /// The destination-folder picker resolved for a confirmed tree artifact
    /// (`None` = the user cancelled). Blobs never pick a folder — they stage
    /// to a temp dir then a Save-as dialog.
    ArtifactTreeDestChosen {
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
    ArtifactRequested(Result<Vec<zensight_common::ArtifactState>, String>),
    /// The operator picked which host's artifact to download (index into the
    /// `PickingHolder` state's holder list).
    ArtifactHolderChosen(usize),
    /// Streaming download progress (units resolved / total).
    ArtifactProgress {
        /// Units resolved so far.
        got: u64,
        /// Total units.
        total: u64,
    },
    /// The transfer entered its verify/materialize phase (#624): zblob emits
    /// `Progress::Verifying` after a tree download's last chunk, before
    /// `reconstruct_tree` — which can take a while on a large snapshot.
    ArtifactVerifying,
    /// The artifact finished downloading (a temp file for a blob, the chosen
    /// folder for a tree), or failed.
    ArtifactDownloaded(Result<std::path::PathBuf, String>),
    /// Outcome of the "Save as…" dialog for a downloaded blob artifact.
    ArtifactSaved(Result<Option<String>, String>),
    /// Outcome of tagging a downloaded snapshot in the local chunk cache
    /// (keeps its chunks warm for re-download dedup; log-only either way).
    BlobCacheTagged(Result<(), String>),
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

    /// One groups-panel interaction (#1306): `GroupsState::update` applies
    /// it and says whether the set must be persisted.
    Groups(crate::view::groups::Action),

    // Overview messages
    /// Select a producer tab for the overview section.
    SelectOverviewProducer(String),

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

    /// Collapse/expand the host identity details (facts + resolution group) in
    /// the merged host nav bar (#350). Persisted.
    ToggleIdentityDetails,

    /// Open the global bandwidth monitor pre-scoped to one host (#351).
    OpenBandwidthForHost(String),

    /// Clear the bandwidth monitor's host scope (#351).
    ClearBandwidthHostFilter,

    /// One logs-feed interaction (#1306): `SyslogFilterState::update` applies
    /// it and says whether the app must fetch history or an older page.
    Logs(crate::view::specialized::syslog::Action),

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

/// One reconnect's view of the catalog's three operator-authored classes
/// (#1116). Boxed into [`Message::CatalogSeed`] because a message enum's size
/// is every variant's.
#[derive(Debug, Clone, Default)]
pub struct CatalogSnapshot {
    pub acks: Vec<zensight_common::ack::AlertAck>,
    pub silences: Vec<zensight_common::silence::Silence>,
    pub incidents: Vec<zensight_common::incident::Incident>,
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
    /// The producer that published this device — chunk 4 of every key it
    /// publishes, read from the key. A **name**, not the closed `Protocol`
    /// enum (#1256): a producer the GUI was not compiled with gets a device
    /// like any other; the bespoke views ask [`DeviceId::protocol`] for the
    /// enum and fall back to the generic rendering when it is not one.
    pub producer: String,
    /// The publishing host's v1 origin (`h-<12hex>`) — chunk 3 of every key it
    /// publishes. Read from the key, never from the payload.
    pub origin: String,
    /// The payload's `source`: the publisher's own hostname for a host sensor,
    /// the polled device's name for a proxy sensor.
    pub source: String,
}

/// The placeholder origin worn by every [`DeviceId::fixture`].
///
/// Must be a **real** origin (`h-` + 12 hex): since #485 the drill-down key
/// builders take a parsed [`RemoteOrigin`], so a fixture that cannot parse
/// would make every fixture-built call key silently unroutable — which is
/// exactly what the old spelling (`h-fixture0000`, not hex) did, hidden by a
/// fallback that produced a matches-nothing key.
pub const FIXTURE_ORIGIN: &str = "h-facade000000";

impl DeviceId {
    /// This device's origin as the typed address a caller needs (#485).
    ///
    /// The parse happens here, once, at the boundary where an origin enters
    /// the GUI from a wire key — so the `@rpc` builders can take a type that
    /// cannot be confused with this process's own origin, and a malformed
    /// origin surfaces as `None` instead of a key that matches nothing.
    pub fn remote_origin(&self) -> Option<zenkey::RemoteOrigin> {
        zenkey::RemoteOrigin::parse(&self.origin).ok()
    }

    pub fn new(
        producer: impl Into<String>,
        origin: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        Self {
            producer: producer.into(),
            origin: origin.into(),
            source: source.into(),
        }
    }

    /// A device from an unspecified host — **fixtures only** (tests, mock data,
    /// the demo simulator's static environment). Every such device gets the same
    /// placeholder origin, so `(producer, source)` still tells them apart.
    ///
    /// Real code never calls this: the origin arrives on the key, and inventing
    /// one is the bug this type exists to prevent (#474). The placeholder is
    /// deliberately not a valid minted id, so if one ever escapes onto the wire
    /// it fails the grammar rather than quietly addressing a host that isn't
    /// there.
    pub fn fixture(producer: impl Into<String>, source: impl Into<String>) -> Self {
        Self::new(producer, FIXTURE_ORIGIN, source)
    }

    /// The origin comes from the **key** (chunk 3), not the payload — a
    /// `TelemetryPoint` has never carried one, which is exactly why the GUI used
    /// to need a `source -> origin` side map fed out-of-band from health docs.
    /// It is on every sample; it was simply being thrown away at decode. So
    /// does the producer, since #1255 (chunk 4).
    ///
    /// Infallible since #1256: a producer outside the closed `Protocol` enum
    /// is a device too. Before, it was dropped here — the first of the gates
    /// the system-view ratchet (#1254) counts.
    pub fn from_reading(reading: &Reading) -> Self {
        Self {
            producer: reading.producer.clone(),
            origin: reading.origin.clone(),
            source: reading.point.source.clone(),
        }
    }

    /// The closed enum this producer maps to, when it is one the GUI was
    /// compiled with. **Bespoke sites only** — a specialized view, a tab
    /// prefetch, an icon. Everything generic keys on [`DeviceId::producer`].
    pub fn protocol(&self) -> Option<Protocol> {
        self.producer.parse().ok()
    }

    /// `self.protocol() == Some(p)` — for the `== Protocol::X` probes.
    pub fn is(&self, p: Protocol) -> bool {
        self.protocol() == Some(p)
    }

    /// The producer's human label: the enum's `display_name` for a known
    /// producer (`Logs`, `PVE`, `BMC`), the producer name verbatim otherwise.
    pub fn display_name(&self) -> String {
        producer_display_name(&self.producer)
    }
}

/// The human label for a producer name — see [`DeviceId::display_name`].
pub fn producer_display_name(producer: &str) -> String {
    match producer.parse::<Protocol>() {
        Ok(p) => p.display_name().to_string(),
        Err(()) => producer.to_string(),
    }
}

impl std::fmt::Display for DeviceId {
    /// Human-facing: the hostname is what an operator recognises, so it stays
    /// the label. The origin is an *address*, not a name (RFC 06 §1).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.producer, self.source)
    }
}

#[cfg(test)]
mod origin_tests {
    use super::*;

    /// #485: every fixture must carry an origin a caller can actually
    /// address. The old placeholder (`h-fixture0000`) was not hex, so it
    /// never parsed — and the pre-#485 builder quietly hand-spelled a key
    /// that matched nothing, which is why no test ever noticed that every
    /// fixture-built drill-down was aimed at nobody.
    #[test]
    fn fixture_origin_is_addressable() {
        assert!(
            zenkey::RemoteOrigin::parse(FIXTURE_ORIGIN).is_ok(),
            "{FIXTURE_ORIGIN} must parse as a real origin"
        );
        let id = DeviceId::fixture("sysinfo", "web01".to_string());
        assert!(id.remote_origin().is_some());
    }

    /// A device whose origin is junk yields no callee address — the caller
    /// then falls back to the fleet selector instead of building a key aimed
    /// at nobody.
    #[test]
    fn junk_origin_is_not_addressable() {
        let id = DeviceId::new("sysinfo", "not-an-origin", "web01");
        assert!(id.remote_origin().is_none());
    }
}
