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
use crate::status::StatusPublisher;

/// Sensor runner that manages the lifecycle of a protocol sensor.
///
/// Handles:
/// - Configuration loading
/// - Logging initialization
/// - Zenoh connection
/// - Task spawning and management
/// - Graceful shutdown on Ctrl+C
/// - Status publishing (optional)
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
    /// The instance's `<source>` key segment (hostname / device-poller id).
    /// Host-scopes the control-plane keys (`{key_prefix}/{source}/@/…`) and
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
    /// Status publisher (optional).
    status_publisher: Option<StatusPublisher>,
    /// Liveliness manager for presence detection.
    liveliness: Option<LivelinessManager>,
    /// Sensor health tracker, published periodically to the host-scoped
    /// `<prefix>/<source>/@/health` so
    /// the frontend's Sensors view / health bar populate. Sensors may update it
    /// (device counts, poll durations) via [`Self::health`].
    health: Arc<crate::health::SensorHealth>,
    /// Host identity envelope (identity envelope, #301). Set via
    /// [`Self::with_identity`]; drives the `_meta/sensors` +
    /// `_meta/evidence` publication task (keyed by [`Self::source`]).
    identity: Option<crate::identity::SharedIdentity>,
    /// Spawned tasks.
    tasks: Vec<JoinHandle<()>>,
}

impl<C: SensorConfig> SensorRunner<C> {
    /// Create a new sensor runner.
    ///
    /// `source` is the instance's `<source>` key segment (typically the
    /// sensor config's `resolved_source()`): it host-scopes the control-plane
    /// keys (`{key_prefix}/{source}/@/health` etc.) and feeds the
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

