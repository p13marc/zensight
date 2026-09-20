//! `zensight-desired` — compile one fleet policy and publish what follows.
//!
//! Four subcommands, and the split is deliberate: **`plan` must be runnable on
//! a laptop with no bus**, because a policy nobody can check before pushing is
//! a policy that gets checked by the fleet.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use zensight_desired::PRODUCER;
use zensight_desired::config::DesiredDaemonConfig;
use zensight_desired::policy::Policy;
use zensight_sensor_core::SensorRunner;

/// How long a one-shot catalog GET waits. Short: a compiler with no fleet
/// answers "no hosts", which is a *report*, not a hang.
const CATALOG_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a one-shot command keeps asking `@catalog` before believing an
/// empty answer (#1045).
///
/// `await_peer` proves the session has a link; it does not prove the catalog's
/// queryable has been declared to it, and declarations propagate after the link
/// comes up. So a GET issued the instant `connect` returns can reach nobody on
/// a perfectly healthy bus. This is the window that closes.
///
/// It costs nothing on the ordinary path: a settled session answers on the
/// first attempt and never waits. Only a fleet that genuinely has no hosts pays
/// the full deadline, and it is about to be told exactly that.
const FLEET_SETTLE: Duration = Duration::from_secs(10);

/// How often to re-ask inside [`FLEET_SETTLE`].
const FLEET_POLL: Duration = Duration::from_millis(250);

/// How long [`connect`] waits for the session to have a neighbour before it
/// asks anything (#1039).
///
/// `zenoh::open` returns before the link to a `connect` endpoint is up, so a
/// GET issued immediately reaches nobody and answers with zero replies —
/// indistinguishable, to every caller in this crate, from a fleet of zero
/// hosts. That is exactly how `apply` came to publish nothing and report it as
/// a successful no-op.
///
/// Not fatal on expiry: a controller started before its hub must still come up
/// and converge on the next refresh. `apply` refuses separately, on the fleet
/// it actually read.
const PEER_WAIT: Duration = Duration::from_secs(5);

/// How long to wait for every declared procedure to be serving before saying
/// `alive`. The same two seconds `SensorRunner` waits.
const DECLARATION_GRACE: Duration = Duration::from_secs(2);

