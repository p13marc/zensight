//! Debug-report generation + serving (`@/report`).
//!
//! Wires the generic [`zenoh_blob`] transfer onto the ZenSight keyspace so every
//! sensor can serve an operator-requested debug bundle:
//!
//! - a **request** subscriber (`@/report/request`) — the single authorization
//!   trigger (R7);
//! - a **status** queryable (`@/report/status`) — the report lifecycle;
//! - a **blob** queryable (`@/report/blob/**`, a [`zenoh_blob::BlobServer`]) —
//!   the bytes, with progress / resume / integrity handled by `zenoh-blob`;
//! - a **cancel** subscriber (`@/report/cancel`) — free a temp artifact early.
//!
//! Generation runs off the capture/poll path (`spawn_blocking`), is bounded and
//! rate-limited, and the bundle's config is **redacted** of secrets.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::Mutex;
use ulid::Ulid;
use zenoh_blob::{BlobServer, FileBlobSource, FixedSizeChunker, Manifest, Sha256Digest};
use zensight_common::report::{ReportKind, ReportRequest, ReportState, ReportStatus};
use zensight_common::{
    ReportLimits, report_blob_prefix, report_cancel_key, report_request_key, report_status_key,
};

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
struct BundleInputs {
    sensor_name: String,
    source_id: String,
    config: serde_json::Value,
    health: serde_json::Value,
    counters: serde_json::Value,
    created_ms: i64,
}

