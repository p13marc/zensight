//! End-to-end tests of the unified artifact channel (the
//! `@rpc/<producer>/artifact/*` procedures + `@blob` delivery): a Tier-1 report
//! blob and a Tier-2 snapshot tree served by one channel (per-kind status), plus
//! the cancel-an-in-flight-production path (the design's risk #2).
//! Single-session loopback (scouting off), mirroring `snapshot_channel.rs`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ulid::Ulid;
use zblob::{BlobClient, CancelToken, DownloadRequest, MemoryStore, TreeClient};
use zensight_common::artifact::{
    ArtifactKind, ArtifactRequest, ArtifactState, ArtifactStatus, Delivery, KindAdvert,
};
use zensight_common::{
    ArtifactReportLimits, ArtifactSnapshotLimits, SnapshotDir, artifact_cancel_key,
    artifact_request_key, artifact_status_key,
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
    let prefix = "arttest1";

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
            incremental: false,
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
    // v1 (RFC 05): requests are write procedures — GET with a body; the
    // value reply is the ack, errors ride reply_err.
    let replies = session
        .get(artifact_request_key(prefix))
        .payload(
            serde_json::to_vec(&ArtifactRequest {
                id: report_id,
                kind: ArtifactKind::Report {},
                opts: Default::default(),
            })
            .unwrap(),
        )
        .await
        .unwrap();
    let reply = replies.recv_async().await.expect("request reply");
    assert!(reply.result().is_ok(), "request refused: {reply:?}");

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
    assert_eq!(manifest.id.as_str(), report_id.to_string());

    let dest = tempfile::tempdir().unwrap();
    let path = dest.path().join("report.tar.zst");
    let client = BlobClient::new(&session, zblob::QueryPrefix::new(blob_prefix).unwrap());
    let cancel = CancelToken::new();
    // Pinned to the manifest's root, which is the shape every consumer uses
    // (RFC 07 §2.1): the transfer fails rather than writing bytes whose
    // identity does not match what was advertised.
    let req = DownloadRequest::pinned(report_id.to_string(), manifest.root);
    tokio::time::timeout(
        Duration::from_secs(10),
        client.download_to(&req, &path).cancel(&cancel),
    )
    .await
    .expect("blob download timed out")
    .expect("blob download failed");
    assert!(path.exists());

    // The pin discriminates: a request for the same id under a *wrong* root
    // must fail, or "pinned" would be decoration. Without this the happy path
    // above passes whether or not the anchor is checked at all.
    let wrong = DownloadRequest::pinned(report_id.to_string(), zblob::Hash::of(b"not the report"));
    let decoy = dest.path().join("decoy.tar.zst");
    assert!(
        tokio::time::timeout(
            Duration::from_secs(10),
            client.download_to(&wrong, &decoy).cancel(&cancel),
        )
        .await
        .expect("mispinned download timed out")
        .is_err(),
        "a mispinned request must be refused"
    );
    assert!(!decoy.exists(), "nothing may be written for a refused pin");
    // It's a valid tar.zst with the redacted config.
    let f = std::fs::File::open(&path).unwrap();
    let dec = zstd::Decoder::new(f).unwrap();
    let mut ar = tar::Archive::new(dec);
    let mut names = Vec::new();
    for e in ar.entries().unwrap() {
        names.push(e.unwrap().path().unwrap().to_string_lossy().to_string());
    }
    assert!(names.iter().any(|n| n == "config.json"));

    // Cancelling a Ready report releases it: the in-memory blob is
    // unregistered (Cleanup::Blob is unregister-only — there is no temp file
    // to remove since the report is served from memory), so a fresh pinned
    // download must now fail rather than resolve.
    let replies = session
        .get(format!("{}?id={report_id}", artifact_cancel_key(prefix)))
        .await
        .unwrap();
    let reply = replies.recv_async().await.expect("cancel reply");
    assert!(reply.result().is_ok(), "cancel refused: {reply:?}");
    let after = dest.path().join("after-cancel.tar.zst");
    assert!(
        tokio::time::timeout(
            Duration::from_secs(10),
            client.download_to(&req, &after).cancel(&cancel),
        )
        .await
        .expect("post-cancel download timed out")
        .is_err(),
        "a released report must no longer be served"
    );

    // --- Tier-2: request a snapshot, download the tree. ---
    let snap_id = Ulid::from_parts(2, 2);
    let replies = session
        .get(artifact_request_key(prefix))
        .payload(
            serde_json::to_vec(&ArtifactRequest {
                id: snap_id,
                kind: ArtifactKind::Snapshot { dir: "snap".into() },
                opts: Default::default(),
            })
            .unwrap(),
        )
        .await
        .unwrap();
    let reply = replies.recv_async().await.expect("snapshot request reply");
    assert!(reply.result().is_ok(), "snapshot refused: {reply:?}");

    let (root, store_prefix, tree_prefix) = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(s) = poll_status(&session, &status_key).await
                && let Some(ArtifactState::Ready {
                    delivery:
                        Delivery::Tree {
                            root,
                            store_prefix,
                            tree_prefix,
                            summary,
                        },
                    ..
                }) = kind_current(&s, "snapshot")
            {
                assert_eq!(summary.file_count, 2);
                return (root, store_prefix, tree_prefix);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("snapshot never became Ready");

    // RFC 07 §2.3: the delivery names the tree by its own root, not by the
    // caller-minted ULID that identifies the *artifact*. The two must not be
    // confused — that confusion is what made a Tier-2 key mutable.
    assert_ne!(
        root.to_string(),
        snap_id.to_string(),
        "the tree key must be the content root, not the artifact id"
    );

    let tdest = tempfile::tempdir().unwrap();
    let tclient = TreeClient::new(
        &session,
        zblob::QueryPrefix::new(store_prefix).unwrap(),
        zblob::QueryPrefix::new(tree_prefix).unwrap(),
    );
    let tstore: Arc<dyn zblob::ContentStore> = Arc::new(MemoryStore::new());
    let cancel = CancelToken::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        tclient
            .download_tree(&DownloadRequest::by_root(root), tdest.path(), &tstore)
            .cancel(&cancel),
    )
    .await
    .expect("tree download timed out")
    .expect("tree download failed");
    assert_eq!(std::fs::read(tdest.path().join("a.txt")).unwrap(), b"alpha");

    // `by_root` is pinned by construction, so a root nobody serves resolves to
    // nothing rather than to whatever index happens to answer.
    let bogus = tempfile::tempdir().unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_secs(10),
            tclient
                .download_tree(
                    &DownloadRequest::by_root(zblob::Hash::of(b"no such tree")),
                    bogus.path(),
                    &tstore,
                )
                .cancel(&cancel),
        )
        .await
        .expect("bogus-root download timed out")
        .is_err(),
        "an unserved root must not resolve to some other snapshot"
    );
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
    let prefix = "arttest2";

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
    let replies = session
        .get(artifact_request_key(prefix))
        .payload(
            serde_json::to_vec(&ArtifactRequest {
                id,
                kind: ArtifactKind::Report {},
                opts: Default::default(),
            })
            .unwrap(),
        )
        .await
        .unwrap();
    let reply = replies.recv_async().await.expect("request reply");
    assert!(reply.result().is_ok(), "request refused: {reply:?}");

    // Wait until the producer is actually running, then cancel it.
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .expect("producer never started");
    let cancel_replies = session
        .get(format!(
            "{}?id={}",
            zensight_common::artifact_cancel_key(prefix),
            id
        ))
        .await
        .unwrap();
    let cancel_reply = cancel_replies.recv_async().await.expect("cancel reply");
    assert!(
        cancel_reply.result().is_ok(),
        "cancel refused: {cancel_reply:?}"
    );

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

