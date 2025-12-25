//! Zensight - Observability frontend for Zenoh telemetry.
//!
//! This application subscribes to `zensight/**` and displays telemetry
//! from all connected bridges (SNMP, Syslog, gNMI, etc.).

use iced::application;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

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

    // Run the Iced application
    application(Zensight::boot, Zensight::update, Zensight::view)
        .title("Zensight")
        .subscription(Zensight::subscription)
        .theme(Zensight::theme)
        .run()
        .map_err(|e| anyhow::anyhow!("Application error: {}", e))?;

    Ok(())
}
