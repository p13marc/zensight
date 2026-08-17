//! Runtime detection-tuning control channel (#121).
//!
//! Mirrors the netlink sentinel's `command.rs`: a `(write + read queryable)`
//! loop lets the GUI tune anomaly detection without restarting the sensor —
//! add/remove allowlist entries, mute/unmute a detector, and adjust a
//! detector's threshold. The live config lives behind a lock-free
//! [`arc_swap::ArcSwap`] that the hot-path detectors read (see `monitor.rs`).
//!
//! Keys (via `zensight-common`):
//! - write: `@rpc/netring/detectors/set`  (a [`DetectorCommand`])
//! - read:  `@rpc/netring/detectors`      (the current `AnomalyConfig`)
//!
//! Scope note: a detector that was **off at startup is not built into the
//! capture pipeline**, so enabling it takes effect on the next restart. Tuning
//! (allowlist / threshold) and muting/unmuting a built detector are immediate.

use std::sync::Arc;

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use zensight_common::command::{command_key, status_key};

use crate::config::{AnomalyConfig, IocConfig};

/// The control topic under `@rpc/netring/` (write `…/set`, read bare).
pub const DETECTORS_TOPIC: &str = "detectors";

/// The capture-focus control topic (netring 0.28, issue #225).
pub const CAPTURE_FILTER_TOPIC: &str = "capture_filter";

/// The threat-intel (IOC / YARA) hot-reload control topic (#328).
pub const THREAT_INTEL_TOPIC: &str = "threat_intel";

/// The capture-to-disk control topic (#327).
pub const CAPTURE_DISK_TOPIC: &str = "capture_disk";

/// A runtime capture-focus command (tagged JSON), applied to the reloadable
/// packet-tier subscription via netring's `ReloadHandle::set_packet_filter`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CaptureFilterCommand {
    /// Narrow (or replace) the live packet filter with this `.expr()` / BPF
    /// expression — e.g. `"host 10.0.0.5 and port 443"`. Validated before swap.
    SetPacketFilter { expr: String },
    /// Restore the configured base filter (revert an in-incident narrow).
    ClearPacketFilter,
}

/// The capture-focus status served on `@rpc/netring/capture_filter` (#225) so the
/// GUI can show what is live and surface a friendly error for a bad expression.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CaptureFilterStatus {
    /// Whether capture focus is armed (a reloadable packet sub exists).
    pub enabled: bool,
    /// Number of reloadable packet-tier filters (`packet_filter_count()`).
    pub reloadable: usize,
    /// The currently-applied filter expression (the base when not narrowed).
    pub current: String,
    /// The configured base filter, restored by `clear_packet_filter`.
    pub base: String,
    /// Last validation error, if the most recent `set_packet_filter` was
    /// rejected (the previous filter stayed live). `None` once a valid one lands.
    pub last_error: Option<String>,
}

/// A lock-free, cheaply-cloneable handle to the live [`AnomalyConfig`]. The
/// monitor's detectors hold a clone and `load()` the current config per scored
/// candidate; the command loop `store`s a new `Arc` on each change.
#[derive(Clone)]
pub struct DetectorHandle {
    cfg: Arc<ArcSwap<AnomalyConfig>>,
}

impl DetectorHandle {
    /// Seed the handle from the startup config.
    pub fn new(cfg: AnomalyConfig) -> Self {
        Self {
            cfg: Arc::new(ArcSwap::from_pointee(cfg)),
        }
    }

    /// The shared cell the hot-path detectors read from (`load()` per use).
    pub fn shared(&self) -> Arc<ArcSwap<AnomalyConfig>> {
        self.cfg.clone()
    }

    /// A clone of the current config (serves the status queryable).
    pub fn snapshot(&self) -> AnomalyConfig {
        AnomalyConfig::clone(&self.cfg.load())
    }

    /// Apply a command by mutating a copy of the current config and swapping it
    /// in atomically. Returns the new config (post-change) for logging/tests.
    pub fn apply(&self, cmd: DetectorCommand) -> AnomalyConfig {
        let mut next = self.snapshot();
        apply_to(&mut next, cmd);
        self.cfg.store(Arc::new(next.clone()));
        next
    }
}

