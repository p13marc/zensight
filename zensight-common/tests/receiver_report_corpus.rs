//! `MediaReceiverReport` CBOR conformance vectors (#714).
//!
//! # This corpus is a *forward* pin, not a twin binding
//!
//! `tests/framemeta_corpus.rs` exists because `parallax::wire::FrameMeta` is a
//! byte-compatible twin in another repo that neither crate can import: those
//! vectors are copied verbatim from upstream and pinned on both sides, and the
//! corpus is the only thing keeping two encoders honest.
//!
//! **This type has no twin.** It is ours, and today it has exactly one
//! producer. Copying the twin-binding ceremony would produce fixtures nobody
//! could explain in six months.
//!
//! What the FrameMeta corpus is *actually* protecting, and what does transfer,
//! is the **omission discipline** — and this type needs it more, because RFC 07
//! §1.3 makes it normative: *where a deployment does not timestamp, frame age
//! is not asked, **never zero***. A `frame_age_ms` that serializes as `0.0`
//! instead of vanishing tells a controller the stream is perfectly fresh at the
//! exact moment it has no idea.
//!
//! So these vectors are a forward pin, for the implementations that do not
//! exist yet: #718's Rust publisher in the iced tiles and #722's TypeScript one
//! in the browser. Both will be checked against these bytes. Hand-rolled
//! encoders get `Option` omission wrong first; that is what this catches.
//!
//! Re-encoding byte-for-byte also pins field *order*, since serde emits struct
//! fields in declaration order — so reordering the struct is a wire break and
//! shows up here.
//!
//! Regenerate with `cargo test -p zensight-common --test receiver_report_corpus
//! -- --ignored regenerate_the_corpus` after a *deliberate* shape change, and
//! say why in the commit.

use zensight_common::serialization::{Format, decode, encode};
use zensight_common::stream::MediaReceiverReport;

fn corpus_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/receiver_report")
}

