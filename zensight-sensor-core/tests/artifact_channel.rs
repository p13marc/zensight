//! End-to-end tests of the unified `@/artifact` channel: a Tier-1 report blob and
//! a Tier-2 snapshot tree served by one channel (per-kind status), plus the
//! cancel-an-in-flight-production path (the design's risk #2). Single-session
//! loopback (scouting off), mirroring `snapshot_channel.rs`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ulid::Ulid;
use zenoh_blob::{BlobClient, CancelToken, Format, MemoryStore, TreeClient};
use zensight_common::artifact::{
    ArtifactKind, ArtifactRequest, ArtifactState, ArtifactStatus, Delivery, KindAdvert,
};
use zensight_common::{
    ArtifactReportLimits, ArtifactSnapshotLimits, SnapshotDir, artifact_request_key,
    artifact_status_key,
};
use zensight_sensor_core::artifact::{ProduceCtx, Produced};
use zensight_sensor_core::{
    ArtifactChannel, ArtifactProducer, DeliveryKind, ReportProducer, SensorHealth,
    SimpleBundleSource, SnapshotProducer,
};

fn isolated_config() -> zenoh::Config {
    let mut config = zenoh::Config::default();
    config
        .insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    config
        .insert_json5("scouting/gossip/enabled", "false")
        .unwrap();
    config
}

async fn poll_status(session: &zenoh::Session, key: &str) -> Option<ArtifactStatus> {
    let replies = session.get(key).await.ok()?;
    let reply = replies.recv_async().await.ok()?;
    let sample = reply.result().ok()?;
    serde_json::from_slice(&sample.payload().to_bytes()).ok()
}

fn kind_current(status: &ArtifactStatus, kind: &str) -> Option<ArtifactState> {
    status
        .kinds
        .iter()
        .find(|k| k.kind == kind)
        .and_then(|k| k.current.clone())
}

#[derive(serde::Serialize)]
struct DummyConfig {
    community: String,
    key_prefix: String,
}

