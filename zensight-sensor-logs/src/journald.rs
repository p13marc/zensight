//! systemd-journald ingestion (#57).
//!
//! Reads the local systemd journal directly through libsystemd (the `systemd`
//! crate) — **not** by spawning `journalctl` — and feeds each entry into the
//! same `mpsc::Receiver<ReceivedMessage>` the UDP/TCP/Unix listeners use, so
//! everything downstream (filtering, telemetry mapping, frontend, OTEL-logs
//! export) is reused unchanged.
//!
//! ## Threading
//!
//! `systemd::journal::Journal` is `!Send + !Sync`: it can only be used on the
//! thread that created it. The reader therefore runs on a dedicated OS thread
//! (not a tokio task) and hands entries to the async world via
//! [`mpsc::Sender::blocking_send`].
//!
//! ## Scope of this module (#57)
//!
//! Tail-only ingestion + field mapping. Cursor-based no-loss resume (#58) and
//! server-side matching / namespaces beyond `scope` (#59) build on top of this.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{TimeZone, Utc};
use systemd::id128::Id128;
use systemd::journal::{Journal, JournalRecord, JournalWaitResult, OpenOptions};
use tokio::sync::mpsc;

use crate::config::{JournaldConfig, JournaldScope, MissingCursor, OverflowPolicy, StartFrom};
use crate::parser::{Facility, Severity, SyslogMessage, SyslogVersion};
use crate::receiver::{JournaldStats, MessageSource, ReceivedMessage};

/// Global token-bucket rate limiter over a 1-second window (#62). Single-reader,
/// so no synchronization is needed. Beyond `max_eps`, keeps 1-in-`sample_ratio`
/// over-budget entries and reports the rest as sampled-out.
struct RateLimiter {
    max_eps: Option<u64>,
    sample_ratio: u64,
    window_start: Instant,
    in_window: u64,
    over_budget: u64,
}

impl RateLimiter {
    fn new(cfg: &JournaldConfig, now: Instant) -> Self {
        Self {
            max_eps: cfg.max_eps,
            sample_ratio: cfg.sample_ratio.max(1),
            window_start: now,
            in_window: 0,
            over_budget: 0,
        }
    }

    /// `true` to forward this entry, `false` to shed it (sampled-out).
    fn allow(&mut self, now: Instant) -> bool {
        let Some(max) = self.max_eps else {
            return true;
        };
        if now.duration_since(self.window_start) >= Duration::from_secs(1) {
            self.window_start = now;
            self.in_window = 0;
            self.over_budget = 0;
        }
        self.in_window += 1;
        if self.in_window <= max {
            return true;
        }
        // Over budget: keep 1-in-N, shed the rest.
        self.over_budget += 1;
        self.over_budget.is_multiple_of(self.sample_ratio)
    }
}

/// How long the reader blocks in `wait()` before looping (lets it notice a
/// closed channel / process shutdown promptly).
const WAIT_TIMEOUT: Duration = Duration::from_millis(500);

/// Hostname used only when an entry has no `_HOSTNAME` (practically never).
const HOST_FALLBACK: &str = "localhost";

/// Minimum interval between cursor-file writes (#58), to bound write rate under
/// a log storm while keeping the persisted cursor reasonably fresh.
const CURSOR_PERSIST_INTERVAL: Duration = Duration::from_secs(2);

/// Default `since` window when `start_from: "since"` is selected without one.
const DEFAULT_SINCE: Duration = Duration::from_secs(900);

/// Spawn the journald reader on a dedicated OS thread.
///
/// Returns the [`thread::JoinHandle`]; callers may ignore it (the thread stops
/// when the telemetry channel closes, i.e. on shutdown).
pub fn spawn_reader(
    cfg: JournaldConfig,
    tx: mpsc::Sender<ReceivedMessage>,
) -> (thread::JoinHandle<()>, Arc<JournaldStats>) {
    let stats = Arc::new(JournaldStats::default());
    let stats_thread = stats.clone();
    let handle = thread::Builder::new()
        .name("journald-reader".to_string())
        .spawn(move || {
            if let Err(e) = run(&cfg, &tx, &stats_thread) {
                tracing::error!(error = %e, "journald reader exited with error");
            }
        })
        .expect("failed to spawn journald reader thread");
    (handle, stats)
}