/// A runtime detection-tuning command (tagged JSON, mirroring the sentinel).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DetectorCommand {
    /// Replace the whole anomaly config (the GUI's "apply all").
    Replace(AnomalyConfig),
    /// Mute/unmute a detector by name (see [`detector_names`]).
    SetEnabled { detector: String, enabled: bool },
    /// Set a detector's threshold (ignored for detectors without one).
    SetThreshold { detector: String, value: f64 },
    /// Replace the allowlist wholesale.
    SetAllowlist { entries: Vec<String> },
    /// Add one allowlist entry (no-op if already present).
    AddAllowlist { entry: String },
    /// Remove one allowlist entry (no-op if absent).
    RemoveAllowlist { entry: String },
}

/// The tunable detector names accepted by `SetEnabled` / `SetThreshold`, paired
/// with whether the detector has a threshold. Drives the GUI panel and keeps the
/// names in one place.
pub fn detector_names() -> &'static [(&'static str, bool)] {
    &[
        ("port_scan", false),
        ("beaconing", true),
        ("rita_beacon", true),
        // Shares `rita_beacon_threshold` — mute/unmute only, no own threshold.
        ("rita_beacon_fqdn", false),
        ("dns_tunnel", false),
        ("nod", false),
        ("connection_flood", true),
        ("dga", true),
        ("lateral_movement", false),
        ("data_exfil", true),
    ]
}

/// Mutate `cfg` in place per `cmd`. Pure — the unit of testing for the handler.
pub fn apply_to(cfg: &mut AnomalyConfig, cmd: DetectorCommand) {
    match cmd {
        DetectorCommand::Replace(new) => *cfg = new,
        DetectorCommand::SetEnabled { detector, enabled } => match detector.as_str() {
            "port_scan" => cfg.port_scan = enabled,
            "beaconing" => cfg.beaconing = enabled,
            "rita_beacon" => cfg.rita_beacon = enabled,
            "rita_beacon_fqdn" => cfg.rita_beacon_fqdn = enabled,
            "dns_tunnel" => cfg.dns_tunnel = enabled,
            "nod" => cfg.nod = enabled,
            "connection_flood" => cfg.connection_flood = enabled,
            "dga" => cfg.dga = enabled,
            "lateral_movement" => cfg.lateral_movement = enabled,
            "data_exfil" => cfg.data_exfil = enabled,
            other => tracing::warn!(detector = %other, "netring: unknown detector in SetEnabled"),
        },
        DetectorCommand::SetThreshold { detector, value } => match detector.as_str() {
            "beaconing" => cfg.beacon_threshold = value,
            "rita_beacon" => cfg.rita_beacon_threshold = value,
            "connection_flood" => cfg.flood_threshold = value.max(0.0) as u64,
            "dga" => cfg.dga_threshold = value,
            // The exfil "threshold" is its sigma multiplier.
            "data_exfil" => cfg.exfil_sigma = value,
            other => {
                tracing::warn!(detector = %other, "netring: SetThreshold for detector without a threshold")
            }
        },
        DetectorCommand::SetAllowlist { entries } => cfg.allowlist = normalize_allowlist(entries),
        DetectorCommand::AddAllowlist { entry } => {
            let entry = entry.trim().to_string();
            if !entry.is_empty() && !cfg.allowlist.iter().any(|e| e == &entry) {
                cfg.allowlist.push(entry);
            }
        }
        DetectorCommand::RemoveAllowlist { entry } => {
            let entry = entry.trim();
            cfg.allowlist.retain(|e| e != entry);
        }
    }
}

/// Trim, drop empties, and de-duplicate allowlist entries (order-preserving).
fn normalize_allowlist(entries: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(entries.len());
    for e in entries {
        let e = e.trim().to_string();
        if !e.is_empty() && !out.iter().any(|x| x == &e) {
            out.push(e);
        }
    }
    out
}

/// Run the command subscriber + status queryable until the session closes.
pub async fn run(session: Arc<zenoh::Session>, producer: String, handle: DetectorHandle) {
    let cmd_key = command_key(&producer, DETECTORS_TOPIC);
    let stat_key = status_key(&producer, DETECTORS_TOPIC);

    let subscriber = match zensight_common::served::serve_queryable(&session, &cmd_key).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, key = %cmd_key, "netring: failed to subscribe to detector commands");
            return;
        }
    };
    let queryable = match zensight_common::served::serve_queryable(&session, &stat_key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %stat_key, "netring: failed to declare detector status queryable");
            return;
        }
    };
    tracing::info!(commands = %cmd_key, status = %stat_key, "netring: detection-tuning channel ready");

    loop {
        tokio::select! {
            query = subscriber.recv_async() => {
                match query {
                    Ok(query) => {
                        let payload = query
                            .payload()
                            .map(|p| p.to_bytes().to_vec())
                            .unwrap_or_default();
                        match serde_json::from_slice::<DetectorCommand>(&payload) {
                            Ok(cmd) => {
                                let next = handle.apply(cmd);
                                tracing::info!(allowlist = next.allowlist.len(), "netring: detector config updated");
                                ack(&query, &cmd_key).await;
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "netring: bad detector command");
                                nack_invalid(&query, &e.to_string()).await;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "netring: detector command subscriber ended");
                        return;
                    }
                }
            }
            query = queryable.recv_async() => {
                match query {
                    Ok(query) => {
                        let snapshot = handle.snapshot();
                        match serde_json::to_vec(&snapshot) {
                            Ok(payload) => {
                                if let Err(e) = query.reply(stat_key.as_str(), payload).await {
                                    tracing::warn!(error = %e, "netring: failed to reply to detector status query");
                                }
                            }
                            Err(e) => tracing::warn!(error = %e, "netring: failed to serialize detector status"),
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "netring: detector status queryable ended");
                        return;
                    }
                }
            }
        }
    }
}

