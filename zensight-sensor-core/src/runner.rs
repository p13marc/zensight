//! Sensor runner for lifecycle management.

use std::future::Future;
use std::sync::Arc;

use tokio::signal;
use tokio::task::JoinHandle;

use zensight_common::{Format, LoggingConfig, connect, init_tracing};

use crate::SensorArgs;
use crate::config::SensorConfig;
use crate::error::{Result, SensorError};
use crate::liveliness::LivelinessManager;
use crate::publisher::Publisher;

/// How long [`SensorRunner::run`] waits for this producer's spawned tasks to
/// declare their queryables before reporting a registry-coverage gap (#648).
///
/// This is also the deadline the `alive` token implies: RFC 04 §5 says a
/// producer is callable once alive, so a queryable declared later than this is
/// late whatever the check does. Two seconds is far beyond any local
/// `declare_queryable` and short enough not to delay liveliness noticeably.
const DECLARATION_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// How long the startup alert adoption GET waits for the bus to answer (#882).
///
/// Only a storage replies, and only with keys this producer itself wrote, so
/// the answer is small and local-ish. Three seconds matches the exporters'
/// own alert-seed GET; a slower bus simply means starting with an empty firing
/// set, which is what every build before this one did.
const ALERT_ADOPTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// How long shutdown waits for the firing set to be retracted and tombstoned.
///
/// Two writes per firing alert on an express, reliable publisher. The bound
/// exists so a wedged session cannot hold a `systemctl stop` open until its
/// own timeout turns into a SIGKILL — which would strand the very alerts this
/// drain is here to clear.
const ALERT_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Sensor runner that manages the lifecycle of a protocol sensor.
///
/// Handles:
/// - Configuration loading
/// - Logging initialization
/// - Zenoh connection
/// - Task spawning and management
/// - Graceful shutdown on Ctrl+C
///
/// # Example
///
/// ```ignore
/// use zensight_sensor_core::{SensorArgs, SensorConfig, SensorRunner};
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     let args = SensorArgs::parse_with_default("mysensor.json5");
///     let config = MySensorConfig::load(&args.config)?;
///     let source = config.resolved_source();
///
///     let runner = SensorRunner::new("mysensor", source, config).await?;
///
///     // Spawn workers using the publisher
///     let publisher = runner.publisher();
///     runner.spawn(async move {
///         // Worker logic here
///     });
///
///     runner.run().await
/// }
/// ```
pub struct SensorRunner<C: SensorConfig> {
    /// Sensor name for logging and status.
    name: String,
    /// The instance's host id (hostname / device-poller id). Keys are
    /// origin-scoped (`zensight/v1/<origin>/…`); this value
    /// feeds the identity/artifact channels.
    source: String,
    /// Sensor version.
    version: String,
    /// The loaded configuration.
    config: C,
    /// Zenoh session.
    session: Arc<zenoh::Session>,
    /// Publisher for telemetry.
    publisher: Publisher,
    /// Liveliness manager for presence detection.
    liveliness: Option<Arc<LivelinessManager>>,
    /// Sensor health tracker, published periodically to the origin-scoped
    /// `state/<producer>/health` so
    /// the frontend's Sensors view / health bar populate. Sensors may update it
    /// (device counts, poll durations) via [`Self::health`].
    health: Arc<crate::health::SensorHealth>,
    /// Host identity envelope (identity envelope, #301). Set via
    /// [`Self::with_identity`]; drives the `state/<producer>/sensor` +
    /// `state/<producer>/evidence/**` publication task (keyed by [`Self::source`]).
    identity: Option<crate::identity::SharedIdentity>,
    /// The memory governor (#812): registered tables/degradables + the shed
    /// ladder, stepped on the health tick.
    governor: Arc<crate::governor::MemoryGovernor>,
    /// The sensor's own alert reporter, when it shared one via
    /// [`Self::with_alert_reporter`] — runner-emitted alerts (`sensor-budget`)
    /// then ride the same reporter `serve_alerts_query` seeds from.
    alert_reporter: Option<Arc<crate::alert::AlertReporter>>,
    /// Spawned workers, by abort handle (#1082).
    ///
    /// Abort handles rather than `JoinHandle`s because each handle itself is
    /// moved into a supervisor that awaits it — and shutdown must abort the
    /// **worker**, not the supervisor watching it.
    aborts: Vec<tokio::task::AbortHandle>,
    /// The supervisors watching those workers (#1082), aborted after them.
    supervisors: Vec<JoinHandle<()>>,
}

