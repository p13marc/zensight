//! ZenSight identity correlator daemon.
//!
//! Two identities on one process (#1202). It is `@catalog` — the single-writer
//! **service origin** whose entities, incidents, acks and silences every
//! consumer reads, elected through `ServiceGuard::catalog` — and it is also an
//! ordinary host-origin **producer** named `correlator`, through
//! `SensorRunner`, which opens the one session, publishes the five framework
//! documents (a health document with `self_stats`, the declared budget and the
//! shed ladder) under `state/correlator/…`, and serves `introspect`/`describe`
//! there. The `@catalog` keys are untouched by that: the service's alive token
//! and the producer's are different keys, and a second correlator on the same
//! bus still loses the election.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::{CommandFactory, FromArgMatches, Parser};
use tokio::sync::{mpsc, watch};
use tracing::{error, info};
use zensight_common::{catalog_rpc_key, entities_query_key, names_query_key};
use zensight_sensor_core::{SensorArgs, SensorRunner};

use zensight_common::service_guard::{self, ServiceGuard, Standing};
use zensight_correlator::config::CorrelatorConfig;
use zensight_correlator::engine::{CorrelatorState, Engine};
use zensight_correlator::{PRODUCER, pdns, publisher, query, subscriber};

/// Cross-sensor identity correlation service for ZenSight.
#[derive(Parser, Debug)]
#[command(name = "zensight-correlator")]
#[command(about = "Merge host evidence into the single-writer entity keyspace")]
#[command(version)]
struct Args {
    /// The flags every producer takes: `--config` (default
    /// `correlator.json5`, as every sensor defaults to its own file),
    /// `--log-level` (overrides the file's `logging.level`) and
    /// `--check-config` (#1150).
    #[command(flatten)]
    common: SensorArgs,

    /// Run with synthetic evidence instead of subscribing to the bus (GUI dev).
    #[arg(long)]
    demo: bool,
}

impl Args {
    fn parse_with_default_config() -> Self {
        let matches = Self::command()
            .mut_arg("config", |arg| arg.default_value("correlator.json5"))
            .get_matches();
        Self::from_arg_matches(&matches).expect("the arguments parse")
    }
}

/// How long to wait for the spawned tasks to declare their queryables before
/// asserting `alive` (RFC 04 §5: alive ⇒ callable).
///
/// Mirrors `zensight_sensor_core`'s `DECLARATION_GRACE` and for the same
/// reason: far beyond any local `declare_queryable`, short enough not to delay
/// presence noticeably.
const DECLARATION_GRACE: Duration = Duration::from_secs(2);

/// Bound on the single-instance liveliness probe.
const GUARD_TIMEOUT: Duration = Duration::from_secs(3);

