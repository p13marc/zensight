//! The two built-in artifact producers: [`ReportProducer`] (Tier-1 debug bundle)
//! and [`SnapshotProducer`] (Tier-2 directory tree). Ported from the former
//! `ReportChannel` / `SnapshotChannel`; the transfer plumbing now lives in the
//! [`ArtifactChannel`](super::ArtifactChannel).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use zensight_common::artifact::{ArtifactKind, KindAdvert};
use zensight_common::{ArtifactReportLimits, ArtifactSnapshotLimits, CommonArtifactLimits};

use super::{ArtifactProducer, DeliveryKind, ProduceCtx, Produced};
use crate::report::{BundleInputs, DebugBundleSource, build_debug_bundle};

/// Tier-1 debug-bundle producer. Wraps a [`DebugBundleSource`] (config + health +
/// counters) and packages a redacted `tar.zst`.
pub struct ReportProducer {
    source: Arc<dyn DebugBundleSource>,
    common: CommonArtifactLimits,
    redact_extra: Vec<String>,
}

impl ReportProducer {
    /// Build from a bundle source and the `artifacts.report` limits.
    pub fn new(source: Arc<dyn DebugBundleSource>, limits: &ArtifactReportLimits) -> Self {
        ReportProducer {
            source,
            common: limits.common(),
            redact_extra: limits.redact_extra.clone(),
        }
    }
}

#[async_trait]
impl ArtifactProducer for ReportProducer {
    fn kind(&self) -> &'static str {
        "report"
    }
    fn common(&self) -> &CommonArtifactLimits {
        &self.common
    }
    fn delivery_kind(&self) -> DeliveryKind {
        DeliveryKind::Blob
    }
    fn advert(&self) -> KindAdvert {
        KindAdvert::Report {}
    }
    fn accepts(&self, kind: &ArtifactKind) -> Result<(), String> {
        match kind {
            ArtifactKind::Report {} => Ok(()),
            _ => Err("report producer given a non-report request".into()),
        }
    }
    async fn produce(&self, _kind: ArtifactKind, _ctx: ProduceCtx) -> anyhow::Result<Produced> {
        let inputs = BundleInputs {
            sensor_name: self.source.sensor_name(),
            source_id: self.source.source_id(),
            config: self.source.config_json(),
            health: serde_json::to_value(self.source.health()).unwrap_or(serde_json::Value::Null),
            counters: self.source.counters(),
            created_ms: chrono::Utc::now().timestamp_millis(),
        };
        let max_bytes = self.common.max_bytes;
        let redact_extra = self.redact_extra.clone();
        let (data, filename) = tokio::task::spawn_blocking(move || {
            build_debug_bundle(inputs, max_bytes, &redact_extra)
        })
        .await??;
        Ok(Produced::Bytes { data, filename })
    }
}

/// Tier-2 directory-snapshot producer. Resolves an operator-supplied allowlisted
/// name to a path; the channel walks + chunks it.
pub struct SnapshotProducer {
    limits: ArtifactSnapshotLimits,
    common: CommonArtifactLimits,
}

impl SnapshotProducer {
    /// Build from the `artifacts.snapshot` limits (incl. the directory allowlist).
    pub fn new(limits: &ArtifactSnapshotLimits) -> Self {
        SnapshotProducer {
            common: limits.common(),
            limits: limits.clone(),
        }
    }
}

#[async_trait]
impl ArtifactProducer for SnapshotProducer {
    fn kind(&self) -> &'static str {
        "snapshot"
    }
    fn common(&self) -> &CommonArtifactLimits {
        &self.common
    }
    fn delivery_kind(&self) -> DeliveryKind {
        DeliveryKind::Tree
    }
    fn advert(&self) -> KindAdvert {
        KindAdvert::Snapshot {
            dirs: self.limits.dir_names(),
        }
    }
    fn accepts(&self, kind: &ArtifactKind) -> Result<(), String> {
        match kind {
            ArtifactKind::Snapshot { dir } => self
                .limits
                .resolve(dir)
                .map(|_| ())
                .ok_or_else(|| format!("unknown directory: {dir}")),
            _ => Err("snapshot producer given a non-snapshot request".into()),
        }
    }
    fn tree_max_files(&self) -> Option<u64> {
        Some(self.limits.max_files)
    }
    fn store_state_dir(&self) -> Option<std::path::PathBuf> {
        self.limits.state_dir.as_ref().map(std::path::PathBuf::from)
    }
    async fn produce(&self, kind: ArtifactKind, _ctx: ProduceCtx) -> anyhow::Result<Produced> {
        let ArtifactKind::Snapshot { dir } = kind else {
            anyhow::bail!("snapshot producer given a non-snapshot request");
        };
        let path = self
            .limits
            .resolve(&dir)
            .ok_or_else(|| anyhow::anyhow!("unknown directory: {dir}"))?;
        // Per-dir opt-in (config `incremental`): the lineage tag keys the
        // channel's parent-index map and the durable tag file. The name is a
        // zblob tag name (validated on `tags.set`); dir names are operator
        // config, so an exotic one degrades to a warned, un-warm build.
        let lineage = self
            .limits
            .dirs
            .iter()
            .find(|d| d.name == dir)
            .filter(|d| d.incremental)
            .map(|d| format!("snapshot-{}", d.name));
        Ok(Produced::Dir {
            path: PathBuf::from(path),
            lineage,
        })
    }
}
