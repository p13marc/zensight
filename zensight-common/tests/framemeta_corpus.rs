//! `FrameMeta` conformance against parallax's canonical CBOR corpus (#728).
//!
//! # Two types, one corpus (#711)
//!
//! `zensight_common::stream::FrameMeta` and `parallax::wire::FrameMeta` are
//! deliberately **two types**, and this is the artifact that binds them. Neither
//! crate can import the other's: `zensight-common` is linked by every sensor,
//! including the ones that never touch video, and making it depend on
//! `parallax-pipeline` would drag the whole video engine (openh264, V4L2,
//! retina) into all of them; the reverse direction would drag Zenoh into
//! parallax. Upstream says the same thing from its side — see the module doc on
//! `parallax`'s `src/wire/frame_meta.rs`, which names `zensight-common` as its
//! byte-compatible twin.
//!
//! So instead of a shared type there is a shared **corpus**: the three vectors
//! in `tests/fixtures/framemeta/`, copied verbatim from
//! `parallax-pipeline/tests/fixtures/framemeta/`, which upstream pins with a
//! test of its own (`frame_meta_matches_the_checked_in_corpus`). If either
//! encoder drifts, these bytes stop round-tripping on one side and the drift is
//! caught at `cargo test` rather than by a viewer that cannot decode a frame.
//!
//! # What the vectors encode
//!
//! Two rules are normative wire shape, not style, and each vector exercises one:
//!
//! - **Absent ≠ null.** The `#[serde(default, skip_serializing_if =
//!   "Option::is_none")]` on the three timing fields means an unstamped
//!   timestamp is *missing from the CBOR map*, not present-and-null. A consumer
//!   reading the map sees the difference. `minimal-no-timing.cbor` is a 4-entry
//!   map; `pts-only.cbor` is a 5-entry one.
//! - **`dts_ns` is omitted when it equals `pts_ns`** — the field is documented
//!   "if distinct from `pts_ns`", and without B-frames every single frame would
//!   otherwise carry a redundant copy of its own pts. `full.cbor` is the
//!   7-entry map where dts genuinely differs.
//!
//! Re-encoding byte-for-byte also pins field *order* (serde emits struct fields
//! in declaration order), so reordering the struct is a wire break and shows up
//! here.

use zensight_common::serialization::{Format, decode, encode};
use zensight_common::stream::FrameMeta;

fn corpus_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/framemeta")
}

fn vectors() -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<_> = std::fs::read_dir(corpus_dir())
        .expect("corpus directory")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("cbor"))
        .map(|p| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            (name, std::fs::read(&p).expect("corpus vector"))
        })
        .collect();
    out.sort();
    out
}

/// Every vector decodes, and re-encodes to exactly the bytes it came from.
#[test]
fn corpus_vectors_round_trip_byte_for_byte() {
    let vectors = vectors();
    assert!(
        vectors.len() >= 3,
        "expected the three checked-in vectors, found {}",
        vectors.len()
    );

    for (name, bytes) in vectors {
        let decoded: FrameMeta = decode(&bytes, Format::Cbor)
            .unwrap_or_else(|e| panic!("{name} failed to decode as FrameMeta: {e}"));
        let reencoded = encode(&decoded, Format::Cbor).expect("re-encode");
        assert_eq!(
            reencoded, bytes,
            "{name} did not re-encode byte for byte\n  decoded: {decoded:?}\n  \
             expected: {bytes:02x?}\n  got:      {reencoded:02x?}"
        );
    }
}

/// The vectors say what they are named after — decoded field by field, so a
/// failure names the field rather than a hex blob.
#[test]
fn corpus_vectors_decode_to_the_documented_values() {
    let read = |name: &str| -> FrameMeta {
        let bytes = std::fs::read(corpus_dir().join(name)).expect(name);
        decode(&bytes, Format::Cbor).expect(name)
    };

    // Everything present, dts genuinely distinct from pts (B-frames).
    let full = read("full.cbor");
    assert!(!full.keyframe);
    assert_eq!(full.pts_ns, Some(1_234_567_890));
    assert_eq!(full.dts_ns, Some(1_234_000_000));
    assert_ne!(full.dts_ns, full.pts_ns, "the point of this vector");
    assert_eq!(full.duration_ns, Some(16_683_333));
    assert_eq!(full.sequence, 4_294_967_296);
    assert_eq!((full.width, full.height), (3840, 2160));

    // A keyframe with a pts and nothing else: dts absent because it equals pts.
    let pts_only = read("pts-only.cbor");
    assert!(pts_only.keyframe);
    assert_eq!(pts_only.pts_ns, Some(1_000_000));
    assert_eq!(pts_only.dts_ns, None, "omitted because it equals pts_ns");
    assert_eq!(pts_only.duration_ns, None);
    assert_eq!(pts_only.sequence, 0);
    assert_eq!((pts_only.width, pts_only.height), (1920, 1080));

    // No clock at all — the shape a source that never stamps timestamps emits.
    let minimal = read("minimal-no-timing.cbor");
    assert!(minimal.keyframe);
    assert_eq!(minimal.pts_ns, None);
    assert_eq!(minimal.dts_ns, None);
    assert_eq!(minimal.duration_ns, None);
    assert_eq!(minimal.sequence, 1);
    assert_eq!((minimal.width, minimal.height), (16, 16));
}

/// Absent is not null: a `None` timing field must be *missing from the map*,
/// which in CBOR is visible as the map's entry count.
///
/// Byte-level rather than via `serde_json` (which `stream.rs`'s own unit test
/// already covers) because CBOR is what actually rides the `@media` plane, and
/// a serialized `null` would round-trip through `Option<u64>` invisibly while
/// changing every published attachment.
#[test]
fn absent_timing_is_a_missing_map_entry_not_a_null() {
    let bare = FrameMeta {
        keyframe: true,
        pts_ns: None,
        dts_ns: None,
        duration_ns: None,
        sequence: 1,
        width: 16,
        height: 16,
    };
    let bytes = encode(&bare, Format::Cbor).expect("encode");
    // CBOR major type 5 (map), 4 entries → 0xa4. Not 0xa7 with three nulls.
    assert_eq!(
        bytes[0], 0xa4,
        "expected a 4-entry CBOR map, got header {:#04x} — a `None` timing \
         field is serializing as null instead of being omitted",
        bytes[0]
    );
    assert!(
        !bytes.windows(6).any(|w| w == b"pts_ns"),
        "an omitted field must not appear as a key at all"
    );
}
