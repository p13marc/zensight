//! Multiline / stacktrace joining for the stream (TCP/Unix) paths (#107, C6).
//!
//! LF framing shatters a Java/Python/Go traceback into one syslog record per
//! line: the stack frames lose their head, the per-line event store fills with
//! orphaned fragments, and template mining sees garbage. journald is already
//! one-record-per-entry, so this is **network-stream only**.
//!
//! [`MultilineJoiner`] sits between [`crate::ingest::FrameReader`] and the
//! parser: it buffers a head line and folds following **continuation** lines
//! (indented frames, `Caused by:`, `...`, `Traceback …`) into it, emitting the
//! joined record when the next real syslog record (`<PRI>…`) arrives or a flush
//! timeout elapses. The join is done on the raw frame text, so the existing
//! parser runs once over the whole record and the multiline body lands in the
//! message value (one per-line event, one uid — #104).
//!
//! The `push`/`flush` core is pure (no clock, no I/O) so the join decisions are
//! unit-testable; the listener drives the flush timeout with `tokio::select!`.

use crate::config::MultilineConfig;

/// Stateful joiner: feed it raw frames in order; it yields completed (possibly
/// multi-line) raw records. Bounded by `max_lines` / `max_bytes` so a runaway
/// "continuation" stream can't grow a single record without limit.
#[derive(Debug)]
pub struct MultilineJoiner {
    enabled: bool,
    max_lines: usize,
    max_bytes: usize,
    /// The record currently being accumulated (head + folded continuations).
    pending: Option<String>,
    pending_lines: usize,
}

impl MultilineJoiner {
    pub fn new(cfg: &MultilineConfig) -> Self {
        Self {
            enabled: cfg.enabled,
            max_lines: cfg.max_lines.max(1),
            max_bytes: cfg.max_bytes.max(1),
            pending: None,
            pending_lines: 0,
        }
    }

    /// `true` while a record is buffered awaiting more continuations — the
    /// listener arms its flush timeout only then.
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Feed the next raw frame. Returns a completed record to parse/forward, if
    /// this frame closed one off (i.e. it begins a new record, or the buffer hit
    /// a cap). When joining is disabled the frame passes straight through.
    pub fn push(&mut self, raw: String) -> Option<String> {
        if !self.enabled {
            return Some(raw);
        }
        // No head yet → this frame becomes the head, nothing to emit.
        let Some(pending) = self.pending.as_mut() else {
            self.start(raw);
            return None;
        };

        if is_continuation(&raw) {
            // Appending keeps the same record — unless it would breach a cap, in
            // which case flush what we have and start a fresh record with this
            // line so neither memory nor a single event grows without bound.
            let would_bytes = pending.len() + 1 + raw.len();
            if self.pending_lines + 1 > self.max_lines || would_bytes > self.max_bytes {
                let done = self.pending.take();
                self.start(raw);
                return done;
            }
            pending.push('\n');
            pending.push_str(&raw);
            self.pending_lines += 1;
            None
        } else {
            // A new syslog record: emit the buffered one, buffer this.
            let done = self.pending.take();
            self.start(raw);
            done
        }
    }

    /// Force-emit the buffered record (flush timeout / end of stream).
    pub fn flush(&mut self) -> Option<String> {
        self.pending_lines = 0;
        self.pending.take()
    }

    fn start(&mut self, raw: String) {
        self.pending = Some(raw);
        self.pending_lines = 1;
    }
}

