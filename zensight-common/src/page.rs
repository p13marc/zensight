//! `Page<T>` — the RFC 05 §3.2 bounded-reply envelope (#1157).
//!
//! **The gap it closes.** A `Vec<LogRecord>` has nowhere to say "there is
//! more", "I stopped early", or "this is what the walk cost". Every bounded
//! handler in the tree had that gap and each one filled it differently, or not
//! at all: `logs/events` truncates a search at `MAX_SEARCH_SCAN` and a
//! truncated page is indistinguishable from no matches; `netlink/sockets` and
//! `sysinfo/processes` cap and say nothing; the historian's `RangeReply` grew
//! its own `truncated` and `next_cursor` and still cannot say what window a
//! tier could actually cover.
//!
//! **Why this spelling and not ours.** The RFC 05 row was filed upstream from
//! this application (zenkey #423) and ratified in v1.31, shipped in zenkey
//! 0.8.0 — so the field names here are normative, not a ZenSight habit, and
//! `zenkey_fleet::CallAnswer::page_signal()` already reads them. That has two
//! consequences worth stating where the type is defined, because both are
//! silent failures:
//!
//! - **`partial` is the required marker.** `page_signal()` returns `None`
//!   unless the reply is a JSON object with a *boolean* field named exactly
//!   `partial`. It deliberately does not synthesise `partial: false` for a bare
//!   list: an absent envelope and a complete walk are different facts. A reply
//!   that says `truncated` instead — which the historian's does — is invisible
//!   to `zenctl call` and to every RFC 13 judge.
//! - **`covers_from` is a string instant.** It is read with `as_str()`, so an
//!   epoch-millis *number* is read as **absent** by exactly the tooling that
//!   would otherwise catch the bug it exists to report.
//!
//! **The cursor is a value, never a position.** RFC 05 §3.2 names the
//! reference historian's positional cursor as the defect that motivated the
//! row: sorting keeps the *order* stable, not the *indices*, so a row interned
//! between two pages makes the second repeat one or skip one, and neither is
//! signalled. The cursor is opaque to the caller and must be the last emitted
//! key, id or instant.

use serde::{Deserialize, Serialize};

/// One page of a bounded reply, and what the producer says about the walk.
///
/// Field order is the RFC's. Every optional is absent rather than null, which
/// is what the rest of the wire does and what `page_signal()` reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Page<T> {
    /// This page's rows, in the order the procedure documents.
    pub items: Vec<T>,
    /// Opaque cursor for the next page — a **value**: the last emitted key, id
    /// or instant, never a position. `None` means the walk is complete for the
    /// filter given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// The producer stopped before completing the walk — a scan cap, a tier
    /// that could not cover the window, a time budget — so a short page is not
    /// the end.
    ///
    /// Always serialized, at `false` as much as at `true`: it is the marker the
    /// envelope is recognised by, and a reply that omits it is not an envelope
    /// at all.
    pub partial: bool,
    /// What the page cost, so an expensive empty page can be told from a cheap
    /// one. Absent from procedures that do not count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scanned: Option<u64>,
    /// The oldest instant the answer *could* have covered, for a computed
    /// answer whose coverage is narrower than what was asked — a sub-minute
    /// query over a hot ring answering ten minutes as if they were the whole
    /// day. An RFC 3339 instant, because that is what the generic reader
    /// parses. Absent from procedures with no notion of coverage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub covers_from: Option<String>,
}

impl<T> Page<T> {
    /// A complete walk: every row that matched, no more to come.
    pub fn complete(items: Vec<T>) -> Self {
        Page {
            items,
            next_cursor: None,
            partial: false,
            scanned: None,
            covers_from: None,
        }
    }

    /// A page that stopped early and says where to resume.
    ///
    /// Takes the cursor by value and not by `Option`, because the pairing is
    /// the contract: see [`Page::is_contract_violation`].
    pub fn more(items: Vec<T>, next_cursor: impl Into<String>) -> Self {
        Page {
            items,
            next_cursor: Some(next_cursor.into()),
            partial: true,
            scanned: None,
            covers_from: None,
        }
    }

    /// Record what the walk cost.
    pub fn scanned(mut self, n: u64) -> Self {
        self.scanned = Some(n);
        self
    }