fn report_producer(prefix: &str) -> Arc<dyn ArtifactProducer> {
    let health = Arc::new(SensorHealth::new("test"));
    let source = Arc::new(SimpleBundleSource::new(
        "test",
        "host1",
        DummyConfig {
            community: "public".to_string(),
            key_prefix: prefix.to_string(),
        },
        health,
    ));
    let limits = ArtifactReportLimits {
        enabled: true,
        cooldown_secs: 0,
        ..Default::default()
    };
    Arc::new(ReportProducer::new(source, &limits))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn report_and_snapshot_on_one_channel() {
    let session = Arc::new(zenoh::open(isolated_config()).await.unwrap());
    let prefix = "zensight/arttest1";

    // A snapshot source dir.
    let src = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("a.txt"), b"alpha").unwrap();
    std::fs::write(src.path().join("b.txt"), b"bravo bravo").unwrap();

    let snap_limits = ArtifactSnapshotLimits {
        enabled: true,
        cooldown_secs: 0,
        dirs: vec![SnapshotDir {
            name: "snap".into(),
            path: src.path().to_string_lossy().to_string(),
        }],
        ..Default::default()
    };

    let channel = ArtifactChannel::new(
        session.clone(),
        prefix,
        "host1",
        vec![
            report_producer(prefix),
            Arc::new(SnapshotProducer::new(&snap_limits)),
        ],
    )
    .expect("at least one producer enabled");
    tokio::spawn(channel.run());
    tokio::time::sleep(Duration::from_millis(250)).await;

    // Status advertises both kinds; snapshot advertises its dir.
    let status_key = artifact_status_key(prefix);
    let status = poll_status(&session, &status_key).await.unwrap();
    assert_eq!(status.kinds.len(), 2);
    let snap = status.kinds.iter().find(|k| k.kind == "snapshot").unwrap();
    assert!(
        matches!(&snap.advert, KindAdvert::Snapshot { dirs } if dirs.iter().any(|d| d == "snap"))
    );
    assert!(status.kinds.iter().any(|k| k.kind == "report"));

    // --- Tier-1: request a report, download the blob. ---
    let report_id = Ulid::from_parts(1, 1);
    session
        .put(
            artifact_request_key(prefix),
            serde_json::to_vec(&ArtifactRequest {
                id: report_id,
                kind: ArtifactKind::Report {},
                opts: Default::default(),
            })
            .unwrap(),
        )
        .await
        .unwrap();

    let (manifest, blob_prefix) = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(s) = poll_status(&session, &status_key).await
                && let Some(ArtifactState::Ready {
                    delivery:
                        Delivery::Blob {
                            manifest,
                            blob_prefix,
                        },
                    ..
                }) = kind_current(&s, "report")
            {
                return (manifest, blob_prefix);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("report never became Ready");
    assert_eq!(manifest.id, report_id.to_string());

    let dest = tempfile::tempdir().unwrap();
    let client = BlobClient::new(session.clone(), blob_prefix, Format::Json);
    let cancel = CancelToken::new();
    let path = tokio::time::timeout(
        Duration::from_secs(10),
        client.download_cancellable(&report_id.to_string(), dest.path(), &(), &cancel),
    )
    .await
    .expect("blob download timed out")
    .expect("blob download failed");
    assert!(path.exists());
    // It's a valid tar.zst with the redacted config.
    let f = std::fs::File::open(&path).unwrap();
    let dec = zstd::Decoder::new(f).unwrap();
    let mut ar = tar::Archive::new(dec);
    let mut names = Vec::new();
    for e in ar.entries().unwrap() {
        names.push(e.unwrap().path().unwrap().to_string_lossy().to_string());
    }
    assert!(names.iter().any(|n| n == "config.json"));

    // --- Tier-2: request a snapshot, download the tree. ---
    let snap_id = Ulid::from_parts(2, 2);
    session
        .put(
            artifact_request_key(prefix),
            serde_json::to_vec(&ArtifactRequest {
                id: snap_id,
                kind: ArtifactKind::Snapshot { dir: "snap".into() },
                opts: Default::default(),
            })
            .unwrap(),
        )
        .await
        .unwrap();

    let (tree_id, store_prefix, tree_prefix) =
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(s) = poll_status(&session, &status_key).await
                    && let Some(ArtifactState::Ready {
                        delivery:
                            Delivery::Tree {
                                tree_id,
                                store_prefix,
                                tree_prefix,
                                summary,
                            },
                        ..
                    }) = kind_current(&s, "snapshot")
                {
                    assert_eq!(summary.file_count, 2);
                    return (tree_id, store_prefix, tree_prefix);
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("snapshot never became Ready");

    let tdest = tempfile::tempdir().unwrap();
    let tclient = TreeClient::new(session.clone(), store_prefix, tree_prefix, Format::Json);
    let tstore = MemoryStore::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        tclient.download_tree(&tree_id, tdest.path(), &tstore),
    )
    .await
    .expect("tree download timed out")
    .expect("tree download failed");
    assert_eq!(std::fs::read(tdest.path().join("a.txt")).unwrap(), b"alpha");
}

/// A producer that blocks until cancelled, to exercise cancel-mid-production.
struct SlowProducer {
    common: zensight_common::CommonArtifactLimits,
    started: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl ArtifactProducer for SlowProducer {
    fn kind(&self) -> &'static str {
        "report"
    }
    fn common(&self) -> &zensight_common::CommonArtifactLimits {
        &self.common
    }
    fn delivery_kind(&self) -> DeliveryKind {
        DeliveryKind::Blob
    }
    fn advert(&self) -> KindAdvert {
        KindAdvert::Report {}
    }
    fn accepts(&self, _kind: &ArtifactKind) -> Result<(), String> {
        Ok(())
    }
    async fn produce(&self, _kind: ArtifactKind, ctx: ProduceCtx) -> anyhow::Result<Produced> {
        self.started.notify_one();
        // Wait for cancellation, then error out (as a real capture would).
        for _ in 0..200 {
            if ctx.cancel.is_cancelled() {
                anyhow::bail!("cancelled");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        anyhow::bail!("timed out waiting for cancel");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_aborts_in_flight_production() {
    let session = Arc::new(zenoh::open(isolated_config()).await.unwrap());
    let prefix = "zensight/arttest2";

    let started = Arc::new(tokio::sync::Notify::new());
    let producer = Arc::new(SlowProducer {
        common: zensight_common::CommonArtifactLimits {
            enabled: true,
            max_bytes: 1 << 20,
            cooldown_secs: 0,
            ttl_secs: 600,
            chunk_size: 256 * 1024,
        },
        started: started.clone(),
    });
    let channel = ArtifactChannel::new(session.clone(), prefix, "host1", vec![producer]).unwrap();
    tokio::spawn(channel.run());
    tokio::time::sleep(Duration::from_millis(250)).await;

    let id = Ulid::from_parts(5, 5);
    session
        .put(
            artifact_request_key(prefix),
            serde_json::to_vec(&ArtifactRequest {
                id,
                kind: ArtifactKind::Report {},
                opts: Default::default(),
            })
            .unwrap(),
        )
        .await
        .unwrap();

    // Wait until the producer is actually running, then cancel it.
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .expect("producer never started");
    session
        .put(zensight_common::artifact_cancel_key(prefix), id.to_string())
        .await
        .unwrap();

    let status_key = artifact_status_key(prefix);
    let reason = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(s) = poll_status(&session, &status_key).await
                && let Some(ArtifactState::Failed { reason, .. }) = kind_current(&s, "report")
            {
                return reason;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("cancelled production should report Failed");
    assert!(reason.contains("cancel"), "reason was: {reason}");
}
