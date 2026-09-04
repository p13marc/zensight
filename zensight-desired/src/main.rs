//! `zensight-desired` — compile one fleet policy and publish what follows.
//!
//! Four subcommands, and the split is deliberate: **`plan` must be runnable on
//! a laptop with no bus**, because a policy nobody can check before pushing is
//! a policy that gets checked by the fleet.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use zensight_desired::compile::compile;
use zensight_desired::config::DesiredDaemonConfig;
use zensight_desired::policy::Policy;

/// How long a one-shot catalog GET waits. Short: a compiler with no fleet
/// answers "no hosts", which is a *report*, not a hang.
const CATALOG_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Parser, Debug)]
#[command(name = "zensight-desired")]
#[command(about = "Compile a fleet policy into the per-host @desired documents")]
#[command(version)]
struct Args {
    /// Daemon configuration (JSON5).
    #[arg(
        short,
        long,
        default_value = "/etc/zensight/desired.json5",
        global = true
    )]
    config: String,
    /// Override the policy path from the config.
    #[arg(long, global = true)]
    policy: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Validate the policy and show what would change. Publishes nothing.
    ///
    /// Exits 1 when the policy is invalid — so CI can gate a policy change the
    /// way it gates code. A fleet that cannot be reached is *not* an error:
    /// the policy is still checkable, and `plan` says so.
    Plan {
        /// Check the policy alone; do not open a session at all.
        #[arg(long)]
        offline: bool,
    },
    /// Compile once, publish the difference, exit.
    Apply,
    /// Stay up: re-evaluate on catalog change and on a periodic floor.
    Run,
    /// Print the documents one host would receive, as the sensor will see
    /// them.
    Render {
        /// The target host id (`h-<12hex>`).
        host: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let config = DesiredDaemonConfig::load(&args.config).map_err(|e| anyhow::anyhow!("{e}"))?;
    init_tracing(&config.logging.level);

    let policy_path = args.policy.clone().unwrap_or(config.desired.policy.clone());
    let policy =
        Policy::load(std::path::Path::new(&policy_path)).map_err(|e| anyhow::anyhow!("{e}"))?;

    // Every subcommand validates first. A policy that is wrong is wrong for
    // `run` too, and the daemon that publishes it unattended is the one place
    // the mistake is least visible.
    let problems = policy.validate();
    if !problems.is_empty() {
        eprintln!(
            "{policy_path}: {} problem(s):\n{problems}",
            problems.0.len()
        );
        anyhow::bail!("policy is invalid");
    }

    match args.command {
        Command::Plan { offline } => {
            if offline {
                println!("{policy_path}: valid ({} classes)", policy.classes.len());
                return Ok(());
            }
            let session = connect(&config).await?;
            let fleet = zensight_desired::fleet::fetch(&session, CATALOG_TIMEOUT).await;
            report(&policy, &fleet, &policy_path);
            let _ = session.close().await;
            Ok(())
        }
        Command::Apply => {
            let session = connect(&config).await?;
            let fleet = zensight_desired::fleet::fetch(&session, CATALOG_TIMEOUT).await;
            let compiled = compile(&policy, &fleet);
            log_rejections(&compiled);
            if config.desired.dry_run {
                println!("dry_run: {} document(s) withheld", compiled.docs.len());
                let _ = session.close().await;
                return Ok(());
            }
            let mut pubr = zensight_desired::publish::Publisher0::new(
                session.clone(),
                config.desired.delete_grace_periods,
            );
            pubr.seed(CATALOG_TIMEOUT).await;
            let r = pubr.apply(&compiled).await;
            println!(
                "added {} changed {} deleted {} unchanged {}",
                r.added.len(),
                r.changed.len(),
                r.deleted.len(),
                r.unchanged
            );
            let _ = session.close().await;
            Ok(())
        }
        Command::Run => run(config, policy).await,
        Command::Render { host } => {
            let session = connect(&config).await?;
            let fleet = zensight_desired::fleet::fetch(&session, CATALOG_TIMEOUT).await;
            let compiled = compile(&policy, &fleet);
            let mut found = false;
            for (h, d) in compiled.docs.iter().map(|((h, _, _), d)| (h, d)) {
                if *h != host {
                    continue;
                }
                found = true;
                let key = zensight_desired::publish::host_key(d).unwrap_or_default();
                println!("# {key}");
                println!(
                    "{}\n",
                    serde_json::to_string_pretty(&d.doc).unwrap_or_default()
                );
            }
            // "Nothing" has three causes and they want different fixes, so
            // say which. An empty render that does not distinguish "the
            // catalog has never heard of this host" from "your policy selects
            // nobody" sends the reader to the wrong file.
            if !found {
                if fleet.is_empty() {
                    println!(
                        "# no documents for {host}: the catalog returned no entities at all \
                         — is the correlator running, and on this bus?"
                    );
                } else if !compiled.hosts_seen.contains(&host) {
                    println!(
                        "# no documents for {host}: the catalog does not know that host id. \
                         It knows {}: {}",
                        compiled.hosts_seen.len(),
                        compiled
                            .hosts_seen
                            .iter()
                            .take(8)
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                } else {
                    println!(
                        "# no documents for {host}: the catalog knows it, and no class in \
                         this policy selects it"
                    );
                }
            }
            let _ = session.close().await;
            Ok(())
        }
    }
}

/// The daemon loop.
///
/// The catalog subscription is the accelerator and the periodic pass is the
/// floor — the same two-path shape the sensor-side reconciler uses, and for
/// the same reason: a missed sample must cost latency, never correctness.
async fn run(config: DesiredDaemonConfig, policy: Policy) -> Result<()> {
    let session = connect(&config).await?;

    // Declared BEFORE the first pass, so a change arriving during it is not
    // lost between the GET and the subscription.
    let entity_sub = session
        .declare_subscriber(zensight_common::keyexpr::all_entity_wildcard())
        .with(flume::unbounded())
        .await
        .ok();
    if entity_sub.is_none() {
        tracing::warn!("no entity subscriber; falling back to the periodic pass alone");
    }

    let mut pubr = zensight_desired::publish::Publisher0::new(
        session.clone(),
        config.desired.delete_grace_periods,
    );
    pubr.seed(CATALOG_TIMEOUT).await;

    let period = Duration::from_secs(config.desired.refresh_secs.max(1));
    let mut tick = tokio::time::interval(period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut shutdown = Box::pin(wait_for_shutdown());

    loop {
        let compiled = {
            let fleet = zensight_desired::fleet::fetch(&session, CATALOG_TIMEOUT).await;
            compile(&policy, &fleet)
        };
        log_rejections(&compiled);
        if config.desired.dry_run {
            tracing::info!(
                documents = compiled.docs.len(),
                "dry_run: nothing published"
            );
        } else {
            let r = pubr.apply(&compiled).await;
            if r.wrote_nothing() {
                tracing::debug!(unchanged = r.unchanged, "pass wrote nothing");
            } else {
                tracing::info!(
                    added = r.added.len(),
                    changed = r.changed.len(),
                    deleted = r.deleted.len(),
                    unchanged = r.unchanged,
                    "pass applied"
                );
            }
        }

        // Wait for the next trigger: a catalog change, the floor, or a
        // signal. An entity sample only wakes the loop — the pass itself
        // always re-reads the whole fleet, so a burst of samples costs one
        // pass rather than one each.
        //
        // **In practice the cadence is the correlator's re-emit, not
        // `refresh_secs`.** The catalog republishes every entity every
        // `reemit_secs` (60 by default) whether or not anything changed, so
        // this loop wakes about once a minute on a steady fleet, and the
        // 300-second floor is what remains if the subscription is unavailable.
        // That is affordable precisely because of the property the whole crate
        // is built on: a pass over an unchanged fleet publishes nothing. It
        // costs one catalog GET and one compile, and it means a genuine change
        // converges in seconds rather than in up to five minutes.
        tokio::select! {
            _ = &mut shutdown => break,
            _ = tick.tick() => {}
            _ = async {
                match &entity_sub {
                    Some(sub) => { let _ = sub.recv_async().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {
                // Coalesce the rest of the burst rather than compiling once
                // per entity in a fleet that just restarted.
                tokio::time::sleep(Duration::from_secs(2)).await;
                if let Some(sub) = &entity_sub {
                    while sub.try_recv().is_ok() {}
                }
            }
        }
    }

    let _ = session.close().await;
    Ok(())
}

fn report(policy: &Policy, fleet: &[zensight_common::HostEntity], path: &str) {
    let compiled = compile(policy, fleet);
    println!("{path}: valid");
    println!("  fleet: {} entities", fleet.len());
    println!("  documents: {}", compiled.docs.len());
    for (host, producer, topic) in compiled.docs.keys() {
        println!("    {host}  {producer}/{topic}");
    }
    if !compiled.unmatched.is_empty() {
        println!(
            "  matched no class ({}): {}",
            compiled.unmatched.len(),
            compiled.unmatched.join(", ")
        );
    }
    for r in &compiled.rejected {
        println!("  REFUSED {r}");
    }
}

/// Refusals are logged at `error`, not `warn`.
///
/// A document the compiler refused is a host that is **not** getting the
/// policy someone wrote, and the sensor will never mention it because nothing
/// reached it. If this line is not loud, nothing is.
fn log_rejections(c: &zensight_desired::compile::Compiled) {
    for r in &c.rejected {
        tracing::error!(detail = %r, "document refused — this host is not receiving it");
    }
}

async fn connect(config: &DesiredDaemonConfig) -> Result<Arc<zenoh::Session>> {
    Ok(Arc::new(
        zensight_common::session::connect(&config.zenoh)
            .await
            .map_err(|e| anyhow::anyhow!("failed to connect to Zenoh: {e}"))?,
    ))
}

fn init_tracing(level: &str) {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    let _ = fmt().with_env_filter(filter).try_init();
}

async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "failed to install SIGTERM handler");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
