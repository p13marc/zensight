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
            other => tracing::warn!(detector = %other, "netring: unknown detector in SetEnabled"),
        },
        DetectorCommand::SetThreshold { detector, value } => match detector.as_str() {
            "beaconing" => cfg.beacon_threshold = value,
            "rita_beacon" => cfg.rita_beacon_threshold = value,
            "connection_flood" => cfg.flood_threshold = value.max(0.0) as u64,
            "dga" => cfg.dga_threshold = value,
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

#[cfg(test)]
mod tests {
    use super::*;

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
