//! Kill + resume: an interrupted transfer continues from the first missing chunk
//! (`?from=K`) rather than restarting, and reassembles correctly.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{BytesSource, open_session, pseudo_random, sha256, unique_prefix};
use zenoh_blob::{
    BlobClient, BlobError, BlobServer, FixedSizeChunker, Format, MIN_CHUNK_SIZE, Manifest,
    Progress, ProgressSink, Sha256Digest,
};

/// A progress sink that records every `Chunk` event's `received` counter.
#[derive(Default, Clone)]
struct RecordingSink(Arc<Mutex<Vec<u32>>>);

impl ProgressSink for RecordingSink {
    fn emit(&self, p: Progress) {
        if let Progress::Chunk { received, .. } = p {
            self.0.lock().unwrap().push(received);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_then_resume() {
    let session = open_session().await;
    let prefix = unique_prefix();
    let dir = tempfile::tempdir().unwrap();

    // 8 full chunks; the source for round 1 only has the first 5.
    let chunk = MIN_CHUNK_SIZE as usize;
    let data = Arc::new(pseudo_random(chunk * 8, 0x1234));
    let truncated = Arc::new(data[..chunk * 5].to_vec());

    let mut reader = std::io::Cursor::new(data.as_slice());
    let chunker = FixedSizeChunker::new(MIN_CHUNK_SIZE);
    let manifest =
        Manifest::compute::<_, Sha256Digest>(&mut reader, &chunker, "blob-r", "data.bin", 1)
            .await
            .unwrap();
    assert_eq!(manifest.chunk_count, 8);

    let server = BlobServer::new(session.clone(), prefix.clone(), Format::Json);
    // Round 1: a truncated source → the server errors mid-stream after 5 chunks.
    server
        .register(manifest.clone(), Arc::new(BytesSource(truncated)))
        .await;
    tokio::spawn(server.clone().run());
    tokio::time::sleep(Duration::from_millis(150)).await;

    let client = BlobClient::new(session.clone(), prefix.clone(), Format::Json);
    let err = client
        .download("blob-r", dir.path(), &())
        .await
        .unwrap_err();
    match err {
        BlobError::Incomplete { received, total } => {
            assert_eq!(received, 5);
            assert_eq!(total, 8);
        }
        other => panic!("expected Incomplete, got {other:?}"),
    }

    // Round 2: re-register the full source; resume should pull only chunks 5..7.
    server
        .register(manifest.clone(), Arc::new(BytesSource(data.clone())))
        .await;

    let sink = RecordingSink::default();
    let path = tokio::time::timeout(
        Duration::from_secs(20),
        client.download("blob-r", dir.path(), &sink),
    )
    .await
    .expect("resume timed out")
    .expect("resume failed");

    // Final bytes verify.
    let got = tokio::fs::read(&path).await.unwrap();
    assert_eq!(sha256(&got), sha256(&data));

    // Resume pulled exactly the 3 missing chunks, and the first one bumped the
    // received counter to 6 (proving 5 were already on disk → `?from=5`).
    let events = sink.0.lock().unwrap().clone();
    assert_eq!(events.len(), 3, "should re-fetch only the 3 missing chunks");
    assert_eq!(
        events[0], 6,
        "resume should start from chunk 5 (received→6)"
    );
    assert_eq!(events.last(), Some(&8));
}
