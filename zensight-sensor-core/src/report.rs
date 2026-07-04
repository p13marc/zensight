//! Debug-bundle contents + packaging (the `report` artifact producer's payload).
//!
//! Provides the [`DebugBundleSource`] trait (+ the [`SimpleBundleSource`] blanket
//! impl) that a sensor supplies to describe its config/health/counters, central
//! secret [`redact`]ion, and [`build_debug_bundle`] which packages a redacted
//! `tar.zst`. The request/status/serve/TTL plumbing lives in the unified
//! [`ArtifactChannel`](crate::ArtifactChannel); the `report` kind is a
//! [`ReportProducer`](crate::ReportProducer) over this module.
//!
//! Packaging runs off the capture/poll path (`spawn_blocking`), is bounded, and
//! the bundle's config is **redacted** of secrets.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;

use crate::health::HealthSnapshot;

/// Everything the bundle needs from a running sensor. A blanket
/// [`SimpleBundleSource`] covers the common case (serialize the config + a health
/// snapshot), so most sensors don't implement this directly.
pub trait DebugBundleSource: Send + Sync + 'static {
    /// Sensor name (e.g. `"netlink"`), used in the bundle filename.
    fn sensor_name(&self) -> String;
    /// Host/source id this sensor reports for (used for `target_source` matching
    /// and the filename).
    fn source_id(&self) -> String;
    /// The sensor's config as JSON. Returned **raw**; secrets are redacted
    /// centrally in [`build_debug_bundle`], so a sensor can never forget to.
    fn config_json(&self) -> serde_json::Value;
    /// Current health snapshot.
    fn health(&self) -> HealthSnapshot;
    /// Free-form counters (ingest/throughput) → `counters.json`. Default empty.
    fn counters(&self) -> serde_json::Value {
        serde_json::json!({})
    }
}

/// The common [`DebugBundleSource`]: a serializable config + the shared health
/// tracker. Build one with [`SimpleBundleSource::new`].
pub struct SimpleBundleSource<C: Serialize + Send + Sync + 'static> {
    sensor_name: String,
    source_id: String,
    config: C,
    health: Arc<crate::health::SensorHealth>,
    counters: serde_json::Value,
}

impl<C: Serialize + Send + Sync + 'static> SimpleBundleSource<C> {
    /// Build a source from the sensor name, host id, config, and health tracker.
    pub fn new(
        sensor_name: impl Into<String>,
        source_id: impl Into<String>,
        config: C,
        health: Arc<crate::health::SensorHealth>,
    ) -> Self {
        SimpleBundleSource {
            sensor_name: sensor_name.into(),
            source_id: source_id.into(),
            config,
            health,
            counters: serde_json::json!({}),
        }
    }

    /// Attach sensor-specific counters (ingest/throughput) to the bundle.
    pub fn with_counters(mut self, counters: serde_json::Value) -> Self {
        self.counters = counters;
        self
    }
}

impl<C: Serialize + Send + Sync + 'static> DebugBundleSource for SimpleBundleSource<C> {
    fn sensor_name(&self) -> String {
        self.sensor_name.clone()
    }
    fn source_id(&self) -> String {
        self.source_id.clone()
    }
    fn config_json(&self) -> serde_json::Value {
        serde_json::to_value(&self.config).unwrap_or(serde_json::Value::Null)
    }
    fn health(&self) -> HealthSnapshot {
        self.health.snapshot()
    }
    fn counters(&self) -> serde_json::Value {
        self.counters.clone()
    }
}

/// Config field names whose value is redacted if the (lowercased) key *contains*
/// one of these. Kept narrow enough not to clobber benign keys like `key_prefix`.
const REDACT_CONTAINS: &[&str] = &[
    "password",
    "passwd",
    "passphrase",
    "secret",
    "token",
    "apikey",
    "credential",
    "bearer",
];

/// Config field names redacted on an exact (lowercased) match.
const REDACT_EXACT: &[&str] = &[
    "community",
    "auth",
    "authorization",
    "private_key",
    "privatekey",
    "priv_key",
    "api_key",
];

const REDACTED: &str = "***REDACTED***";

fn is_secret_key(key: &str, extra: &[String]) -> bool {
    let lk = key.to_ascii_lowercase();
    REDACT_CONTAINS.iter().any(|p| lk.contains(p))
        || REDACT_EXACT.iter().any(|p| lk == *p)
        || extra.iter().any(|p| lk.contains(&p.to_ascii_lowercase()))
}

/// Recursively replace any object value whose key looks secret with a redaction
/// marker. Generic over every sensor config (they share no supertype but all
/// serialize to JSON).
pub fn redact(value: &mut serde_json::Value, extra: &[String]) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if is_secret_key(k, extra) {
                    *v = serde_json::Value::String(REDACTED.to_string());
                } else {
                    redact(v, extra);
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                redact(v, extra);
            }
        }
        _ => {}
    }
}

/// The owned inputs assembled (on the async side) before the blocking build.
pub(crate) struct BundleInputs {
    pub(crate) sensor_name: String,
    pub(crate) source_id: String,
    pub(crate) config: serde_json::Value,
    pub(crate) health: serde_json::Value,
    pub(crate) counters: serde_json::Value,
    pub(crate) created_ms: i64,
}