/// The self-report this sensor publishes on `state/<producer>/evidence/self`.
///
/// A free function rather than fourteen lines inside the identity task's
/// closure, because it is the only place `observer: None` is set and therefore
/// the only place a self-report's *content* is decided — and because for the
/// life of the crate two of its fields were hard-coded `None` with nothing
/// able to notice (#935). A closure inside a `tokio::spawn` cannot be asserted
/// on; this can.
fn self_evidence(
    sensor: &str,
    source: &str,
    id: crate::identity::HostIdentity,
    facts: &crate::hostfacts::HostFacts,
    now: i64,
) -> zensight_common::HostEvidence {
    zensight_common::HostEvidence {
        sensor: sensor.to_string(),
        source: source.to_string(),
        // A self-report. Everything downstream ranks these above third-party
        // claims — `merge::representative`, and `HostEntity::origins` since
        // #1007 — so this field is not a label, it is the claim's authority.
        observer: None,
        host_id: id.host_id,
        boot_id: id.boot_id,
        hostname: Some(id.hostname),
        fqdn: id.fqdn,
        ips: id.ips,
        macs: id.macs,
        vendor: facts.vendor.clone(),
        platform: facts.platform.clone(),
        container_id: id.container_id,
        cloud: id.cloud,
        last_updated: now,
    }
}

impl<C: SensorConfig> SensorRunner<C> {
    /// Create a new sensor runner.
    ///
    /// `source` is the instance's host id (typically the
    /// sensor config's `resolved_source()`): keys themselves are origin-scoped
    /// (`zensight/v1/<origin>/state/<producer>/health` etc.); it feeds the
    /// identity/artifact channels.
    ///
    /// This will:
    /// 1. Initialize logging based on config (with optional CLI override)
    /// 2. Connect to Zenoh
    /// 3. Create the publisher
    pub async fn new(
        name: impl Into<String>,
        source: impl Into<String>,
        config: C,
    ) -> Result<Self> {
        Self::new_with_args(name, source, config, None).await
    }

    /// Create a new sensor runner with CLI args for log level override.
    pub async fn new_with_args(
        name: impl Into<String>,
        source: impl Into<String>,
        config: C,
        args: Option<&SensorArgs>,
    ) -> Result<Self> {
        let name = name.into();
        let source = source.into();
        let version = env!("CARGO_PKG_VERSION").to_string();

        // Initialize logging with optional CLI override
        let log_config = if let Some(args) = args {
            if let Some(ref level) = args.log_level {
                LoggingConfig {
                    level: level.clone(),
                    // Preserve format from config, only override level from CLI
                    format: config.logging().format,
                }
            } else {
                config.logging().clone()
            }
        } else {
            config.logging().clone()
        };

        init_tracing(&log_config).map_err(|e| SensorError::config(e.to_string()))?;

        // Register this process as an audit client (#957), so every write
        // procedure it serves records under the right producer and origin. A
        // sensor with no write surface never emits a record and pays only this
        // one assignment.
        zensight_common::audit::init(name.clone());

        tracing::info!(
            sensor = %name,
            source = %source,
            version = %version,
            audit_delivering = zensight_common::audit::is_delivering(),
            "Starting sensor"
        );

        // Connect to Zenoh
        let session = Arc::new(
            connect(config.zenoh())
                .await
                .map_err(|e| SensorError::ZenohConnection(e.to_string()))?,
        );

        tracing::info!(zid = %session.zid(), "Connected to Zenoh");

        // Create publisher
        let publisher = Publisher::new(
            session.clone(),
            config.producer(),
            Format::Json, // Default to JSON, can be overridden
        );

        // Health tracker publishes JSON to the origin-scoped
        // `state/<producer>/health` (publish_health ignores the publisher's
        // format, so the initial publisher is fine even if `with_format` later
        // changes telemetry encoding).
        let health = Arc::new(
            crate::health::SensorHealth::new(name.clone())
                .with_publisher(publisher.clone())
                .with_source(source.clone())
                // Self-telemetry (#811): the health tick reads the baseline
                // tier's publish counters and any declared budget.
                .with_publish_counters(publisher.counters()),
        );
        health.set_budget_bytes(config.budget_bytes().unwrap_or(0));

        Ok(Self {
            name,
            source,
            version,
            config,
            session,
            publisher,
            liveliness: None,
            health,
            identity: None,
            governor: Arc::new(crate::governor::MemoryGovernor::default()),
            alert_reporter: None,
            aborts: Vec::new(),
            supervisors: Vec::new(),
        })
    }

    /// The memory governor (#812) — sensors register evictable tables and
    /// degradables on it; the health tick drives its shed ladder.
    pub fn governor(&self) -> Arc<crate::governor::MemoryGovernor> {
        self.governor.clone()
    }

    /// Share the sensor's own [`AlertReporter`](crate::alert::AlertReporter)
    /// with the runner, so runner-emitted alerts (`sensor-budget`, #812) ride
    /// the reporter whose `serve_alerts_query` seed late joiners read —
    /// instead of a runner-private reporter the seed cannot see.
    pub fn with_alert_reporter(mut self, reporter: Arc<crate::alert::AlertReporter>) -> Self {
        self.alert_reporter = Some(reporter);
        self
    }

    /// Declare the sensor-level liveliness token now instead of at [`Self::run`].
    ///
    /// The runner declares the token automatically when it starts, so most
    /// sensors never call this. Call it only to get the [`LivelinessManager`]
    /// early (via [`Self::liveliness`]) — e.g. to declare device-level tokens
    /// with [`LivelinessManager::declare_device_alive`] before `run()`.
    pub async fn with_liveliness(mut self) -> Result<Self> {
        let liveliness =
            LivelinessManager::new(self.session.clone(), self.publisher.v1().clone()).await?;
        self.liveliness = Some(Arc::new(liveliness));
        Ok(self)
    }

