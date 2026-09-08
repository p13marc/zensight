//! Deterministic replay of `.zrec` captures into the GUI's decode path (#747).
//!
//! A hand-built test sample encodes what we *think* a sensor publishes; a
//! `.zrec` capture encodes what one *did*. This module turns a capture into
//! the two shapes the GUI can consume without a bus:
//!
//! - [`decode_row`] / [`Replay::messages`] — each captured row through the
//!   live subscription decode path ([`crate::subscription`]), yielding the
//!   same [`Message`]s a connected session would, in capture order.
//! - [`sample_view`] — a captured row as the wire fact
//!   `zenkey_fleet::MonitorCore::ingest_at` consumes, with the arrival clock
//!   injected, so a capture can drive a monitor-backed view with no live time.
//!
//! Nothing here can publish: there is no session in this module at all, which
//! is the RFC 09 §5.2 *pane replay* posture. Re-publishing a capture onto a
//! bus is `zenkey_fleet::replay()`'s job and carries obligations this module
//! deliberately has no surface for — re-stamping, the retire gate, the
//! foreign-base refusal, and RFC 09 §5.3's synthetic-marker etiquette.
//!
//! **`"bytes"` is the payload; `"value"` is a rendering.** [`parse_row`]
//! (upstream) enforces the precedence; `bytes_are_the_payload` below pins it
//! from this side, because a fixture that round-trips through the rendering
//! is not a fixture.

use std::io::BufRead;
use std::time::Instant;

use zenkey_fleet::{IngestRow, Transition, ZrecItem, ZrecReader};
// Report shapes ride `zenkey_fleet::report::*` — the one sanctioned module
// path (the crate root carries this one too; the module spelling matches the
// conformance crate's imports).
use zenkey_fleet::report::ZrecHeader;

use crate::message::Message;
use crate::subscription::{decode_sample, parse_tombstone};

/// A parsed `.zrec` capture: the header's coverage statement, every sample
/// row in file order with its pacing offset, and what the capture itself
/// missed.
#[derive(Debug)]
pub struct Replay {
    /// Line 1 of the file: version, selectors (what was watched), the
    /// operator's stated base, and when the capture started.
    pub header: ZrecHeader,
    /// Preamble rows, in file order — state *fetched* at capture time rather
    /// than observed on the wire (`.zrec` version 2, RFC 13 §4.1). A
    /// triggered capture writes these ahead of the first observed row so the
    /// window is not read against an empty world; they are kept apart from
    /// [`Replay::rows`] because they are a different fact, and folded first
    /// by [`Replay::messages`] — the §4.2 *seed the fold* posture a pane
    /// replay takes, as against the publish posture, which needs
    /// `ReplaySpec.seed_state` to touch them at all.
    pub preamble: Vec<IngestRow>,
    /// Sample rows in file order, each with its `t` offset (µs since the
    /// capture epoch, on the observer's arrival clock — the pacing clock).
    pub rows: Vec<(IngestRow, Option<u64>)>,
    /// The transitions that fired a triggered capture (version 2), each with
    /// the index into [`Replay::rows`] it was observed at — the position a
    /// scrubber marks. A time-triggered capture carries none.
    pub triggers: Vec<(usize, Transition)>,
    /// Samples the *capture* dropped, summed from the interleaved drop
    /// records. A consumer rendering this replay owes the same honesty the
    /// file does: surface it, never fold it away.
    pub dropped: u64,
}

/// Read a capture from disk. See [`read`].
pub fn load(path: impl AsRef<std::path::Path>) -> anyhow::Result<Replay> {
    let path = path.as_ref();
    let file =
        std::fs::File::open(path).map_err(|e| anyhow::anyhow!("open {}: {e}", path.display()))?;
    read(std::io::BufReader::new(file))
}

/// Read a capture from any buffered source (inline fixtures in tests).
///
/// A malformed line is an error, not a skip: upstream's reader names the
/// offending line, and a fixture with a bad line is a broken fixture — the
/// counted-never-skipped rule, applied at load instead of at judge.
pub fn read(source: impl BufRead) -> anyhow::Result<Replay> {
    let mut reader = ZrecReader::new(source).map_err(|e| anyhow::anyhow!("{e}"))?;
    let header = reader.header().clone();
    let mut preamble = Vec::new();
    let mut rows = Vec::new();
    let mut triggers = Vec::new();
    let mut dropped = 0u64;
    while let Some(item) = reader.next() {
        match item.map_err(|e| anyhow::anyhow!("{e}"))? {
            ZrecItem::Sample { row, t_us, .. } => rows.push((row, t_us)),
            ZrecItem::Dropped(n) => dropped += n,
            // Fetched, not observed: kept out of `rows` so the pacing clock
            // and the coverage statement stay about what the wire carried.
            ZrecItem::Preamble { row, .. } => preamble.push(row),
            // Recorded at the position it was seen, so a renderer can mark
            // the row the rule fired on rather than a wall-clock guess.
            ZrecItem::Trigger(t) => triggers.push((rows.len(), *t)),
        }
    }
    Ok(Replay {
        header,
        preamble,
        rows,
        triggers,
        dropped,
    })
}

