//! Zenoh session opening, with an isolation mode for demos/tests.

use zensight_common::config::ZenohConfig;

/// Build the Zenoh config for an ISOLATED adapter session: scouting
/// (multicast + gossip) disabled, so the session only ever talks to the
/// explicit `connect`/`listen` endpoints. A live sensor fleet on the same
/// host joins any default-scouting session — isolation is what keeps demo
/// recordings and tests deterministic.
///
/// The mode/connect/listen/timestamping half necessarily mirrors
/// `zensight_common::session::connect` (zensight-common/src/session.rs):
/// that helper builds its `zenoh::Config` internally and opens the session
/// in one call, so there is no seam to inject the scouting knobs without
/// modifying zensight-common. The non-isolated path delegates to the common
/// helper (see [`open_session`]); only this isolate delta is kept local.
pub fn build_isolated_zenoh_config(config: &ZenohConfig) -> anyhow::Result<zenoh::Config> {
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

    zenoh_config
        .insert_json5("scouting/multicast/enabled", "false")
        .map_err(|e| anyhow::anyhow!("failed to disable multicast scouting: {e}"))?;
    zenoh_config
        .insert_json5("scouting/gossip/enabled", "false")
        .map_err(|e| anyhow::anyhow!("failed to disable gossip scouting: {e}"))?;

    Ok(zenoh_config)
}

/// Open the adapter's Zenoh session.
///
/// Non-isolated sessions go through `zensight_common::session::connect` —
/// the shared mode/connect/listen validation + timestamping workaround (it
/// also re-applies `ZENSIGHT_ZENOH_*` env overrides; idempotent with the
/// overrides main.rs applies up front). Isolated sessions use the local
/// scouting-off builder, which reads no env — demos/tests stay hermetic.
pub async fn open_session(config: &ZenohConfig, isolate: bool) -> anyhow::Result<zenoh::Session> {
    if !isolate {
        return zensight_common::session::connect(config)
            .await
            .map_err(|e| anyhow::anyhow!("failed to open Zenoh session: {e}"));
    }

    let zenoh_config = build_isolated_zenoh_config(config)?;
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
            scouting: true,
        };
        assert!(build_isolated_zenoh_config(&config).is_err());
    }

    #[test]
    fn isolated_config_disables_scouting_and_enables_timestamping() {
        let config = ZenohConfig::default();
        let zc = build_isolated_zenoh_config(&config).unwrap();
        // zenoh::Config renders as JSON; pin that both scouting knobs are off
        // and the timestamping workaround is applied.
        let json: serde_json::Value = serde_json::from_str(&zc.to_string()).unwrap();
        assert_eq!(json["scouting"]["multicast"]["enabled"], false);
        assert_eq!(json["scouting"]["gossip"]["enabled"], false);
        assert_eq!(json["timestamping"]["enabled"], true);
    }
}