/// Build a `tar.zst` debug bundle into a temp file under `dir`, returning its
/// path + suggested filename. Runs in `spawn_blocking` (it's synchronous I/O).
/// Enforces `max_bytes` on the uncompressed entry sizes and redacts the config
/// (built-in denylist plus `redact_extra`).
pub(crate) fn build_debug_bundle(
    mut inputs: BundleInputs,
    max_bytes: u64,
    redact_extra: &[String],
    dir: &Path,
) -> std::io::Result<(PathBuf, String)> {
    redact(&mut inputs.config, redact_extra);

    let meta = serde_json::json!({
        "schema": 1,
        "kind": "debug_bundle",
        "sensor": inputs.sensor_name,
        "source": inputs.source_id,
        "created_ms": inputs.created_ms,
    });

    let entries: [(&str, &serde_json::Value); 4] = [
        ("config.json", &inputs.config),
        ("health.json", &inputs.health),
        ("counters.json", &inputs.counters),
        ("meta.json", &meta),
    ];

    // Serialize + enforce the size bound before writing anything.
    let mut serialized: Vec<(&str, Vec<u8>)> = Vec::with_capacity(entries.len());
    let mut total: u64 = 0;
    for (name, value) in entries {
        let data = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
        total += data.len() as u64;
        serialized.push((name, data));
    }
    if total > max_bytes {
        return Err(std::io::Error::other(format!(
            "bundle ({total} bytes) exceeds max_bytes ({max_bytes})"
        )));
    }

    let tmp = tempfile::Builder::new()
        .prefix("zsreport-")
        .suffix(".tar.zst")
        .tempfile_in(dir)?;
    let file = tmp.reopen()?;
    let encoder = zstd::Encoder::new(file, 3)?;
    let mut builder = tar::Builder::new(encoder);
    let mtime = (inputs.created_ms / 1000).max(0) as u64;
    for (name, data) in &serialized {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(mtime);
        header.set_cksum();
        builder.append_data(&mut header, name, data.as_slice())?;
    }
    let encoder = builder.into_inner()?;
    encoder.finish()?;

    let filename = format!(
        "zensight-debug-{}-{}-{}.tar.zst",
        sanitize(&inputs.sensor_name),
        sanitize(&inputs.source_id),
        inputs.created_ms
    );
    let (_file, path) = tmp.keep().map_err(|e| e.error)?;
    Ok((path, filename))
}

/// Make a string safe for a filename segment.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_hits_secrets_not_benign_keys() {
        let mut v = serde_json::json!({
            "key_prefix": "zensight/netlink",
            "community": "public",
            "auth_password": "hunter2",
            "nested": { "api_key": "abc", "token": "xyz", "name": "ok" },
            "list": [ { "password": "p" } ],
        });
        redact(&mut v, &[]);
        assert_eq!(v["key_prefix"], "zensight/netlink"); // benign, preserved
        assert_eq!(v["community"], REDACTED);
        assert_eq!(v["auth_password"], REDACTED);
        assert_eq!(v["nested"]["api_key"], REDACTED);
        assert_eq!(v["nested"]["token"], REDACTED);
        assert_eq!(v["nested"]["name"], "ok");
        assert_eq!(v["list"][0]["password"], REDACTED);
    }

    #[test]
    fn redact_extra_patterns() {
        let mut v = serde_json::json!({ "custom_secret_field": "s", "normal": "n" });
        redact(&mut v, &["custom_secret_field".to_string()]);
        assert_eq!(v["custom_secret_field"], REDACTED);
        assert_eq!(v["normal"], "n");
    }

    #[test]
    fn build_bundle_is_a_valid_tar_zst_with_redaction() {
        let dir = tempfile::tempdir().unwrap();
        let inputs = BundleInputs {
            sensor_name: "netlink".into(),
            source_id: "host1".into(),
            config: serde_json::json!({ "community": "public", "key_prefix": "zensight/netlink" }),
            health: serde_json::json!({ "status": "healthy" }),
            counters: serde_json::json!({ "received": 10 }),
            created_ms: 1_700_000_000_000,
        };
        let (path, filename) =
            build_debug_bundle(inputs, 64 * 1024 * 1024, &[], dir.path()).unwrap();
        assert!(filename.starts_with("zensight-debug-netlink-host1-"));
        assert!(path.exists());

        // Decompress + untar and check entries + redaction.
        let f = std::fs::File::open(&path).unwrap();
        let dec = zstd::Decoder::new(f).unwrap();
        let mut ar = tar::Archive::new(dec);
        let mut found = std::collections::HashMap::new();
        for entry in ar.entries().unwrap() {
            let mut entry = entry.unwrap();
            let name = entry.path().unwrap().to_string_lossy().to_string();
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut entry, &mut buf).unwrap();
            found.insert(name, buf);
        }
        assert!(found.contains_key("config.json"));
        assert!(found.contains_key("meta.json"));
        let config = &found["config.json"];
        assert!(config.contains(REDACTED), "community should be redacted");
        assert!(!config.contains("public"), "secret value must not leak");
        assert!(config.contains("zensight/netlink"), "benign key preserved");
    }

    #[test]
    fn build_bundle_enforces_max_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let big = "x".repeat(2000);
        let inputs = BundleInputs {
            sensor_name: "s".into(),
            source_id: "h".into(),
            config: serde_json::json!({ "blob": big }),
            health: serde_json::json!({}),
            counters: serde_json::json!({}),
            created_ms: 1,
        };
        assert!(build_debug_bundle(inputs, 100, &[], dir.path()).is_err());
    }
}