/// Run the capture-focus command subscriber + status queryable (#225) until the
/// session closes. `reload` is netring's handle; `base_expr` is the configured
/// default restored by `clear_packet_filter`. The packet filter lives at index 0
/// (our single capture-focus subscription). Validation happens in
/// `set_packet_filter` (parse-before-swap), so a bad expression becomes a status
/// error and the previous filter keeps running — never a panic or dropped capture.
pub async fn run_capture_filter(
    session: Arc<zenoh::Session>,
    producer: String,
    reload: netring::monitor::ReloadHandle,
    base_expr: String,
) {
    let cmd_key = command_key(&producer, CAPTURE_FILTER_TOPIC);
    let stat_key = status_key(&producer, CAPTURE_FILTER_TOPIC);

    let subscriber = match zensight_common::served::serve_queryable(&session, &cmd_key).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, key = %cmd_key, "netring: failed to subscribe to capture-filter commands");
            return;
        }
    };
    let queryable = match zensight_common::served::serve_queryable(&session, &stat_key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %stat_key, "netring: failed to declare capture-filter status queryable");
            return;
        }
    };
    tracing::info!(commands = %cmd_key, status = %stat_key, "netring: capture-focus channel ready");

    let mut current = base_expr.clone();
    let mut last_error: Option<String> = None;

    loop {
        tokio::select! {
            query = subscriber.recv_async() => {
                let Ok(query) = query else {
                    tracing::warn!("netring: capture-filter command subscriber ended");
                    return;
                };
                let payload = query
                    .payload()
                    .map(|p| p.to_bytes().to_vec())
                    .unwrap_or_default();
                match serde_json::from_slice::<CaptureFilterCommand>(&payload) {
                    Ok(CaptureFilterCommand::SetPacketFilter { expr }) => {
                        apply_filter(&reload, &expr, &mut current, &mut last_error);
                        ack(&query, &cmd_key).await;
                    }
                    Ok(CaptureFilterCommand::ClearPacketFilter) => {
                        let base = base_expr.clone();
                        apply_filter(&reload, &base, &mut current, &mut last_error);
                        ack(&query, &cmd_key).await;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "netring: bad capture-filter command");
                        nack_invalid(&query, &e.to_string()).await;
                    }
                }
            }
            query = queryable.recv_async() => {
                let Ok(query) = query else {
                    tracing::warn!("netring: capture-filter status queryable ended");
                    return;
                };
                let status = CaptureFilterStatus {
                    enabled: reload.packet_filter_count() > 0,
                    reloadable: reload.packet_filter_count(),
                    current: current.clone(),
                    base: base_expr.clone(),
                    last_error: last_error.clone(),
                };
                match serde_json::to_vec(&status) {
                    Ok(payload) => {
                        if let Err(e) = query.reply(stat_key.as_str(), payload).await {
                            tracing::warn!(error = %e, "netring: failed to reply to capture-filter status query");
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "netring: failed to serialize capture-filter status"),
                }
            }
        }
    }
}

/// Apply `expr` to the packet filter at index 0, updating `current`/`last_error`.
/// A parse error or absent filter leaves the live filter untouched (fail-safe).
fn apply_filter(
    reload: &netring::monitor::ReloadHandle,
    expr: &str,
    current: &mut String,
    last_error: &mut Option<String>,
) {
    match reload.set_packet_filter(0, expr) {
        Ok(true) => {
            *current = expr.to_string();
            *last_error = None;
            tracing::info!(filter = %expr, "netring: capture filter hot-reloaded");
        }
        Ok(false) => {
            *last_error = Some("no reloadable packet filter (capture_focus disabled)".to_string());
            tracing::warn!("netring: set_packet_filter with no reloadable filter");
        }
        Err(e) => {
            *last_error = Some(format!("invalid filter: {e}"));
            tracing::warn!(error = %e, filter = %expr, "netring: rejected invalid capture filter");
        }
    }
}