/// Open the journal according to `scope` / `namespace`.
fn open(cfg: &JournaldConfig) -> std::io::Result<Journal> {
    let mut opts = OpenOptions::default();
    match cfg.scope {
        JournaldScope::System => opts.system(true),
        JournaldScope::User => opts.current_user(true),
        JournaldScope::LocalOnly => opts.local_only(true),
        JournaldScope::RuntimeOnly => opts.runtime_only(true),
    };
    match &cfg.namespace {
        Some(ns) => opts.open_namespace(ns.as_str()),
        None => opts.open(),
    }
}

/// Compile the declarative server-side filters (#59) into journald match pairs.
///
/// Pure (no journal handle) so the priority OR-expansion is unit-testable.
/// libsystemd ORs matches on the same field and ANDs across fields, so:
/// `(unitA OR unitB) AND (PRIORITY 0..=min) AND (transportX OR …) AND raw…`
/// falls out automatically — no explicit `match_or` needed.
fn build_matches(cfg: &JournaldConfig) -> Vec<(String, String)> {
    let mut matches = Vec::new();
    for unit in &cfg.units {
        matches.push(("_SYSTEMD_UNIT".to_string(), unit.clone()));
    }
    if let Some(min) = cfg.min_priority {
        // libsystemd has no `<=`; enumerate PRIORITY=0..=min (same field → OR).
        for p in 0..=min.min(7) {
            matches.push(("PRIORITY".to_string(), p.to_string()));
        }
    }
    for transport in &cfg.transports {
        matches.push(("_TRANSPORT".to_string(), transport.clone()));
    }
    for (field, value) in &cfg.match_fields {
        matches.push((field.clone(), value.clone()));
    }
    matches
}

/// Install the server-side filters on the journal handle.
fn apply_matches(journal: &mut Journal, cfg: &JournaldConfig) -> std::io::Result<()> {
    let matches = build_matches(cfg);
    if matches.is_empty() {
        return Ok(());
    }
    for (field, value) in &matches {
        journal.match_add(field.as_str(), value.clone())?;
    }
    tracing::info!(
        count = matches.len(),
        "journald: server-side matches applied"
    );
    Ok(())
}

