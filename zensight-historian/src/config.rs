//! Configuration for the historian (#906).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use zensight_common::config::{LoggingConfig, ZenohConfig};
use zensight_common::serialization::Format;
use zensight_sensor_core::SensorConfig;

/// Floor on the flush interval. A flush is one redb transaction; running it
/// more than once a second turns a batching store into a write amplifier.
pub const MIN_FLUSH_INTERVAL_SECS: u64 = 1;

/// Floor on the prune interval. Retention is a slow bound, and a prune that
/// runs every few seconds spends its time proving there is nothing to do.
pub const MIN_PRUNE_INTERVAL_SECS: u64 = 30;

fn default_key_expr() -> String {
    zensight_common::subscribe::DEFAULT_TELEMETRY_KEY_EXPR.to_string()
}
fn default_hot_secs() -> usize {
    600
}
fn default_minute_days() -> i64 {
    2
}
fn default_hour_days() -> i64 {
    90
}
fn default_max_db_bytes() -> u64 {
    2 * 1024 * 1024 * 1024
}
fn default_cache_bytes() -> usize {
    zensight_store::DEFAULT_CACHE_BYTES
}
fn default_batch_size() -> usize {
    4096
}
fn default_flush_interval_secs() -> u64 {
    10
}
fn default_prune_interval_secs() -> u64 {
    300
}

/// Days kept per persisted tier.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionConfig {
    /// Days of minute buckets.
    #[serde(default = "default_minute_days")]
    pub minute_days: i64,
    /// Days of hour buckets.
    ///
    /// 90 rather than the GUI cache's 365: a year of hour buckets across a
    /// fleet's worth of series is the single biggest term in the file size,
    /// and no question has yet been asked of this service that a quarter could
    /// not answer. #911 measures it; raise it when a measurement says to.
    #[serde(default = "default_hour_days")]
    pub hour_days: i64,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            minute_days: default_minute_days(),
            hour_days: default_hour_days(),
        }
    }
}

/// Where and how the tiers are held.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreConfig {
    /// Explicit database path. `None` resolves `$STATE_DIRECTORY`, then
    /// `$XDG_STATE_HOME/zensight`, then `~/.local/state/zensight`.
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// Seconds of per-second samples held in memory per series.
    #[serde(default = "default_hot_secs")]
    pub hot_secs: usize,
    #[serde(default)]
    pub retention: RetentionConfig,
    /// Hard ceiling on the database file; `0` disables it and leaves only the
    /// per-tier retention.
    #[serde(default = "default_max_db_bytes")]
    pub max_db_bytes: u64,
    /// redb's page cache. Its own default is 1 GiB (#625), which on a 1–2 GB
    /// VM reads as a slow multi-day RSS climb toward OOM.
    #[serde(default = "default_cache_bytes")]
    pub cache_bytes: usize,
    /// Samples buffered before a flush is triggered early.
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
    /// Flush interval regardless of buffer depth.
    #[serde(default = "default_flush_interval_secs")]
    pub flush_interval_secs: u64,
    /// How often retention runs.
    #[serde(default = "default_prune_interval_secs")]
    pub prune_interval_secs: u64,
}

impl StoreConfig {
    /// The per-tier windows this configuration means (#1063). The per-second
    /// tier is not persisted by the historian (#911), so its window is the
    /// store's own.
    pub fn retention(&self) -> zensight_store::Retention {
        zensight_store::Retention {
            minute_secs: self.retention.minute_days * 86_400,
            hour_secs: self.retention.hour_days * 86_400,
            ..zensight_store::Retention::default()
        }
    }
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            path: None,
            hot_secs: default_hot_secs(),
            retention: RetentionConfig::default(),
            max_db_bytes: default_max_db_bytes(),
            cache_bytes: default_cache_bytes(),
            batch_size: default_batch_size(),
            flush_interval_secs: default_flush_interval_secs(),
            prune_interval_secs: default_prune_interval_secs(),
        }
    }
}

/// The governor's budget (#811/#812).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourceConfig {
    /// RSS the governor holds this process to, in MiB. Absent means no budget
    /// and no ladder — which reads as *undeclared*, never as fine.
    #[serde(default)]
    pub budget_rss_mb: Option<u64>,
}

/// The historian's own settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistorianConfig {
    /// The class selector to ingest. Base-**relative** (#466): the session
    /// applies the deployment namespace, so a selector spelling the base
    /// matches nothing — with a healthy session and an empty database, which
    /// is precisely the failure this default exists to avoid restating
    /// wrongly in a second place.
    #[serde(default = "default_key_expr")]
    pub key_expr: String,
    #[serde(default)]
    pub store: StoreConfig,
    #[serde(default)]
    pub resources: ResourceConfig,
}

impl Default for HistorianConfig {
    fn default() -> Self {
        Self {
            key_expr: default_key_expr(),
            store: StoreConfig::default(),
            resources: ResourceConfig::default(),
        }
    }
}

/// The whole config file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HistorianSensorConfig {
    #[serde(default)]
    pub zenoh: ZenohConfig,
    #[serde(default)]
    pub serialization: Format,
    #[serde(default)]
    pub historian: HistorianConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
}

impl HistorianSensorConfig {
    /// The `source` this service reports as: its own hostname. A historian is
    /// not a proxy — it observes no device but the host it runs on, and the
    /// series it holds carry their own origins.
    pub fn resolved_source(&self) -> String {
        hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| "unknown".to_string())
    }
}