/// Count regular files under a directory, recursively (the DirStore layout).
fn file_count(root: &std::path::Path) -> usize {
    let mut n = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                n += 1;
            }
        }
    }
    n
}

async fn request_snapshot(session: &zenoh::Session, prefix: &str, id: Ulid) {
    let replies = session
        .get(artifact_request_key(prefix))
        .payload(
            serde_json::to_vec(&ArtifactRequest {
                id,
                kind: ArtifactKind::Snapshot { dir: "snap".into() },
                opts: Default::default(),
            })
            .unwrap(),
        )
        .await
        .unwrap();
    let reply = replies.recv_async().await.expect("request reply");
    assert!(reply.result().is_ok(), "request refused: {reply:?}");
}

async fn wait_tree_root(session: &zenoh::Session, status_key: &str, id: Ulid) -> zblob::Hash {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(s) = poll_status(session, status_key).await
                && let Some(ArtifactState::Ready {
                    id: got,
                    delivery: Delivery::Tree { root, .. },
                    ..
                }) = kind_current(&s, "snapshot")
                && got == id
            {
                return root;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("snapshot never became Ready")
}

/// Durable-store lineage: with `state_dir` + `incremental`, a released
/// snapshot's chunks stay warm (the lineage tag marks them live through the
/// sweep), and a later build of a grown directory adds chunks on top instead
/// of starting from an empty store. This is the observable half of
/// `build_tree_from` parent reuse — the store is inspected on disk, where the
/// DirStore actually lives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_snapshot_lineage_survives_release() {
    let session = Arc::new(zenoh::open(isolated_config()).await.unwrap());
    let prefix = "arttest3";

    let src = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("a.bin"), vec![7u8; 200_000]).unwrap();
    std::fs::write(src.path().join("b.bin"), vec![9u8; 150_000]).unwrap();

    let state = tempfile::tempdir().unwrap();
    let snap_limits = ArtifactSnapshotLimits {
        enabled: true,
        cooldown_secs: 0,
        state_dir: Some(state.path().to_string_lossy().to_string()),
        dirs: vec![SnapshotDir {
            name: "snap".into(),
            path: src.path().to_string_lossy().to_string(),
            incremental: true,
        }],
        ..Default::default()
    };
    let channel = ArtifactChannel::new(
        session.clone(),
        prefix,
        "host1",
        vec![Arc::new(SnapshotProducer::new(&snap_limits))],
    )
    .expect("snapshot producer enabled");
    tokio::spawn(channel.run());
    tokio::time::sleep(Duration::from_millis(250)).await;

    let status_key = artifact_status_key(prefix);
    let chunks_dir = state.path().join("chunks");

    // Build #1.
    let id1 = Ulid::from_parts(10, 10);
    request_snapshot(&session, prefix, id1).await;
    let root1 = wait_tree_root(&session, &status_key, id1).await;
    let after_first = file_count(&chunks_dir);
    assert!(after_first > 0, "durable store must hold chunks on disk");

    // Cancel (release + sweep): the lineage tag keeps the chunks warm.
    let replies = session
        .get(format!("{}?id={id1}", artifact_cancel_key(prefix)))
        .await
        .unwrap();
    assert!(replies.recv_async().await.unwrap().result().is_ok());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        file_count(&chunks_dir),
        after_first,
        "a released lineage snapshot's chunks must survive the sweep"
    );

    // Grow the directory; build #2 dedups against the warm chunks.
    std::fs::write(src.path().join("c.bin"), vec![3u8; 120_000]).unwrap();
    let id2 = Ulid::from_parts(20, 20);
    request_snapshot(&session, prefix, id2).await;
    let root2 = wait_tree_root(&session, &status_key, id2).await;
    assert_ne!(root1, root2, "a grown tree must have a new root");
    assert!(
        file_count(&chunks_dir) > after_first,
        "the grown tree adds chunks on top of the warm store"
    );
}

