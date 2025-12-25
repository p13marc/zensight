//! ZenSight - Observability frontend for Zenoh telemetry.
//!
//! This application subscribes to `zensight/**` and displays telemetry
//! from all connected bridges (SNMP, Syslog, gNMI, etc.).

use iced::application;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

mod app;
mod message;
mod subscription;
mod view;

use app::ZenSight;

fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer())
        .init();

    tracing::info!("Starting ZenSight");

    // Run the Iced application
    application(ZenSight::boot, ZenSight::update, ZenSight::view)
        .title("ZenSight")
        .subscription(ZenSight::subscription)
        .theme(ZenSight::theme)
        .run()
        .map_err(|e| anyhow::anyhow!("Application error: {}", e))?;

    Ok(())
}