/// One captured row through the live subscription decode path.
///
/// `base` is the capture header's stated base: a `.zrec` records **full wire
/// keys** (RFC 09 §5.2 — never re-derived from the base), while the live
/// session's `namespace` strips the base before `decode_sample` ever sees a
/// key. Replay must do the same stripping, or a namespaced capture decodes
/// nothing. Tombstones route through the retire-aware parser exactly as the
/// drain loop routes a `SampleKind::Delete`.
pub fn decode_row(row: &IngestRow, base: &str) -> Option<Message> {
    let key = if base.is_empty() {
        row.key.as_str()
    } else {
        row.key.strip_prefix(base)?.strip_prefix('/')?
    };
    if row.delete {
        parse_tombstone(key)
    } else {
        decode_sample(key, &row.payload)
    }
}

impl Replay {
    /// Every row decoded in file order — the deterministic fold input for an
    /// `App::update` test. Rows the GUI deliberately ignores (unregistered
    /// subjects, evidence, artifacts) yield `None` and are omitted, same as
    /// the live drain loop.
    ///
    /// Preamble rows come first, which is what makes a triggered capture
    /// readable: they are the state as of `t = 0`, so folding them ahead of
    /// the observed window is the difference between "this host went unready"
    /// and "this host appeared, already unready". They are seeded, never
    /// paced — a `.zrec` version 1 capture has none and this is the old
    /// behaviour exactly.
    pub fn messages(&self) -> Vec<Message> {
        self.preamble
            .iter()
            .chain(self.rows.iter().map(|(row, _)| row))
            .filter_map(|row| decode_row(row, &self.header.base))
            .collect()
    }
}