        tracing::info!(sensor = %name, source = %source, version = %version, "Starting sensor");

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
            config.key_prefix(),
            Format::Json, // Default to JSON, can be overridden
        );

        // Health tracker publishes JSON to the host-scoped
        // `<prefix>/<source>/@/health` (publish_health ignores the publisher's
        // format, so the initial publisher is fine even if `with_format` later
        // changes telemetry encoding).
        let health = Arc::new(
            crate::health::SensorHealth::new(name.clone())
                .with_publisher(publisher.clone())
                .with_source(source.clone()),
        );

        Ok(Self {
            name,
            source,
            version,
            config,
            session,
            publisher,
            status_publisher: None,
            liveliness: None,
            health,
            identity: None,
            tasks: Vec::new(),
        })
    }

    /// The host-scoped control prefix for this instance:
    /// `{key_prefix}/{source}` (e.g. `zensight/sysinfo/hostA`). All per-instance
    /// state channels (`@/health`, `@/errors`, `@/status`, `@/alive`,
    /// `@/devices/**`) hang off it.
    fn control_prefix(&self) -> String {
        format!("{}/{}", self.config.key_prefix(), self.source)
    }

    /// Enable status publishing.
    ///
    /// When enabled, the runner will publish status messages on startup and shutdown.
    pub fn with_status_publishing(mut self) -> Self {
        self.status_publisher = Some(StatusPublisher::new(
            self.publisher.clone(),
            self.control_prefix(),
            &self.name,
            &self.version,
        ));
        self
    }

    /// Enable liveliness tokens for presence detection.
    ///
    /// When enabled, the runner will declare a sensor-level liveliness token
    /// that allows the frontend to instantly detect when this sensor comes
    /// online or goes offline.
    ///
    /// The liveliness manager can also be used to declare device-level tokens
    /// via [`LivelinessManager::declare_device_alive`].
    pub async fn with_liveliness(mut self) -> Result<Self> {
        let liveliness =
            LivelinessManager::new(self.session.clone(), self.control_prefix()).await?;
        self.liveliness = Some(liveliness);
        Ok(self)
    }

    /// Enable the unified on-demand artifact channel (`@/artifact`).
    ///
    /// Registers the given producers (e.g. [`ReportProducer`](crate::ReportProducer)
    /// for debug bundles, [`SnapshotProducer`](crate::SnapshotProducer) for
    /// directory snapshots, or a sensor-specific capture producer). A no-op unless
    /// at least one producer is enabled in the sensor's `artifacts.*` config; when
    /// enabled, spawns one [`ArtifactChannel`](crate::ArtifactChannel) as a tracked
    /// worker. The runner's `source` is this host's id (for a request's
    /// `target_source` filter).
    pub fn with_artifacts(
        mut self,
        producers: Vec<Arc<dyn crate::artifact::ArtifactProducer>>,
    ) -> Self {
        if let Some(channel) = crate::artifact::ArtifactChannel::new(
            self.session.clone(),
            self.config.key_prefix().to_string(),
            self.source.clone(),
            producers,
        ) {
            self.spawn(channel.run());
            tracing::info!("artifact channel enabled");
        }
        self
    }

    /// Enable the host-identity envelope (#301).
    ///
    /// Detects the local [`HostIdentity`](crate::HostIdentity) (hashed
    /// machine-id, boot id, hostname, IPs, MACs), stamps `host_id` onto health
    /// snapshots, and — once [`run`](Self::run) starts — publishes the sensor's
    /// registration (`zensight/_meta/sensors/<name>/<source>`) and self-report
    /// evidence (`zensight/_meta/evidence/host/<name>/<source>`) every 60 s via
    /// cached publishers, re-detecting on a slow timer for DHCP churn.
    ///
    /// The keys use the runner's `source` — the same value that host-scopes
    /// the control-plane keys and the telemetry `<source>` segment.
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
        self.publisher = Publisher::new(self.session.clone(), self.config.key_prefix(), format);
        // Recreate status publisher with new publisher (control prefix survives
        // the rebuild — it derives from config + source, not the publisher).
        if self.status_publisher.is_some() {
            self.status_publisher = Some(StatusPublisher::new(
                self.publisher.clone(),
                self.control_prefix(),
                &self.name,
                &self.version,
            ));
        }
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
    /// Returns `None` if liveliness was not enabled via [`Self::with_liveliness`].
    pub fn liveliness(&self) -> Option<&LivelinessManager> {
        self.liveliness.as_ref()
    }

    /// Create a publisher with a different key prefix.
    pub fn publisher_with_prefix(&self, prefix: impl Into<String>) -> Publisher {
        Publisher::new(self.session.clone(), prefix, self.publisher.format())
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
    /// 1. Publish "running" status (if enabled)
    /// 2. Wait for a shutdown signal (Ctrl+C / SIGINT or, on Unix, SIGTERM)
    /// 3. Abort all spawned tasks
    /// 4. Publish "offline" status (if enabled)
    /// 5. Close the Zenoh session
    pub async fn run(self) -> Result<()> {
        self.run_with_metadata(None).await
    }

    /// Run the sensor with custom status metadata.
    pub async fn run_with_metadata(mut self, metadata: Option<serde_json::Value>) -> Result<()> {
        // Publish running status
        if let Some(ref status_pub) = self.status_publisher
            && let Err(e) = status_pub.publish_running(metadata).await
        {
            tracing::warn!(error = %e, "Failed to publish running status");
        }

        // Periodically publish sensor health to `<prefix>/<source>/@/health` so the
        // frontend's Sensors view and dashboard health bar populate. The first
        // tick fires immediately, then every 10s.
        {
            let health = self.health.clone();
            let task = tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
                loop {
                    tick.tick().await;
                    if let Err(e) = health.publish_health().await {
                        tracing::warn!(error = %e, "Failed to publish sensor health");
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
                self.config.key_prefix().to_string(),
                Format::Json,
                crate::advanced_publisher::AdvancedPublisherConfig::cache_only(1),
            )
            .with_qos(zensight_common::QosClass::Evidence);
            let name = self.name.clone();
            let version = self.version.clone();
            let key_prefix = self.config.key_prefix().to_string();
            let health = self.health.clone();
            let identity_cfg = self.config.identity_config();
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
                let info_key = zensight_common::sensor_info_key(&name, &source);
                let evidence_key = zensight_common::host_evidence_key(&name, &source);
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
                        key_prefix: key_prefix.clone(),
                        source: source.clone(),
                        host_id: id.host_id.clone(),
                        boot_id: id.boot_id.clone(),
                        hostname: Some(id.hostname.clone()),
                        fqdn: id.fqdn.clone(),
                        ips: id.ips.clone(),
                        macs: id.macs.clone(),
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
        // graceful path below (offline status + alert tombstones).
        wait_for_shutdown().await;

        tracing::info!(sensor = %self.name, "Received shutdown signal");

        // Abort all tasks
        for task in &self.tasks {
            task.abort();
        }

        // Wait briefly for tasks to clean up
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Publish offline status
        if let Some(ref status_pub) = self.status_publisher
            && let Err(e) = status_pub.publish_offline().await
        {
            tracing::warn!(error = %e, "Failed to publish offline status");
        }

        // Close Zenoh session
        if let Err(e) = self.session.close().await {
            tracing::warn!(error = %e, "Error closing Zenoh session");
        }

        tracing::info!(sensor = %self.name, "Goodbye!");

        Ok(())
    }
}

/// Wait for an OS shutdown signal: Ctrl+C (SIGINT) or, on Unix, SIGTERM.
///
/// systemd and Docker stop a process with SIGTERM, so handling only Ctrl+C
/// would let the orchestrator SIGKILL the sensor after its stop timeout,
/// skipping the graceful shutdown (offline status + alert tombstones).
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
