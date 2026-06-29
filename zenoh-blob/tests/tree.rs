//! Tier-2 directory transfer over a loopback Zenoh session: build a tree, serve
//! its chunks + index, download into an empty store, and verify byte-for-byte.
//! Then prove the casync properties — re-pull after an edit transfers only the
//! changed chunks, and an interrupted pull resumes from the on-disk store.

mod common;

use std::sync::Arc;

use common::{isolated_config, unique_prefix};
use zenoh_blob::{
    ContentStore, DirStore, Entry, FastCdcChunker, FixedSizeChunker, Format, MIN_CHUNK_SIZE,
    MemoryStore, TreeClient, TreeServer, build_tree,
};

/// Populate a temp directory tree: a nested dir, two files (one large enough to
/// span several chunks), and — on unix — a symlink.
fn make_tree(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("sub/deep")).unwrap();
    // ~3.5 chunks so we exercise the short tail + multi-chunk file.
    let big = common::pseudo_random(MIN_CHUNK_SIZE as usize * 3 + 1234, 42);
    std::fs::write(root.join("big.bin"), &big).unwrap();
    std::fs::write(root.join("sub/hello.txt"), b"hello world").unwrap();
    std::fs::write(root.join("sub/deep/note.md"), b"# note\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("hello.txt", root.join("sub/link")).unwrap();
}

/// Recursively compare two directory trees for byte-identical content + structure.
fn assert_dirs_equal(a: &std::path::Path, b: &std::path::Path) {
    let mut ea: Vec<_> = std::fs::read_dir(a)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    let mut eb: Vec<_> = std::fs::read_dir(b)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    ea.sort();
    eb.sort();
    assert_eq!(ea, eb, "entry names differ in {a:?} vs {b:?}");
    for name in ea {
        let pa = a.join(&name);
        let pb = b.join(&name);
        let ma = std::fs::symlink_metadata(&pa).unwrap();
        if ma.file_type().is_symlink() {
            assert_eq!(
                std::fs::read_link(&pa).unwrap(),
                std::fs::read_link(&pb).unwrap()
            );
        } else if ma.is_dir() {
            assert_dirs_equal(&pa, &pb);
        } else {
            assert_eq!(
                std::fs::read(&pa).unwrap(),
                std::fs::read(&pb).unwrap(),
                "{name:?}"
            );
        }
    }
}

