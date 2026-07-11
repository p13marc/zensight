//! Zenoh session opening, with an isolation mode for demos/tests.

use zensight_common::config::ZenohConfig;

/// Build the Zenoh config for the adapter.
///
/// Mirrors `zensight_common::session::connect` (mode/connect/listen +
/// timestamping) but adds `isolate`: with it set, scouting (multicast +
/// gossip) is disabled so the session only ever talks to the explicit
/// `connect`/`listen` endpoints. A live sensor fleet on the same host joins
/// any default-scouting session — isolation is what keeps demo recordings
/// and tests deterministic.
pub fn build_zenoh_config(config: &ZenohConfig, isolate: bool) -> anyhow::Result<zenoh::Config> {
    let mut zenoh_config = zenoh::Config::default();

    match config.mode.as_str() {
        mode @ ("client" | "peer" | "router") => {
            zenoh_config
                .insert_json5("mode", &format!("\"{mode}\""))
                .map_err(|e| anyhow::anyhow!("failed to set mode: {e}"))?;
        }
        other => {
            anyhow::bail!("invalid Zenoh mode '{other}' (expected client, peer, or router)");
        }
    }

    if !config.connect.is_empty() {
        let endpoints = serde_json::to_string(&config.connect)?;
        zenoh_config
            .insert_json5("connect/endpoints", &endpoints)
            .map_err(|e| anyhow::anyhow!("failed to set connect endpoints: {e}"))?;
    }

    if !config.listen.is_empty() {
        let endpoints = serde_json::to_string(&config.listen)?;
        zenoh_config
            .insert_json5("listen/endpoints", &endpoints)
            .map_err(|e| anyhow::anyhow!("failed to set listen endpoints: {e}"))?;
    }

    // Declared publishers on the control plane need session timestamping
    // (same rationale as zensight_common::session::connect).
    zenoh_config
        .insert_json5("timestamping/enabled", "true")
        .map_err(|e| anyhow::anyhow!("failed to enable timestamping: {e}"))?;

    if isolate {
        zenoh_config
            .insert_json5("scouting/multicast/enabled", "false")
            .map_err(|e| anyhow::anyhow!("failed to disable multicast scouting: {e}"))?;
        zenoh_config
            .insert_json5("scouting/gossip/enabled", "false")
            .map_err(|e| anyhow::anyhow!("failed to disable gossip scouting: {e}"))?;
    }

    Ok(zenoh_config)
}

/// Open the adapter's Zenoh session.
pub async fn open_session(config: &ZenohConfig, isolate: bool) -> anyhow::Result<zenoh::Session> {
    let zenoh_config = build_zenoh_config(config, isolate)?;
    tracing::info!(
        mode = %config.mode,
        connect = ?config.connect,
        listen = ?config.listen,
        isolate,
        "Connecting to Zenoh"
    );
    let session = zenoh::open(zenoh_config)
        .await
        .map_err(|e| anyhow::anyhow!("failed to open Zenoh session: {e}"))?;
    tracing::info!(zid = %session.zid(), "Connected to Zenoh");
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_mode() {
        let config = ZenohConfig {
            mode: "gateway".into(),
            connect: vec![],
            listen: vec![],
        };
        assert!(build_zenoh_config(&config, false).is_err());
    }

    #[test]
    fn isolate_disables_scouting() {
        let config = ZenohConfig::default();
        let zc = build_zenoh_config(&config, true).unwrap();
        // zenoh::Config renders as JSON; pin that both scouting knobs are off.
        let json: serde_json::Value = serde_json::from_str(&zc.to_string()).unwrap();
        assert_eq!(json["scouting"]["multicast"]["enabled"], false);
        assert_eq!(json["scouting"]["gossip"]["enabled"], false);
        // ...and that the non-isolated session leaves them at default (null).
        let default = build_zenoh_config(&config, false).unwrap();
        let json: serde_json::Value = serde_json::from_str(&default.to_string()).unwrap();
        assert!(json["scouting"]["multicast"]["enabled"].is_null());
    }
}