/// A runtime threat-intel command (tagged JSON) on `@rpc/netring/threat_intel/set`
/// (#328), applied to the monitor's live IOC / YARA matchers via `ReloadHandle`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThreatIntelCommand {
    /// Replace the inline IOC indicators (the configured indicator *files* are
    /// kept and re-read on apply). A full replace of the inline lists.
    SetIoc {
        #[serde(default)]
        ips: Vec<String>,
        #[serde(default)]
        domains: Vec<String>,
        #[serde(default)]
        ja4: Vec<String>,
        #[serde(default)]
        ja3: Vec<String>,
    },
    /// Re-read the configured indicator files and re-apply (external-feed refresh).
    ReloadIocFiles,
    /// Clear all live IOC indicators (apply an empty set).
    ClearIoc,
    /// Compile and hot-swap YARA rules (needs the `yara` build feature). A
    /// compile error is returned in the status reply and the live rules stay put.
    SetYara { rules: String },
}

/// The threat-intel status served on `@rpc/netring/threat_intel` (#328) so the GUI
/// can show what is armed / loaded and surface a YARA compile error.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThreatIntelStatus {
    /// IOC reload is armed (monitor built with `ioc(..)` — needs `threat.reload`
    /// or startup indicators). `set_ioc` is a no-op when false.
    pub ioc_armed: bool,
    /// Live IOC indicator count (all kinds) after the last apply.
    pub ioc_total: usize,
    /// The configured indicator files re-read by `reload_ioc_files`.
    pub ioc_files: Vec<String>,
    /// YARA reload is armed (built `--features yara` + `yara(..)`).
    pub yara_armed: bool,
    /// Outcome of the last reload attempt (`"ok: ..."` / `"error: ..."`), if any.
    pub last_reload: Option<String>,
}

/// Runtime IOC / YARA hot-reload channel (#328): a `(subscriber + queryable)`
/// loop that swaps the monitor's live matchers through its [`ReloadHandle`]
/// without a capture restart. Mirrors [`run_capture_filter`]. A bad YARA source
/// becomes a status error and the previous rules keep scanning — never a panic.
pub async fn run_threat_intel(
    session: Arc<zenoh::Session>,
    producer: String,
    reload: netring::monitor::ReloadHandle,
    startup_ioc: IocConfig,
) {
    let cmd_key = command_key(&producer, THREAT_INTEL_TOPIC);
    let stat_key = status_key(&producer, THREAT_INTEL_TOPIC);

    let subscriber = match zensight_common::served::serve_queryable(&session, &cmd_key).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, key = %cmd_key, "netring: failed to subscribe to threat-intel commands");
            return;
        }
    };
    let queryable = match zensight_common::served::serve_queryable(&session, &stat_key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %stat_key, "netring: failed to declare threat-intel status queryable");
            return;
        }
    };
    tracing::info!(commands = %cmd_key, status = %stat_key, "netring: threat-intel reload channel ready");

    // The live IOC config the loop mutates and re-applies; seeded from startup.
    let mut live_ioc = startup_ioc;
    let mut ioc_total = apply_ioc(&reload, &live_ioc);
    let mut last_reload: Option<String> = None;

    loop {
        tokio::select! {
            query = subscriber.recv_async() => {
                let Ok(query) = query else {
                    tracing::warn!("netring: threat-intel command subscriber ended");
                    return;
                };
                let payload = query
                    .payload()
                    .map(|p| p.to_bytes().to_vec())
                    .unwrap_or_default();
                match serde_json::from_slice::<ThreatIntelCommand>(&payload) {
                    Ok(cmd) => {
                        let outcome = apply_threat_intel(&reload, &mut live_ioc, &mut ioc_total, cmd);
                        last_reload = Some(outcome);
                        ack(&query, &cmd_key).await;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "netring: bad threat-intel command");
                        nack_invalid(&query, &e.to_string()).await;
                    }
                }
            }
            query = queryable.recv_async() => {
                let Ok(query) = query else {
                    tracing::warn!("netring: threat-intel status queryable ended");
                    return;
                };
                let status = ThreatIntelStatus {
                    ioc_armed: reload.has_ioc(),
                    ioc_total,
                    ioc_files: live_ioc.files.clone(),
                    yara_armed: threat_intel_yara_armed(&reload),
                    last_reload: last_reload.clone(),
                };
                match serde_json::to_vec(&status) {
                    Ok(payload) => {
                        if let Err(e) = query.reply(stat_key.as_str(), payload).await {
                            tracing::warn!(error = %e, "netring: failed to reply to threat-intel status query");
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "netring: failed to serialize threat-intel status"),
                }
            }
        }
    }
}