fn read(name: &str) -> Vec<u8> {
    std::fs::read(corpus_dir().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// Everything present, a healthy stream with a decode queue.
fn full() -> MediaReceiverReport {
    MediaReceiverReport {
        stream: "cam0".into(),
        codec: Some("h264".into()),
        tier: Some("high".into()),
        consumer_id: "tile-7f3a".into(),
        interval_ms: 2000,
        received_frames: 3600,
        lost_frames: 12,
        dropped_frames: 4,
        decoded_frames: 3584,
        last_sequence: 3616,
        interarrival_jitter_ms: Some(3.5),
        frame_age_ms: Some(42.0),
        frame_age_max_ms: Some(118.25),
        decoder_queue_depth: Some(2),
        last_keyframe_sequence: Some(3600),
        since_last_keyframe_ms: Some(533),
    }
}

/// The shape a consumer emits when the producer does not timestamp and the
/// consumer has no decode queue: four `Option`s absent, and **absent is not
/// zero** (RFC 07 §1.3). The preview key, so `codec`/`tier` are present too.
fn unstamped() -> MediaReceiverReport {
    MediaReceiverReport {
        stream: "cam0".into(),
        codec: Some("jpeg".into()),
        tier: Some("preview".into()),
        consumer_id: "tile-0001".into(),
        interval_ms: 1000,
        received_frames: 30,
        lost_frames: 0,
        dropped_frames: 0,
        decoded_frames: 30,
        last_sequence: 30,
        interarrival_jitter_ms: None,
        frame_age_ms: None,
        frame_age_max_ms: None,
        decoder_queue_depth: None,
        last_keyframe_sequence: None,
        since_last_keyframe_ms: None,
    }
}

/// Negative frame age: the consumer's clock is behind the publisher's. RFC 07
/// §1.3 says report it, do not clamp it — a negative age *is* the skew
/// evidence, and clamping it to zero destroys the only signal that says so.
/// Also the default-tier shape: `codec`/`tier` absent.
fn skewed() -> MediaReceiverReport {
    MediaReceiverReport {
        stream: "cam0".into(),
        codec: None,
        tier: None,
        consumer_id: "tile-beef".into(),
        interval_ms: 1000,
        received_frames: 60,
        lost_frames: 0,
        dropped_frames: 0,
        decoded_frames: 60,
        last_sequence: 60,
        interarrival_jitter_ms: Some(1.0),
        frame_age_ms: Some(-12.5),
        frame_age_max_ms: Some(-3.0),
        decoder_queue_depth: Some(0),
        last_keyframe_sequence: Some(60),
        since_last_keyframe_ms: Some(16),
    }
}

fn vectors() -> Vec<(&'static str, MediaReceiverReport)> {
    vec![
        ("full.cbor", full()),
        ("unstamped.cbor", unstamped()),
        ("skewed.cbor", skewed()),
    ]
}

/// Every vector re-encodes to exactly the bytes checked in.
#[test]
fn corpus_vectors_round_trip_byte_for_byte() {
    for (name, value) in vectors() {
        let bytes = read(name);
        let decoded: MediaReceiverReport =
            decode(&bytes, Format::Cbor).unwrap_or_else(|e| panic!("{name} failed to decode: {e}"));
        assert_eq!(decoded, value, "{name} decoded to something else");
        let reencoded = encode(&decoded, Format::Cbor).expect("re-encode");
        assert_eq!(
            reencoded, bytes,
            "{name} did not re-encode byte for byte — the struct's field order \
             or its skip_serializing_if attributes changed, and both are wire shape"
        );
    }
}

/// **The vector that matters.** An absent `Option` is a *missing map entry*,
/// not a null and certainly not a zero.
///
/// Asserted at the byte level, on the CBOR map header, because a serialized
/// `null` round-trips through `Option<f32>` invisibly while changing every
/// report on the wire — and because the browser encoder (#722) will be a
/// hand-rolled one, which is exactly where this goes wrong first.
#[test]
fn absent_optionals_are_missing_map_entries_not_nulls() {
    let bytes = read("unstamped.cbor");
    // 16 fields, 6 absent (codec and tier are present here) → 10 entries.
    // CBOR major type 5, count 10 → 0xaa. A 16-entry map (0xb0) would mean the
    // four timing/queue fields serialized as nulls.
    assert_eq!(
        bytes[0], 0xaa,
        "expected a 10-entry CBOR map, got header {:#04x} — an absent Option is \
         serializing as null instead of being omitted, which tells a controller \
         'measured as zero' where the truth is 'not measured' (RFC 07 §1.3)",
        bytes[0]
    );
    for absent in [
        &b"frame_age_ms"[..],
        b"frame_age_max_ms",
        b"decoder_queue_depth",
        b"interarrival_jitter_ms",
        b"last_keyframe_sequence",
        b"since_last_keyframe_ms",
    ] {
        assert!(
            !bytes.windows(absent.len()).any(|w| w == absent),
            "an omitted field must not appear as a key at all: {}",
            String::from_utf8_lossy(absent)
        );
    }

    // And the full vector proves the same fields DO appear when measured, so
    // the assertion above is about omission rather than about a typo.
    let full_bytes = read("full.cbor");
    assert_eq!(full_bytes[0], 0xb0, "16 fields, all present");
}

/// A negative frame age survives both encodings unclamped (RFC 07 §1.3).
#[test]
fn a_negative_frame_age_survives_both_encodings() {
    let bytes = read("skewed.cbor");
    let cbor: MediaReceiverReport = decode(&bytes, Format::Cbor).expect("cbor");
    assert_eq!(cbor.frame_age_ms, Some(-12.5));
    assert_eq!(cbor.frame_age_max_ms, Some(-3.0));

    // The report rides @rpc, where decode_auto sniffs the first byte, so JSON
    // is equally on the wire and a browser may send it.
    let json = encode(&cbor, Format::Json).expect("json");
    let back: MediaReceiverReport =
        zensight_common::decode_auto(&json).expect("json decodes through the sniffer");
    assert_eq!(back, cbor, "JSON and CBOR must agree, sign included");
    assert!(
        !String::from_utf8_lossy(&json).contains("\"frame_age_ms\":0"),
        "a negative age must not be clamped on the JSON path either"
    );

    // decoder_queue_depth is Some(0) here, not None: a consumer that HAS a
    // queue and finds it empty says so, and that is a different statement from
    // one that has no queue at all.
    assert_eq!(cbor.decoder_queue_depth, Some(0));
}

/// Rewrite the checked-in vectors from the constructors above.
///
/// `#[ignore]`d: it is a generator, not a test, and running it by accident
/// would make every other test in this file tautological.
#[test]
#[ignore = "generator; run explicitly after a deliberate wire-shape change"]
fn regenerate_the_corpus() {
    std::fs::create_dir_all(corpus_dir()).expect("corpus dir");
    for (name, value) in vectors() {
        let bytes = encode(&value, Format::Cbor).expect("encode");
        std::fs::write(corpus_dir().join(name), &bytes).expect("write");
        eprintln!("wrote {name} ({} bytes)", bytes.len());
    }
}
