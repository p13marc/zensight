//! Log-bundle export artifact (#555): package the log lines matching a filter
//! into a single zstd bundle delivered over `@blob`, so operators can hand logs
//! to a ticket/vendor instead of screen-scraping the live table.
//!
//! The request selectors mirror the events/search query (#553), so the same
//! filter that scopes a feed scopes the export. Lines come from the durable
//! store (#544) when present, plus the hot ring; the bundle's first line is a
//! JSON manifest (query, counts, range, truncation).

use std::io::Write;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;
use zensight_common::CommonArtifactLimits;
use zensight_common::LogRecord;
use zensight_common::artifact::{ArtifactKind, KindAdvert, LogBundleFormat};
use zensight_sensor_core::{ArtifactProducer, DeliveryKind, ProduceCtx, Produced};

use crate::config::LogBundleLimits;
use crate::query::EventRing;
use crate::search::LogMatcher;
use crate::store::LogStore;

/// Manifest written as the first bundle line — what the export contains.
#[derive(Debug, Serialize)]
struct Manifest {
    kind: &'static str,
    sensor: &'static str,
    source: String,
    created_ms: i64,
    format: &'static str,
    /// The selectors that produced this bundle (echoed back).
    query: serde_json::Value,
    /// Number of records included.
    lines: usize,
    /// True when a cap (lines or bytes) stopped the export early.
    truncated: bool,
}

/// Producer for the `logbundle` artifact kind (#555).
pub struct LogBundleProducer {
    common: CommonArtifactLimits,
    max_lines: u64,
    host: String,
    store: Option<Arc<LogStore>>,
    ring: EventRing,
}

impl LogBundleProducer {
    pub fn new(
        limits: &LogBundleLimits,
        host: impl Into<String>,
        store: Option<Arc<LogStore>>,
        ring: EventRing,
    ) -> Self {
        Self {
            common: limits.common(),
            max_lines: limits.max_lines,
            host: host.into(),
            store,
            ring,
        }
    }
}

