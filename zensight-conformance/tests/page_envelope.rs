//! The `Page<T>` envelope is readable by the tooling it exists for (#1157).
//!
//! `zensight-common::page::Page` is written against RFC 05 §3.2, and its own
//! unit tests assert the field names and types. They cannot assert the thing
//! that actually matters: that `zenkey_fleet::CallAnswer::page_signal()` — the
//! reader `zenctl call` and the RFC 13 judges use — recognises it. That check
//! has to live in a crate allowed to link `zenkey-fleet`, and `zensight-common`
//! is linked by every sensor, so it is not one.
//!
//! This is the failure mode the test is for: an envelope that is *almost*
//! right — `truncated` instead of `partial`, or `covers_from` as epoch millis
//! instead of an instant string — parses fine, serializes fine, round-trips
//! fine, and is read as **absent** by every generic consumer. The historian's
//! `RangeReply` has been in exactly that state since it was written.

use zenkey_fleet::report::{CallAnswer, CallOutcome};
use zensight_common::page::Page;

/// Wrap a serialized envelope in the answer shape `page_signal` reads.
fn answer(value: serde_json::Value) -> CallAnswer {
    CallAnswer {
        origin: "h-0123456789ab".to_string(),
        outcome: CallOutcome::Ok {
            value: Some(value),
            text: None,
        },
        attachment: None,
        attachment_bytes: None,
    }
}

#[test]
fn the_generic_reader_sees_a_complete_walk() {
    let page = Page::complete(vec![1u8, 2, 3]);
    let sig = answer(serde_json::to_value(&page).unwrap())
        .page_signal()
        .expect("a Page must be recognised as an RFC 05 §3.2 envelope");
    assert!(!sig.partial);
    assert_eq!(sig.next_cursor, None);
    assert!(!sig.is_contract_violation());
}

#[test]
fn the_generic_reader_sees_the_cursor_the_scan_and_the_coverage() {
    let page = Page::more(vec!["a".to_string()], "h-0123456789ab/sysinfo/cpu:40")
        .scanned(4096)
        .covers_from("2026-09-08T09:00:00Z");
    let sig = answer(serde_json::to_value(&page).unwrap())
        .page_signal()
        .expect("recognised");
    assert!(sig.partial);
    assert_eq!(
        sig.next_cursor.as_deref(),
        Some("h-0123456789ab/sysinfo/cpu:40")
    );
    assert_eq!(sig.scanned, Some(4096));
    assert_eq!(sig.covers_from.as_deref(), Some("2026-09-08T09:00:00Z"));
    assert!(!sig.is_contract_violation());
}

/// The two near-misses, stated as tests so the next handler does not have to
/// rediscover them on a live bus.
#[test]
fn a_reply_that_says_truncated_is_invisible_to_the_reader() {
    let almost = serde_json::json!({ "items": [1], "truncated": false, "next_cursor": null });
    assert!(
        answer(almost).page_signal().is_none(),
        "`truncated` is not the marker; an envelope spelled that way is not \
         seen as one at all"
    );
}

#[test]
fn coverage_as_a_number_is_read_as_absent() {
    let almost = serde_json::json!({
        "items": [1],
        "partial": true,
        "next_cursor": "c1",
        "covers_from": 1_757_325_600_000i64,
    });
    let sig = answer(almost).page_signal().expect("recognised");
    assert_eq!(
        sig.covers_from, None,
        "read with as_str(): epoch millis is silently no coverage statement"
    );
}

/// `partial: true` with a null cursor is what an observer MAY report as a
/// finding, and it is what `Page::is_contract_violation` guards against.
#[test]
fn the_reader_and_the_type_agree_on_what_a_violation_is() {
    let bad = Page {
        partial: true,
        ..Page::complete(vec![1u8])
    };
    assert!(bad.is_contract_violation());
    let sig = answer(serde_json::to_value(&bad).unwrap())
        .page_signal()
        .expect("recognised");
    assert!(sig.is_contract_violation());
}