    /// Enable the unified on-demand artifact channel (`@rpc/<producer>/artifact/*`).
    ///
    /// Registers the given producers (e.g. [`ReportProducer`](crate::ReportProducer)
    /// for debug bundles, [`SnapshotProducer`](crate::SnapshotProducer) for
    /// directory snapshots, or a sensor-specific capture producer). When at
    /// least one is enabled in the sensor's `artifacts.*` config this spawns a
    /// full [`ArtifactChannel`](crate::ArtifactChannel) as a tracked worker.
    /// The runner's `source` is this host's id (for a request's `target_source`
    /// filter).
    ///
    /// When none is enabled the three `artifact/*` procedures are **still
    /// declared**, answering `error/gated`. They are registered
    /// unconditionally by every artifact-capable producer's registry slice, so
    /// a build that skipped them entirely made `introspect` advertise three
    /// surfaces it did not serve — which is a lie to the fleet (RFC 08 §6.1)
    /// and, since #484, a `debug_assert!` that killed the sensor at startup.
    /// Artifacts are disabled by default, so that was every stock debug sensor
    /// on a stock config (#648).
    pub fn with_artifacts(
        mut self,
        producers: Vec<Arc<dyn crate::artifact::ArtifactProducer>>,
    ) -> Self {
        if let Some(channel) = crate::artifact::ArtifactChannel::new(
            self.session.clone(),
            self.config.producer().to_string(),
            self.source.clone(),
            producers,
        ) {
            self.spawn(channel.run());
            tracing::info!("artifact channel enabled");
        } else {
            let session = self.session.clone();
            let producer = self.config.producer().to_string();
            self.spawn(async move {
                crate::artifact::serve_disabled(session, producer).await;
            });
        }
        self
    }

    /// Enable the host-identity envelope (#301).
    ///
    /// Detects the local [`HostIdentity`](crate::HostIdentity) (hashed
    /// machine-id, boot id, hostname, IPs, MACs), stamps `host_id` onto health
    /// snapshots, and — once [`run`](Self::run) starts — publishes the sensor's
    /// registration (`state/<producer>/sensor`) and self-report evidence
    /// (`state/<producer>/evidence/self`) every 60 s via cached publishers,
    /// re-detecting on a slow timer for DHCP churn.
    pub fn with_identity(self) -> Self {
        let identity = crate::identity::SharedIdentity::detect();
        self.with_shared_identity(identity)
    }

    /// [`with_identity`](Self::with_identity) with a pre-built identity (tests).
    pub fn with_shared_identity(mut self, identity: crate::identity::SharedIdentity) -> Self {
        self.health.set_host_id(identity.get().host_id.clone());
        self.identity = Some(identity);
        self
    }

    /// The shared host identity, when [`with_identity`](Self::with_identity)
    /// was enabled — for stamping alerts via
    /// [`AlertReporter::with_identity`](crate::AlertReporter::with_identity).
    pub fn identity(&self) -> Option<crate::identity::SharedIdentity> {
        self.identity.clone()
    }

    /// Set a custom serialization format for the publisher.
    ///
    /// Re-formats the existing publisher rather than constructing a new one,
    /// so the health tracker's publish counters keep pointing at the
    /// registry every put goes through (#1078).
    pub fn with_format(mut self, format: Format) -> Self {
        self.publisher = self.publisher.clone().with_format(format);
        self
    }

    /// Get the sensor name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the instance's `<source>` key segment.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Get the sensor version.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Get a reference to the configuration.
    pub fn config(&self) -> &C {
        &self.config
    }

    /// Get a reference to the Zenoh session.
    pub fn session(&self) -> &Arc<zenoh::Session> {
        &self.session
    }

    /// Get a clone of the publisher.
    pub fn publisher(&self) -> Publisher {
        self.publisher.clone()
    }

    /// Get the shared sensor-health tracker. Sensors may update it (device
    /// counts, poll durations, errors); the runner publishes it periodically.
    pub fn health(&self) -> Arc<crate::health::SensorHealth> {
        self.health.clone()
    }

    /// Get a reference to the liveliness manager.
    ///
    /// Returns `None` before [`Self::run`] unless [`Self::with_liveliness`]
    /// declared it early.
    pub fn liveliness(&self) -> Option<&LivelinessManager> {
        self.liveliness.as_deref()
    }

    /// A shared handle to the liveliness manager, for a task that outlives the
    /// borrow [`liveliness`](Self::liveliness) hands out.
    ///
    /// Device tokens were, until #410, all declared in one loop at startup and
    /// never touched again, so a borrow was enough. Hotplug makes them a thing
    /// that happens *later*: the parallax sensor declares a token when a camera
    /// is plugged in and undeclares it when it is pulled, from a task spawned on
    /// this runner — and a `&` cannot cross a `'static` spawn.
    pub fn liveliness_shared(&self) -> Option<Arc<LivelinessManager>> {
        self.liveliness.clone()
    }

