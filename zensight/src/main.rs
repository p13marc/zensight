//! Zensight - Observability frontend for Zenoh telemetry.
//!
//! This application subscribes to `zensight/**` and displays telemetry
//! from all connected bridges (SNMP, Syslog, gNMI, etc.).

use iced::application;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use zensight_common::ZenohConfig;

mod app;
mod message;
mod subscription;
mod view;

use app::Zensight;

fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer())
        .init();

    tracing::info!("Starting Zensight");

    // Default Zenoh configuration (peer mode, connect to default router)
    let zenoh_config = ZenohConfig {
        mode: "peer".to_string(),
        connect: vec![], // Will use default discovery
        listen: vec![],
    };

    // Run the Iced application
    application("Zensight", Zensight::update, Zensight::view)
        .subscription(Zensight::subscription)
        .theme(Zensight::theme)
        .run_with(move || Zensight::new(zenoh_config))
        .map_err(|e| anyhow::anyhow!("Application error: {}", e))?;

    Ok(())
}