/// Reader loop: open, position per `start_from`, then drain-persist-wait.
fn run(
    cfg: &JournaldConfig,
    tx: &mpsc::Sender<ReceivedMessage>,
    stats: &JournaldStats,
) -> std::io::Result<()> {
    let mut journal = open(cfg)?;
    // Server-side filters must be installed before seeking so they constrain
    // iteration from the first entry (#59).
    apply_matches(&mut journal, cfg)?;
    let cursor_path = resolve_cursor_path(cfg);
    position(&mut journal, cfg, cursor_path.as_deref())?;
    tracing::info!(
        scope = ?cfg.scope,
        namespace = ?cfg.namespace,
        start_from = ?cfg.start_from,
        cursor_file = ?cursor_path,
        "journald reading"
    );

    let mut last_persist = Instant::now();
    let mut pending_cursor: Option<String> = None;
    // No-data diagnostic: a journal that yields nothing for a while usually means
    // the process lacks journal-read access (common for `scope: "system"` when
    // the user isn't in the `systemd-journal`/`adm` group). Warn once so the
    // empty stream is explained rather than silent.
    let started = Instant::now();
    let mut total_read: u64 = 0;
    let mut warned_no_data = false;
    let mut limiter = RateLimiter::new(cfg, started);

    loop {
        // Drain everything currently available.
        let mut advanced = false;
        loop {
            // Tolerate a transient read/decode error: count it and stop draining
            // this batch (go wait) rather than killing the reader.
            let record = match journal.next_entry() {
                Ok(Some(record)) => record,
                Ok(None) => break,
                Err(e) => {
                    JournaldStats::inc(&stats.decode_errors);
                    tracing::warn!(error = %e, "journald: entry read failed; skipping batch");
                    break;
                }
            };
            advanced = true;
            total_read += 1;
            JournaldStats::inc(&stats.read);

            // Rate limit (#62): shed over-budget entries (sampled-out), keeping
            // 1-in-N. Done before mapping so we don't pay decode cost on shed.
            if !limiter.allow(Instant::now()) {
                JournaldStats::inc(&stats.sampled_out);
                continue;
            }

            let recv_usec = journal.timestamp_usec().ok();
            let message = map_record(&record, cfg, recv_usec);
            let resolved_hostname = message
                .hostname
                .clone()
                .unwrap_or_else(|| HOST_FALLBACK.to_string());
            tracing::trace!(
                host = %resolved_hostname,
                sev = message.severity.as_str(),
                "journald entry read"
            );
            let received = ReceivedMessage {
                message,
                source: MessageSource::Journald,
                resolved_hostname,
            };
            // Overflow policy (#62): block (backpressure) vs drop_newest (shed +
            // count). Either way a closed channel means shutdown.
            match send_entry(tx, received, cfg.overflow, stats) {
                SendOutcome::Sent => {}
                SendOutcome::Dropped => {}
                SendOutcome::Closed => {
                    tracing::info!("journald: telemetry channel closed, stopping reader");
                    if let (Some(path), Ok(cur)) = (&cursor_path, journal.cursor()) {
                        let _ = write_cursor_atomic(path, &cur);
                    }
                    return Ok(());
                }
            }
        }

        // After draining, the journal is positioned on the last entry read; grab
        // its cursor once per batch (cheaper than per-entry) for persistence.
        if advanced {
            pending_cursor = journal.cursor().ok();
        }
        if let (Some(path), Some(cur)) = (&cursor_path, &pending_cursor)
            && last_persist.elapsed() >= CURSOR_PERSIST_INTERVAL
        {
            if let Err(e) = write_cursor_atomic(path, cur) {
                tracing::warn!(error = %e, "journald: cursor persist failed");
            }
            last_persist = Instant::now();
        }

        // One-time no-data diagnostic (~15s of nothing read).
        if !warned_no_data && total_read == 0 && started.elapsed() >= Duration::from_secs(15) {
            warned_no_data = true;
            tracing::warn!(
                scope = ?cfg.scope,
                "journald: no entries read after 15s — the reader likely lacks \
                 journal-read access. For scope=system, add this user to the \
                 `systemd-journal` group (or run as a system service), or set \
                 `scope: \"user\"` to read the per-user journal."
            );
        }

        // Block for new entries (bounded so shutdown is observed promptly).
        match journal.wait(Some(WAIT_TIMEOUT)) {
            // Rotation / files added or removed: libsystemd follows it
            // transparently, so this is NOT EOF — count it and loop back to
            // re-drain. (#62: `journalctl --rotate` must not stop the stream.)
            Ok(JournalWaitResult::Invalidate) => {
                JournaldStats::inc(&stats.invalidations);
                tracing::debug!("journald: journal invalidated (rotation); continuing tail");
            }
            Ok(result) => tracing::trace!(?result, "journald wait returned"),
            Err(e) => {
                tracing::warn!(error = %e, "journald wait failed; backing off");
                thread::sleep(WAIT_TIMEOUT);
            }
        }
    }
}

/// Outcome of trying to forward one entry to the telemetry channel.
enum SendOutcome {
    Sent,
    Dropped,
    Closed,
}