/// Build `live_ioc` into an `IocSet` and swap it live. Returns the applied count
/// (0 when the monitor wasn't armed with `ioc(..)`, i.e. `set_ioc` no-op).
fn apply_ioc(reload: &netring::monitor::ReloadHandle, live_ioc: &IocConfig) -> usize {
    let set = crate::monitor::build_ioc_set(live_ioc);
    let total = set.len();
    if reload.set_ioc(set) { total } else { 0 }
}

/// Apply a [`ThreatIntelCommand`], mutating `live_ioc`/`ioc_total` and returning a
/// human-readable outcome for the status reply.
fn apply_threat_intel(
    reload: &netring::monitor::ReloadHandle,
    live_ioc: &mut IocConfig,
    ioc_total: &mut usize,
    cmd: ThreatIntelCommand,
) -> String {
    match cmd {
        ThreatIntelCommand::SetIoc {
            ips,
            domains,
            ja4,
            ja3,
        } => {
            live_ioc.ips = ips;
            live_ioc.domains = domains;
            live_ioc.ja4 = ja4;
            live_ioc.ja3 = ja3;
            apply_ioc_reporting(reload, live_ioc, ioc_total, "set_ioc")
        }
        ThreatIntelCommand::ReloadIocFiles => {
            apply_ioc_reporting(reload, live_ioc, ioc_total, "reload_ioc_files")
        }
        ThreatIntelCommand::ClearIoc => {
            *live_ioc = IocConfig::default();
            apply_ioc_reporting(reload, live_ioc, ioc_total, "clear_ioc")
        }
        ThreatIntelCommand::SetYara { rules } => apply_yara(reload, &rules),
    }
}

/// Shared apply+report for the three IOC verbs.
fn apply_ioc_reporting(
    reload: &netring::monitor::ReloadHandle,
    live_ioc: &IocConfig,
    ioc_total: &mut usize,
    verb: &str,
) -> String {
    if !reload.has_ioc() {
        return format!(
            "error: {verb} ignored — IOC reload not armed (set threat.reload=true or provide startup indicators)"
        );
    }
    *ioc_total = apply_ioc(reload, live_ioc);
    tracing::info!(verb, count = *ioc_total, "netring: IOC set hot-reloaded");
    format!("ok: {verb} applied {} indicators", *ioc_total)
}

/// A capture-to-disk command (tagged JSON) on `@rpc/netring/capture_disk/set` (#327),
/// applied to the disk engine via its [`crate::disk::CaptureDiskHandle`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CaptureDiskCommand {
    /// Manual trigger: in `triggered` mode fire the pre-trigger ring now (with
    /// an optional operator tag recorded on the capture); in `rotating` mode
    /// finalize the current spool file.
    CaptureNow {
        #[serde(default)]
        tag: Option<String>,
    },
    /// Hot-switch the live mode. Only effective when capture-to-disk was armed
    /// at startup (`capture.to_disk.mode != off` — the packet subscription is a
    /// build-time decision, like the detector registry).
    SetCapture {
        mode: crate::config::CaptureDiskMode,
    },
}

/// The capture-to-disk status served on `@rpc/netring/capture_disk` (#327): the
/// live mode, ring occupancy, retention usage and the last lifecycle event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CaptureDiskStatus {
    /// Whether capture-to-disk was armed at startup (a packet sub exists).
    pub armed: bool,
    /// Live mode: `off` / `rotating` / `triggered`.
    pub mode: String,
    /// True while a triggered capture streams its post-trigger window.
    pub recording: bool,
    /// Pre-trigger ring occupancy (triggered mode; zero otherwise).
    pub ring_packets: u64,
    pub ring_bytes: u64,
    /// Retention usage vs the configured caps.
    pub retained_files: u64,
    pub retained_bytes: u64,
    pub max_files: u64,
    pub max_total_bytes: u64,
    /// Frames dropped on the engine channel + files evicted by retention +
    /// triggers accepted, since start.
    pub dropped: u64,
    pub evictions: u64,
    pub triggers: u64,
    /// Human-readable last lifecycle event (trigger fired / capture ready / …).
    pub last_event: Option<String>,
}