/// Resolve the database path: explicit, then `$STATE_DIRECTORY` (systemd sets
/// it from `StateDirectory=`), then `$XDG_STATE_HOME/zensight`, then
/// `~/.local/state/zensight`. `None` when the process has no home either, in
/// which case the caller runs memory-only and says so.
pub fn resolve_store_path(explicit: Option<&Path>) -> Option<PathBuf> {
    const FILE: &str = "history.redb";
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    if let Ok(state) = std::env::var("STATE_DIRECTORY") {
        let first = state.split(':').next().unwrap_or(state.as_str());
        if !first.is_empty() {
            return Some(Path::new(first).join(FILE));
        }
    }
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        // Split the join so no source literal spells the deployment base
        // (CI guard #466) — this is a filesystem path, not a Zenoh key.
        return Some(Path::new(&xdg).join("zensight").join(FILE));
    }
    if let Ok(home) = std::env::var("HOME") {
        return Some(Path::new(&home).join(".local/state/zensight").join(FILE));
    }
    None
}

impl SensorConfig for HistorianSensorConfig {
    fn zenoh(&self) -> &ZenohConfig {
        &self.zenoh
    }

    fn logging(&self) -> &LoggingConfig {
        &self.logging
    }

    fn producer(&self) -> &'static str {
        crate::PRODUCER
    }

    fn budget_bytes(&self) -> Option<u64> {
        self.historian
            .resources
            .budget_rss_mb
            .map(|mb| mb.saturating_mul(1024 * 1024))
    }

    fn validate(&self) -> zensight_sensor_core::Result<()> {
        let h = &self.historian;
        let mut problems = Vec::new();

        // A selector that spells the deployment base matches nothing, and the
        // symptom is a healthy session with an empty database — the one
        // failure mode worth refusing at startup rather than discovering in a
        // week's missing history (#466).
        if let Err(e) = zensight_common::keyexpr::validate_relative_selector(&h.key_expr) {
            problems.push(format!("historian.key_expr: {e}"));
        }
        if h.store.hot_secs == 0 {
            problems.push("historian.store.hot_secs must be > 0".to_string());
        }
        if h.store.batch_size == 0 {
            problems.push("historian.store.batch_size must be > 0".to_string());
        }
        if h.store.flush_interval_secs < MIN_FLUSH_INTERVAL_SECS {
            problems.push(format!(
                "historian.store.flush_interval_secs must be >= {MIN_FLUSH_INTERVAL_SECS}"
            ));
        }
        if h.store.prune_interval_secs < MIN_PRUNE_INTERVAL_SECS {
            problems.push(format!(
                "historian.store.prune_interval_secs must be >= {MIN_PRUNE_INTERVAL_SECS}"
            ));
        }
        if h.store.retention.minute_days <= 0 || h.store.retention.hour_days <= 0 {
            problems.push(
                "historian.store.retention days must be > 0 — a tier kept for zero days is \
                 a tier that is written and immediately deleted, which costs the writes and \
                 answers nothing"
                    .to_string(),
            );
        }
        if h.store.retention.hour_days < h.store.retention.minute_days {
            problems.push(
                "historian.store.retention.hour_days must be >= minute_days — the coarse tier \
                 exists to outlive the fine one, and a shorter hour tier leaves a hole between \
                 them that no query can fill"
                    .to_string(),
            );
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(zensight_sensor_core::SensorError::config(
                problems.join("; "),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate() {
        HistorianSensorConfig::default()
            .validate()
            .expect("the shipped defaults must be a valid configuration");
    }

    /// A selector that spells the deployment base matches nothing (#466), and
    /// the symptom — a healthy session and an empty database — looks exactly
    /// like "the fleet is quiet". Refuse it at startup instead.
    #[test]
    fn a_selector_spelling_the_deployment_base_is_refused() {
        let mut c = HistorianSensorConfig::default();
        c.historian.key_expr = "zensight/v1/*/telemetry/**".to_string();
        assert!(c.validate().is_err());
    }

    /// The coarse tier exists to outlive the fine one. Inverted, they leave a
    /// window that neither tier can answer — old enough that the minute
    /// buckets are gone, recent enough that the hour buckets never covered it.
    #[test]
    fn a_shorter_hour_tier_than_minute_tier_is_refused() {
        let mut c = HistorianSensorConfig::default();
        c.historian.store.retention.minute_days = 30;
        c.historian.store.retention.hour_days = 7;
        let err = c
            .validate()
            .expect_err("inverted retention must be refused");
        assert!(format!("{err}").contains("hour_days"));
    }

    #[test]
    fn zero_length_retention_is_refused() {
        let mut c = HistorianSensorConfig::default();
        c.historian.store.retention.minute_days = 0;
        assert!(c.validate().is_err());
    }

    /// The budget is what the governor holds the process to, and this service
    /// is the one that holds a database on a small VM.
    #[test]
    fn the_budget_is_reported_in_bytes() {
        let mut c = HistorianSensorConfig::default();
        assert_eq!(c.budget_bytes(), None, "absent means undeclared, not fine");
        c.historian.resources.budget_rss_mb = Some(256);
        assert_eq!(c.budget_bytes(), Some(256 * 1024 * 1024));
    }

    #[test]
    fn an_explicit_store_path_wins() {
        let p = PathBuf::from("/var/tmp/h.redb");
        assert_eq!(resolve_store_path(Some(&p)), Some(p));
    }
}