/// Bounds the ownership election's claim-set and incumbent queries (#1104), so
/// a bus with no other instance does not stall startup for zenoh's default.
const GUARD_TIMEOUT: Duration = Duration::from_secs(2);

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
    /// Optional **only** so `--check-config` can stand alone; every other
    /// invocation needs one, and `None` without the flag is a usage error.
    #[command(subcommand)]
    command: Option<Command>,
    /// Parse and validate the config, the policy and the overrides, print the
    /// verdict, and exit — open no session, publish nothing (#1150). A deploy
    /// script gates on the exit status.
    #[arg(long)]
    check_config: bool,
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
    // `run` is a producer (#1202): its runner initialises tracing from
    // `logging` itself, and a second init is an error, not a no-op. The
    // one-shot commands keep the light init.
    if !matches!(args.command, Some(Command::Run)) {
        init_tracing(&config.logging.level);
    }

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

    // Adoptions are part of what a host gets, so `plan` and `render` must see
    // them too — a render that showed only the policy would be a confident
    // answer to the wrong question.
    let overrides = zensight_desired::overrides::Overrides::load(std::path::Path::new(
        &config.desired.overrides,
    ))
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    // `--check-config` stops here, before any subcommand and before the
    // session (#1150). It is placed *after* the policy and the overrides load
    // and the policy validates, because for this daemon those are the config
    // — a daemon config that parses while the policy it compiles does not is
    // not a deployable host.
    if args.check_config {
        println!(
            "config ok: {} (policy {policy_path}: {} classes, {} adopted host(s))",
            args.config,
            policy.classes.len(),
            overrides.hosts.len()
        );
        return Ok(());
    }

    let Some(command) = args.command else {
        anyhow::bail!("a subcommand is required (plan, apply, run, render) — see --help");
    };

    match command {
        Command::Plan { offline } => {
            if offline {
                println!(
                    "{policy_path}: valid ({} classes, {} adopted host(s))",
                    policy.classes.len(),
                    overrides.hosts.len()
                );
                return Ok(());
            }
            let session = connect(&config).await?;
            // Deliberately ONE GET, unlike `apply` and `render` (#1045).
            // `plan`'s contract is "what can you see right now", and callers
            // loop it precisely to watch a fleet appear — `demo-verify.sh`
            // phase 4 does exactly that. Making each call wait ten seconds
            // would turn a cheap repeated probe into a slow one and change what
            // the command means.
            let fleet = zensight_desired::fleet::fetch(&session, CATALOG_TIMEOUT).await;
            report(&policy, &fleet, &overrides, &policy_path);
            let _ = session.close().await;
            Ok(())
        }
        Command::Apply => {
            let session = connect(&config).await?;
            // One writer per origin, and `apply` is a writer (#1104). A
            // one-shot `apply` beside a live `run` is the same two-writer
            // failure as two daemons: each seeds its `published` diff map from
            // the storage, reads the other's write as a change, and rewrites
            // it — so every sensor flaps between two configurations, silently.
            // Refusing by name is the whole difference between that and an
            // operator who knows what happened.
            let guard = zensight_common::service_guard::ServiceGuard::desired(session.clone());
            if let Some(owner) = guard.incumbent(GUARD_TIMEOUT).await {
                let _ = session.close().await;
                anyhow::bail!(
                    "a `zensight-desired run` instance already owns @desired ({owner}).\n\
                     Two writers on one service origin make every sensor flap between two \
                     configurations, with nothing on the bus to say so.\n\
                     Stop the daemon, or let it apply this policy itself."
                );
            }
            // Retried, not asked once (#1045): an empty answer has two causes
            // and only one of them is stable.
            let fleet = zensight_desired::fleet::settle(
                || zensight_desired::fleet::fetch(&session, CATALOG_TIMEOUT),
                FLEET_SETTLE,
                FLEET_POLL,
            )
            .await;
            // An empty fleet is not a no-op, it is an unanswered question
            // (#1039). `fetch` returns `vec![]` both when the catalog says
            // "no hosts" and when nobody answered at all, and this command is
            // run from deploy scripts that read the exit code. Publishing
            // nothing under exit 0 is the one outcome nobody can act on.
            if fleet.is_empty() {
                let _ = session.close().await;
                anyhow::bail!(
                    "@catalog reported no hosts, so there is nothing to compile and \
                     nothing would be published.\n\
                     This command cannot tell the two causes apart, so check both: \
                     no correlator is answering on this bus, or one is and it has \
                     fused no host yet. `plan` shows what it can see.\n\
                     (Asked for {}s before concluding — this is not a session \
                     that had not settled yet.)",
                    FLEET_SETTLE.as_secs()
                );
            }
            let compiled = zensight_desired::compile::compile_with(&policy, &fleet, &overrides);
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
            // Settled like `apply` (#1045): render answers "what would this
            // host get", and "nothing, because my session was a second old" is
            // the wrong answer to it.
            let fleet = zensight_desired::fleet::settle(
                || zensight_desired::fleet::fetch(&session, CATALOG_TIMEOUT),
                FLEET_SETTLE,
                FLEET_POLL,
            )
            .await;
            let compiled = zensight_desired::compile::compile_with(&policy, &fleet, &overrides);
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
/// The `run` daemon: a host-origin producer named `policy-compiler` (#1202)
/// that owns the `@desired` service origin. `SensorRunner` opens the one
/// session and publishes the framework documents; the election, the
/// procedures and the compile loop ride that session, as runner tasks.
async fn run(config: DesiredDaemonConfig, policy: Policy) -> Result<()> {
    let controller = config.desired.clone();
    let source = zensight_sensor_core::resolved_source(None);
    let mut runner = SensorRunner::new_with_args(PRODUCER, source, config, None)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let format = runner.config().serialization;
    runner = runner.with_format(format).with_identity();
    let session = runner.session().clone();
    // The same wait the one-shot commands take: a compile pass against a bus
    // nobody has joined yet is a pass for a fleet of zero.
    zensight_common::session::await_peer(&session, PEER_WAIT).await;

    let guard = zensight_common::service_guard::ServiceGuard::desired(session.clone());
    let _claim = loop {
        match guard.campaign(GUARD_TIMEOUT).await? {
            zensight_common::service_guard::Standing::Owner(claim) => break claim,
            zensight_common::service_guard::Standing::StandBy { owner } => {
                tracing::info!(
                    owner = owner.as_deref().unwrap_or("unknown"),
                    "another @desired instance owns this origin — standing by"
                );
                tokio::select! {
                    _ = guard.wait_for_vacancy(
                        GUARD_TIMEOUT,
                        zensight_common::service_guard::STANDBY_POLL,
                    ) => {}
                    _ = wait_for_shutdown() => {
                        tracing::info!("shutting down while standing by");
                        return Ok(());
                    }
                }
            }
        }
    };

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
        controller.delete_grace_periods,
    );
    pubr.seed(CATALOG_TIMEOUT).await;

    let overrides_path = std::path::PathBuf::from(&controller.overrides);
    let overrides = Arc::new(tokio::sync::Mutex::new(
        zensight_desired::overrides::Overrides::load(&overrides_path)
            .map_err(|e| anyhow::anyhow!("{e}"))?,
    ));
    let (wake_tx, mut wake_rx) = tokio::sync::watch::channel(0u64);
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    {
        let ctx = Arc::new(zensight_desired::serve::OverrideCtx {
            overrides: overrides.clone(),
            path: overrides_path.clone(),
            allowed: controller.allow_overrides,
            wake: wake_tx,
        });
        let s = session.clone();
        let rx = shutdown_rx.clone();
        runner.spawn_named("desired-procedures", async move {
            if let Err(e) = zensight_desired::serve::serve(s, ctx, rx).await {
                tracing::error!(error = %e, "the @desired procedures stopped");
            }
        });
    }

    let missing = zensight_common::served::await_served(
        "desired",
        &zensight_desired::serve::declared_rpc_keys(),
        DECLARATION_GRACE,
    )
    .await;
    if !missing.is_empty() {
        tracing::error!(
            missing = ?missing,
            "declaring `alive` with procedures still undeclared — a caller will get \
             silence from those, which `alive ⇒ callable` forbids"
        );
    }
    let _alive = match guard.declare_alive().await {
        Ok(token) => Some(token),
        Err(e) => {
            tracing::error!(error = %e, "failed to declare the @desired alive token");
            None
        }
    };

    // The compile loop, as a runner task: wakes on the period, on an
    // override write, and on an entity change (debounced), and stops on the
    // shutdown the runner's return flips.
    {
        let session = session.clone();
        let policy = policy.clone();
        let overrides = overrides.clone();
        let dry_run = controller.dry_run;
        let period = Duration::from_secs(controller.refresh_secs.max(1));
        runner.spawn_named("compile-loop", async move {
            let mut tick = tokio::time::interval(period);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                let compiled = {
                    let fleet = zensight_desired::fleet::fetch(&session, CATALOG_TIMEOUT).await;
                    let ov = overrides.lock().await;
                    zensight_desired::compile::compile_with(&policy, &fleet, &ov)
                };
                log_rejections(&compiled);
                if dry_run {
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

                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    _ = tick.tick() => {}
                    _ = wake_rx.changed() => {}
                    _ = async {
                        match &entity_sub {
                            Some(sub) => { let _ = sub.recv_async().await; }
                            None => std::future::pending::<()>().await,
                        }
                    } => {
                        // Debounce: an entity sample only wakes the loop, and a
                        // catalog re-emit is a burst of them.
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        if let Some(sub) = &entity_sub {
                            while sub.try_recv().is_ok() {}
                        }
                    }
                }
            }
        });
    }

    // The runner declares the `policy-compiler` alive token, waits for
    // SIGTERM/Ctrl+C, then aborts the tasks, retracts this process's alerts
    // and closes the session. The `@desired` claim and alive tokens live
    // until this function returns.
    let result = runner
        .run_with_metadata(Some(serde_json::json!({
            "service_origin": zensight_desired::ORIGIN,
            "policy": controller.policy,
            "refresh_secs": controller.refresh_secs,
            "dry_run": controller.dry_run,
            "allow_overrides": controller.allow_overrides,
            "action_surface": false,
        })))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"));
    let _ = shutdown_tx.send(true);
    result
}