    /// Record the oldest instant this answer could have covered — an RFC 3339
    /// string, because a number is read as absent by every generic consumer.
    pub fn covers_from(mut self, instant: impl Into<String>) -> Self {
        self.covers_from = Some(instant.into());
        self
    }

    /// Mark the answer as narrower than what was asked, without a cursor to
    /// resume from — for a coverage gap, which is a gap in *time* rather than
    /// in count and has no next page.
    ///
    /// This is the one shape that is deliberately allowed to set `partial`
    /// without a cursor, and it is why [`Page::is_contract_violation`] exists
    /// as a check the caller runs rather than an invariant the type enforces.
    pub fn partial_coverage(mut self, covers_from: impl Into<String>) -> Self {
        self.partial = true;
        self.covers_from = Some(covers_from.into());
        self
    }

    /// `partial: true` **with** `next_cursor: null` and no coverage statement:
    /// the producer says it stopped early and offers no way on, and no reason.
    ///
    /// RFC 05 §3.2 names this a contract violation an observer MAY report (RFC
    /// 13 §3), and `zenkey_fleet::PageSignal::is_contract_violation` computes
    /// exactly it — so a handler that gets this wrong is a finding on a live
    /// bus, not only a failing unit test. Kept here so a handler can assert it
    /// before the bus does.
    pub fn is_contract_violation(&self) -> bool {
        self.partial && self.next_cursor.is_none() && self.covers_from.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The names on the wire are the RFC's, and `partial` is present even when
    /// false — `zenkey_fleet::CallAnswer::page_signal()` returns `None`
    /// without a boolean field spelled exactly that, so an envelope that omits
    /// it is invisible to `zenctl call` and to every RFC 13 judge.
    #[test]
    fn the_marker_is_always_on_the_wire_and_spelled_partial() {
        let v = serde_json::to_value(Page::complete(vec![1u8, 2, 3])).unwrap();
        let o = v.as_object().expect("an object, not a bare list");
        assert_eq!(o.get("partial").and_then(|p| p.as_bool()), Some(false));
        assert!(o.contains_key("items"));
        // Absent, not null: what the rest of the wire does.
        assert!(!o.contains_key("next_cursor"));
        assert!(!o.contains_key("scanned"));
        assert!(!o.contains_key("covers_from"));
    }

    /// `covers_from` is a **string** instant. An epoch-millis number is read
    /// as absent by `page_signal()`, which reads it with `as_str()` — so the
    /// field would be invisible to exactly the tooling that exists to catch
    /// the bug it reports.
    #[test]
    fn covers_from_is_a_string_instant() {
        let p = Page::complete(vec![0u8]).covers_from("2026-09-08T10:00:00Z");
        let v = serde_json::to_value(p).unwrap();
        assert!(v["covers_from"].is_string());
    }

    /// `partial: true` with a null cursor and no coverage is the RFC's named
    /// contract violation; a coverage gap is the one shape allowed to say
    /// `partial` without offering a next page.
    #[test]
    fn partial_without_a_way_on_is_a_contract_violation_unless_it_is_coverage() {
        let bad = Page {
            partial: true,
            ..Page::complete(vec![1u8])
        };
        assert!(bad.is_contract_violation());

        let paged = Page::more(vec![1u8], "h-0123456789ab/sysinfo/cpu:40");
        assert!(!paged.is_contract_violation());

        let short_window = Page::complete(vec![1u8]).partial_coverage("2026-09-08T09:50:00Z");
        assert!(short_window.partial);
        assert!(
            !short_window.is_contract_violation(),
            "a gap in time has no next page, and says why instead"
        );
    }

    /// A round trip through the envelope keeps every field, and a reply
    /// written by an older producer (no optionals at all) still parses.
    #[test]
    fn the_envelope_round_trips_and_tolerates_a_minimal_producer() {
        let p = Page::more(vec!["a".to_string()], "cursor-1")
            .scanned(4096)
            .covers_from("2026-09-08T09:00:00Z");
        let back: Page<String> = serde_json::from_slice(&serde_json::to_vec(&p).unwrap()).unwrap();
        assert_eq!(back, p);

        let minimal: Page<String> =
            serde_json::from_str(r#"{"items":["a"],"partial":false}"#).unwrap();
        assert_eq!(minimal.items, vec!["a".to_string()]);
        assert!(minimal.next_cursor.is_none());
        assert!(minimal.scanned.is_none());
        assert!(minimal.covers_from.is_none());
    }
}
