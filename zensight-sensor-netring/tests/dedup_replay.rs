//! Duplicate-frame filtering on replay (netring 0.31.1 `MonitorBuilder::dedup`).
//!
//! A capture on `lo`, a SPAN port or a bridge can hand the sensor every frame
//! twice. Nothing downstream can tell: flow packet/byte counts double and the
//! second copy of a TCP segment reads as a retransmission. This replays the
//! committed `passive_dns.pcap` once as is and once with every record written
//! twice (built here, at test time — no second fixture to keep in sync), and
//! proves three things:
//!
//! - the doubled capture really does double the counts without a filter (so
//!   the last assertion is not vacuous);
//! - `dedup: "content"` brings them back to exactly the original;
//! - the filter drops only twins: the original capture replays unchanged
//!   through it.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use zensight_sensor_netring::config::NetringSensorConfig;
use zensight_sensor_netring::monitor;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/passive_dns.pcap"
);

/// Classic pcap: a 24-byte global header, then records of a 16-byte header
/// (`incl_len` at offset 8, in the file's byte order) and `incl_len` bytes.
fn write_doubled(src: &Path, dst: &Path) -> usize {
    let data = std::fs::read(src).expect("fixture");
    let le = match data[..4] {
        [0xD4, 0xC3, 0xB2, 0xA1] | [0x4D, 0x3C, 0xB2, 0xA1] => true,
        [0xA1, 0xB2, 0xC3, 0xD4] | [0xA1, 0xB2, 0x3C, 0x4D] => false,
        _ => panic!("fixture is not a classic pcap"),
    };
    let mut out = data[..24].to_vec();
    let (mut at, mut records) = (24, 0);
    while at < data.len() {
        let len_bytes: [u8; 4] = data[at + 8..at + 12].try_into().unwrap();
        let incl = if le {
            u32::from_le_bytes(len_bytes)
        } else {
            u32::from_be_bytes(len_bytes)
        } as usize;
        let record = &data[at..at + 16 + incl];
        out.extend_from_slice(record);
        out.extend_from_slice(record);
        at += 16 + incl;
        records += 1;
    }
    std::fs::write(dst, out).expect("write doubled pcap");
    records
}

#[derive(Debug, PartialEq, Eq)]
struct Counts {
    flows: u64,
    packets: u64,
    bytes: u64,
    retransmits: u64,
}

async fn replay(pcap: &Path, dedup: &str) -> Counts {
    let cfg: NetringSensorConfig = json5::from_str(&format!(
        r#"{{ netring: {{ source: "dedup-test", pcap: "{}", dedup: "{dedup}" }} }}"#,
        pcap.display()
    ))
    .expect("test config parses");
    let (mon, channels, keepalive, _handle, _tap_index) = monitor::build(
        &cfg.netring,
        zensight_sensor_netring::capture::CaptureTap::default(),
    )
    .expect("monitor builds");
    mon.replay().await.expect("pcap replay");
    drop(keepalive);
    Counts {
        flows: channels.flow_ended.load(Ordering::Relaxed),
        packets: channels.flow_packets.load(Ordering::Relaxed),
        bytes: channels.flow_bytes.load(Ordering::Relaxed),
        retransmits: channels.flow_retransmits.load(Ordering::Relaxed),
    }
}

#[tokio::test]
async fn content_dedup_undoes_a_doubled_capture() {
    let doubled: PathBuf = Path::new(env!("CARGO_TARGET_TMPDIR")).join("passive_dns_doubled.pcap");
    let records = write_doubled(Path::new(FIXTURE), &doubled);
    assert!(records > 0);

    let original = replay(Path::new(FIXTURE), "off").await;
    assert!(original.flows > 0 && original.packets > 0, "{original:?}");

    // Without a filter the twins are counted: the premise of the fix.
    let unfiltered = replay(&doubled, "off").await;
    assert_eq!(unfiltered.flows, original.flows, "{unfiltered:?}");
    assert_eq!(unfiltered.packets, 2 * original.packets, "{unfiltered:?}");
    assert_eq!(unfiltered.bytes, 2 * original.bytes, "{unfiltered:?}");
    assert!(
        unfiltered.retransmits > original.retransmits,
        "a duplicated segment reads as a retransmission: {unfiltered:?}"
    );

    // With it, the doubled capture is indistinguishable from the original.
    assert_eq!(replay(&doubled, "content").await, original);
    // And a capture without twins loses nothing to the filter.
    assert_eq!(replay(Path::new(FIXTURE), "content").await, original);

    let _ = std::fs::remove_file(&doubled);
}