/// Forward `received` per the overflow policy, updating `stats`. `block` applies
/// backpressure (waits for room); `drop_newest` sheds + counts when full (#62).
fn send_entry(
    tx: &mpsc::Sender<ReceivedMessage>,
    received: ReceivedMessage,
    policy: OverflowPolicy,
    stats: &JournaldStats,
) -> SendOutcome {
    match policy {
        OverflowPolicy::Block => match tx.blocking_send(received) {
            Ok(()) => {
                JournaldStats::inc(&stats.published);
                SendOutcome::Sent
            }
            Err(_) => SendOutcome::Closed,
        },
        OverflowPolicy::DropNewest => match tx.try_send(received) {
            Ok(()) => {
                JournaldStats::inc(&stats.published);
                SendOutcome::Sent
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                JournaldStats::inc(&stats.dropped);
                SendOutcome::Dropped
            }
            Err(mpsc::error::TrySendError::Closed(_)) => SendOutcome::Closed,
        },
    }
}

/// Position the journal read pointer according to `start_from` (#58).
fn position(
    journal: &mut Journal,
    cfg: &JournaldConfig,
    cursor_path: Option<&Path>,
) -> std::io::Result<()> {
    match cfg.start_from {
        StartFrom::Head => {
            journal.seek_head()?;
        }
        StartFrom::Tail => seek_tail_anchored(journal)?,
        StartFrom::Since => seek_since(journal, cfg)?,
        StartFrom::Boot => {
            // Restrict to the current boot, then start at its first entry.
            let boot = Id128::from_boot()?.to_string();
            journal.match_add("_BOOT_ID", boot)?;
            journal.seek_head()?;
        }
        StartFrom::Cursor => match cursor_path.and_then(read_cursor_file) {
            Some(saved) => {
                journal.seek_cursor(saved.as_str())?;
                // Load the entry at/after the cursor so test_cursor is meaningful.
                journal.next()?;
                if journal.test_cursor(saved.as_str()).unwrap_or(false) {
                    // Positioned on the already-processed entry; the drain loop's
                    // first next() advances past it → resume with no duplicate.
                    tracing::info!("journald: resumed from saved cursor");
                } else {
                    tracing::warn!(
                        "journald: saved cursor not found (rotated out); applying on_missing_cursor"
                    );
                    apply_missing_cursor(journal, cfg)?;
                }
            }
            None => {
                // First run (no cursor yet): behave like tail.
                seek_tail_anchored(journal)?;
            }
        },
    }
    Ok(())
}

/// `seek_tail()` leaves the pointer in an indeterminate post-tail state where
/// `sd_journal_wait` will not report appends; anchor it on the last entry with
/// `previous()` so subsequent `next()` calls only yield genuinely new entries.
fn seek_tail_anchored(journal: &mut Journal) -> std::io::Result<()> {
    journal.seek_tail()?;
    let _ = journal.previous();
    Ok(())
}

/// Seek to `now - since` (defaulting to [`DEFAULT_SINCE`]).
fn seek_since(journal: &mut Journal, cfg: &JournaldConfig) -> std::io::Result<()> {
    let window = cfg
        .since
        .as_deref()
        .and_then(parse_duration)
        .unwrap_or(DEFAULT_SINCE);
    let now_us = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0);
    journal.seek_realtime_usec(now_us.saturating_sub(window.as_micros() as u64))?;
    Ok(())
}

/// Fallback positioning when a saved cursor can no longer be resolved.
fn apply_missing_cursor(journal: &mut Journal, cfg: &JournaldConfig) -> std::io::Result<()> {
    match cfg.on_missing_cursor {
        MissingCursor::Tail => seek_tail_anchored(journal),
        MissingCursor::Since => seek_since(journal, cfg),
    }
}

/// Resolve the cursor-file path: explicit config, else a systemd `STATE_DIRECTORY`
/// / XDG state location. `None` means "don't persist".
fn resolve_cursor_path(cfg: &JournaldConfig) -> Option<PathBuf> {
    if let Some(p) = &cfg.cursor_file {
        return Some(p.clone());
    }
    if let Ok(state) = std::env::var("STATE_DIRECTORY") {
        let first = state.split(':').next().unwrap_or(state.as_str());
        if !first.is_empty() {
            return Some(Path::new(first).join("journald.cursor"));
        }
    }
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        return Some(Path::new(&xdg).join("zensight/journald.cursor"));
    }
    if let Ok(home) = std::env::var("HOME") {
        return Some(Path::new(&home).join(".local/state/zensight/journald.cursor"));
    }
    None
}

