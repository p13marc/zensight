use zenoh::Session;

use crate::config::ZenohConfig;
use crate::error::{Error, Result};

/// Connect to Zenoh using the provided configuration.
pub async fn connect(config: &ZenohConfig) -> Result<Session> {
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

    tracing::info!(
        mode = %config.mode,
        connect = ?config.connect,
        listen = ?config.listen,
        "Connecting to Zenoh"
    );

    let session = zenoh::open(zenoh_config).await?;

    tracing::info!(zid = %session.zid(), "Connected to Zenoh");

    Ok(session)
}