/// A captured row as the wire fact `MonitorCore::ingest_at` consumes.
///
/// The injected `received` instant is the whole point: a replayed rebuild
/// feeds the *capture* clock, so it folds no live time and two rebuilds of
/// the same file are identical. Mapping rules:
///
/// - QoS: the row's profile *name* resolves to its axes
///   (`QosProfile::from_name`); an absent or unknown name gets zenoh's
///   default axes, which deliberately match no named profile — so a
///   re-written row stays nameless rather than gaining a guessed one.
/// - `delete` becomes `SampleKind::Delete` (a tombstone has no payload).
/// - `timestamp`/`stamped_by`/`source` stay `None`: the capture's HLC string
///   is informative and replay re-stamps (RFC 09 §5.2); inventing a stamp
///   here would fabricate provenance the wire never carried.
pub fn sample_view(row: &IngestRow, received: Instant) -> zenkey_fleet::SampleView {
    let qos = row
        .qos
        .as_deref()
        .and_then(zenkey::qos::QosProfile::from_name);
    zenkey_fleet::SampleView {
        key: row.key.clone(),
        payload: zenoh::bytes::ZBytes::from(row.payload.clone()),
        encoding: row.encoding.clone().unwrap_or_default(),
        kind: if row.delete {
            zenoh::sample::SampleKind::Delete
        } else {
            zenoh::sample::SampleKind::Put
        },
        timestamp: None,
        stamped_by: None,
        attachment: row.attachment.clone().map(zenoh::bytes::ZBytes::from),
        priority: qos.map_or(zenoh::qos::Priority::DEFAULT, |p| p.priority()),
        congestion_control: qos.map_or(zenoh::qos::CongestionControl::DEFAULT, |p| {
            p.congestion_control()
        }),
        reliability: qos.map_or(zenoh::qos::Reliability::DEFAULT, |p| p.reliability()),
        express: qos.is_some_and(|p| p.express()),
        source: None,
        received,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal header line for inline fixtures. The filename-free inline
    /// form is upstream's own test idiom: the dialect is NDJSON, so a
    /// capture is just lines.
    const HEADER: &str =
        r#"{"zrec":1,"selectors":["v1/**"],"base":"","captured_at":"2026-08-30T00:00:00Z"}"#;

    fn replay(lines: &[&str]) -> Replay {
        let file = std::iter::once(HEADER)
            .chain(lines.iter().copied())
            .collect::<Vec<_>>()
            .join("\n");
        read(file.as_bytes()).expect("inline fixture parses")
    }

    /// `"bytes"` wins over `"value"` unconditionally: the payload that comes
    /// out is the base64-decoded bytes, not a re-serialization of the
    /// rendering. `eyJhIjoxfQ==` is `{"a":1}`; the disagreeing `value` would
    /// re-serialize as `{"b":2}`.
    #[test]
    fn bytes_are_the_payload() {
        let r = replay(&[
            r#"{"key":"v1/h-3fa9c2d41b7e/state/netlink/health","t":0,"delete":false,"value":{"b":2},"bytes":"eyJhIjoxfQ=="}"#,
        ]);
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0].0.payload, br#"{"a":1}"#);
    }

    /// A delete row routes through the tombstone parser, not the payload
    /// decoder — same fork the live drain loop takes on `SampleKind`.
    #[test]
    fn delete_routes_to_tombstone() {
        let r = replay(&[
            r#"{"key":"v1/h-3fa9c2d41b7e/state/netlink/alert/9f2c81ab04d7e3f1","t":0,"delete":true}"#,
        ]);
        let msg = decode_row(&r.rows[0].0, &r.header.base);
        assert!(
            matches!(
                msg,
                Some(Message::AlertCleared { ref protocol, ref alert_key, .. })
                    if protocol == "netlink" && alert_key == "9f2c81ab04d7e3f1"
            ),
            "expected AlertCleared, got {msg:?}"
        );
    }

    /// Drop records are counted, never yielded as rows — and never silently
    /// discarded: the sum is the capture's own honesty about what it missed.
    #[test]
    fn dropped_is_counted_not_yielded() {
        let r = replay(&[
            r#"{"key":"v1/h-3fa9c2d41b7e/state/netlink/health","t":0,"delete":false,"bytes":"e30="}"#,
            r#"{"dropped":3}"#,
            r#"{"key":"v1/h-3fa9c2d41b7e/state/netlink/health","t":10,"delete":false,"bytes":"e30="}"#,
        ]);
        assert_eq!(r.rows.len(), 2);
        assert_eq!(r.dropped, 3);
    }

    /// Captured keys are full wire keys; decode strips the header's base the
    /// way the live session's namespace does. The conformance deployment is
    /// base-less, so without this test a missing strip would pass silently
    /// against the checked-in corpus.
    #[test]
    fn base_is_stripped_before_decode() {
        let row = IngestRow {
            key: "prod/v1/h-3fa9c2d41b7e/state/netlink/alert/9f2c81ab04d7e3f1".into(),
            payload: Vec::new(),
            encoding: None,
            qos: None,
            delete: true,
            attachment: None,
        };
        assert!(matches!(
            decode_row(&row, "prod"),
            Some(Message::AlertCleared { .. })
        ));
        // Under the wrong base the key does not parse — no message, never a
        // misroute.
        assert!(decode_row(&row, "").is_none());
        assert!(decode_row(&row, "other").is_none());
    }

    /// The row → `SampleView` mapping: a named profile round-trips through
    /// its axes (`qos_matches`), an absent name gets zenoh's defaults (which
    /// match no profile — a re-written row must not gain a guessed name),
    /// and `delete` becomes the sample kind.
    #[test]
    fn sample_view_mapping() {
        let epoch = Instant::now();
        let mk = |qos: Option<&str>, delete: bool| IngestRow {
            key: "v1/h-3fa9c2d41b7e/state/netlink/health".into(),
            payload: b"{}".to_vec(),
            encoding: Some("application/json".into()),
            qos: qos.map(String::from),
            delete,
            attachment: None,
        };

        let alert = sample_view(&mk(Some("alert"), false), epoch);
        assert!(alert.qos_matches(zenkey::qos::QosProfile::Alert));
        assert!(alert.express);
        assert_eq!(alert.kind, zenoh::sample::SampleKind::Put);
        assert_eq!(alert.received, epoch);
        assert_eq!(alert.encoding, "application/json");
        assert!(alert.timestamp.is_none() && alert.stamped_by.is_none());

        let plain = sample_view(&mk(None, true), epoch);
        assert_eq!(plain.kind, zenoh::sample::SampleKind::Delete);
        assert!(
            zenkey::qos::QosProfile::ALL
                .into_iter()
                .all(|p| !plain.qos_matches(p)),
            "default axes must not match a named profile"
        );
    }
}