/// Bound on the engine→subscriber channel (backpressure on an evidence flood).
const ENGINE_CHANNEL_CAP: usize = 4096;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse_with_default_config();

    let config = CorrelatorConfig::load_from_file(&args.common.config)?;

    // `--check-config` stops here, before the runner and the session exist
    // (#1150). A deploy script gates on the exit status.
    if args.common.check_config {
        zensight_sensor_core::report_config_ok(&args.common.config);
        return Ok(());
    }

    // What the tasks below need is cloned out before the config moves into
    // the runner (the engine takes its own clone).
    let serialization = config.serialization;
    let allow_operator_assertions = config.allow_operator_assertions;
    let incidents_enabled = config.incidents_enabled;
    let operator_decisions = config.operator_decisions.clone();
    let engine_config = config.clone();

    // The runner (#1202) initialises tracing, opens the one session and owns
    // the health, identity and budget publishers of the `correlator`
    // producer. Everything `@catalog` below rides the same session.
    let source = zensight_sensor_core::resolved_source(None);
    let mut runner = SensorRunner::new_with_args(PRODUCER, source, config, Some(&args.common))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    runner = runner.with_format(serialization).with_identity();
    let session = runner.session().clone();
    info!(demo = args.demo, "starting ZenSight correlator");

    // Single-writer guard. This wins the election and takes the claim token;
    // it deliberately does NOT declare `alive` — that happens below, once the
    // queryables are serving. See `zensight_common::service_guard`, "Election
    // and presence are two steps, on purpose".
    //
    // A loser STANDS BY rather than exiting (#1105). Exiting meant that killing
    // the owner left no catalog at all until a supervisor happened to restart a
    // loser whose zid sorted right; waiting here makes takeover cost one poll
    // interval. The loop also re-campaigns after an unreadable claim set, which
    // is the answer that used to be misread as sole candidacy.
    //
    // While standing by the process is not yet a producer either: the runner
    // has not started, so no health document claims a catalog that is not
    // serving. A signal here exits cleanly through the runner's own close.
    let guard = ServiceGuard::catalog(session.clone());
    let _claim = loop {
        match guard.campaign(GUARD_TIMEOUT).await? {
            Standing::Owner(claim) => break claim,
            Standing::StandBy { owner } => {
                info!(
                    owner = owner.as_deref().unwrap_or("unknown"),
                    "another catalog owns @catalog — standing by for it to go away"
                );
                tokio::select! {
                    _ = guard.wait_for_vacancy(GUARD_TIMEOUT, service_guard::STANDBY_POLL) => {}
                    _ = wait_for_shutdown() => {
                        info!("shutting down while standing by");
                        return Ok(());
                    }
                }
            }
        }
    };

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (tx, rx) = mpsc::channel(ENGINE_CHANNEL_CAP);
    let (op_tx, op_rx) = mpsc::channel::<zensight_correlator::EntityOp>(ENGINE_CHANNEL_CAP);
    // Durable historical passive-DNS records (@catalog/state/pdns, #310).
    let (pdns_tx, pdns_rx) = mpsc::channel::<zensight_common::PdnsRecord>(ENGINE_CHANNEL_CAP);

    // Shared correlation state (engine mutates; queryables read).
    // The operator decisions the bus cannot re-derive, loaded BEFORE the bus
    // seed so a live document still wins (#1102).
    let state = {
        let mut st = CorrelatorState::new(engine_config);
        if !operator_decisions.trim().is_empty() {
            st = st.with_journal(zensight_correlator::journal::Journal::new(
                &operator_decisions,
            ));
        }
        Arc::new(Mutex::new(st))
    };

    // The health document reports what this process holds (#1202): the two
    // "for health reporting" accessors finally have a consumer.
    {
        let st = state.clone();
        runner.health().register_table_stats(Box::new(move || {
            let s = st.lock().unwrap_or_else(|e| e.into_inner());
            vec![
                zensight_common::health::TableStats {
                    name: "entities".to_string(),
                    entries: s.entity_count() as u64,
                    bytes: None,
                    capacity_entries: None,
                    capacity_bytes: None,
                },
                zensight_common::health::TableStats {
                    name: "firing_alerts".to_string(),
                    entries: s.firing_alerts() as u64,
                    bytes: None,
                    capacity_entries: None,
                    capacity_bytes: None,
                },
                zensight_common::health::TableStats {
                    name: "relation_claims".to_string(),
                    entries: s.relation_claims() as u64,
                    bytes: None,
                    capacity_entries: None,
                    capacity_bytes: None,
                },
            ]
        }));
    }

    // Engine.
    let (edge_tx, edge_rx) =
        mpsc::channel::<zensight_correlator::edges::EdgeOp>(ENGINE_CHANNEL_CAP);
    // Incidents (#923): on unless the operator disarmed the mechanism.
    let (incident_tx, incident_rx) =
        mpsc::channel::<zensight_correlator::incidents::IncidentOp>(ENGINE_CHANNEL_CAP);
    let mut engine = Engine::new(state.clone(), rx, op_tx)
        .with_pdns(pdns_tx)
        .with_edges(edge_tx);
    if incidents_enabled {
        engine = engine.with_incidents(incident_tx);
    } else {
        info!("incident evaluation disabled by config (incidents_enabled: false)");
    }
    let engine = engine;
    {
        let engine_shutdown = shutdown_rx.clone();
        runner.spawn_named("engine", async move {
            if let Err(e) = engine.run(engine_shutdown).await {
                error!(error = %e, "engine error");
            }
        });
    }

    // Every worker below is a runner task (#1202): supervised, so a panic
    // shows in the health document's `dead_workers`, and aborted by the
    // runner on shutdown. `shutdown_rx` is still handed to each, because the
    // publishers drain on it.
    macro_rules! worker {
        ($name:literal, $body:expr) => {{
            let sh = shutdown_rx.clone();
            let s = session.clone();
            let st = state.clone();
            let _ = &st;
            let fut = $body(s, st, sh);
            runner.spawn_named($name, async move {
                if let Err(e) = fut.await {
                    error!(error = %e, concat!($name, " error"));
                }
            });
        }};
    }

    worker!("publisher", |s, _st, sh| publisher::run(
        s,
        serialization,
        op_rx,
        sh
    ));
    worker!("incident-publisher", |s, _st, sh| publisher::run_incidents(
        s,
        serialization,
        incident_rx,
        sh
    ));
    worker!("edge-publisher", |s, _st, sh| publisher::run_edges(
        s,
        serialization,
        edge_rx,
        sh
    ));
    worker!("pdns-publisher", |s, _st, sh| pdns::run(
        s,
        serialization,
        pdns_rx,
        sh
    ));
    worker!("entities-queryable", query::serve_entities);
    worker!("edges-queryable", query::serve_edges);
    worker!("incidents-queryable", query::serve_incidents);
    worker!("alias-seed", query::serve_alias_seed);
    worker!("assertion-seed", query::serve_assertion_seed);
    worker!("acks-queryable", query::serve_acks);
    worker!("silences-queryable", query::serve_silences);
    worker!("names-queryable", query::serve_names);
    worker!("catalog-introspect", |s, _st, sh| query::serve_introspect(
        s, sh
    ));
    worker!("catalog-describe", |s, _st, sh| query::serve_describe(
        s, sh
    ));
    worker!("assertions", |s, st, sh| query::serve_assertions(
        s,
        st,
        serialization,
        allow_operator_assertions,
        sh
    ));
    worker!("ack-and-silence", |s, st, sh| query::serve_ack_and_silence(
        s,
        st,
        serialization,
        allow_operator_assertions,
        sh
    ));
    worker!("lifecycle-sweep", |s, st, sh| query::run_lifecycle_sweep(
        s,
        st,
        std::time::Duration::from_secs(30),
        sh
    ));

    if args.demo {
        let feed_shutdown = shutdown_rx.clone();
        runner.spawn_named("demo-feed", async move {
            zensight_correlator::demo::feed(tx, feed_shutdown).await;
        });
    } else {
        let sub_session = session.clone();
        let sub_shutdown = shutdown_rx.clone();
        runner.spawn_named("evidence-subscriber", async move {
            if let Err(e) = subscriber::run(sub_session, tx, sub_shutdown).await {
                error!(error = %e, "subscriber error");
            }
        });
    }

    // Declare the catalog `alive` only once every procedure it advertises is
    // served (RFC 04 §5: alive ⇒ callable). A bounded wait, not a snapshot —
    // the tasks above declare their queryables asynchronously. `await_served`
    // takes concrete keys and makes no assumption about how they were
    // declared.
    let callable = [
        entities_query_key(),
        names_query_key(),
        zensight_common::keyexpr::all_incidents_wildcard(),
        zensight_common::keyexpr::all_assertion_wildcard(),
        zensight_common::keyexpr::all_alias_wildcard(),
        zensight_common::keyexpr::all_acks_wildcard(),
        zensight_common::keyexpr::all_silences_wildcard(),
        catalog_rpc_key("introspect"),
        catalog_rpc_key("describe"),
        catalog_rpc_key("link"),
        catalog_rpc_key("unlink"),
        catalog_rpc_key("ack"),
        catalog_rpc_key("unack"),
        catalog_rpc_key("silence"),
        catalog_rpc_key("unsilence"),
    ];
    let missing =
        zensight_common::served::await_served("catalog", &callable, DECLARATION_GRACE).await;
    if !missing.is_empty() {
        error!(
            missing = ?missing,
            "declaring `alive` with queryables still undeclared after {DECLARATION_GRACE:?} — \
             RFC 04 §5 says alive means callable, so this window is a promise this \
             process cannot yet keep"
        );
    }
    let _alive = match guard.declare_alive().await {
        Ok(token) => Some(token),
        Err(e) => {
            error!(error = %e, "failed to declare the catalog alive token");
            None
        }
    };

    // The runner declares the `correlator` producer's own alive token, waits
    // for SIGTERM/Ctrl+C, then aborts the workers, retracts this process's
    // alerts and closes the session. The `@catalog` claim and alive tokens
    // above live until this function returns, which is after that.
    let result = runner
        .run_with_metadata(Some(serde_json::json!({
            "service_origin": "@catalog",
            "demo": args.demo,
            "incidents_enabled": incidents_enabled,
            "allow_operator_assertions": allow_operator_assertions,
            "action_surface": false,
        })))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"));
    info!("shutting down");
    let _ = shutdown_tx.send(true);
    info!("correlator stopped");
    result
}

/// The standby loop's signal wait — before the runner runs, so before the
/// runner's own.
async fn wait_for_shutdown() {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = async {
            #[cfg(unix)]
            {
                let mut sigterm = tokio::signal::unix::signal(
                    tokio::signal::unix::SignalKind::terminate(),
                ).unwrap();
                sigterm.recv().await;
            }
            #[cfg(not(unix))]
            {
                std::future::pending::<()>().await;
            }
        } => {}
    }
}
