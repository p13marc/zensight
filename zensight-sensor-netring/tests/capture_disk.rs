//! Capture-to-disk monitor wiring over pcap replay (#327): the continuous
//! packet subscription feeds real replayed frames into the engine channel, and
//! its presence doesn't break the on-demand tap's index verification. The
//! trigger/finalize path itself is unit-tested in `disk.rs` with synthetic
//! frames (the replay source closes the feed at EOF, so an after-replay trigger
//! would race engine shutdown here).

use std::sync::atomic::Ordering;

use zensight_sensor_netring::capture::CaptureTap;
use zensight_sensor_netring::config::{CaptureDiskMode, NetringSensorConfig};
use zensight_sensor_netring::monitor;

fn fixture_cfg(dir: &std::path::Path, snaplen: u32) -> NetringSensorConfig {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/passive_dns.pcap"
    );
    json5::from_str(&format!(
        r#"{{
            netring: {{
                source: "disk-test",
                pcap: "{fixture}",
                capture: {{
                    on_demand: {{ enabled: true }},
                    to_disk: {{
                        mode: "triggered",
                        dir: "{}",
                        snaplen: {snaplen},
                    }},
                }},
            }},
        }}"#,
        dir.display()
    ))
    .expect("test config parses")
}

#[tokio::test]
async fn replay_feeds_disk_channel_and_keeps_tap_index() {
    let dir = std::env::temp_dir().join(format!("ztest-disk-wire-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let cfg = fixture_cfg(&dir, 96);
    let (mon, mut channels, keepalive, _handle, tap_index) =
        monitor::build(&cfg.netring, CaptureTap::default()).expect("monitor builds");

    // The extra continuous disk subscription must not disturb the on-demand
    // tap's reload index (focus off ⇒ tap at 0), and must arm the disk feed.
    assert_eq!(tap_index, Some(0), "no focus sub ⇒ tap at index 0");
    let (mut disk_rx, stats) = channels.disk.take().expect("disk feed armed");
    assert_eq!(stats.mode(), CaptureDiskMode::Triggered);

    // Collect frames off the feed while the replay runs.
    let collector = tokio::spawn(async move {
        let mut frames = 0u64;
        let mut snapped_ok = true;
        while let Some(f) = disk_rx.recv().await {
            frames += 1;
            // Snaplen was applied at copy time; original_len keeps wire length.
            snapped_ok &= f.data.len() <= 96 && f.original_len >= f.data.len();
        }
        (frames, snapped_ok)
    });

    mon.replay().await.expect("pcap replay");
    drop(keepalive); // replay done — the monitor's senders are gone

    let (frames, snapped_ok) = collector.await.unwrap();
    assert!(frames > 0, "replayed IP frames should reach the disk feed");
    assert!(snapped_ok, "frames are snap-truncated at copy time");
    assert_eq!(stats.dropped.load(Ordering::Relaxed), 0);

    let _ = std::fs::remove_dir_all(&dir);
}
