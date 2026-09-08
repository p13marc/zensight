//! `.zrec` fixture corpus tests (#747).
//!
//! The corpus in `tests/fixtures/zrec/` was recorded from a real, isolated
//! deployment by `scripts/record-fixtures.sh` (sysinfo + logs + systemd + the
//! correlator on a loopback rendezvous). A hand-built sample encodes what we
//! *think* a sensor publishes; these bytes are what one *did* — the gap
//! between the two is where every wire-shape bug we have found lived.
//!
//! ## The assertion policy (the regeneration contract)
//!
//! Re-running `scripts/record-fixtures.sh` and then this suite **must pass
//! unchanged** — that is what makes regeneration safe. So tests here assert
//! only capture-stable facts: which key families decode, which message
//! variants appear, counts derived from the file itself. Never a hostname, an
//! origin hash, a metric value, or a timestamp — those change per capture,
//! and an assertion on one turns the corpus into a snapshot nobody can
//! refresh.

use zensight::replay;

/// Pre-main WGPU guard (#687, #829) — every zensight test binary carries its
/// own; see `docs/testing.md`, "If the `zensight` tests segfault". This
/// target never builds a simulator today, but the rule is per-binary so a
/// later simulator test cannot re-open the coin flip.
#[ctor::ctor(unsafe)]
fn force_gl_backend_for_tests() {
    if std::env::var_os("WGPU_BACKEND").is_none() {
        // SAFETY: `ctor` runs before `main`, single-threaded — the condition
        // edition-2024 `set_var` asks for.
        unsafe { std::env::set_var("WGPU_BACKEND", "gl") };
    }
}

fn fixture_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/zrec")
}

