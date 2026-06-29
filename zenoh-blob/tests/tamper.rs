//! Integrity: a corrupted blob is rejected (whole-blob hash) and a wrong-length
//! chunk is rejected (per-chunk length) — neither produces a saved file.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{BytesSource, open_session, pseudo_random, unique_prefix};
use zenoh_blob::{
    BlobClient, BlobError, BlobServer, Chunker, FixedSizeChunker, Format, MIN_CHUNK_SIZE, Manifest,
    Sha256Digest, chunk_key, encode, manifest_key,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrupted_bytes_fail_hash() {
    let session = open_session().await;
    let prefix = unique_prefix();
    let dir = tempfile::tempdir().unwrap();

    // Manifest hashes the *clean* data; the served source is corrupted (one byte
    // flipped) but the right length, so it passes per-chunk checks and fails only
    // at the final whole-blob hash.
    let clean = pseudo_random(MIN_CHUNK_SIZE as usize * 3, 0xABCD);
    let mut corrupted = clean.clone();
    corrupted[MIN_CHUNK_SIZE as usize + 7] ^= 0xff;

    let mut reader = std::io::Cursor::new(clean.as_slice());
    let chunker = FixedSizeChunker::new(MIN_CHUNK_SIZE);
    let manifest =
        Manifest::compute::<_, Sha256Digest>(&mut reader, &chunker, "blob-t", "data.bin", 1)
            .await
            .unwrap();

    let server = BlobServer::new(session.clone(), prefix.clone(), Format::Json);
    server
        .register(manifest, Arc::new(BytesSource(Arc::new(corrupted))))
        .await;
    tokio::spawn(server.run());
    tokio::time::sleep(Duration::from_millis(150)).await;

    let client = BlobClient::new(session.clone(), prefix.clone(), Format::Json);
    let err = client
        .download("blob-t", dir.path(), &())
        .await
        .unwrap_err();
    assert!(matches!(err, BlobError::HashMismatch), "got {err:?}");

    // No partial and no final file left behind.
    assert!(!dir.path().join("blob-t.part").exists());
    assert!(!dir.path().join("data.bin").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_length_chunk_rejected() {
    let session = open_session().await;
    let prefix = unique_prefix();
    let dir = tempfile::tempdir().unwrap();

    let data = Arc::new(pseudo_random(MIN_CHUNK_SIZE as usize * 4, 0x55AA));
    let mut reader = std::io::Cursor::new(data.as_slice());
    let chunker = FixedSizeChunker::new(MIN_CHUNK_SIZE);
    let manifest =
        Manifest::compute::<_, Sha256Digest>(&mut reader, &chunker, "blob-c", "data.bin", 1)
            .await
            .unwrap();

    // A hand-rolled, malicious server that truncates chunk index 2's payload.
    let bad_index = 2u32;
    let manifest_c = manifest.clone();
    let data_c = data.clone();
    let prefix_c = prefix.clone();
    let session_c = session.clone();
    tokio::spawn(async move {
        let q = session_c
            .declare_queryable(format!("{prefix_c}/**"))
            .await
            .unwrap();
        let chunker = FixedSizeChunker::new(manifest_c.chunk_size);
        while let Ok(query) = q.recv_async().await {
            let key = query.key_expr().as_str().to_string();
            let id = &manifest_c.id;
            let _ = query
                .reply(
                    manifest_key(&prefix_c, id),
                    encode(&manifest_c, Format::Json).unwrap(),
                )
                .await;
            if key.ends_with("/manifest") {
                continue;
            }
            for index in 0..manifest_c.chunk_count {
                let off = chunker.offset(index) as usize;
                let len = chunker.chunk_len(index, manifest_c.total_len) as usize;
                let mut payload = data_c[off..off + len].to_vec();
                if index == bad_index {
                    payload.truncate(1); // wrong length on the wire
                }
                let _ = query.reply(chunk_key(&prefix_c, id, index), payload).await;
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(150)).await;

    let client = BlobClient::new(session.clone(), prefix.clone(), Format::Json);
    let err = client
        .download("blob-c", dir.path(), &())
        .await
        .unwrap_err();
    assert!(
        matches!(err, BlobError::ChunkLen { index } if index == bad_index),
        "got {err:?}"
    );
}