/// Read a persisted cursor, treating empty/whitespace as absent.
fn read_cursor_file(path: &Path) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// Atomically write the cursor (temp file + rename), creating parents as needed.
fn write_cursor_atomic(path: &Path, cursor: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(cursor.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// Parse a human duration like `30s`, `15m`, `2h`, `1d`. `None` on bad input.
fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit())?;
    let (num, unit) = s.split_at(split);
    let n: u64 = num.trim().parse().ok()?;
    let secs = match unit.trim() {
        "s" | "sec" | "secs" => n,
        "m" | "min" | "mins" => n * 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => n * 3600,
        "d" | "day" | "days" => n * 86400,
        _ => return None,
    };
    Some(Duration::from_secs(secs))
}

/// Standard journald field → label name mapping (single source of truth so the
/// extra-fields allowlist can avoid duplicating these).
const STANDARD_FIELDS: &[(&str, &str)] = &[
    ("_SYSTEMD_UNIT", "unit"),
    ("_SYSTEMD_USER_UNIT", "user_unit"),
    ("_SYSTEMD_SLICE", "slice"),
    ("_COMM", "comm"),
    ("_EXE", "exe"),
    ("_CMDLINE", "cmdline"),
    ("_UID", "uid"),
    ("_GID", "gid"),
    ("_BOOT_ID", "boot_id"),
    ("_MACHINE_ID", "machine_id"),
    ("_TRANSPORT", "transport"),
];

const DEV_FIELDS: &[(&str, &str)] = &[
    ("CODE_FILE", "code_file"),
    ("CODE_LINE", "code_line"),
    ("CODE_FUNC", "code_func"),
    ("ERRNO", "errno"),
];

/// Map a journald entry to a [`SyslogMessage`].
///
/// Pure (no journal handle) so it is unit-testable from synthetic records.
/// journald entries are already structured, so this bypasses the syslog regex
/// parser entirely. `recv_usec` is the journal's own receive timestamp
/// (`__REALTIME_TIMESTAMP`), used only when the entry carries no
/// `_SOURCE_REALTIME_TIMESTAMP`.
pub fn map_record(
    record: &JournalRecord,
    cfg: &JournaldConfig,
    recv_usec: Option<u64>,
) -> SyslogMessage {
    let get = |k: &str| record.get(k).map(String::as_str);

    let message = get("MESSAGE").unwrap_or_default().to_string();

    // PRIORITY is the syslog severity 0..7; default to Notice if absent/odd.
    let severity = get("PRIORITY")
        .and_then(|p| p.trim().parse::<u8>().ok())
        .and_then(Severity::from_code)
        .unwrap_or(Severity::Notice);

    // SYSLOG_FACILITY when present; otherwise infer kernel vs user from transport.
    let facility = get("SYSLOG_FACILITY")
        .and_then(|f| f.trim().parse::<u8>().ok())
        .and_then(Facility::from_code)
        .unwrap_or_else(|| {
            if get("_TRANSPORT") == Some("kernel") {
                Facility::Kern
            } else {
                Facility::User
            }
        });

    let app_name = get("SYSLOG_IDENTIFIER")
        .or_else(|| get("_COMM"))
        .map(String::from);
    let hostname = get("_HOSTNAME").map(String::from);
    let proc_id = get("_PID").or_else(|| get("SYSLOG_PID")).map(String::from);
    let msg_id = get("MESSAGE_ID").map(String::from);

    // Prefer the application-supplied source time; fall back to journal receive
    // time, then to now(). All journald timestamps are microseconds.
    let ts_usec = get("_SOURCE_REALTIME_TIMESTAMP")
        .and_then(|s| s.trim().parse::<u64>().ok())
        .or(recv_usec);
    let timestamp = ts_usec
        .and_then(datetime_from_usec)
        .or_else(|| Some(Utc::now()));

    // Collect the rich structured fields under the "journald" SD-element; the
    // existing telemetry mapper flattens these to `sd.journald.<label>` labels.
    let mut fields: HashMap<String, String> = HashMap::new();
    for (key, label) in STANDARD_FIELDS {
        if let Some(v) = record.get(*key) {
            fields.insert((*label).to_string(), v.clone());
        }
    }
    if cfg.include_dev_fields {
        for (key, label) in DEV_FIELDS {
            if let Some(v) = record.get(*key) {
                fields.insert((*label).to_string(), v.clone());
            }
        }
    }
    for key in &cfg.extra_fields {
        if let Some(v) = record.get(key) {
            // Preserve the raw field name for operator-specified extras.
            fields.entry(key.clone()).or_insert_with(|| v.clone());
        }
    }

    let mut structured_data = HashMap::new();
    if !fields.is_empty() {
        structured_data.insert("journald".to_string(), fields);
    }

    SyslogMessage {
        facility,
        severity,
        timestamp,
        hostname,
        app_name,
        proc_id,
        msg_id,
        structured_data,
        message,
        raw: String::new(),
        version: SyslogVersion::Rfc5424,
    }
}