/// Run the capture-to-disk command subscriber + status queryable (#327) until
/// the session closes. Mirrors [`run_threat_intel`]: bad commands are logged
/// (never a panic), the status reply always reflects the live engine state.
pub async fn run_capture_disk(
    session: Arc<zenoh::Session>,
    producer: String,
    handle: crate::disk::CaptureDiskHandle,
    max_files: u64,
    max_total_bytes: u64,
) {
    use std::sync::atomic::Ordering;

    let cmd_key = command_key(&producer, CAPTURE_DISK_TOPIC);
    let stat_key = status_key(&producer, CAPTURE_DISK_TOPIC);

    let subscriber = match zensight_common::served::serve_queryable(&session, &cmd_key).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, key = %cmd_key, "netring: failed to subscribe to capture-disk commands");
            return;
        }
    };
    let queryable = match zensight_common::served::serve_queryable(&session, &stat_key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %stat_key, "netring: failed to declare capture-disk status queryable");
            return;
        }
    };
    tracing::info!(commands = %cmd_key, status = %stat_key, "netring: capture-to-disk channel ready");

    loop {
        tokio::select! {
            query = subscriber.recv_async() => {
                let Ok(query) = query else {
                    tracing::warn!("netring: capture-disk command subscriber ended");
                    return;
                };
                let payload = query
                    .payload()
                    .map(|p| p.to_bytes().to_vec())
                    .unwrap_or_default();
                match serde_json::from_slice::<CaptureDiskCommand>(&payload) {
                    Ok(CaptureDiskCommand::CaptureNow { tag }) => {
                        handle.capture_now(tag);
                        ack(&query, &cmd_key).await;
                    }
                    Ok(CaptureDiskCommand::SetCapture { mode }) => {
                        handle.set_mode(mode);
                        ack(&query, &cmd_key).await;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "netring: bad capture-disk command");
                        nack_invalid(&query, &e.to_string()).await;
                    }
                }
            }
            query = queryable.recv_async() => {
                let Ok(query) = query else {
                    tracing::warn!("netring: capture-disk status queryable ended");
                    return;
                };
                let stats = handle.stats();
                let status = CaptureDiskStatus {
                    armed: true,
                    mode: match stats.mode() {
                        crate::config::CaptureDiskMode::Off => "off",
                        crate::config::CaptureDiskMode::Rotating => "rotating",
                        crate::config::CaptureDiskMode::Triggered => "triggered",
                    }
                    .to_string(),
                    recording: stats.recording.load(Ordering::Relaxed),
                    ring_packets: stats.ring_packets.load(Ordering::Relaxed),
                    ring_bytes: stats.ring_bytes.load(Ordering::Relaxed),
                    retained_files: stats.retained_files.load(Ordering::Relaxed),
                    retained_bytes: stats.retained_bytes.load(Ordering::Relaxed),
                    max_files,
                    max_total_bytes,
                    dropped: stats.dropped.load(Ordering::Relaxed),
                    evictions: stats.evictions.load(Ordering::Relaxed),
                    triggers: stats.triggers.load(Ordering::Relaxed),
                    last_event: stats.last_event.lock().ok().and_then(|l| l.clone()),
                };
                match serde_json::to_vec(&status) {
                    Ok(payload) => {
                        if let Err(e) = query.reply(stat_key.as_str(), payload).await {
                            tracing::warn!(error = %e, "netring: failed to reply to capture-disk status query");
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "netring: failed to serialize capture-disk status"),
                }
            }
        }
    }
}

#[cfg(feature = "yara")]
fn threat_intel_yara_armed(reload: &netring::monitor::ReloadHandle) -> bool {
    reload.has_yara()
}

#[cfg(not(feature = "yara"))]
fn threat_intel_yara_armed(_reload: &netring::monitor::ReloadHandle) -> bool {
    false
}

#[cfg(feature = "yara")]
fn apply_yara(reload: &netring::monitor::ReloadHandle, rules: &str) -> String {
    if !reload.has_yara() {
        return "error: set_yara ignored — YARA reload not armed (set threat.reload=true or threat.yara.file)".to_string();
    }
    match netring::monitor::yara::YaraRules::compile(rules) {
        Ok(compiled) => {
            reload.set_yara(compiled);
            tracing::info!("netring: YARA rules hot-reloaded");
            "ok: yara rules compiled and applied".to_string()
        }
        Err(e) => {
            tracing::warn!(error = %e, "netring: rejected invalid YARA rules (kept previous)");
            format!("error: yara compile failed: {e}")
        }
    }
}