/// Build a `tar.zst` debug bundle into a temp file under `dir`, returning its
/// path + suggested filename. Runs in `spawn_blocking` (it's synchronous I/O).
/// Enforces `limits.max_bytes` on the uncompressed entry sizes and redacts the
/// config.
fn build_debug_bundle(
    mut inputs: BundleInputs,
    limits: &ReportLimits,
    dir: &Path,
) -> std::io::Result<(PathBuf, String)> {
    redact(&mut inputs.config, &limits.redact_extra);

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
    if total > limits.max_bytes {
        return Err(std::io::Error::other(format!(
            "bundle ({total} bytes) exceeds max_bytes ({})",
            limits.max_bytes
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

/// Mutable runtime state shared between the channel loop and generation tasks.
#[derive(Default)]
struct Runtime {
    current: Option<ReportState>,
    busy: bool,
    last_gen: Option<Instant>,
    /// The live artifact: id, temp path, and TTL deadline.
    active: Option<Active>,
}

struct Active {
    id: Ulid,
    temp_path: PathBuf,
    expires: Instant,
}

/// Serves the `@/report` channel for one sensor.
pub struct ReportChannel {
    session: Arc<zenoh::Session>,
    key_prefix: String,
    limits: ReportLimits,
    source: Arc<dyn DebugBundleSource>,
    blob: BlobServer,
    state: Arc<Mutex<Runtime>>,
}

impl ReportChannel {
    /// Build a channel for `key_prefix` (e.g. `"zensight/netlink"`).
    pub fn new(
        session: Arc<zenoh::Session>,
        key_prefix: impl Into<String>,
        limits: ReportLimits,
        source: Arc<dyn DebugBundleSource>,
    ) -> Self {
        let key_prefix = key_prefix.into();
        let blob = BlobServer::new(
            session.clone(),
            report_blob_prefix(&key_prefix),
            zenoh_blob::Format::Json,
        );
        ReportChannel {
            session,
            key_prefix,
            limits,
            source,
            blob,
            state: Arc::new(Mutex::new(Runtime::default())),
        }
    }

    /// Serve forever. Spawned as a worker by `SensorRunner::with_report`.
    pub async fn run(self) {
        if let Err(e) = self.run_inner().await {
            tracing::error!(error = %e, "report channel exited");
        }
    }

    async fn run_inner(&self) -> anyhow::Result<()> {
        let req_sub = self
            .session
            .declare_subscriber(report_request_key(&self.key_prefix))
            .await
            .map_err(|e| anyhow::anyhow!("declare request sub: {e}"))?;
        let status_q = self
            .session
            .declare_queryable(report_status_key(&self.key_prefix))
            .await
            .map_err(|e| anyhow::anyhow!("declare status queryable: {e}"))?;
        let cancel_sub = self
            .session
            .declare_subscriber(report_cancel_key(&self.key_prefix))
            .await
            .map_err(|e| anyhow::anyhow!("declare cancel sub: {e}"))?;

        // Run the blob server on its own task.
        tokio::spawn(self.blob.clone().run());

        let mut ttl_tick = tokio::time::interval(Duration::from_secs(5));
        tracing::info!(prefix = %self.key_prefix, "report channel ready");

        loop {
            tokio::select! {
                Ok(sample) = req_sub.recv_async() => {
                    self.handle_request(&sample.payload().to_bytes()).await;
                }
                Ok(query) = status_q.recv_async() => {
                    let status = self.status().await;
                    let payload = serde_json::to_vec(&status).unwrap_or_default();
                    if let Err(e) = query.reply(query.key_expr().clone(), payload).await {
                        tracing::warn!(error = %e, "report status reply failed");
                    }
                }
                Ok(sample) = cancel_sub.recv_async() => {
                    let body = sample.payload().to_bytes();
                    if let Ok(id) = std::str::from_utf8(&body).unwrap_or("").trim().parse::<Ulid>() {
                        self.expire(id).await;
                    }
                }
                _ = ttl_tick.tick() => {
                    self.reap_expired().await;
                }
            }
        }
    }

    async fn status(&self) -> ReportStatus {
        let rt = self.state.lock().await;
        ReportStatus {
            current: rt.current.clone(),
            busy: rt.busy,
            max_bytes: self.limits.max_bytes,
            cooldown_secs: self.limits.cooldown_secs,
        }
    }

    async fn handle_request(&self, payload: &[u8]) {
        let req: ReportRequest = match serde_json::from_slice(payload) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "bad report request");
                return;
            }
        };

        // Authorization / validation (R7).
        if !matches!(req.kind, ReportKind::DebugBundle) {
            self.set_failed(req.id, "unsupported report kind").await;
            return;
        }
        if let Some(target) = &req.opts.target_source
            && target != &self.source.source_id()
        {
            // Not for us — another host running this protocol will answer.
            return;
        }

        {
            let mut rt = self.state.lock().await;
            if rt.busy {
                drop(rt);
                self.set_failed(req.id, "already generating a report").await;
                return;
            }
            if let Some(last) = rt.last_gen
                && last.elapsed() < Duration::from_secs(self.limits.cooldown_secs)
            {
                drop(rt);
                self.set_failed(req.id, "cooling down; try again shortly")
                    .await;
                return;
            }
            rt.busy = true;
            rt.current = Some(ReportState::Generating { id: req.id });
        }

        // Generate off the loop so status stays responsive.
        let inputs = BundleInputs {
            sensor_name: self.source.sensor_name(),
            source_id: self.source.source_id(),
            config: self.source.config_json(),
            health: serde_json::to_value(self.source.health()).unwrap_or(serde_json::Value::Null),
            counters: self.source.counters(),
            created_ms: chrono::Utc::now().timestamp_millis(),
        };
        let limits = self.limits.clone();
        let blob = self.blob.clone();
        let state = self.state.clone();
        let blob_prefix = report_blob_prefix(&self.key_prefix);
        let id = req.id;
        tokio::spawn(async move {
            let outcome = generate(id, inputs, &limits, &blob, blob_prefix).await;
            let mut rt = state.lock().await;
            rt.busy = false;
            rt.last_gen = Some(Instant::now());
            match outcome {
                Ok(output) => {
                    // Replace any prior artifact.
                    if let Some(prev) = rt.active.take() {
                        blob.unregister(&prev.id.to_string()).await;
                        let _ = tokio::fs::remove_file(&prev.temp_path).await;
                    }
                    rt.active = Some(output.active);
                    rt.current = Some(output.state);
                }
                Err(e) => {
                    rt.current = Some(ReportState::Failed {
                        id,
                        reason: e.to_string(),
                    });
                }
            }
        });
    }

    async fn set_failed(&self, id: Ulid, reason: &str) {
        let mut rt = self.state.lock().await;
        rt.current = Some(ReportState::Failed {
            id,
            reason: reason.to_string(),
        });
    }

    /// Expire a specific report now (cancel): drop its artifact + blob registration.
    async fn expire(&self, id: Ulid) {
        let mut rt = self.state.lock().await;
        if let Some(active) = &rt.active
            && active.id == id
        {
            let path = active.temp_path.clone();
            rt.active = None;
            self.blob.unregister(&id.to_string()).await;
            let _ = tokio::fs::remove_file(&path).await;
            rt.current = Some(ReportState::Expired { id });
        }
    }

    /// Reap any artifact past its TTL.
    async fn reap_expired(&self) {
        let expired = {
            let rt = self.state.lock().await;
            rt.active
                .as_ref()
                .filter(|a| a.expires <= Instant::now())
                .map(|a| (a.id, a.temp_path.clone()))
        };
        if let Some((id, path)) = expired {
            let mut rt = self.state.lock().await;
            rt.active = None;
            self.blob.unregister(&id.to_string()).await;
            let _ = tokio::fs::remove_file(&path).await;
            rt.current = Some(ReportState::Expired { id });
        }
    }
}

/// Output of a successful generation: the `Ready` state to publish + the live
/// artifact record for TTL reaping.
struct GenOutput {
    state: ReportState,
    active: Active,
}

/// The blocking-build + manifest + registration step.
async fn generate(
    id: Ulid,
    inputs: BundleInputs,
    limits: &ReportLimits,
    blob: &BlobServer,
    blob_prefix: String,
) -> anyhow::Result<GenOutput> {
    let dir = std::env::temp_dir();
    let limits_blocking = limits.clone();
    let (temp_path, filename) =
        tokio::task::spawn_blocking(move || build_debug_bundle(inputs, &limits_blocking, &dir))
            .await??;

    // Compute the manifest by streaming the temp file (never read_to_end).
    let chunker = FixedSizeChunker::new(limits.chunk_size);
    let mut reader = tokio::fs::File::open(&temp_path).await?;
    let created_ms = chrono::Utc::now().timestamp_millis();
    let manifest = Manifest::compute::<_, Sha256Digest>(
        &mut reader,
        &chunker,
        id.to_string(),
        filename,
        created_ms,
    )
    .await
    .map_err(|e| anyhow::anyhow!("manifest: {e}"))?;

    blob.register(manifest.clone(), Arc::new(FileBlobSource::new(&temp_path)))
        .await;

    let expires_ms = created_ms + (limits.ttl_secs as i64) * 1000;
    Ok(GenOutput {
        state: ReportState::Ready {
            id,
            manifest,
            blob_prefix,
            expires_ms,
        },
        active: Active {
            id,
            temp_path,
            expires: Instant::now() + Duration::from_secs(limits.ttl_secs),
        },
    })
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
        let limits = ReportLimits {
            enabled: true,
            ..Default::default()
        };
        let (path, filename) = build_debug_bundle(inputs, &limits, dir.path()).unwrap();
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
        let limits = ReportLimits {
            enabled: true,
            max_bytes: 100,
            ..Default::default()
        };
        assert!(build_debug_bundle(inputs, &limits, dir.path()).is_err());
    }
}