    /// Spawn a worker task.
    ///
    /// The task is tracked, **supervised** (#1082) and aborted on shutdown. It
    /// takes its name from the call site, so the ~100 existing callers keep
    /// working and an operator still gets something they can go and look at;
    /// use [`Self::spawn_named`] to say what the worker *is*.
    #[track_caller]
    pub fn spawn<F>(&mut self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let name = std::panic::Location::caller().to_string();
        self.spawn_named(&name, future);
    }

    /// Spawn a named worker task, supervised (#1082).
    ///
    /// Nothing used to join or poll these handles until shutdown aborted them.
    /// If a collector panicked — a slice index in a `/proc` parser, a poisoned
    /// lock — the task died, telemetry stopped, the liveliness token stayed
    /// declared, and the health task kept publishing
    /// `{status: "Healthy", devices_responding: 1}` every five seconds.
    ///
    /// A second task now awaits the worker's handle and tells health when it
    /// ends. It is *notice*, not recovery: `F` is not clonable, so the runner
    /// cannot re-run what it was handed. What it can do is stop saying the
    /// sensor is fine.
    pub fn spawn_named<F>(&mut self, name: &str, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        // Shutdown must abort the WORKER, not the supervisor: aborting the
        // supervisor would leave the worker running, and a cancelled task
        // cannot run code to abort anything on its way out. So the abort
        // handle is kept separately, and the supervisor waits on a
        // `JoinHandle` — which is also what lets it tell a panic
        // (`is_cancelled() == false`) from shutdown's own `abort()`.
        self.adopt_named(name, tokio::spawn(future));
    }

    /// Supervise an already-spawned task (#1082).
    ///
    /// The runner's own workers — `introspect`, `describe`, the alert seed, the
    /// health tick, the identity tick — are handed to it as `JoinHandle`s
    /// rather than futures, and they need watching for exactly the same reason
    /// a sensor's collector does. The health tick most of all: it is the task
    /// whose silence is indistinguishable from a healthy sensor.
    pub fn adopt_named(&mut self, name: &str, handle: JoinHandle<()>) {
        self.aborts.push(handle.abort_handle());
        self.supervisors
            .push(supervise(self.health.clone(), name, handle));
    }

    /// Spawn a worker task that returns a Result.
    ///
    /// Errors are logged automatically, and the task is supervised like any
    /// other (#1082).
    pub fn spawn_with_error<F, E>(&mut self, name: String, future: F)
    where
        F: Future<Output = std::result::Result<(), E>> + Send + 'static,
        E: std::fmt::Display + Send + 'static,
    {
        let label = name.clone();
        self.spawn_named(&label, async move {
            if let Err(e) = future.await {
                tracing::error!(worker = %name, error = %e, "Worker failed");
            }
        });
    }

    /// Run the sensor until a shutdown signal (Ctrl+C / SIGINT or SIGTERM) is received.
    ///
    /// This will:
    /// 1. Serve `introspect`, declare liveliness, start health/identity tasks
    /// 2. Wait for a shutdown signal (Ctrl+C / SIGINT or, on Unix, SIGTERM)
    /// 3. Abort all spawned tasks and close the Zenoh session
    pub async fn run(self) -> Result<()> {
        self.run_with_metadata(None).await
    }

