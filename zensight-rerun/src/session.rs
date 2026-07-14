//! Zenoh session opening, with an isolation mode for demos/tests.

use zensight_common::config::ZenohConfig;

/// Build the Zenoh config for an ISOLATED adapter session: scouting
/// (multicast + gossip) disabled, so the session only ever talks to the
/// explicit `connect`/`listen` endpoints. A live sensor fleet on the same
/// host joins any default-scouting session — isolation is what keeps demo
/// recordings and tests deterministic.
///
/// Everything shared — mode validation, endpoints, timestamping, and (since
/// #466) the session **`namespace`** that carries the deployment base — comes
/// from `zensight_common::session::build_config`, which exists precisely to be
/// this seam. Only the isolate *delta* is local: gossip off as well, so the
/// session cannot reach a mesh even through an already-connected peer.
///
/// Rebuilding a `zenoh::Config` from scratch here would mean a rerun adapter
/// that quietly subscribes to a keyspace nothing publishes to.
pub fn build_isolated_zenoh_config(config: &ZenohConfig) -> anyhow::Result<zenoh::Config> {
    let mut zenoh_config = zensight_common::session::build_config(&ZenohConfig {
        // multicast off, via the shared knob
        scouting: false,
        ..config.clone()
    })
    .map_err(|e| anyhow::anyhow!("failed to build Zenoh config: {e}"))?;

    // The isolate delta. RFC 09 §0.1: multicast and gossip are independent
    // switches — an isolated *recording* wants both off (there is no hub to
    // traverse), which is stricter than the isolated *deployment* posture.
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
            ..Default::default()
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
