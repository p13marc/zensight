//! ZenSight - Observability frontend for Zenoh telemetry.
//!
//! This application subscribes to `zensight/**` and displays telemetry
//! from all connected sensors (SNMP, Syslog, gNMI, etc.).

use std::env;

use iced::application;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use zensight::app::ZenSight;

fn main() -> anyhow::Result<()> {
    // Check for --demo flag
    // `--print-subscription` (#1262): the key expressions this build would
    // subscribe to on its overview from its registries and bundled view
    // definitions alone — one per line — and exit. No window, no session;
    // what `scripts/demo-verify.sh` reads to say the GUI does not fetch the
    // firehose.
    if env::args().any(|arg| arg == "--print-subscription") {
        for key in zensight::view::plan::bundled_plan() {
            println!("{key}");
        }
        return Ok(());
    }
    let demo_mode = env::args().any(|arg| arg == "--demo" || arg == "-d");

    // Initialize tracing
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer())
        .init();

    if demo_mode {
        tracing::info!("Starting ZenSight in DEMO mode (mock data, no Zenoh connection)");
    } else {
        tracing::info!("Starting ZenSight");
    }

    // Run the Iced application
    application(
        move || ZenSight::boot(demo_mode),
        ZenSight::update,
        ZenSight::view,
    )
    .title("ZenSight")
    .subscription(ZenSight::subscription)
    .theme(ZenSight::theme)
    .run()
    .map_err(|e| anyhow::anyhow!("Application error: {}", e))?;

    Ok(())
}