    /// Run the sensor with free-form metadata carried on the registration doc
    /// (`state/<producer>/sensor` — the legacy `@/status` document retired
    /// with the v1 cutover).
    pub async fn run_with_metadata(mut self, metadata: Option<serde_json::Value>) -> Result<()> {
        // Serve `introspect` (RFC 08 §6) — the registry slice this build was
        // compiled against — before the liveliness token, so "alive ⇒
        // callable" holds (RFC 04 §5). A producer missing from the registry
        // is a build-time error elsewhere; here it just has no slice.
        {
            let ctx = self.publisher.v1().clone();
            let producer_name = ctx.producer().name().to_string();
            if let Some(toml) = zensight_common::registry::registry_toml(&producer_name) {
                match crate::rpc::serve_introspect(self.session.clone(), &ctx, toml).await {
                    Ok(task) => self.adopt_named("introspect", task),
                    Err(e) => tracing::warn!(error = %e, "failed to serve introspect"),
                }
                // `describe` rides next to `introspect` (RFC 08 §7): the
                // schema table for every payload type the fleet references.
                match crate::rpc::serve_describe(
                    self.session.clone(),
                    &ctx,
                    zensight_common::schema::DESCRIBE_JSON.as_str(),
                )
                .await
                {
                    Ok(task) => self.adopt_named("describe", task),
                    Err(e) => tracing::warn!(error = %e, "failed to serve describe"),
                }
            } else {
                tracing::debug!(producer = %producer_name, "no registry slice; introspect not served");
            }
            // registry ⊆ served (RFC 08 §6.1, #484). Checked *here* because
            // this is the moment the claim is made: `introspect` is about to
            // hand the fleet this producer's registry slice as truth, and
            // `alive` is about to say it is callable. Anything the slice
            // advertises that this build never declared is a lie from now on.
            // Debug panics (a sensor's own tests fail); release warns.
            //
            // A bounded *wait*, not a snapshot: every sensor declares its
            // queryables inside `Self::spawn` tasks, so reading the served set
            // once here races them (#648).
            zensight_common::served::await_registry_coverage(&producer_name, DECLARATION_GRACE)
                .await;
        }

        // Alerts are state, and state outlives the process that wrote it
        // (#882). Before anything else claims a firing set, take ownership of
        // the one a previous incarnation of this producer left on the bus:
        // adopt what is still ours to retract, tombstone what no sweep could
        // ever reach. Ordered before `serve_alerts_query` so the only answers
        // are the bus's — a storage's, in practice — and not our own empty
        // set. In a deployment with no storage on `v1/*/state/**` nobody
        // answers and this is a no-op, which is exactly right.
        //
        // A sensor's own evaluation tasks are spawned before `run()`, so a
        // first sweep can race ahead of this. That is harmless and bounded:
        // adoption never overwrites an alert this process has already
        // observed, and anything adopted late is retracted by the *next*
        // sweep of its rule rather than the first.
        if let Some(reporter) = self.alert_reporter.clone() {
            reporter.adopt_persisted(ALERT_ADOPTION_TIMEOUT).await;
            // The late-joiner seed (RFC 05 §4) rides the same registration, so
            // a sensor declares it by handing the runner its reporter rather
            // than by remembering to spawn this itself.
            self.adopt_named(
                "alert-seed",
                tokio::spawn(crate::alert::serve_alerts_query(reporter)),
            );
        }

        // Presence is not optional: declare the sensor-level liveliness token
        // (`state/<producer>/alive`) unless [`Self::with_liveliness`] already
        // did. The frontend flips this sensor's card Offline when the token
        // vanishes (clean close or lease expiry), so a sensor without a token
        // would read as its last health forever. Declaration failure is only a
        // warning — a broken liveliness path must never stop telemetry.
        if self.liveliness.is_none() {
            match LivelinessManager::new(self.session.clone(), self.publisher.v1().clone()).await {
                Ok(manager) => self.liveliness = Some(Arc::new(manager)),
                Err(e) => tracing::warn!(error = %e, "Failed to declare liveliness token"),
            }
        }

        // Periodically publish sensor health to `state/<producer>/health` so
        // the frontend's Sensors view and dashboard health bar populate. The
        // first tick fires immediately, then every 5s. The tick also steps
        // the memory governor's shed ladder (#812) and grades the
        // `sensor-budget` rule (#811) against the same measurement it
        // publishes (sampling twice would corrupt the CPU diff).
        {
            let health = self.health.clone();
            let governor = self.governor.clone();
            // The budget reporter needs a Protocol; a producer without one
            // (custom sensors) just skips the rule with a note. Built whenever
            // the Protocol parses — not only when the config declares a
            // budget — because a budget may also be *discovered* from the
            // cgroup below. Prefers the sensor's own reporter
            // (`with_alert_reporter`) so `serve_alerts_query` seeds these
            // alerts too.
            let budget_reporter = match self.name.parse::<zensight_common::Protocol>() {
                Ok(proto) => {
                    let reporter = self.alert_reporter.clone().unwrap_or_else(|| {
                        Arc::new(crate::alert::AlertReporter::new(
                            self.publisher.clone(),
                            proto,
                            Format::Json,
                        ))
                    });
                    Some((reporter, proto))
                }
                Err(_) => {
                    if self.config.budget_bytes().is_some() {
                        tracing::warn!(
                            producer = %self.name,
                            "budget declared but producer has no Protocol; sensor-budget rule disabled"
                        );
                    }
                    None
                }
            };
            let (source, sensor_name) = (self.source.clone(), self.name.clone());
            let config_budget = self.config.budget_bytes();
            let task = tokio::spawn(async move {
                // Budget discovery (#812): a config-declared budget always
                // wins; in a container with none, take a fraction of the
                // cgroup's memory.max so the ladder cannot disagree with the
                // drop-in the operator actually wrote. `max` (unlimited)
                // discovers nothing — no budget, no ladder.
                if config_budget.is_none()
                    && let Some(max) =
                        crate::procutil::self_cgroup().and_then(|c| c.memory_max_bytes)
                {
                    let budget = (max as f64 * crate::governor::CGROUP_BUDGET_FRACTION) as u64;
                    health.set_budget_bytes(budget);
                    tracing::info!(
                        budget_bytes = budget,
                        cgroup_memory_max = max,
                        "memory budget discovered from cgroup"
                    );
                }
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
                let mut firing = false;
                loop {
                    tick.tick().await;
                    let snapshot = crate::governor::governed_snapshot(&health, &governor);
                    if let Err(e) = health.publish_snapshot(&snapshot).await {
                        tracing::warn!(error = %e, "Failed to publish sensor health");
                    }
                    if let (Some((reporter, proto)), Some(stats)) =
                        (&budget_reporter, &snapshot.self_stats)
                    {
                        firing =
                            grade_budget(reporter, *proto, &source, &sensor_name, stats, firing)
                                .await;
                    }
                }
            });
            self.adopt_named("health-tick", task);
        }

        // Identity envelope (#301): publish the sensor registration + self-report
        // host evidence every 60 s via cached publishers (late-joiner seed), and
        // re-detect the identity every 5th tick so DHCP address churn is
        // eventually reflected (health host_id refreshes alongside).
        if let Some(identity) = self.identity.clone() {
            let source = self.source.clone();
            let registry = crate::advanced_publisher::AdvancedPublisherRegistry::new(
                self.session.clone(),
                self.config.producer().to_string(),
                Format::Json,
                crate::advanced_publisher::AdvancedPublisherConfig::cache_only(1),
            )
            .with_qos(zensight_common::QosClass::Evidence)
            .with_counters(self.publisher.counters());
            let name = self.name.clone();
            let version = self.version.clone();
            let producer_name = self.config.producer().to_string();
            let v1_ctx = self.publisher.v1().clone();
            let health = self.health.clone();
            let identity_cfg = self.config.identity_config();
            let metadata = metadata.clone();
            let task = tokio::spawn(async move {
                // Opt-in cloud-metadata probe (#311): one shot before the first
                // emit — an instance's cloud identity never changes while it
                // runs, and refresh() preserves the result. Off by default
                // (identity.cloud_metadata) because it makes network requests.
                if identity_cfg.cloud_metadata {
                    let timeout =
                        std::time::Duration::from_millis(identity_cfg.cloud_timeout_ms.max(1));
                    match crate::cloud::detect_cloud(timeout).await {
                        Some(facts) => {
                            tracing::info!(
                                provider = %facts.provider,
                                instance_id = %facts.instance_id,
                                "cloud metadata detected"
                            );
                            identity.set_cloud(Some(facts));
                        }
                        None => tracing::debug!("cloud metadata probe: no provider found"),
                    }
                }
                // Vendor and platform (#935), read ONCE. Unlike the address
                // set, neither changes while the process runs: a machine does
                // not change manufacturer, and a distribution upgrade that
                // moved `platform` also restarted every service on the host.
                // Re-reading them on the DHCP refresh would spend two file
                // reads a minute to learn nothing.
                let facts = crate::hostfacts::HostFacts::detect();
                tracing::debug!(
                    vendor = ?facts.vendor,
                    platform = ?facts.platform,
                    "host facts"
                );

                let info_key = v1_ctx.sensor_info_key();
                let evidence_key = v1_ctx.evidence_self_key();
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
                let mut n: u64 = 0;
                loop {
                    tick.tick().await;
                    if n > 0 && n.is_multiple_of(5) {
                        identity.refresh();
                        health.set_host_id(identity.get().host_id.clone());
                    }
                    n += 1;
                    let id = identity.get();
                    let now = zensight_common::current_timestamp_millis();
                    let info = zensight_common::SensorInfo {
                        name: name.clone(),
                        version: version.clone(),
                        producer: producer_name.clone(),
                        source: source.clone(),
                        host_id: id.host_id.clone(),
                        boot_id: id.boot_id.clone(),
                        hostname: Some(id.hostname.clone()),
                        fqdn: id.fqdn.clone(),
                        ips: id.ips.clone(),
                        macs: id.macs.clone(),
                        metadata: metadata.clone(),
                        last_updated: now,
                    };
                    let evidence = self_evidence(&name, &source, id, &facts, now);
                    if let Err(e) = registry.publish_serializable(&info_key, &info).await {
                        tracing::warn!(error = %e, "Failed to publish sensor registration");
                    }
                    if let Err(e) = registry
                        .publish_serializable(&evidence_key, &evidence)
                        .await
                    {
                        tracing::warn!(error = %e, "Failed to publish host evidence");
                    }
                }
            });
            self.adopt_named("identity-tick", task);
        }

        tracing::info!(
            sensor = %self.name,
            tasks = self.aborts.len(),
            "Sensor running. Press Ctrl+C or send SIGTERM to stop."
        );

        // Wait for a shutdown signal. Catch both Ctrl+C (SIGINT) and SIGTERM:
        // systemd `stop` and `docker stop` send SIGTERM, and if we only awaited
        // Ctrl+C we'd be SIGKILLed after the stop timeout — never reaching the
        // graceful path below (alert tombstones + a clean liveliness close).
        wait_for_shutdown().await;

        tracing::info!(sensor = %self.name, "Received shutdown signal");

        // Abort the workers first, then the supervisors watching them (#1082).
        // In this order every supervisor observes `is_cancelled()` and reports
        // nothing; the other order would abort the watchers and leave the
        // workers running through the alert drain below.
        for task in &self.aborts {
            task.abort();
        }
        for sup in &self.supervisors {
            sup.abort();
        }

        // Wait briefly for tasks to clean up
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Retract every alert this sensor is still asserting, and tombstone
        // its key (#882). A stopped sensor asserts nothing; leaving the
        // `Firing` documents behind would have a `latest` storage serve them
        // to every late joiner for as long as the storage lives.
        //
        // Ordered *after* the task aborts, so nothing can raise a new alert
        // into the set we are draining, and *before* `session.close()`,
        // because a closed session publishes nothing. Awaited rather than
        // raced against the sleep above, for the same reason.
        if let Some(reporter) = &self.alert_reporter {
            let pending = reporter.active_count();
            match tokio::time::timeout(ALERT_DRAIN_TIMEOUT, reporter.resolve_all()).await {
                Ok(Ok(())) if pending > 0 => {
                    tracing::info!(sensor = %self.name, alerts = pending, "retracted firing alerts")
                }
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!(error = %e, "failed to retract firing alerts"),
                Err(_) => tracing::warn!(
                    alerts = pending,
                    "timed out retracting firing alerts; some may be left firing on the bus"
                ),
            }
        }

        // Close Zenoh session
        if let Err(e) = self.session.close().await {
            tracing::warn!(error = %e, "Error closing Zenoh session");
        }

        tracing::info!(sensor = %self.name, "Goodbye!");

        Ok(())
    }
}

