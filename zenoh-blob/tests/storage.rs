//! Serverless Tier-2 (#201): a producer publishes a snapshot into a Zenoh
//! *storage* and exits; a client reconstructs it from the storage with no
//! `TreeServer` ever running.
//!
//! Real deployments use `zenoh-plugin-storage-manager` on a router. Here a tiny
//! in-process `StandInStorage` stands in for it — it does exactly what a storage
//! does: subscribe to a key range to capture PUTs, and answer GETs on that range
//! from what it captured. That is sufficient to prove the `publish_*` API +
//! `TreeClient` work without a live server.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use common::{isolated_config, unique_prefix};
use zenoh_blob::{
    FixedSizeChunker, Format, MIN_CHUNK_SIZE, MemoryStore, TreeClient, build_tree, publish_snapshot,
};

/// A minimal stand-in for `zenoh-plugin-storage-manager`: retain PUTs on a key
/// range and reply to GETs on that range. Content-addressed keys are immutable,
/// so last-writer-wins storage is exact.
fn spawn_storage(session: Arc<zenoh::Session>, root: String) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let map: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::new(Mutex::new(HashMap::new()));
        let sub = session
            .declare_subscriber(format!("{root}/**"))
            .await
            .unwrap();
        let q = session
            .declare_queryable(format!("{root}/**"))
            .await
            .unwrap();
        loop {
            tokio::select! {
                Ok(sample) = sub.recv_async() => {
                    let key = sample.key_expr().as_str().to_string();
                    let bytes = sample.payload().to_bytes().to_vec();
                    map.lock().unwrap().insert(key, bytes);
                }
                Ok(query) = q.recv_async() => {
                    let key = query.key_expr().as_str().to_string();
                    let value = map.lock().unwrap().get(&key).cloned();
                    if let Some(bytes) = value {
                        let _ = query.reply(query.key_expr().clone(), bytes).await;
                    }
                }
                else => break,
            }
        }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publish_to_storage_then_download_without_server() {
    let session = Arc::new(zenoh::open(isolated_config()).await.unwrap());
    let root = unique_prefix();
    let store_prefix = format!("{root}/store");
    let tree_prefix = format!("{root}/tree");

    // Stand-in storage covers both the chunk and index key ranges.
    let storage = spawn_storage(session.clone(), root.clone());
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Producer builds a snapshot and publishes it into the storage.
    let src = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(src.path().join("sub")).unwrap();
    let big = common::pseudo_random(MIN_CHUNK_SIZE as usize * 2 + 99, 7);
    std::fs::write(src.path().join("big.bin"), &big).unwrap();
    std::fs::write(src.path().join("sub/a.txt"), b"alpha").unwrap();

    let chunker = FixedSizeChunker::new(MIN_CHUNK_SIZE);
    let (index, chunks) = build_tree(src.path(), "snap1", &chunker).unwrap();
    publish_snapshot(
        &session,
        &store_prefix,
        &tree_prefix,
        &index,
        chunks,
        Format::Json,
    )
    .await
    .expect("publish snapshot");

    // Let the storage capture every PUT, then the producer is "gone": no
    // TreeServer is ever spawned — only the storage answers from here on.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client_dir = tempfile::tempdir().unwrap();
    let client = TreeClient::new(session.clone(), store_prefix, tree_prefix, Format::Json);
    let client_store = MemoryStore::new();
    client
        .download_tree("snap1", client_dir.path(), &client_store)
        .await
        .expect("download from storage");

    // Reconstructed byte-for-byte from the storage alone.
    assert_eq!(
        std::fs::read(client_dir.path().join("big.bin")).unwrap(),
        big
    );
    assert_eq!(
        std::fs::read(client_dir.path().join("sub/a.txt")).unwrap(),
        b"alpha"
    );

    storage.abort();
    session.close().await.unwrap();
}
