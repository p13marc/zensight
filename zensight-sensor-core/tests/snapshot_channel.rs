//! End-to-end test of the `@/snapshot` channel: request a named directory →
//! status `Ready` → download the tree over `zenoh-blob`'s `TreeClient` →
//! verify byte-for-byte. Plus: unknown dir name is rejected. Single-session
//! loopback (scouting off), mirroring `report_channel.rs`.

use std::sync::Arc;
use std::time::Duration;

use ulid::Ulid;
use zenoh_blob::{Format, MemoryStore, TreeClient};
use zensight_common::snapshot::{SnapshotRequest, SnapshotState, SnapshotStatus};
use zensight_common::{SnapshotDir, SnapshotLimits, snapshot_request_key, snapshot_status_key};
use zensight_sensor_core::SnapshotChannel;

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

fn make_tree(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("sub/deep")).unwrap();
    std::fs::write(root.join("a.txt"), b"alpha contents").unwrap();
    std::fs::write(root.join("sub/b.txt"), b"bravo contents, a bit longer").unwrap();
    std::fs::write(root.join("sub/deep/c.md"), b"# charlie\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("b.txt", root.join("sub/link")).unwrap();
}

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

async fn poll_status(session: &zenoh::Session, key: &str) -> Option<SnapshotStatus> {
    let replies = session.get(key).await.ok()?;
    let reply = replies.recv_async().await.ok()?;
    let sample = reply.result().ok()?;
    serde_json::from_slice(&sample.payload().to_bytes()).ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_snapshot_download_verify() {
    let session = Arc::new(zenoh::open(isolated_config()).await.unwrap());
    let prefix = "zensight/sysinfo";

    let src = tempfile::tempdir().unwrap();
    make_tree(src.path());

    let limits = SnapshotLimits {
        enabled: true,
        cooldown_secs: 0,
        dirs: vec![SnapshotDir {
            name: "snap".into(),
            path: src.path().to_string_lossy().to_string(),
        }],
        ..Default::default()
    };
    let channel = SnapshotChannel::new(session.clone(), prefix, "host1", limits);
    tokio::spawn(channel.run());
    tokio::time::sleep(Duration::from_millis(250)).await;

    // The status queryable advertises the allowlisted dir.
    let status_key = snapshot_status_key(prefix);
    let status = poll_status(&session, &status_key).await.unwrap();
    assert!(status.dirs.iter().any(|d| d.name == "snap"));

    // Request a snapshot of "snap".
    let id = Ulid::from_parts(7, 7);
    let req = SnapshotRequest {
        id,
        dir: "snap".into(),
        opts: Default::default(),
    };
    session
        .put(
            snapshot_request_key(prefix),
            serde_json::to_vec(&req).unwrap(),
        )
        .await
        .unwrap();

    // Poll until Ready.
    let ready = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(s) = poll_status(&session, &status_key).await
                && let Some(SnapshotState::Ready {
                    tree_id,
                    store_prefix,
                    tree_prefix,
                    summary,
                    ..
                }) = s.current
            {
                return (tree_id, store_prefix, tree_prefix, summary);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("snapshot never became Ready");

    let (tree_id, store_prefix, tree_prefix, summary) = ready;
    assert_eq!(tree_id, id.to_string());
    assert_eq!(store_prefix, format!("{prefix}/@/store"));
    assert_eq!(tree_prefix, format!("{prefix}/@/tree"));
    assert_eq!(summary.file_count, 3, "three regular files");
    assert!(summary.total_bytes > 0);

    // Download the tree into a fresh dir + store.
    let dest = tempfile::tempdir().unwrap();
    let client = TreeClient::new(session.clone(), store_prefix, tree_prefix, Format::Json);
    let client_store = MemoryStore::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        client.download_tree(&tree_id, dest.path(), &client_store),
    )
    .await
    .expect("download timed out")
    .expect("download failed");

    assert_dirs_equal(src.path(), dest.path());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_dir_is_rejected() {
    let session = Arc::new(zenoh::open(isolated_config()).await.unwrap());
    let prefix = "zensight/test";

    let limits = SnapshotLimits {
        enabled: true,
        cooldown_secs: 0,
        dirs: vec![SnapshotDir {
            name: "allowed".into(),
            path: "/tmp".into(),
        }],
        ..Default::default()
    };
    tokio::spawn(SnapshotChannel::new(session.clone(), prefix, "h", limits).run());
    tokio::time::sleep(Duration::from_millis(250)).await;

    let id = Ulid::from_parts(2, 2);
    let req = SnapshotRequest {
        id,
        dir: "/etc/shadow".into(), // not in the allowlist
        opts: Default::default(),
    };
    session
        .put(
            snapshot_request_key(prefix),
            serde_json::to_vec(&req).unwrap(),
        )
        .await
        .unwrap();

    let status_key = snapshot_status_key(prefix);
    let failed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(s) = poll_status(&session, &status_key).await
                && let Some(SnapshotState::Failed { reason, .. }) = s.current
            {
                return reason;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("unknown dir should be rejected");
    assert!(failed.contains("unknown directory"));
}