#[cfg(not(feature = "yara"))]
fn apply_yara(_reload: &netring::monitor::ReloadHandle, _rules: &str) -> String {
    "error: set_yara ignored — sensor built without the `yara` feature".to_string()
}

/// Ack a write procedure: empty value reply on the concrete key (RFC 05 §3).
async fn ack(query: &zenoh::query::Query, key: &str) {
    if let Err(e) = query.reply(key, Vec::<u8>::new()).await {
        tracing::warn!(error = %e, "netring: failed to ack command");
    }
}

/// Refuse a write with `error/invalid-args` via reply_err (RFC 05 §3).
async fn nack_invalid(query: &zenoh::query::Query, message: &str) {
    let err = zensight_sensor_core::rpc::RpcError::invalid_args(message);
    let payload = serde_json::to_vec(&err).unwrap_or_default();
    if let Err(e) = query.reply_err(payload).await {
        tracing::warn!(error = %e, "netring: failed to reply_err");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threat_intel_command_wire_format() {
        // Pin the JSON the GUI (#328) sends on @rpc/netring/threat_intel/set.
        let set: ThreatIntelCommand = serde_json::from_str(
            r#"{"type":"set_ioc","ips":["198.51.100.7"],"domains":["malware.test"]}"#,
        )
        .unwrap();
        assert_eq!(
            set,
            ThreatIntelCommand::SetIoc {
                ips: vec!["198.51.100.7".into()],
                domains: vec!["malware.test".into()],
                ja4: vec![],
                ja3: vec![],
            }
        );
        let reload: ThreatIntelCommand =
            serde_json::from_str(r#"{"type":"reload_ioc_files"}"#).unwrap();
        assert_eq!(reload, ThreatIntelCommand::ReloadIocFiles);
        let clear: ThreatIntelCommand = serde_json::from_str(r#"{"type":"clear_ioc"}"#).unwrap();
        assert_eq!(clear, ThreatIntelCommand::ClearIoc);
        let yara: ThreatIntelCommand =
            serde_json::from_str(r#"{"type":"set_yara","rules":"rule r { condition: true }"}"#)
                .unwrap();
        assert_eq!(
            yara,
            ThreatIntelCommand::SetYara {
                rules: "rule r { condition: true }".into()
            }
        );
    }

    #[test]
    fn threat_intel_status_roundtrips() {
        let status = ThreatIntelStatus {
            ioc_armed: true,
            ioc_total: 3,
            ioc_files: vec!["/etc/zensight/iocs.txt".into()],
            yara_armed: false,
            last_reload: Some("ok: set_ioc applied 3 indicators".into()),
        };
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(
            serde_json::from_str::<ThreatIntelStatus>(&json).unwrap(),
            status
        );
    }

    #[test]
    fn capture_disk_command_wire_format() {
        // Pin the JSON the GUI (#327) sends on @rpc/netring/capture_disk/set.
        let now: CaptureDiskCommand =
            serde_json::from_str(r#"{"type":"capture_now","tag":"incident-42"}"#).unwrap();
        assert_eq!(
            now,
            CaptureDiskCommand::CaptureNow {
                tag: Some("incident-42".into())
            }
        );
        let bare: CaptureDiskCommand = serde_json::from_str(r#"{"type":"capture_now"}"#).unwrap();
        assert_eq!(bare, CaptureDiskCommand::CaptureNow { tag: None });
        let set: CaptureDiskCommand =
            serde_json::from_str(r#"{"type":"set_capture","mode":"triggered"}"#).unwrap();
        assert_eq!(
            set,
            CaptureDiskCommand::SetCapture {
                mode: crate::config::CaptureDiskMode::Triggered
            }
        );
        let off: CaptureDiskCommand =
            serde_json::from_str(r#"{"type":"set_capture","mode":"off"}"#).unwrap();
        assert_eq!(
            off,
            CaptureDiskCommand::SetCapture {
                mode: crate::config::CaptureDiskMode::Off
            }
        );
    }

    #[test]
    fn capture_disk_status_roundtrips() {
        let status = CaptureDiskStatus {
            armed: true,
            mode: "triggered".into(),
            recording: false,
            ring_packets: 1200,
            ring_bytes: 4 * 1024 * 1024,
            retained_files: 3,
            retained_bytes: 90 * 1024 * 1024,
            max_files: 16,
            max_total_bytes: 1024 * 1024 * 1024,
            dropped: 0,
            evictions: 1,
            triggers: 4,
            last_event: Some("capture ready: x.pcap.zst · 812 pkts · 1.2 MiB".into()),
        };
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(
            serde_json::from_str::<CaptureDiskStatus>(&json).unwrap(),
            status
        );
    }

    #[test]
    fn capture_filter_command_wire_format() {
        // Pin the JSON the GUI (#228) sends on @rpc/netring/capture_filter/set.
        let set: CaptureFilterCommand = serde_json::from_str(
            r#"{"type":"set_packet_filter","expr":"host 10.0.0.5 and port 443"}"#,
        )
        .unwrap();
        assert_eq!(
            set,
            CaptureFilterCommand::SetPacketFilter {
                expr: "host 10.0.0.5 and port 443".into()
            }
        );
        let clear: CaptureFilterCommand =
            serde_json::from_str(r#"{"type":"clear_packet_filter"}"#).unwrap();
        assert_eq!(clear, CaptureFilterCommand::ClearPacketFilter);
        // Status round-trips (the @rpc/netring/capture_filter shape the GUI reads).
        let status = CaptureFilterStatus {
            enabled: true,
            reloadable: 1,
            current: "host 10.0.0.5".into(),
            base: "tcp or udp or icmp".into(),
            last_error: None,
        };
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(
            serde_json::from_str::<CaptureFilterStatus>(&json).unwrap(),
            status
        );
    }

    #[test]
    fn set_enabled_and_threshold() {
        let mut cfg = AnomalyConfig {
            beaconing: false,
            beacon_threshold: 0.8,
            ..Default::default()
        };
        apply_to(
            &mut cfg,
            DetectorCommand::SetEnabled {
                detector: "beaconing".into(),
                enabled: true,
            },
        );
        assert!(cfg.beaconing);
        apply_to(
            &mut cfg,
            DetectorCommand::SetThreshold {
                detector: "beaconing".into(),
                value: 0.95,
            },
        );
        assert_eq!(cfg.beacon_threshold, 0.95);
        // flood threshold rounds from f64.
        apply_to(
            &mut cfg,
            DetectorCommand::SetThreshold {
                detector: "connection_flood".into(),
                value: 250.0,
            },
        );
        assert_eq!(cfg.flood_threshold, 250);
        // The FQDN-pivoted beacon (#308) mutes/unmutes by name.
        assert!(!cfg.rita_beacon_fqdn);
        apply_to(
            &mut cfg,
            DetectorCommand::SetEnabled {
                detector: "rita_beacon_fqdn".into(),
                enabled: true,
            },
        );
        assert!(cfg.rita_beacon_fqdn);
        // Unknown detector is ignored, not a panic.
        apply_to(
            &mut cfg,
            DetectorCommand::SetEnabled {
                detector: "bogus".into(),
                enabled: true,
            },
        );
    }

    #[test]
    fn allowlist_add_remove_dedup() {
        let mut cfg = AnomalyConfig::default();
        apply_to(
            &mut cfg,
            DetectorCommand::AddAllowlist {
                entry: " cdn.example  ".into(),
            },
        );
        apply_to(
            &mut cfg,
            DetectorCommand::AddAllowlist {
                entry: "cdn.example".into(),
            },
        ); // dup
        apply_to(&mut cfg, DetectorCommand::AddAllowlist { entry: "".into() }); // empty
        assert_eq!(cfg.allowlist, vec!["cdn.example".to_string()]);
        apply_to(
            &mut cfg,
            DetectorCommand::RemoveAllowlist {
                entry: "cdn.example".into(),
            },
        );
        assert!(cfg.allowlist.is_empty());
        apply_to(
            &mut cfg,
            DetectorCommand::SetAllowlist {
                entries: vec!["a".into(), "a".into(), " ".into(), "b".into()],
            },
        );
        assert_eq!(cfg.allowlist, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn handle_apply_swaps_live_config() {
        let handle = DetectorHandle::new(AnomalyConfig::default());
        let shared = handle.shared();
        assert!(!shared.load().beaconing);
        handle.apply(DetectorCommand::SetEnabled {
            detector: "beaconing".into(),
            enabled: true,
        });
        // The hot-path view sees the change without rebuilding.
        assert!(shared.load().beaconing);
    }

    #[test]
    fn command_json_roundtrip() {
        let cmd = DetectorCommand::AddAllowlist {
            entry: "telemetry.host".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("add_allowlist") && json.contains("telemetry.host"));
        let back: DetectorCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cmd);
    }
}