/// Drive the `sensor-budget` rule (#811) for one health tick: observe the
/// alert while RSS is over the declared budget, reconcile it away otherwise.
/// Returns whether the alert is firing (the caller's hysteresis state).
async fn grade_budget(
    reporter: &crate::alert::AlertReporter,
    proto: zensight_common::Protocol,
    source: &str,
    sensor_name: &str,
    stats: &zensight_common::SelfStats,
    firing: bool,
) -> bool {
    use crate::health::{SENSOR_BUDGET_RULE, budget_level, budget_summary};

    let level = match (stats.rss_bytes, stats.budget_bytes) {
        // Not measured, or no budget: nothing to grade — clear stale state.
        (Some(rss), Some(budget)) => budget_level(firing, rss, budget),
        _ => None,
    };
    match level {
        Some(severity) => {
            let alert = zensight_common::Alert::new(
                source,
                proto,
                zensight_common::AlertKind::SensorHealth,
                SENSOR_BUDGET_RULE,
                severity,
                budget_summary(stats),
            )
            .with_label("sensor", sensor_name.to_string());
            let key = alert.alert_key();
            // Two-tick debounce lives in the rule's own cadence: observe with
            // zero for-duration but only after `budget_level` said so — the
            // 80/95/75 hysteresis is the anti-flap, not a timer.
            if let Err(e) = reporter
                .observe(alert, Some(std::time::Duration::ZERO))
                .await
            {
                tracing::warn!(error = %e, "sensor-budget: publish failed");
            }
            // Opts out of any recovery window the sensor configured (#929):
            // the 80/95/75 band above IS this rule's hysteresis, and stacking
            // a timer on top would delay a resolve the band has already
            // decided is real.
            if let Err(e) = reporter
                .reconcile_opts(
                    SENSOR_BUDGET_RULE,
                    &[key],
                    crate::alert::ReconcileOpts::immediate(),
                )
                .await
            {
                tracing::warn!(error = %e, "sensor-budget: reconcile failed");
            }
            true
        }
        None => {
            if let Err(e) = reporter
                .reconcile_opts(
                    SENSOR_BUDGET_RULE,
                    &[],
                    crate::alert::ReconcileOpts::immediate(),
                )
                .await
            {
                tracing::warn!(error = %e, "sensor-budget: reconcile failed");
            }
            false
        }
    }
}

