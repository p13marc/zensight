//! Runtime detection-tuning control channel (#121).
//!
//! Mirrors the netlink sentinel's `command.rs`: a `(subscriber + queryable)`
//! loop lets the GUI tune anomaly detection without restarting the sensor —
//! add/remove allowlist entries, mute/unmute a detector, and adjust a
//! detector's threshold. The live config lives behind a lock-free
//! [`arc_swap::ArcSwap`] that the hot-path detectors read (see `monitor.rs`).
//!
//! Keys (via `zensight-common`):
//! - commands: `zensight/netring/@/commands/detectors`  (a [`DetectorCommand`])
//! - status:   `zensight/netring/@/status/detectors`    (the current `AnomalyConfig`)
//!
//! Scope note: a detector that was **off at startup is not built into the
//! capture pipeline**, so enabling it takes effect on the next restart. Tuning
//! (allowlist / threshold) and muting/unmuting a built detector are immediate.

use std::sync::Arc;

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use zensight_common::command::{command_key, status_key};

use crate::config::AnomalyConfig;

/// The control topic under `@/commands/` and `@/status/`.
pub const DETECTORS_TOPIC: &str = "detectors";

/// The capture-focus control topic (netring 0.28, issue #225).
pub const CAPTURE_FILTER_TOPIC: &str = "capture_filter";

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

/// The capture-focus status served on `@/status/capture_filter` (#225) so the
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
pub async fn run(session: Arc<zenoh::Session>, key_prefix: String, handle: DetectorHandle) {
    let cmd_key = command_key(&key_prefix, DETECTORS_TOPIC);
    let stat_key = status_key(&key_prefix, DETECTORS_TOPIC);

    let subscriber = match session.declare_subscriber(&cmd_key).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, key = %cmd_key, "netring: failed to subscribe to detector commands");
            return;
        }
    };
    let queryable = match session.declare_queryable(&stat_key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %stat_key, "netring: failed to declare detector status queryable");
            return;
        }
    };
    tracing::info!(commands = %cmd_key, status = %stat_key, "netring: detection-tuning channel ready");

    loop {
        tokio::select! {
            sample = subscriber.recv_async() => {
                match sample {
                    Ok(sample) => {
                        let payload = sample.payload().to_bytes();
                        match serde_json::from_slice::<DetectorCommand>(&payload) {
                            Ok(cmd) => {
                                let next = handle.apply(cmd);
                                tracing::info!(allowlist = next.allowlist.len(), "netring: detector config updated");
                            }
                            Err(e) => tracing::warn!(error = %e, "netring: bad detector command"),
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
                                if let Err(e) = query.reply(query.key_expr().clone(), payload).await {
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
    key_prefix: String,
    reload: netring::monitor::ReloadHandle,
    base_expr: String,
) {
    let cmd_key = command_key(&key_prefix, CAPTURE_FILTER_TOPIC);
    let stat_key = status_key(&key_prefix, CAPTURE_FILTER_TOPIC);

    let subscriber = match session.declare_subscriber(&cmd_key).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, key = %cmd_key, "netring: failed to subscribe to capture-filter commands");
            return;
        }
    };
    let queryable = match session.declare_queryable(&stat_key).await {
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
            sample = subscriber.recv_async() => {
                let Ok(sample) = sample else {
                    tracing::warn!("netring: capture-filter command subscriber ended");
                    return;
                };
                let payload = sample.payload().to_bytes();
                match serde_json::from_slice::<CaptureFilterCommand>(&payload) {
                    Ok(CaptureFilterCommand::SetPacketFilter { expr }) => {
                        apply_filter(&reload, &expr, &mut current, &mut last_error);
                    }
                    Ok(CaptureFilterCommand::ClearPacketFilter) => {
                        let base = base_expr.clone();
                        apply_filter(&reload, &base, &mut current, &mut last_error);
                    }
                    Err(e) => tracing::warn!(error = %e, "netring: bad capture-filter command"),
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
                        if let Err(e) = query.reply(query.key_expr().clone(), payload).await {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_filter_command_wire_format() {
        // Pin the JSON the GUI (#228) sends on @/commands/capture_filter.
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
        // Status round-trips (the @/status/capture_filter shape the GUI reads).
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