/// Heuristic: does this raw frame *continue* the previous record rather than
/// start a new one? Conservative on purpose — only clear stack-trace shapes
/// fold, so ordinary unindented log lines are never wrongly coalesced.
///
/// Matches: any leading whitespace (indented frames — Java `\tat`, Python
/// `  File "…"`, Go `\t…`), and a small set of unindented continuation markers
/// (`Caused by:`, `... N more`, `Traceback …`, Python exception-chaining notes).
pub fn is_continuation(line: &str) -> bool {
    // Indented lines are the universal stack-frame shape.
    if line.starts_with(' ') || line.starts_with('\t') {
        return true;
    }
    let t = line.trim_start();
    const MARKERS: &[&str] = &[
        "Caused by:",
        "...",
        "Traceback (most recent call last):",
        "During handling of the above exception",
        "The above exception was the direct cause",
    ];
    MARKERS.iter().any(|m| t.starts_with(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool) -> MultilineConfig {
        MultilineConfig {
            enabled,
            flush_timeout_ms: 200,
            max_lines: 500,
            max_bytes: 65536,
        }
    }

    #[test]
    fn disabled_is_passthrough() {
        let mut j = MultilineJoiner::new(&cfg(false));
        assert_eq!(j.push("<14>a".into()), Some("<14>a".into()));
        assert_eq!(j.push("  indented".into()), Some("  indented".into()));
        assert!(!j.has_pending());
    }

    #[test]
    fn continuation_lines_fold_into_head() {
        let mut j = MultilineJoiner::new(&cfg(true));
        // Head buffered, nothing emitted yet.
        assert_eq!(j.push("<11>java.lang.NPE".into()), None);
        assert!(j.has_pending());
        // Indented frame + unindented "Caused by:" fold in.
        assert_eq!(j.push("\tat Foo.bar(Foo.java:42)".into()), None);
        assert_eq!(j.push("Caused by: java.io.IOException".into()), None);
        assert_eq!(j.push("\t... 3 more".into()), None);
        // Next real record flushes the joined stack trace.
        let done = j.push("<14>next line".into()).unwrap();
        assert_eq!(
            done,
            "<11>java.lang.NPE\n\tat Foo.bar(Foo.java:42)\nCaused by: java.io.IOException\n\t... 3 more"
        );
        // The new head is now buffered.
        assert_eq!(j.flush(), Some("<14>next line".into()));
    }

    #[test]
    fn distinct_records_do_not_join() {
        let mut j = MultilineJoiner::new(&cfg(true));
        assert_eq!(j.push("<14>one".into()), None);
        assert_eq!(j.push("<14>two".into()), Some("<14>one".into()));
        assert_eq!(j.push("<14>three".into()), Some("<14>two".into()));
        assert_eq!(j.flush(), Some("<14>three".into()));
    }

    #[test]
    fn flush_emits_buffered_record() {
        let mut j = MultilineJoiner::new(&cfg(true));
        assert_eq!(j.push("<14>solo".into()), None);
        assert_eq!(j.flush(), Some("<14>solo".into()));
        assert_eq!(j.flush(), None); // nothing left
    }

    #[test]
    fn max_lines_cap_flushes_partial() {
        let mut c = cfg(true);
        c.max_lines = 2;
        let mut j = MultilineJoiner::new(&c);
        assert_eq!(j.push("<14>head".into()), None); // 1 line
        assert_eq!(j.push("  cont1".into()), None); // 2 lines
        // Third continuation would exceed max_lines → flush head+cont1, restart.
        let done = j.push("  cont2".into()).unwrap();
        assert_eq!(done, "<14>head\n  cont1");
        assert_eq!(j.flush(), Some("  cont2".into()));
    }

    #[test]
    fn max_bytes_cap_flushes_partial() {
        let mut c = cfg(true);
        c.max_bytes = 12;
        let mut j = MultilineJoiner::new(&c);
        assert_eq!(j.push("<14>hi".into()), None); // 6 bytes
        // "  world" (7) + 1 newline + 6 = 14 > 12 → flush, restart.
        let done = j.push("  world".into()).unwrap();
        assert_eq!(done, "<14>hi");
        assert_eq!(j.flush(), Some("  world".into()));
    }

    #[test]
    fn continuation_detection() {
        assert!(is_continuation("    at com.Foo.bar(Foo.java:1)"));
        assert!(is_continuation("\tat runtime.main()"));
        assert!(is_continuation("Caused by: x"));
        assert!(is_continuation("... 5 more"));
        assert!(is_continuation("Traceback (most recent call last):"));
        assert!(!is_continuation("<14>real syslog line"));
        assert!(!is_continuation("plain unindented line"));
    }
}