fn report(
    policy: &Policy,
    fleet: &[zensight_common::HostEntity],
    overrides: &zensight_desired::overrides::Overrides,
    path: &str,
) {
    let compiled = zensight_desired::compile::compile_with(policy, fleet, overrides);
    println!("{path}: valid");
    println!("  fleet: {} entities", fleet.len());
    if !overrides.is_empty() {
        println!(
            "  adoptions: {} host(s) with a recorded override",
            overrides.hosts.len()
        );
    }
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
    // The mirror image, and the one `validate` cannot see (#1109): a class
    // whose selector is well-formed and simply wrong — `platform: "debian"`
    // where the field is `debian-13`, a CIDR for a renumbered subnet. Nothing
    // errors; those hosts just never receive the documents someone wrote for
    // them.
    let idle = policy.classes_matching_nothing(fleet);
    if !idle.is_empty() {
        println!(
            "  WARNING: {} class(es) selected no host of this fleet: {}",
            idle.len(),
            idle.join(", ")
        );
    }
    for r in &compiled.rejected {
        println!("  REFUSED {r}");
    }
}

fn log_rejections(c: &zensight_desired::compile::Compiled) {
    for r in &c.rejected {
        tracing::error!(detail = %r, "document refused — this host is not receiving it");
    }
}

async fn connect(config: &DesiredDaemonConfig) -> Result<Arc<zenoh::Session>> {
    let session = zensight_common::session::connect(&config.zenoh)
        .await
        .map_err(|e| anyhow::anyhow!("failed to connect to Zenoh: {e}"))?;
    // Every command goes through here, and every one of them asks the catalog
    // a question as its first act (#1039).
    zensight_common::session::await_peer(&session, PEER_WAIT).await;
    Ok(Arc::new(session))
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
