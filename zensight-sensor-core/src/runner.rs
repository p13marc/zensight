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
    liveliness: Option<LivelinessManager>,
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
    /// Spawned tasks.
    tasks: Vec<JoinHandle<()>>,
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
            tasks: Vec::new(),
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
        self.liveliness = Some(liveliness);
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
    pub fn with_format(mut self, format: Format) -> Self {
        self.publisher = Publisher::new(self.session.clone(), self.config.producer(), format);
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
        self.liveliness.as_ref()
    }

    /// Spawn a worker task.
    ///
    /// The task will be tracked and aborted on shutdown.
    pub fn spawn<F>(&mut self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let handle = tokio::spawn(future);
        self.tasks.push(handle);
    }

    /// Spawn a worker task that returns a Result.
    ///
    /// Errors are logged automatically.
    pub fn spawn_with_error<F, E>(&mut self, name: String, future: F)
    where
        F: Future<Output = std::result::Result<(), E>> + Send + 'static,
        E: std::fmt::Display + Send + 'static,
    {
        let handle = tokio::spawn(async move {
            if let Err(e) = future.await {
                tracing::error!(worker = %name, error = %e, "Worker failed");
            }
        });
        self.tasks.push(handle);
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
                    Ok(task) => self.tasks.push(task),
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
                    Ok(task) => self.tasks.push(task),
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
            self.tasks
                .push(tokio::spawn(crate::alert::serve_alerts_query(reporter)));
        }

        // Presence is not optional: declare the sensor-level liveliness token
        // (`state/<producer>/alive`) unless [`Self::with_liveliness`] already
        // did. The frontend flips this sensor's card Offline when the token
        // vanishes (clean close or lease expiry), so a sensor without a token
        // would read as its last health forever. Declaration failure is only a
        // warning — a broken liveliness path must never stop telemetry.
        if self.liveliness.is_none() {
            match LivelinessManager::new(self.session.clone(), self.publisher.v1().clone()).await {
                Ok(manager) => self.liveliness = Some(manager),
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
            self.tasks.push(task);
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
            .with_qos(zensight_common::QosClass::Evidence);
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
                    let evidence = zensight_common::HostEvidence {
                        sensor: name.clone(),
                        source: source.clone(),
                        observer: None, // self-report
                        host_id: id.host_id,
                        boot_id: id.boot_id,
                        hostname: Some(id.hostname),
                        fqdn: id.fqdn,
                        ips: id.ips,
                        macs: id.macs,
                        vendor: None,
                        platform: None,
                        container_id: id.container_id,
                        cloud: id.cloud,
                        last_updated: now,
                    };
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
            self.tasks.push(task);
        }

        tracing::info!(
            sensor = %self.name,
            tasks = self.tasks.len(),
            "Sensor running. Press Ctrl+C or send SIGTERM to stop."
        );

        // Wait for a shutdown signal. Catch both Ctrl+C (SIGINT) and SIGTERM:
        // systemd `stop` and `docker stop` send SIGTERM, and if we only awaited
        // Ctrl+C we'd be SIGKILLed after the stop timeout — never reaching the
        // graceful path below (alert tombstones + a clean liveliness close).
        wait_for_shutdown().await;

        tracing::info!(sensor = %self.name, "Received shutdown signal");

        // Abort all tasks
        for task in &self.tasks {
            task.abort();
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
            if let Err(e) = reporter.reconcile(SENSOR_BUDGET_RULE, &[key]).await {
                tracing::warn!(error = %e, "sensor-budget: reconcile failed");
            }
            true
        }
        None => {
            if let Err(e) = reporter.reconcile(SENSOR_BUDGET_RULE, &[]).await {
                tracing::warn!(error = %e, "sensor-budget: reconcile failed");
            }
            false
        }
    }
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
    // Runner tests require a Zenoh session, which we can't easily mock.
    // Integration tests should cover the runner functionality.
}