/// Watch one worker task and tell `health` if it ends (#1082).
///
/// Free rather than a method so the discrimination below can be tested without
/// a bus, a config or a global tracing init — and that discrimination is the
/// whole of it:
///
/// - `Ok(())` — the worker returned. Every worker in this tree is a loop, so
///   falling out of one is a finding, not a completion.
/// - `Err(e)` with `e.is_cancelled()` — **shutdown's own `abort()`**. Not a
///   death, and not reported as one: otherwise every clean stop would publish
///   a false finding on its last health tick. This is why the supervisor waits
///   on a `JoinHandle` rather than wrapping the future — wrapping cannot tell
///   a cancel from a panic, and a wrapper around a panicking future panics
///   with it and records nothing at all.
/// - any other `Err` — the worker panicked.
pub fn supervise(
    health: Arc<crate::health::SensorHealth>,
    name: &str,
    handle: JoinHandle<()>,
) -> JoinHandle<()> {
    let name = name.to_string();
    tokio::spawn(async move {
        match handle.await {
            Ok(()) => {
                tracing::warn!(worker = %name, "worker task returned");
                health.record_worker_exit(&name, false);
            }
            Err(e) if e.is_cancelled() => {}
            Err(e) => {
                tracing::error!(worker = %name, error = %e, "worker task panicked");
                health.record_worker_exit(&name, true);
            }
        }
    })
}

/// Wait for an OS shutdown signal: Ctrl+C (SIGINT) or, on Unix, SIGTERM.
///
/// systemd and Docker stop a process with SIGTERM, so handling only Ctrl+C
/// would let the orchestrator SIGKILL the sensor after its stop timeout,
/// skipping the graceful shutdown (alert tombstones + a clean liveliness close).
async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        let mut sigterm = match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                // Fall back to Ctrl+C-only if SIGTERM can't be registered.
                tracing::error!(error = %e, "Failed to install SIGTERM handler");
                if let Err(e) = signal::ctrl_c().await {
                    tracing::error!(error = %e, "Failed to listen for Ctrl+C");
                }
                return;
            }
        };
        tokio::select! {
            r = signal::ctrl_c() => {
                if let Err(e) = r {
                    tracing::error!(error = %e, "Failed to listen for Ctrl+C");
                }
            }
            _ = sigterm.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        if let Err(e) = signal::ctrl_c().await {
            tracing::error!(error = %e, "Failed to listen for Ctrl+C");
        }
    }
}

