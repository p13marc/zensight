//! End-to-end test of the `@/report` channel: request → status `Ready` →
//! download the bundle over `zenoh-blob` → verify it is a valid, redacted
//! `tar.zst`. Single-session loopback (scouting off), mirroring the repo pattern.

use std::sync::Arc;
use std::time::Duration;

use ulid::Ulid;
use zenoh_blob::BlobClient;
use zensight_common::report::{ReportKind, ReportRequest, ReportState, ReportStatus};
use zensight_common::{report_request_key, report_status_key};
use zensight_sensor_core::{ReportChannel, ReportLimits, SensorHealth, SimpleBundleSource};

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

async fn poll_status(session: &zenoh::Session, key: &str) -> Option<ReportStatus> {
    let replies = session.get(key).await.ok()?;
    let reply = replies.recv_async().await.ok()?;
    let sample = reply.result().ok()?;
    serde_json::from_slice(&sample.payload().to_bytes()).ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_generate_download_verify() {
    let session = Arc::new(zenoh::open(isolated_config()).await.unwrap());
    let prefix = "zensight/netlink";
    let dir = tempfile::tempdir().unwrap();

    let health = Arc::new(SensorHealth::new("netlink"));
    let config = serde_json::json!({
        "community": "public",                 // secret → must be redacted
        "key_prefix": "zensight/netlink",      // benign → preserved
    });
    let source = Arc::new(SimpleBundleSource::new("netlink", "host1", config, health));
    let limits = ReportLimits {
        enabled: true,
        cooldown_secs: 0,
        ..Default::default()
    };
    let channel = ReportChannel::new(session.clone(), prefix, limits, source);
    tokio::spawn(channel.run());
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Request a debug bundle.
    let id = Ulid::from_parts(42, 7);
    let req = ReportRequest {
        id,
        kind: ReportKind::DebugBundle,
        opts: Default::default(),
    };
    session
        .put(
            report_request_key(prefix),
            serde_json::to_vec(&req).unwrap(),
        )
        .await
        .unwrap();

    // Poll status until Ready (or timeout).
    let status_key = report_status_key(prefix);
    let ready = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(s) = poll_status(&session, &status_key).await
                && let Some(ReportState::Ready {
                    manifest,
                    blob_prefix,
                    ..
                }) = s.current
            {
                return (manifest, blob_prefix);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("report never became Ready");

    let (manifest, blob_prefix) = ready;
    assert_eq!(manifest.id, id.to_string());
    assert!(
        manifest
            .filename
            .starts_with("zensight-debug-netlink-host1-")
    );

    // Download the bundle over zenoh-blob.
    let client = BlobClient::new(session.clone(), blob_prefix, zenoh_blob::Format::Json);
    let path = tokio::time::timeout(
        Duration::from_secs(10),
        client.download(&id.to_string(), dir.path(), &()),
    )
    .await
    .expect("download timed out")
    .expect("download failed");

    // It's a valid tar.zst whose config.json is redacted.
    let f = std::fs::File::open(&path).unwrap();
    let dec = zstd::Decoder::new(f).unwrap();
    let mut ar = tar::Archive::new(dec);
    let mut config_json = None;
    for entry in ar.entries().unwrap() {
        let mut entry = entry.unwrap();
        let name = entry.path().unwrap().to_string_lossy().to_string();
        if name == "config.json" {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut entry, &mut buf).unwrap();
            config_json = Some(buf);
        }
    }
    let config_json = config_json.expect("config.json present in bundle");
    assert!(config_json.contains("***REDACTED***"));
    assert!(!config_json.contains("public"), "secret must not leak");
    assert!(config_json.contains("zensight/netlink"), "benign key kept");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsupported_kind_is_rejected() {
    let session = Arc::new(zenoh::open(isolated_config()).await.unwrap());
    let prefix = "zensight/test";

    let health = Arc::new(SensorHealth::new("test"));
    let source = Arc::new(SimpleBundleSource::new(
        "test",
        "h",
        serde_json::json!({}),
        health,
    ));
    let limits = ReportLimits {
        enabled: true,
        ..Default::default()
    };
    tokio::spawn(ReportChannel::new(session.clone(), prefix, limits, source).run());
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Send a request with an unknown kind (decodes to Unsupported).
    let id = Ulid::from_parts(1, 1);
    let raw = serde_json::json!({ "id": id.to_string(), "kind": "pcap_future" });
    session
        .put(
            report_request_key(prefix),
            serde_json::to_vec(&raw).unwrap(),
        )
        .await
        .unwrap();

    let status_key = report_status_key(prefix);
    let failed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(s) = poll_status(&session, &status_key).await
                && let Some(ReportState::Failed { .. }) = s.current
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    assert!(failed.is_ok(), "unsupported kind should be rejected");
}
