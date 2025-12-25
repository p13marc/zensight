//! Bridge runner for lifecycle management.

use std::future::Future;
use std::sync::Arc;

use tokio::signal;
use tokio::task::JoinHandle;

use zensight_common::{Format, LoggingConfig, connect, init_tracing};

use crate::BridgeArgs;
use crate::config::BridgeConfig;
use crate::error::{BridgeError, Result};
use crate::publisher::Publisher;
use crate::status::StatusPublisher;

/// Bridge runner that manages the lifecycle of a protocol bridge.
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
/// use zensight_bridge_framework::{BridgeArgs, BridgeConfig, BridgeRunner};
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     let args = BridgeArgs::parse_with_default("mybridge.json5");
///     let config = MyBridgeConfig::load(&args.config)?;
///
///     let runner = BridgeRunner::new("mybridge", config).await?;
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
pub struct BridgeRunner<C: BridgeConfig> {
    /// Bridge name for logging and status.
    name: String,
    /// Bridge version.
    version: String,
    /// The loaded configuration.
    config: C,
    /// Zenoh session.
    session: Arc<zenoh::Session>,
    /// Publisher for telemetry.
    publisher: Publisher,
    /// Status publisher (optional).
    status_publisher: Option<StatusPublisher>,
    /// Spawned tasks.
    tasks: Vec<JoinHandle<()>>,
}

impl<C: BridgeConfig> BridgeRunner<C> {
    /// Create a new bridge runner.
    ///
    /// This will:
    /// 1. Initialize logging based on config (with optional CLI override)
    /// 2. Connect to Zenoh
    /// 3. Create the publisher
    pub async fn new(name: impl Into<String>, config: C) -> Result<Self> {
        Self::new_with_args(name, config, None).await
    }

    /// Create a new bridge runner with CLI args for log level override.
    pub async fn new_with_args(
        name: impl Into<String>,
        config: C,
        args: Option<&BridgeArgs>,
    ) -> Result<Self> {
        let name = name.into();
        let version = env!("CARGO_PKG_VERSION").to_string();

        // Initialize logging with optional CLI override
        let log_config = if let Some(args) = args {
            if let Some(ref level) = args.log_level {
                LoggingConfig {
                    level: level.clone(),
                }
            } else {
                config.logging().clone()
            }
        } else {
            config.logging().clone()
        };

        init_tracing(&log_config).map_err(|e| BridgeError::config(e.to_string()))?;

        tracing::info!(bridge = %name, version = %version, "Starting bridge");

        // Connect to Zenoh
        let session = Arc::new(
            connect(config.zenoh())
                .await
                .map_err(|e| BridgeError::ZenohConnection(e.to_string()))?,
        );

        tracing::info!(zid = %session.zid(), "Connected to Zenoh");

        // Create publisher
        let publisher = Publisher::new(
            session.clone(),
            config.key_prefix(),
            Format::Json, // Default to JSON, can be overridden
        );

        Ok(Self {
            name,
            version,
            config,
            session,
            publisher,
            status_publisher: None,
            tasks: Vec::new(),
        })
    }

    /// Enable status publishing.
    ///
    /// When enabled, the runner will publish status messages on startup and shutdown.
    pub fn with_status_publishing(mut self) -> Self {
        self.status_publisher = Some(StatusPublisher::new(
            self.publisher.clone(),
            &self.name,
            &self.version,
        ));
        self
    }

    /// Set a custom serialization format for the publisher.
    pub fn with_format(mut self, format: Format) -> Self {
        self.publisher = Publisher::new(self.session.clone(), self.config.key_prefix(), format);
        // Recreate status publisher with new publisher
        if self.status_publisher.is_some() {
            self.status_publisher = Some(StatusPublisher::new(
                self.publisher.clone(),
                &self.name,
                &self.version,
            ));
        }
        self
    }

    /// Get the bridge name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the bridge version.
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

    /// Run the bridge until Ctrl+C is received.
    ///
    /// This will:
    /// 1. Publish "running" status (if enabled)
    /// 2. Wait for Ctrl+C signal
    /// 3. Abort all spawned tasks
    /// 4. Publish "offline" status (if enabled)
    /// 5. Close the Zenoh session
    pub async fn run(self) -> Result<()> {
        self.run_with_metadata(None).await
    }

    /// Run the bridge with custom status metadata.
    pub async fn run_with_metadata(self, metadata: Option<serde_json::Value>) -> Result<()> {
        // Publish running status
        if let Some(ref status_pub) = self.status_publisher {
            if let Err(e) = status_pub.publish_running(metadata).await {
                tracing::warn!(error = %e, "Failed to publish running status");
            }
        }

        tracing::info!(
            bridge = %self.name,
            tasks = self.tasks.len(),
            "Bridge running. Press Ctrl+C to stop."
        );

        // Wait for shutdown signal
        if let Err(e) = signal::ctrl_c().await {
            tracing::error!(error = %e, "Failed to listen for Ctrl+C");
        }

        tracing::info!(bridge = %self.name, "Received shutdown signal");

        // Abort all tasks
        for task in &self.tasks {
            task.abort();
        }

        // Wait briefly for tasks to clean up
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Publish offline status
        if let Some(ref status_pub) = self.status_publisher {
            if let Err(e) = status_pub.publish_offline().await {
                tracing::warn!(error = %e, "Failed to publish offline status");
            }
        }

        // Close Zenoh session
        if let Err(e) = self.session.close().await {
            tracing::warn!(error = %e, "Error closing Zenoh session");
        }

        tracing::info!(bridge = %self.name, "Goodbye!");

        Ok(())
    }
}

/// Convenience function to run a bridge with minimal boilerplate.
///
/// # Example
///
/// ```ignore
/// use zensight_bridge_framework::{run_bridge, BridgeConfig};
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     run_bridge::<MyBridgeConfig>("mybridge", "mybridge.json5", |runner| {
///         // Configure and spawn workers
///         let publisher = runner.publisher();
///         runner.spawn(my_worker(publisher));
///     }).await
/// }
/// ```
pub async fn run_bridge<C, F>(
    name: &str,
    default_config: &'static str,
    setup: F,
) -> anyhow::Result<()>
where
    C: BridgeConfig,
    F: FnOnce(&mut BridgeRunner<C>),
{
    let args = BridgeArgs::parse_with_default(default_config);
    let config = C::load(&args.config).map_err(|e| anyhow::anyhow!("{}", e))?;

    let mut runner = BridgeRunner::new_with_args(name, config, Some(&args))
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    setup(&mut runner);

    runner.run().await.map_err(|e| anyhow::anyhow!("{}", e))
}

#[cfg(test)]
mod tests {
    // Runner tests require a Zenoh session, which we can't easily mock.
    // Integration tests should cover the runner functionality.
}