/// Spawn a `TreeServer` serving `index` + `store` under the given prefixes.
async fn serve(
    session: Arc<zenoh::Session>,
    store_prefix: String,
    tree_prefix: String,
    store: Arc<dyn ContentStore>,
    index: zenoh_blob::TreeIndex,
) -> tokio::task::JoinHandle<()> {
    let server = TreeServer::new(session, store_prefix, tree_prefix, Format::Json, store);
    server.register(index).await;
    let handle = tokio::spawn(async move {
        let _ = server.run().await;
    });
    // Let both queryables settle before the client GETs (mirrors the Tier-1 test).
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    handle
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tree_roundtrip() {
    let session = Arc::new(zenoh::open(isolated_config()).await.unwrap());
    let p = unique_prefix();
    let store_prefix = format!("{p}/store");
    let tree_prefix = format!("{p}/tree");

    let src = tempfile::tempdir().unwrap();
    make_tree(src.path());

    // Build the snapshot + populate the server store.
    let chunker = FixedSizeChunker::new(MIN_CHUNK_SIZE);
    let (index, chunks) = build_tree(src.path(), "snap1", &chunker).unwrap();
    let server_store = Arc::new(MemoryStore::new());
    for (h, bytes) in &chunks {
        server_store.put(h, bytes).unwrap();
    }
    let n_chunks = server_store.len();

    let handle = serve(
        session.clone(),
        store_prefix.clone(),
        tree_prefix.clone(),
        server_store,
        index,
    )
    .await;

    // Download into an empty client store + fresh dest dir.
    let client_dir = tempfile::tempdir().unwrap();
    let client = TreeClient::new(session.clone(), store_prefix, tree_prefix, Format::Json);
    let client_store = MemoryStore::new();
    client
        .download_tree("snap1", client_dir.path(), &client_store)
        .await
        .expect("download tree");

    assert_dirs_equal(src.path(), client_dir.path());
    // Every needed chunk was fetched exactly into the client store.
    assert_eq!(client_store.len(), n_chunks);

    handle.abort();
    session.close().await.unwrap();
}

/// The whole tree pipeline (build → serve → download → reconstruct → verify root)
/// works with a content-defined chunker too: chunks are variable-length, so this
/// exercises the index's per-chunk `len` on the reconstruction path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tree_roundtrip_fastcdc() {
    let session = Arc::new(zenoh::open(isolated_config()).await.unwrap());
    let p = unique_prefix();
    let store_prefix = format!("{p}/store");
    let tree_prefix = format!("{p}/tree");

    let src = tempfile::tempdir().unwrap();
    make_tree(src.path());

    let chunker = FastCdcChunker::new(8192);
    let (index, chunks) = build_tree(src.path(), "snap1", &chunker).unwrap();
    assert!(index.chunk_policy.starts_with("fastcdc-"));
    // The big file spans several variable-length chunks.
    let big_chunks = index
        .entries
        .iter()
        .find_map(|e| match e {
            Entry::File { path, chunks, .. } if path == "big.bin" => Some(chunks.clone()),
            _ => None,
        })
        .unwrap();
    assert!(big_chunks.len() > 1);
    assert!(
        big_chunks
            .iter()
            .map(|c| c.len)
            .collect::<std::collections::HashSet<_>>()
            .len()
            > 1,
        "FastCDC should produce variable-length chunks"
    );

    let server_store = Arc::new(MemoryStore::new());
    for (h, bytes) in &chunks {
        server_store.put(h, bytes).unwrap();
    }
    let handle = serve(
        session.clone(),
        store_prefix.clone(),
        tree_prefix.clone(),
        server_store,
        index,
    )
    .await;

    let client_dir = tempfile::tempdir().unwrap();
    let client = TreeClient::new(session.clone(), store_prefix, tree_prefix, Format::Json);
    let client_store = MemoryStore::new();
    client
        .download_tree("snap1", client_dir.path(), &client_store)
        .await
        .expect("download fastcdc tree");

    assert_dirs_equal(src.path(), client_dir.path());

    handle.abort();
    session.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reedit_transfers_only_changed_chunks() {
    let session = Arc::new(zenoh::open(isolated_config()).await.unwrap());
    let p = unique_prefix();
    let store_prefix = format!("{p}/store");
    let tree_prefix = format!("{p}/tree");
    let chunker = FixedSizeChunker::new(MIN_CHUNK_SIZE);

    let src = tempfile::tempdir().unwrap();
    make_tree(src.path());

    // Snapshot 1 → client store now holds every chunk.
    let (index1, chunks1) = build_tree(src.path(), "snap1", &chunker).unwrap();
    let server_store = Arc::new(MemoryStore::new());
    for (h, bytes) in &chunks1 {
        server_store.put(h, bytes).unwrap();
    }
    let server = TreeServer::new(
        session.clone(),
        store_prefix.clone(),
        tree_prefix.clone(),
        Format::Json,
        server_store.clone(),
    );
    server.register(index1).await;
    let srv = {
        let server = server.clone();
        tokio::spawn(async move {
            let _ = server.run().await;
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let client_dir = tempfile::tempdir().unwrap();
    // Persistent client store survives "across syncs" (DirStore on disk).
    let store_dir = tempfile::tempdir().unwrap();
    let client_store = DirStore::open(store_dir.path()).unwrap();
    let client = TreeClient::new(
        session.clone(),
        store_prefix.clone(),
        tree_prefix.clone(),
        Format::Json,
    );
    client
        .download_tree("snap1", client_dir.path(), &client_store)
        .await
        .unwrap();
    let after_first = std::fs::read_dir(store_dir.path()).unwrap().count();

    // Edit one small file → only its chunk changes. Append to the big file's tail
    // would change one chunk too; editing the tiny file changes exactly one chunk.
    std::fs::write(src.path().join("sub/hello.txt"), b"hello CHANGED world").unwrap();
    let (index2, chunks2) = build_tree(src.path(), "snap2", &chunker).unwrap();
    for (h, bytes) in &chunks2 {
        server_store.put(h, bytes).unwrap();
    }
    server.register(index2.clone()).await;

    client
        .download_tree("snap2", client_dir.path(), &client_store)
        .await
        .unwrap();
    let after_second = std::fs::read_dir(store_dir.path()).unwrap().count();

    // The re-pull added exactly the one new (changed) chunk to the store.
    assert_eq!(
        after_second - after_first,
        1,
        "re-pull should transfer only the single changed chunk"
    );
    // And the edited content is on disk.
    assert_eq!(
        std::fs::read(client_dir.path().join("sub/hello.txt")).unwrap(),
        b"hello CHANGED world"
    );
    // The unchanged big file still verifies (its chunks were reused from the store).
    let big_entry_chunks = index2
        .entries
        .iter()
        .find_map(|e| match e {
            Entry::File { path, chunks, .. } if path == "big.bin" => Some(chunks.len()),
            _ => None,
        })
        .unwrap();
    assert!(big_entry_chunks >= 3);

    srv.abort();
    session.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_from_prepopulated_store() {
    let session = Arc::new(zenoh::open(isolated_config()).await.unwrap());
    let p = unique_prefix();
    let store_prefix = format!("{p}/store");
    let tree_prefix = format!("{p}/tree");
    let chunker = FixedSizeChunker::new(MIN_CHUNK_SIZE);

    let src = tempfile::tempdir().unwrap();
    make_tree(src.path());
    let (index, chunks) = build_tree(src.path(), "snap1", &chunker).unwrap();
    let server_store = Arc::new(MemoryStore::new());
    for (h, bytes) in &chunks {
        server_store.put(h, bytes).unwrap();
    }
    let total = server_store.len();

    let handle = serve(
        session.clone(),
        store_prefix.clone(),
        tree_prefix.clone(),
        server_store,
        index.clone(),
    )
    .await;

    // Simulate an interrupted earlier pull: half the chunks already on disk.
    let store_dir = tempfile::tempdir().unwrap();
    let client_store = DirStore::open(store_dir.path()).unwrap();
    let needed = index.needed_chunks();
    let half = needed.len() / 2;
    for h in needed.iter().take(half) {
        let bytes = chunks.iter().find(|(ch, _)| ch == h).unwrap().1.clone();
        client_store.put(h, &bytes).unwrap();
    }
    let on_disk = || std::fs::read_dir(store_dir.path()).unwrap().count();
    assert!(on_disk() < total);

    // Resume: download_tree fetches only the missing remainder.
    let client_dir = tempfile::tempdir().unwrap();
    let client = TreeClient::new(session.clone(), store_prefix, tree_prefix, Format::Json);
    client
        .download_tree("snap1", client_dir.path(), &client_store)
        .await
        .unwrap();

    assert_dirs_equal(src.path(), client_dir.path());
    assert_eq!(on_disk(), total);

    handle.abort();
    session.close().await.unwrap();
}
