use zenoh::Session;

use crate::config::ZenohConfig;
use crate::error::{Error, Result};

/// Connect to Zenoh using the provided configuration.
pub async fn connect(config: &ZenohConfig) -> Result<Session> {
    // Honor ZENSIGHT_ZENOH_* env overrides (e.g. set by `just run` to pin a
    // local rendezvous endpoint instead of relying on multicast discovery).
    let config = &config.clone().with_env_overrides();
    let mut zenoh_config = zenoh::Config::default();

    // Set mode
    let mode_str = match config.mode.as_str() {
        "client" | "peer" | "router" => format!("\"{}\"", config.mode),
        other => {
            return Err(Error::Config(format!(
                "Invalid Zenoh mode: '{}'. Expected 'client', 'peer', or 'router'",
                other
            )));
        }
    };

    zenoh_config
        .insert_json5("mode", &mode_str)
        .map_err(|e| Error::Config(format!("Failed to set mode: {}", e)))?;

    // Set connect endpoints
    if !config.connect.is_empty() {
        let endpoints_json = serde_json::to_string(&config.connect)
            .map_err(|e| Error::Config(format!("Failed to serialize connect endpoints: {}", e)))?;

        zenoh_config
            .insert_json5("connect/endpoints", &endpoints_json)
            .map_err(|e| Error::Config(format!("Failed to set connect endpoints: {}", e)))?;
    }

    // Set listen endpoints
    if !config.listen.is_empty() {
        let endpoints_json = serde_json::to_string(&config.listen)
            .map_err(|e| Error::Config(format!("Failed to serialize listen endpoints: {}", e)))?;

        zenoh_config
            .insert_json5("listen/endpoints", &endpoints_json)
            .map_err(|e| Error::Config(format!("Failed to set listen endpoints: {}", e)))?;
    }

    // zenoh-ext AdvancedPublisher caches sequence by timestamp, which requires
    // session timestamping — zenoh enables it by default only for routers, so
    // without this every peer/client-mode sensor fails to create its cached
    // identity/evidence/registration publishers ("the 'timestamping' setting
    // must be enabled") and the correlator never receives evidence.
    zenoh_config
        .insert_json5("timestamping/enabled", "true")
        .map_err(|e| Error::Config(format!("Failed to enable timestamping: {}", e)))?;

    // Isolated runs (tests, smoke, pinned-endpoint deployments) turn
    // multicast scouting off so the session can NEVER join a mesh beyond its
    // explicit endpoints. Gossip stays on: it only propagates within the
    // already-connected graph, and hub-and-spoke deployments rely on it.
    if !config.scouting {
        zenoh_config
            .insert_json5("scouting/multicast/enabled", "false")
            .map_err(|e| Error::Config(format!("Failed to disable multicast scouting: {}", e)))?;
    }

    tracing::info!(
        mode = %config.mode,
        connect = ?config.connect,
        listen = ?config.listen,
        scouting = config.scouting,
        "Connecting to Zenoh"
    );

    let session = zenoh::open(zenoh_config).await?;

    tracing::info!(zid = %session.zid(), "Connected to Zenoh");

    Ok(session)
}