/// Every capture in the corpus, sorted by name so failures are stable.
fn captures() -> Vec<(String, replay::Replay)> {
    let mut out: Vec<(String, replay::Replay)> = std::fs::read_dir(fixture_dir())
        .expect("tests/fixtures/zrec exists")
        .filter_map(|e| {
            let path = e.expect("readable dir entry").path();
            (path.extension().is_some_and(|x| x == "zrec")).then(|| {
                let name = path.file_stem().unwrap().to_string_lossy().into_owned();
                let r =
                    replay::load(&path).unwrap_or_else(|e| panic!("{} loads: {e}", path.display()));
                (name, r)
            })
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    // An emptied directory must fail loudly, not vacuously pass.
    assert!(out.len() >= 3, "corpus floor: expected >= 3 captures");
    out
}

fn capture(name: &str) -> replay::Replay {
    replay::load(fixture_dir().join(format!("{name}.zrec"))).expect("fixture loads")
}

/// Every file parses, states its version, and says what it watched — the
/// header is the capture's coverage statement (RFC 09 §5.1 O5).
///
/// The version is checked against `ZREC_READS`, not `ZREC_VERSION`: the
/// corpus is deliberately version 1 and stays there. A capture is a
/// historical fact, so a reader that stopped reading the old dialect is a
/// regression this corpus should catch — pinning it to whatever the *writer*
/// currently emits would instead force a regeneration on every dialect bump
/// and quietly drop the version-1 coverage while doing it.
#[test]
fn corpus_headers_parse() {
    for (name, r) in captures() {
        assert!(
            zenkey_fleet::ZREC_READS.contains(&r.header.zrec),
            "{name}: version {} is outside the dialects this reader speaks ({:?})",
            r.header.zrec,
            zenkey_fleet::ZREC_READS
        );
        assert!(!r.header.selectors.is_empty(), "{name}: empty selectors");
        assert!(!r.header.captured_at.is_empty(), "{name}: no capture time");
    }
}

/// Which decode families the GUI claims are asserted per row: a row that
/// yields no message must belong to a family the GUI *deliberately* ignores
/// (evidence — the catalog's input, not the GUI's). A decode regression on a
/// family we consume cannot hide behind "deliberately ignored".
#[test]
fn corpus_rows_decode() {
    for (name, r) in captures() {
        assert!(!r.rows.is_empty(), "{name}: no rows");
        for (row, _) in &r.rows {
            let msg = replay::decode_row(row, &r.header.base);
            if msg.is_none() && !row.delete {
                let key = &row.key;
                assert!(
                    key.contains("/evidence/"),
                    "{name}: {key} decoded to nothing, and it is not a family \
                     the GUI deliberately ignores"
                );
            }
        }
    }
}

/// The telemetry capture is all telemetry: every row decodes to a `Reading`
/// that knows its publishing origin (#474 — the origin rides the key).
#[test]
fn telemetry_capture_decodes_to_readings() {
    let r = capture("sysinfo-telemetry");
    let msgs = r.messages();
    assert!(msgs.len() >= 50, "telemetry floor: got {}", msgs.len());
    for m in &msgs {
        match m {
            zensight::Message::TelemetryReceived(reading) => {
                assert!(
                    reading.origin.starts_with("h-"),
                    "a reading without a host origin: {:?}",
                    reading.origin
                );
                assert_eq!(reading.point.protocol, zensight_common::Protocol::Sysinfo);
            }
            other => panic!("non-telemetry message in a telemetry capture: {other:?}"),
        }
    }
}

/// The state capture carries the framework state set: health from every
/// sensor in the reference deployment, and the sensor registration docs.
/// (Names of *sensors* are deployment facts, not capture accidents — the
/// regeneration script pins the sensor set.)
#[test]
fn state_capture_covers_the_framework_families() {
    let r = capture("state-plane");
    let msgs = r.messages();
    let mut health_sensors = std::collections::BTreeSet::new();
    let mut sensor_docs = 0usize;
    for m in &msgs {
        match m {
            zensight::Message::HealthSnapshotReceived(s) => {
                health_sensors.insert(s.sensor.clone());
            }
            zensight::Message::SensorInfoReceived(_) => sensor_docs += 1,
            _ => {}
        }
    }
    for sensor in ["sysinfo", "logs", "systemd"] {
        assert!(
            health_sensors.contains(sensor),
            "no health snapshot from {sensor}; saw {health_sensors:?}"
        );
    }
    assert!(sensor_docs >= 1, "no sensor registration doc decoded");
}

/// The catalog capture holds the correlator's output: at least one fused
/// `HostEntity` — the one key family a `v1/*` selector can never see
/// (grammar D4), which is why it is its own capture.
#[test]
fn catalog_capture_holds_an_entity() {
    let r = capture("catalog-entities");
    let entities = r
        .messages()
        .iter()
        .filter(|m| matches!(m, zensight::Message::EntityReceived(_)))
        .count();
    assert!(
        entities >= 1,
        "no HostEntity decoded from the catalog capture"
    );
}

/// **`bytes` is the payload** — the round trip that proves it. Each row is
/// lifted to a `SampleView` on the capture's own clock, written through
/// `ZrecWriter`, read back, and must yield the identical `IngestRow` at the
/// identical offset. Byte-identity of the *lines* is deliberately not
/// asserted: the informative HLC `timestamp` is not reconstructed (replay
/// re-stamps, RFC 09 §5.2) — the payload, key, kind, encoding, QoS name and
/// pacing are the contract, and `IngestRow`'s `Eq` covers them.
#[test]
fn rows_round_trip_lossless() {
    use std::time::{Duration, Instant};
    for (name, r) in captures() {
        let epoch = Instant::now();
        let mut out = Vec::new();
        {
            let mut w = zenkey_fleet::ZrecWriter::new_at(&mut out, &r.header, epoch)
                .expect("writer starts");
            for (row, t) in &r.rows {
                let received = epoch + Duration::from_micros(t.expect("captured rows carry t"));
                w.write_sample(&replay::sample_view(row, received))
                    .expect("row writes");
            }
            w.finish().expect("writer flushes");
        }
        let rewritten = replay::read(out.as_slice()).expect("rewritten capture parses");
        assert_eq!(rewritten.rows.len(), r.rows.len(), "{name}: row count");
        for ((orig, t_orig), (rt, t_rt)) in r.rows.iter().zip(rewritten.rows.iter()) {
            assert_eq!(orig, rt, "{name}: a row did not survive the round trip");
            assert_eq!(t_orig, t_rt, "{name}: a pacing offset drifted");
        }
    }
}