#[cfg(test)]
mod tests {
    // Most of the runner needs a Zenoh session, which is what the integration
    // tests are for. What *can* be asserted here is the content of what it
    // publishes — see [`self_evidence`].
    use super::*;

    fn identity() -> crate::identity::HostIdentity {
        crate::identity::HostIdentity {
            host_id: Some("h-3fa9c2d41b7e".into()),
            hostname: "web01".into(),
            ..Default::default()
        }
    }

    /// The regression this exists for: `vendor` and `platform` were literal
    /// `None` on every self-report for the life of the crate, so the catalog
    /// showed a self-reporting host as having neither while showing an
    /// SNMP-polled switch as having both (#935).
    #[test]
    fn a_self_report_carries_the_host_facts() {
        let facts = crate::hostfacts::HostFacts {
            vendor: Some("Dell Inc.".into()),
            platform: Some("debian-13".into()),
        };
        let ev = self_evidence("sysinfo", "web01", identity(), &facts, 42);

        assert_eq!(ev.vendor.as_deref(), Some("Dell Inc."));
        assert_eq!(ev.platform.as_deref(), Some("debian-13"));
        assert!(
            ev.observer.is_none(),
            "a self-report is what makes these outrank a third-party claim"
        );
        assert_eq!(ev.host_id.as_deref(), Some("h-3fa9c2d41b7e"));
        assert_eq!(ev.last_updated, 42);
    }

    /// A host with no DMI and no `/etc/os-release` is common — a container, a
    /// minimal image, a non-Linux target. Absent must stay absent rather than
    /// becoming an empty string that reads as an answer.
    #[test]
    fn a_host_with_no_facts_reports_none_not_empty() {
        let ev = self_evidence(
            "sysinfo",
            "web01",
            identity(),
            &crate::hostfacts::HostFacts::default(),
            0,
        );
        assert_eq!(ev.vendor, None);
        assert_eq!(ev.platform, None);
    }
}

/// Worker supervision (#1082) — the discrimination [`supervise`] exists for.
#[cfg(test)]
mod supervision_tests {
    use super::*;

    fn health() -> Arc<crate::health::SensorHealth> {
        Arc::new(crate::health::SensorHealth::new("sysinfo"))
    }

    /// A collector that panics is noticed, named, and stops the sensor saying
    /// it is Healthy.
    ///
    /// Nothing joined or polled these handles before: the task died, telemetry
    /// stopped, the liveliness token stayed declared, and the health task —
    /// still alive — kept publishing `Healthy` every five seconds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_panicking_worker_stops_the_sensor_saying_it_is_healthy() {
        let h = health();
        assert_eq!(
            h.snapshot().status,
            zensight_common::HealthStatus::Healthy,
            "nothing is wrong yet"
        );

        let worker = tokio::spawn(async { panic!("a slice index in a /proc parser") });
        supervise(h.clone(), "system-collector", worker)
            .await
            .expect("the supervisor itself must not die with its worker");

        let snap = h.snapshot();
        assert_eq!(snap.dead_workers, vec!["system-collector".to_string()]);
        assert_ne!(
            snap.status,
            zensight_common::HealthStatus::Healthy,
            "health kept saying Healthy over a dead collector"
        );
        assert!(snap.last_error.is_some_and(|e| e.contains("panicked")));
    }

    /// A worker that simply returns is reported too: every worker in this tree
    /// is a loop, so falling out of one is a finding.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_worker_that_returns_early_is_reported() {
        let h = health();
        supervise(h.clone(), "collector", tokio::spawn(async {}))
            .await
            .expect("supervisor");
        assert_eq!(h.dead_workers(), vec!["collector".to_string()]);
        assert_ne!(h.snapshot().status, zensight_common::HealthStatus::Healthy);
    }

    /// **Shutdown is not a death.** Aborting a worker on the way out must not
    /// be reported as one, or every clean stop would publish a false finding on
    /// its last health tick.
    ///
    /// This is the case that decides the design: a wrapper *around the future*
    /// cannot tell a cancel from a panic — and, worse, panics along with the
    /// future it wraps and so records nothing at all. Waiting on the
    /// `JoinHandle` can read `JoinError::is_cancelled()`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_aborted_worker_is_not_a_dead_worker() {
        let h = health();
        let worker = tokio::spawn(std::future::pending::<()>());
        let abort = worker.abort_handle();
        let sup = supervise(h.clone(), "long-poll", worker);

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        abort.abort();
        sup.await.expect("supervisor");

        assert!(
            h.dead_workers().is_empty(),
            "shutdown's own abort was reported as a death: {:?}",
            h.dead_workers()
        );
        assert_eq!(h.snapshot().status, zensight_common::HealthStatus::Healthy);
    }
}
