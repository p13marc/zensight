//! End-to-end round-trip of a multi-MB blob over an in-process Zenoh session.
//!
//! Mirrors the repo's single-session loopback test pattern (see
//! `zensight-sensor-core/tests/alert_reporter.rs`): one `Session` serves both
//! sides, scouting disabled so parallel test peers don't cross-talk, unique key
//! prefix per test.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::AsyncReadExt;
use zenoh_blob::{
    BlobClient, BlobServer, FileBlobSource, FixedSizeChunker, Format, MIN_CHUNK_SIZE, Manifest,
    Sha256Digest,
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

fn unique_prefix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("blobtest/{nanos}")
}

/// Deterministic pseudo-random bytes (no rand dependency).
fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut x = seed | 1;
    for _ in 0..len {
        // xorshift64
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push((x & 0xff) as u8);
    }
    out
}

async fn sha256(bytes: &[u8]) -> zenoh_blob::Hash {
    use zenoh_blob::Digest;
    let mut d = Sha256Digest::default();
    d.update(bytes);
    d.finalize()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn roundtrip_multi_mb() {
    let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
    let prefix = unique_prefix();
    let dir = tempfile::tempdir().unwrap();

    // ~4 MB source blob with a short final chunk (256 KiB chunks → ~16 + tail).
    let data = pseudo_random(MIN_CHUNK_SIZE as usize * 16 + 1234, 0xDEADBEEF);
    let src_path = dir.path().join("source.bin");
    tokio::fs::write(&src_path, &data).await.unwrap();

    // Build the manifest by streaming the source.
    let chunker = FixedSizeChunker::new(MIN_CHUNK_SIZE);
    let mut reader = tokio::fs::File::open(&src_path).await.unwrap();
    let manifest = Manifest::compute::<_, Sha256Digest>(
        &mut reader,
        &chunker,
        "blob-1",
        "source.bin",
        1_700_000_000_000,
    )
    .await
    .unwrap();
    assert_eq!(manifest.total_len, data.len() as u64);
    assert_eq!(manifest.chunk_count, 17);

    // Serve it.
    let server = BlobServer::new(session.clone(), prefix.clone(), Format::Json);
    server
        .register(manifest.clone(), Arc::new(FileBlobSource::new(&src_path)))
        .await;
    tokio::spawn(server.run());
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Download it.
    let client = BlobClient::new(session.clone(), prefix, Format::Json);
    let dest = dir.path().join("dest");
    let got_path = tokio::time::timeout(
        Duration::from_secs(20),
        client.download("blob-1", &dest, &()),
    )
    .await
    .expect("download timed out")
    .expect("download failed");

    // Bytes + hash match.
    let mut got = Vec::new();
    tokio::fs::File::open(&got_path)
        .await
        .unwrap()
        .read_to_end(&mut got)
        .await
        .unwrap();
    assert_eq!(got.len(), data.len());
    assert_eq!(sha256(&got).await, sha256(&data).await);
    assert_eq!(got_path.file_name().unwrap(), "source.bin");
}
