//! Synthetic demo publisher for the Rerun adapter evaluation (epic #415).
//!
//! Always opens an ISOLATED session (scouting off) and connects to an
//! explicit endpoint — typically the adapter's `--isolate` listener — so a
//! live sensor fleet never leaks into a demo recording.

use clap::{Parser, Subcommand};
use tracing::info;
use tracing_subscriber::EnvFilter;

use zensight_rerun::demo::{self, DemoContext};

/// Synthetic ZenSight publishers for the Rerun evaluation.
#[derive(Parser, Debug)]
#[command(name = "zensight-rerun-demo")]
#[command(about = "Publish synthetic ZenSight telemetry scenarios (evaluation, #415)")]
#[command(version)]
struct Args {
    /// Zenoh endpoint to connect to (the adapter's isolated listener).
    #[arg(long, global = true, default_value = "tcp/127.0.0.1:7449")]
    connect: String,

    #[command(subcommand)]
    scenario: Scenario,
}

#[derive(Subcommand, Debug)]
enum Scenario {
    /// Six live metric series (gauges + a resetting counter) on real-shaped keys.
    Metrics {
        /// How long to publish, seconds.
        #[arg(long, default_value_t = 30)]
        duration_secs: u64,
        /// Tick interval, milliseconds.
        #[arg(long, default_value_t = 500)]
        interval_ms: u64,
    },
    /// Discrete events (route/peer/reset/anomaly), a health degradation, an
    /// alert firing->resolved pair, then a same-second event burst.
    Events {
        /// Number of events in the closing burst.
        #[arg(long, default_value_t = 50)]
        burst: u64,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("zensight_rerun=info".parse()?)
                .add_directive("zenoh=warn".parse()?),
        )
        .init();

    info!(endpoint = %args.connect, "Connecting demo publisher (isolated session)");
    let ctx = DemoContext::connect(&args.connect).await?;

    match args.scenario {
        Scenario::Metrics {
            duration_secs,
            interval_ms,
        } => {
            info!(duration_secs, interval_ms, "Running metrics scenario");
            let published = demo::metrics::run(&ctx, duration_secs, interval_ms).await?;
            info!(published, "Metrics scenario complete");
        }
        Scenario::Events { burst } => {
            info!(burst, "Running events scenario");
            let (events, alerts, health) = demo::events::run(&ctx, burst).await?;
            info!(events, alerts, health, "Events scenario complete");
        }
    }

    ctx.session
        .close()
        .await
        .map_err(|e| anyhow::anyhow!("failed to close session: {e}"))?;
    Ok(())
}