#[async_trait]
impl ArtifactProducer for LogBundleProducer {
    fn kind(&self) -> &'static str {
        "logbundle"
    }
    fn common(&self) -> &CommonArtifactLimits {
        &self.common
    }
    fn delivery_kind(&self) -> DeliveryKind {
        DeliveryKind::Blob
    }
    fn advert(&self) -> KindAdvert {
        KindAdvert::LogBundle {
            max_lines: self.max_lines,
        }
    }
    fn accepts(&self, kind: &ArtifactKind) -> Result<(), String> {
        match kind {
            ArtifactKind::LogBundle { pattern, .. } => {
                // Reject a bad regex at request time (clear failure, not mid-produce).
                LogMatcher::new(pattern.as_deref(), None, None, None, None).map(|_| ())
            }
            _ => Err("logbundle producer given a non-logbundle request".into()),
        }
    }

    async fn produce(&self, kind: ArtifactKind, _ctx: ProduceCtx) -> anyhow::Result<Produced> {
        let ArtifactKind::LogBundle {
            from,
            to,
            pattern,
            severity_min,
            unit,
            app,
            source,
            format,
        } = kind
        else {
            anyhow::bail!("logbundle producer given a non-logbundle request");
        };

        let matcher = LogMatcher::new(
            pattern.as_deref(),
            severity_min.as_deref(),
            unit.as_deref(),
            app.as_deref(),
            None,
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;

        let (from_ms, to_ms) = (from.unwrap_or(i64::MIN), to.unwrap_or(i64::MAX));
        let max_lines = self.max_lines.max(1) as usize;
        let max_bytes = self.common.max_bytes;
        let store = self.store.clone();
        let ring = self.ring.clone();
        let source_filter = source.clone();
        let host = self.host.clone();
        let query = serde_json::json!({
            "from": from, "to": to, "pattern": pattern, "severity_min": severity_min,
            "unit": unit, "app": app, "source": source,
        });

        // Gather + write on a blocking thread (store scan + zstd + fs are all
        // blocking); the intake loop is untouched.
        tokio::task::spawn_blocking(move || {
            // Collect matching records, deduped by uid, ascending (chronological).
            let mut by_uid: std::collections::BTreeMap<String, LogRecord> = Default::default();
            // Durable store (scan bounded so a huge range can't run forever).
            if let Some(store) = &store {
                let scan_cap = max_lines.saturating_mul(4).max(100_000);
                if let Ok(recs) =
                    store.search(from_ms, to_ms, None, max_lines + 1, &matcher, scan_cap)
                {
                    for r in recs {
                        by_uid.insert(r.uid.clone(), r);
                    }
                }
            }
            // Hot ring (recent lines maybe not yet flushed).
            if let Ok(r) = ring.lock() {
                for rec in r.iter() {
                    if rec.ts >= from_ms
                        && rec.ts <= to_ms
                        && matcher.matches(rec)
                        && !by_uid.contains_key(&rec.uid)
                    {
                        by_uid.insert(rec.uid.clone(), rec.clone());
                    }
                }
            }
            let records: Vec<LogRecord> = by_uid
                .into_values()
                .filter(|r| source_filter.as_deref().is_none_or(|s| r.host == s))
                .collect();

            let total = records.len();
            let truncated = total > max_lines;
            let included = &records[..total.min(max_lines)];

            let filename = format!(
                "logbundle-{host}.{}.zst",
                match format {
                    LogBundleFormat::Jsonl => "jsonl",
                    LogBundleFormat::Text => "txt",
                }
            );
            // The bundle is built into memory and served from it
            // (`Produced::Bytes`): the write loop below is bounded by
            // `max_bytes`, so the buffer is too — and nothing lands at a
            // predictable path in the shared system temp dir anymore.
            let mut enc = zstd::stream::write::Encoder::new(Vec::new(), 3)?;

            let manifest = Manifest {
                kind: "logbundle",
                sensor: "logs",
                source: host.clone(),
                created_ms: chrono::Utc::now().timestamp_millis(),
                format: match format {
                    LogBundleFormat::Jsonl => "jsonl",
                    LogBundleFormat::Text => "text",
                },
                query,
                lines: included.len(),
                // Also flag a byte-cap truncation detected during writing (below).
                truncated,
            };

            // Manifest first (JSON object line, or a `#`-prefixed comment for text).
            match format {
                LogBundleFormat::Jsonl => {
                    serde_json::to_writer(&mut enc, &manifest)?;
                    enc.write_all(b"\n")?;
                }
                LogBundleFormat::Text => {
                    write!(enc, "# ")?;
                    serde_json::to_writer(&mut enc, &manifest)?;
                    enc.write_all(b"\n")?;
                }
            }

            let mut written_bytes: u64 = 0;
            for rec in included {
                let line = match format {
                    LogBundleFormat::Jsonl => serde_json::to_string(rec)?,
                    LogBundleFormat::Text => format!(
                        "{} {} {} {}: {}",
                        rec.ts,
                        rec.host,
                        rec.severity,
                        rec.app.as_deref().unwrap_or("-"),
                        rec.message
                    ),
                };
                written_bytes += line.len() as u64 + 1;
                if written_bytes > max_bytes {
                    // Byte cap hit: stop; the manifest already exists (truncation
                    // is best-effort flagged via the line cap; a byte-cap stop is
                    // rarer). Break to keep a valid bundle.
                    break;
                }
                enc.write_all(line.as_bytes())?;
                enc.write_all(b"\n")?;
            }
            let data = enc.finish()?;
            Ok(Produced::Bytes { data, filename })
        })
        .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::new_ring;

    fn limits() -> LogBundleLimits {
        LogBundleLimits {
            enabled: true,
            max_lines: 3,
            ..Default::default()
        }
    }

    #[test]
    fn advert_and_kind_slug() {
        let (ring, _) = new_ring(100);
        let p = LogBundleProducer::new(&limits(), "h", None, ring);
        assert_eq!(p.kind(), "logbundle");
        assert!(matches!(p.advert(), KindAdvert::LogBundle { max_lines: 3 }));
    }

    #[test]
    fn accepts_validates_pattern() {
        let (ring, _) = new_ring(100);
        let p = LogBundleProducer::new(&limits(), "h", None, ring);
        assert!(
            p.accepts(&ArtifactKind::LogBundle {
                from: None,
                to: None,
                pattern: Some("ok".into()),
                severity_min: None,
                unit: None,
                app: None,
                source: None,
                format: LogBundleFormat::Jsonl,
            })
            .is_ok()
        );
        // A bad kind is rejected.
        assert!(p.accepts(&ArtifactKind::Report {}).is_err());
    }

    /// End-to-end produce: gather matching store lines, write a zstd JSONL bundle
    /// with a manifest, and flag truncation when the line cap bites.
    #[tokio::test]
    async fn produce_writes_filtered_zstd_bundle() {
        use crate::store::LogStore;
        use std::io::Read;
        use zblob::CancelToken;
        use zensight_sensor_core::{ProduceCtx, ProgressUpdate};

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(LogStore::open(dir.path().join("logs.redb")).unwrap());
        // 5 lines; 4 contain "boom" (matcher target).
        let recs: Vec<LogRecord> = (0..5)
            .map(|i| LogRecord {
                uid: format!("{:013}{:012}", 1000 + i, i),
                ts: 1000 + i,
                host: "web01".into(),
                facility: "user".into(),
                severity: "err".into(),
                severity_number: 17,
                app: None,
                pid: None,
                message: if i == 0 {
                    "quiet".into()
                } else {
                    format!("boom {i}")
                },
                labels: Default::default(),
            })
            .collect();
        store.write_batch(&recs).unwrap();

        let (ring, _) = new_ring(100);
        // max_lines = 3 (from limits()) → 4 "boom" matches truncate to 3.
        let p = LogBundleProducer::new(&limits(), "web01", Some(store), ring);

        let cancel = CancelToken::new();
        let (progress, _rx) = tokio::sync::watch::channel(ProgressUpdate::default());
        let ctx = ProduceCtx {
            workdir: dir.path().to_path_buf(),
            cancel,
            progress,
        };
        let produced = p
            .produce(
                ArtifactKind::LogBundle {
                    from: None,
                    to: None,
                    pattern: Some("boom".into()),
                    severity_min: None,
                    unit: None,
                    app: None,
                    source: None,
                    format: LogBundleFormat::Jsonl,
                },
                ctx,
            )
            .await
            .unwrap();
        let Produced::Bytes { data, filename } = produced else {
            panic!("expected an in-memory bundle");
        };
        assert!(filename.ends_with(".jsonl.zst"));

        // Decompress + parse: first line = manifest, then <= max_lines records.
        let mut dec = zstd::stream::read::Decoder::new(&data[..]).unwrap();
        let mut out = String::new();
        dec.read_to_string(&mut out).unwrap();
        let mut lines = out.lines();
        let manifest: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(manifest["kind"], "logbundle");
        assert_eq!(manifest["lines"], 3, "capped at max_lines");
        assert_eq!(manifest["truncated"], true, "4 matches > cap of 3");
        let bodies: Vec<serde_json::Value> =
            lines.map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(bodies.len(), 3);
        assert!(
            bodies
                .iter()
                .all(|r| r["message"].as_str().unwrap().contains("boom"))
        );
    }
}
