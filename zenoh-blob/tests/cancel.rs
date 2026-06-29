//! Pause/cancel: a cancelled download persists its partial and resumes; a
//! deleted partial starts over.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{BytesSource, open_session, pseudo_random, sha256, unique_prefix};
use zenoh_blob::{
    BlobClient, BlobError, BlobServer, CancelToken, FixedSizeChunker, Format, MIN_CHUNK_SIZE,
    Manifest, Progress, ProgressSink, Sha256Digest,
};

/// Cancels the token after the first chunk is written.
struct CancelAfterFirst {
    token: CancelToken,
}
impl ProgressSink for CancelAfterFirst {
    fn emit(&self, p: Progress) {
        if let Progress::Chunk { received, .. } = p
            && received >= 1
        {
            self.token.cancel();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_persists_then_resumes() {
    let session = open_session().await;
    let prefix = unique_prefix();
    let dir = tempfile::tempdir().unwrap();

    let data = Arc::new(pseudo_random(MIN_CHUNK_SIZE as usize * 8, 0xC0FFEE));
    let mut reader = std::io::Cursor::new(data.as_slice());
    let chunker = FixedSizeChunker::new(MIN_CHUNK_SIZE);
    let manifest =
        Manifest::compute::<_, Sha256Digest>(&mut reader, &chunker, "blob-x", "d.bin", 1)
            .await
            .unwrap();

    let server = BlobServer::new(session.clone(), prefix.clone(), Format::Json);
    server
        .register(manifest, Arc::new(BytesSource(data.clone())))
        .await;
    tokio::spawn(server.run());
    tokio::time::sleep(Duration::from_millis(150)).await;

    let client = BlobClient::new(session.clone(), prefix.clone(), Format::Json);

    // Cancel mid-transfer → Cancelled, partial persisted.
    let token = CancelToken::new();
    let sink = CancelAfterFirst {
        token: token.clone(),
    };
    let err = client
        .download_cancellable("blob-x", dir.path(), &sink, &token)
        .await
        .unwrap_err();
    assert!(matches!(err, BlobError::Cancelled { .. }), "got {err:?}");
    assert!(dir.path().join("blob-x.part").exists());

    // Resume (fresh token) → completes + verifies.
    let path = tokio::time::timeout(
        Duration::from_secs(20),
        client.download("blob-x", dir.path(), &()),
    )
    .await
    .expect("resume timed out")
    .expect("resume failed");
    let got = tokio::fs::read(&path).await.unwrap();
    assert_eq!(sha256(&got), sha256(&data));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_partial_clears_state() {
    let session = open_session().await;
    let prefix = unique_prefix();
    let dir = tempfile::tempdir().unwrap();

    let data = Arc::new(pseudo_random(MIN_CHUNK_SIZE as usize * 4, 0xBEEF));
    let mut reader = std::io::Cursor::new(data.as_slice());
    let chunker = FixedSizeChunker::new(MIN_CHUNK_SIZE);
    let manifest =
        Manifest::compute::<_, Sha256Digest>(&mut reader, &chunker, "blob-y", "d.bin", 1)
            .await
            .unwrap();

    let server = BlobServer::new(session.clone(), prefix.clone(), Format::Json);
    server
        .register(manifest, Arc::new(BytesSource(data.clone())))
        .await;
    tokio::spawn(server.run());
    tokio::time::sleep(Duration::from_millis(150)).await;

    let client = BlobClient::new(session.clone(), prefix.clone(), Format::Json);
    let token = CancelToken::new();
    let sink = CancelAfterFirst {
        token: token.clone(),
    };
    let _ = client
        .download_cancellable("blob-y", dir.path(), &sink, &token)
        .await;
    assert!(dir.path().join("blob-y.part").exists());

    client.delete_partial("blob-y", dir.path()).await;
    assert!(!dir.path().join("blob-y.part").exists());
    assert!(!dir.path().join("blob-y.part.meta").exists());
}