/// One-shot (non-incremental) snapshots are fully reclaimed on release: no
/// tag marks them, so cancel → unregister → sweep empties the store.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_shot_snapshot_is_swept_on_release() {
    let session = Arc::new(zenoh::open(isolated_config()).await.unwrap());
    let prefix = "arttest4";

    let src = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("a.bin"), vec![5u8; 200_000]).unwrap();

    let state = tempfile::tempdir().unwrap();
    let snap_limits = ArtifactSnapshotLimits {
        enabled: true,
        cooldown_secs: 0,
        state_dir: Some(state.path().to_string_lossy().to_string()),
        dirs: vec![SnapshotDir {
            name: "snap".into(),
            path: src.path().to_string_lossy().to_string(),
            incremental: false,
        }],
        ..Default::default()
    };
    let channel = ArtifactChannel::new(
        session.clone(),
        prefix,
        "host1",
        vec![Arc::new(SnapshotProducer::new(&snap_limits))],
    )
    .expect("snapshot producer enabled");
    tokio::spawn(channel.run());
    tokio::time::sleep(Duration::from_millis(250)).await;

    let status_key = artifact_status_key(prefix);
    let id = Ulid::from_parts(30, 30);
    request_snapshot(&session, prefix, id).await;
    let _root = wait_tree_root(&session, &status_key, id).await;
    assert!(file_count(&state.path().join("chunks")) > 0);

    let replies = session
        .get(format!("{}?id={id}", artifact_cancel_key(prefix)))
        .await
        .unwrap();
    assert!(replies.recv_async().await.unwrap().result().is_ok());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        file_count(&state.path().join("chunks")),
        0,
        "an untagged snapshot's chunks must be swept once released"
    );
}