/// Convert microseconds-since-epoch to a UTC datetime.
fn datetime_from_usec(usec: u64) -> Option<chrono::DateTime<Utc>> {
    let secs = (usec / 1_000_000) as i64;
    let nanos = ((usec % 1_000_000) * 1_000) as u32;
    Utc.timestamp_opt(secs, nanos).single()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_unlimited_when_no_max() {
        let cfg = JournaldConfig::default(); // max_eps = None
        let now = Instant::now();
        let mut rl = RateLimiter::new(&cfg, now);
        for _ in 0..10_000 {
            assert!(rl.allow(now));
        }
    }

    #[test]
    fn rate_limiter_samples_over_budget() {
        let cfg = JournaldConfig {
            max_eps: Some(2),
            sample_ratio: 5,
            ..Default::default()
        };
        let now = Instant::now();
        let mut rl = RateLimiter::new(&cfg, now);
        // First 2 in the window are within budget.
        assert!(rl.allow(now));
        assert!(rl.allow(now));
        // The next over-budget entries: keep 1-in-5 (the 5th over-budget).
        let kept: usize = (0..10).filter(|_| rl.allow(now)).count();
        assert_eq!(kept, 2, "1-in-5 of 10 over-budget entries kept");
        // A fresh 1s window resets the budget.
        let later = now + Duration::from_secs(1);
        assert!(rl.allow(later));
    }

    #[test]
    fn send_entry_drop_newest_counts_when_full() {
        let (tx, _rx) = mpsc::channel::<ReceivedMessage>(1);
        let stats = JournaldStats::default();
        let msg = |body: &str| ReceivedMessage {
            message: crate::parser::parse(&format!("<14>{body}")).unwrap(),
            source: MessageSource::Journald,
            resolved_hostname: "h".into(),
        };
        // First send fills the capacity-1 channel.
        assert!(matches!(
            send_entry(&tx, msg("a"), OverflowPolicy::DropNewest, &stats),
            SendOutcome::Sent
        ));
        // Second send finds it full → dropped + counted.
        assert!(matches!(
            send_entry(&tx, msg("b"), OverflowPolicy::DropNewest, &stats),
            SendOutcome::Dropped
        ));
        assert_eq!(
            stats.published.load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert_eq!(stats.dropped.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn send_entry_reports_closed_channel() {
        let (tx, rx) = mpsc::channel::<ReceivedMessage>(1);
        drop(rx);
        let stats = JournaldStats::default();
        let msg = ReceivedMessage {
            message: crate::parser::parse("<14>x").unwrap(),
            source: MessageSource::Journald,
            resolved_hostname: "h".into(),
        };
        assert!(matches!(
            send_entry(&tx, msg, OverflowPolicy::DropNewest, &stats),
            SendOutcome::Closed
        ));
    }

    fn rec(pairs: &[(&str, &str)]) -> JournalRecord {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn maps_priority_to_severity() {
        let r = rec(&[("MESSAGE", "boom"), ("PRIORITY", "3")]);
        let m = map_record(&r, &JournaldConfig::default(), None);
        assert_eq!(m.severity, Severity::Error);
        assert_eq!(m.message, "boom");
    }

    #[test]
    fn defaults_severity_when_absent() {
        let m = map_record(&rec(&[("MESSAGE", "hi")]), &JournaldConfig::default(), None);
        assert_eq!(m.severity, Severity::Notice);
    }

    #[test]
    fn maps_syslog_facility() {
        // SYSLOG_FACILITY 4 == auth
        let r = rec(&[("MESSAGE", "x"), ("SYSLOG_FACILITY", "4")]);
        let m = map_record(&r, &JournaldConfig::default(), None);
        assert_eq!(m.facility, Facility::Auth);
    }

    #[test]
    fn infers_kernel_facility_from_transport() {
        let r = rec(&[("MESSAGE", "x"), ("_TRANSPORT", "kernel")]);
        let m = map_record(&r, &JournaldConfig::default(), None);
        assert_eq!(m.facility, Facility::Kern);
    }

    #[test]
    fn infers_user_facility_by_default() {
        let r = rec(&[("MESSAGE", "x"), ("_TRANSPORT", "journal")]);
        let m = map_record(&r, &JournaldConfig::default(), None);
        assert_eq!(m.facility, Facility::User);
    }

    #[test]
    fn app_name_prefers_syslog_identifier_then_comm() {
        let r = rec(&[
            ("MESSAGE", "x"),
            ("SYSLOG_IDENTIFIER", "sshd"),
            ("_COMM", "sshd-session"),
        ]);
        assert_eq!(
            map_record(&r, &JournaldConfig::default(), None)
                .app_name
                .as_deref(),
            Some("sshd")
        );
        let r2 = rec(&[("MESSAGE", "x"), ("_COMM", "bash")]);
        assert_eq!(
            map_record(&r2, &JournaldConfig::default(), None)
                .app_name
                .as_deref(),
            Some("bash")
        );
    }

    #[test]
    fn standard_fields_become_structured_data() {
        let r = rec(&[
            ("MESSAGE", "x"),
            ("_SYSTEMD_UNIT", "nginx.service"),
            ("_PID", "4242"),
            ("_BOOT_ID", "abc"),
            ("MESSAGE_ID", "fc2e22bc6ee647b6b90729ab34a250b1"),
        ]);
        let m = map_record(&r, &JournaldConfig::default(), None);
        let jd = m.structured_data.get("journald").unwrap();
        assert_eq!(jd.get("unit").map(String::as_str), Some("nginx.service"));
        assert_eq!(jd.get("boot_id").map(String::as_str), Some("abc"));
        assert_eq!(m.proc_id.as_deref(), Some("4242"));
        assert_eq!(
            m.msg_id.as_deref(),
            Some("fc2e22bc6ee647b6b90729ab34a250b1")
        );
    }

    #[test]
    fn dev_fields_gated_by_config() {
        let r = rec(&[("MESSAGE", "x"), ("CODE_FILE", "main.rs"), ("ERRNO", "2")]);
        let off = map_record(&r, &JournaldConfig::default(), None);
        assert!(
            off.structured_data
                .get("journald")
                .is_none_or(|j| !j.contains_key("code_file"))
        );

        let cfg = JournaldConfig {
            include_dev_fields: true,
            ..Default::default()
        };
        let on = map_record(&r, &cfg, None);
        let jd = on.structured_data.get("journald").unwrap();
        assert_eq!(jd.get("code_file").map(String::as_str), Some("main.rs"));
        assert_eq!(jd.get("errno").map(String::as_str), Some("2"));
    }

    #[test]
    fn extra_fields_allowlist_copied_verbatim() {
        let r = rec(&[("MESSAGE", "x"), ("_SELINUX_CONTEXT", "system_u")]);
        let cfg = JournaldConfig {
            extra_fields: vec!["_SELINUX_CONTEXT".to_string()],
            ..Default::default()
        };
        let m = map_record(&r, &cfg, None);
        let jd = m.structured_data.get("journald").unwrap();
        assert_eq!(
            jd.get("_SELINUX_CONTEXT").map(String::as_str),
            Some("system_u")
        );
    }

    #[test]
    fn prefers_source_realtime_timestamp() {
        // 2021-01-01T00:00:00Z in microseconds.
        let usec = 1_609_459_200_000_000u64;
        let r = rec(&[
            ("MESSAGE", "x"),
            ("_SOURCE_REALTIME_TIMESTAMP", &usec.to_string()),
        ]);
        let m = map_record(&r, &JournaldConfig::default(), Some(999));
        assert_eq!(m.timestamp.unwrap().timestamp(), 1_609_459_200);
    }

    #[test]
    fn parse_duration_units() {
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("15m"), Some(Duration::from_secs(900)));
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("1d"), Some(Duration::from_secs(86400)));
        assert_eq!(parse_duration(" 5 min "), Some(Duration::from_secs(300)));
        assert_eq!(parse_duration("10"), None); // no unit
        assert_eq!(parse_duration("abc"), None);
        assert_eq!(parse_duration("5y"), None); // unsupported unit
    }

    #[test]
    fn cursor_file_round_trip() {
        let dir = std::env::temp_dir().join(format!("zs-jd-cursor-{}", std::process::id()));
        let path = dir.join("nested/journald.cursor");
        // Parent dirs are created on write.
        write_cursor_atomic(&path, "s=abc123;i=1").unwrap();
        assert_eq!(read_cursor_file(&path).as_deref(), Some("s=abc123;i=1"));
        // Overwrite is atomic and replaces the previous value.
        write_cursor_atomic(&path, "s=def456;i=2").unwrap();
        assert_eq!(read_cursor_file(&path).as_deref(), Some("s=def456;i=2"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_cursor_file_treats_empty_as_absent() {
        let path = std::env::temp_dir().join(format!("zs-jd-empty-{}.cursor", std::process::id()));
        std::fs::write(&path, "   \n").unwrap();
        assert_eq!(read_cursor_file(&path), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn build_matches_empty_by_default() {
        assert!(build_matches(&JournaldConfig::default()).is_empty());
    }

    #[test]
    fn build_matches_units_and_transports() {
        let cfg = JournaldConfig {
            units: vec!["sshd.service".into(), "nginx.service".into()],
            transports: vec!["kernel".into()],
            ..Default::default()
        };
        let m = build_matches(&cfg);
        assert!(m.contains(&("_SYSTEMD_UNIT".into(), "sshd.service".into())));
        assert!(m.contains(&("_SYSTEMD_UNIT".into(), "nginx.service".into())));
        assert!(m.contains(&("_TRANSPORT".into(), "kernel".into())));
    }

    #[test]
    fn build_matches_min_priority_expands_to_or_group() {
        let cfg = JournaldConfig {
            min_priority: Some(3),
            ..Default::default()
        };
        let m = build_matches(&cfg);
        let prios: Vec<&str> = m
            .iter()
            .filter(|(f, _)| f == "PRIORITY")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(prios, ["0", "1", "2", "3"]); // 0..=min
    }

    #[test]
    fn build_matches_min_priority_clamped_to_7() {
        let cfg = JournaldConfig {
            min_priority: Some(50),
            ..Default::default()
        };
        let prio_count = build_matches(&cfg)
            .iter()
            .filter(|(f, _)| f == "PRIORITY")
            .count();
        assert_eq!(prio_count, 8); // 0..=7
    }

    #[test]
    fn build_matches_raw_fields() {
        let mut match_fields = std::collections::HashMap::new();
        match_fields.insert("_SYSTEMD_UNIT".to_string(), "cron.service".to_string());
        let cfg = JournaldConfig {
            match_fields,
            ..Default::default()
        };
        let m = build_matches(&cfg);
        assert!(m.contains(&("_SYSTEMD_UNIT".into(), "cron.service".into())));
    }
}
