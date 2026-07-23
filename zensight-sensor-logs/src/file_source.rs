//! File-tailing source (#549): read `/var/log/*.log`-style files into the same
//! intake pipeline as the network + journald sources, so filtering, templating,
//! rollups, and sentinel rules apply uniformly.
//!
//! Runs on a dedicated OS thread with blocking `std::fs` I/O (like the journald
//! reader), forwarding `ReceivedMessage`s into the shared intake channel — so a
//! slow or large file never blocks the async runtime.
//!
//! Tailing is **rotation-aware**: each tracked file keeps its open handle, so a
//! logrotate rename+recreate keeps draining the rotated-away inode to EOF before
//! switching to the new file (no lost lines); a copytruncate (same inode, size
//! shrank) resets to offset 0. Offsets are persisted atomically (same scheme as
//! the journald cursor) so a restart resumes without re-ingesting or skipping.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::config::{FileFormat, FileSourceConfig, FileTailingConfig, OverflowPolicy};
use crate::ingest::{IngestStats, SharedRateLimiter};
use crate::multiline::MultilineJoiner;
use crate::parser::{Facility, Severity, SyslogMessage, SyslogVersion, TsSource};
use crate::receiver::{MessageSource, ReceivedMessage};

/// Map a syslog severity slug to a [`Severity`]; unknown → `Informational`.
fn severity_from_slug(slug: &str) -> Severity {
    match slug.trim().to_ascii_lowercase().as_str() {
        "emerg" | "emergency" => Severity::Emergency,
        "alert" => Severity::Alert,
        "crit" | "critical" => Severity::Critical,
        "err" | "error" => Severity::Error,
        "warn" | "warning" => Severity::Warning,
        "notice" => Severity::Notice,
        "debug" => Severity::Debug,
        _ => Severity::Informational,
    }
}

/// Map a level word found in a line (`ERROR`, `WARN`, `FATAL`, …) to a severity.
fn severity_from_word(word: &str) -> Option<Severity> {
    match word.trim().to_ascii_uppercase().as_str() {
        "EMERG" | "EMERGENCY" | "PANIC" => Some(Severity::Emergency),
        "ALERT" => Some(Severity::Alert),
        "FATAL" | "CRIT" | "CRITICAL" => Some(Severity::Critical),
        "ERROR" | "ERR" => Some(Severity::Error),
        "WARN" | "WARNING" => Some(Severity::Warning),
        "NOTICE" => Some(Severity::Notice),
        "INFO" | "INFORMATION" => Some(Severity::Informational),
        "DEBUG" | "TRACE" => Some(Severity::Debug),
        _ => None,
    }
}

/// A source config with its severity regex compiled once.
pub struct CompiledSource {
    cfg: FileSourceConfig,
    sev_regex: Option<Regex>,
    default_sev: Severity,
}

impl CompiledSource {
    /// Compile a source; a bad severity regex is dropped with a warning.
    pub fn new(cfg: FileSourceConfig) -> Self {
        let default_sev = cfg
            .severity
            .as_deref()
            .map(severity_from_slug)
            .unwrap_or(Severity::Informational);
        let sev_regex = cfg
            .severity_regex
            .as_deref()
            .and_then(|p| match Regex::new(p) {
                Ok(re) => Some(re),
                Err(e) => {
                    tracing::warn!(error = %e, "file source: bad severity_regex, ignoring");
                    None
                }
            });
        Self {
            cfg,
            sev_regex,
            default_sev,
        }
    }

    /// Severity for a line: regex-extracted word if it matches, else the default.
    fn severity_for(&self, line: &str) -> Severity {
        if let Some(re) = &self.sev_regex
            && let Some(caps) = re.captures(line)
        {
            let word = caps
                .name("severity")
                .or_else(|| caps.get(1))
                .map(|m| m.as_str());
            if let Some(w) = word
                && let Some(sev) = severity_from_word(w)
            {
                return sev;
            }
        }
        self.default_sev
    }
}

/// Build a [`ReceivedMessage`] from one (already multiline-joined) file line.
///
/// Pure — no I/O — so line interpretation is unit-testable. `Plain` synthesizes
/// a message (config severity/app/unit + static labels); `Syslog` runs the
/// parser and falls back to plain on a parse miss.
pub fn build_message(
    src: &CompiledSource,
    path: &Path,
    host: &str,
    line: String,
) -> ReceivedMessage {
    let message = match src.cfg.format {
        FileFormat::Syslog => {
            if let Some(mut parsed) = crate::parser::parse(&line) {
                enrich(&mut parsed, src, path);
                parsed
            } else {
                plain_message(src, path, line)
            }
        }
        FileFormat::Plain => plain_message(src, path, line),
    };
    ReceivedMessage {
        message,
        source: MessageSource::File,
        resolved_hostname: host.to_string(),
    }
}

/// Synthesize a [`SyslogMessage`] for a plain (non-syslog) line.
fn plain_message(src: &CompiledSource, path: &Path, line: String) -> SyslogMessage {
    let severity = src.severity_for(&line);
    let mut msg = SyslogMessage {
        facility: Facility::User,
        severity,
        timestamp: None, // no embedded time → receiver stamps it
        hostname: None,
        app_name: src.cfg.app.clone(),
        proc_id: None,
        msg_id: None,
        structured_data: HashMap::new(),
        message: line,
        raw: String::new(),
        version: SyslogVersion::Rfc3164,
        ts_source: TsSource::Sender,
    };
    enrich(&mut msg, src, path);
    msg
}

/// Attach unit / static labels / source path to a message's structured data so
/// they flow as labels (`unit` like journald; the rest as `sd.file.*`).
fn enrich(msg: &mut SyslogMessage, src: &CompiledSource, path: &Path) {
    if src.cfg.app.is_some() && msg.app_name.is_none() {
        msg.app_name = src.cfg.app.clone();
    }
    if let Some(unit) = &src.cfg.unit {
        msg.structured_data
            .entry("journald".to_string())
            .or_default()
            .insert("unit".to_string(), unit.clone());
    }
    let file = msg.structured_data.entry("file".to_string()).or_default();
    file.insert("path".to_string(), path.to_string_lossy().into_owned());
    for (k, v) in &src.cfg.labels {
        file.insert(k.clone(), v.clone());
    }
}

/// Persisted tail position for one path: identity (`dev`/`inode`) + byte offset.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct Offset {
    dev: u64,
    inode: u64,
    offset: u64,
}

/// One tracked file: its open handle, identity, byte offset, and joiner.
struct TailedFile {
    source_idx: usize,
    file: File,
    dev: u64,
    inode: u64,
    offset: u64,
    pending: Vec<u8>,
    joiner: Option<MultilineJoiner>,
}

/// The file tailer: tracks a set of glob-matched files and forwards their new
/// lines into the intake channel.
pub struct FileTailer {
    cfg: FileTailingConfig,
    sources: Vec<CompiledSource>,
    host: String,
    tx: mpsc::Sender<ReceivedMessage>,
    stats: Arc<IngestStats>,
    limiter: Arc<SharedRateLimiter>,
    overflow: OverflowPolicy,
    offsets_path: Option<PathBuf>,
    files: HashMap<PathBuf, TailedFile>,
}

impl FileTailer {
    pub fn new(
        cfg: FileTailingConfig,
        host: String,
        tx: mpsc::Sender<ReceivedMessage>,
        stats: Arc<IngestStats>,
        limiter: Arc<SharedRateLimiter>,
        overflow: OverflowPolicy,
    ) -> Self {
        let sources = cfg
            .sources
            .iter()
            .cloned()
            .map(CompiledSource::new)
            .collect();
        let offsets_path = resolve_offsets_path(cfg.offsets_path.as_deref());
        Self {
            cfg,
            sources,
            host,
            tx,
            stats,
            limiter,
            overflow,
            offsets_path,
            files: HashMap::new(),
        }
    }

    /// Spawn the tailer on a dedicated OS thread (blocking I/O off the runtime).
    /// The thread stops when the intake channel closes.
    pub fn spawn(self) -> std::thread::JoinHandle<()> {
        std::thread::Builder::new()
            .name("logs-file-tailer".into())
            .spawn(move || self.run_blocking())
            .expect("spawn file tailer thread")
    }

    fn run_blocking(mut self) {
        let persisted = load_offsets(self.offsets_path.as_deref());
        self.rescan(&persisted);

        let poll = Duration::from_millis(self.cfg.poll_ms.max(1));
        let rescan_every = Duration::from_secs(self.cfg.rescan_secs.max(1));
        let mut last_rescan = Instant::now();
        let mut last_persist = Instant::now();
        loop {
            if self.tx.is_closed() {
                break;
            }
            if self.poll_once() {
                // Channel closed mid-poll.
                break;
            }
            let now = Instant::now();
            if now.duration_since(last_rescan) >= rescan_every {
                self.rescan(&HashMap::new());
                last_rescan = now;
            }
            if now.duration_since(last_persist) >= Duration::from_secs(5) {
                self.persist();
                last_persist = now;
            }
            std::thread::sleep(poll);
        }
        self.persist();
    }

    /// Expand every source's globs and begin tracking new matches, resuming from
    /// `resume` (persisted offsets) when the file identity still matches.
    fn rescan(&mut self, resume: &HashMap<PathBuf, Offset>) {
        for (idx, src) in self.sources.iter().enumerate() {
            for pattern in &src.cfg.paths {
                let entries = match glob::glob(pattern) {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!(pattern, error = %e, "file source: bad glob");
                        continue;
                    }
                };
                for path in entries.flatten() {
                    if self.files.contains_key(&path) || !path.is_file() {
                        continue;
                    }
                    if let Some(tailed) = open_file(&path, idx, src, resume.get(&path)) {
                        tracing::info!(path = %path.display(), "file source: tracking");
                        self.files.insert(path, tailed);
                    }
                }
            }
        }
    }

    /// Read new bytes from every tracked file and forward complete lines.
    /// Returns true if the intake channel closed (caller should stop).
    fn poll_once(&mut self) -> bool {
        let paths: Vec<PathBuf> = self.files.keys().cloned().collect();
        for path in paths {
            if self.poll_file(&path) {
                return true;
            }
        }
        false
    }

    /// Poll one file: drain its handle, handle rotation/truncation. Returns true
    /// if the channel closed.
    fn poll_file(&mut self, path: &Path) -> bool {
        // Drain the currently-open handle to EOF.
        if self.read_available(path) {
            return true;
        }

        // Rotation / truncation check against the path's current identity.
        let (cur_dev, cur_inode, cur_size) = match std::fs::metadata(path) {
            Ok(m) => (m.dev(), m.ino(), m.len()),
            Err(_) => {
                // Path gone (deleted / renamed away). We've drained the handle;
                // drop it — a rescan re-adds a recreated path.
                if let Some(f) = self.files.remove(path) {
                    self.flush_joiner(path, f.source_idx, f.joiner);
                }
                return false;
            }
        };

        let Some(f) = self.files.get_mut(path) else {
            return false;
        };
        if cur_dev != f.dev || cur_inode != f.inode {
            // Rotate + recreate: the handle drained the old inode above; flush
            // its joiner, then reopen the new file from the start.
            let idx = f.source_idx;
            let joiner = f.joiner.take();
            self.flush_joiner(path, idx, joiner);
            self.files.remove(path);
            let src = &self.sources[idx];
            if let Some(tailed) = open_file(path, idx, src, None) {
                self.files.insert(path.to_path_buf(), tailed);
                return self.read_available(path);
            }
        } else if cur_size < f.offset {
            // Truncated in place (copytruncate): restart at 0.
            if f.file.seek(SeekFrom::Start(0)).is_ok() {
                f.offset = 0;
                return self.read_available(path);
            }
        }
        false
    }

    /// Read from the tracked file's handle at its offset to EOF, forwarding
    /// complete lines. Returns true if the channel closed.
    fn read_available(&mut self, path: &Path) -> bool {
        let max_line = self.cfg.max_line_bytes.max(1);
        let mut lines: Vec<String> = Vec::new();
        {
            let Some(f) = self.files.get_mut(path) else {
                return false;
            };
            let mut chunk = [0u8; 16384];
            loop {
                match f.file.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        f.offset += n as u64;
                        f.pending.extend_from_slice(&chunk[..n]);
                        // Bound the partial-line buffer.
                        if f.pending.len() > max_line * 2 {
                            f.pending.drain(..f.pending.len() - max_line);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            // Split complete lines (keep the trailing partial in `pending`).
            while let Some(nl) = f.pending.iter().position(|&b| b == b'\n') {
                let raw: Vec<u8> = f.pending.drain(..=nl).collect();
                let mut text = String::from_utf8_lossy(&raw[..raw.len() - 1]).into_owned();
                if text.ends_with('\r') {
                    text.pop();
                }
                if text.len() > max_line {
                    text.truncate(max_line);
                }
                lines.push(text);
            }
        }

        for text in lines {
            // Multiline join happens per file; a joined record is forwarded when
            // the next real line arrives (handled inside the joiner).
            let (idx, emit) = {
                let f = self.files.get_mut(path).expect("present");
                let emit = match &mut f.joiner {
                    Some(j) => j.push(text),
                    None => Some(text),
                };
                (f.source_idx, emit)
            };
            if let Some(line) = emit
                && self.forward(path, idx, line)
            {
                return true;
            }
        }
        false
    }

    /// Flush a file's joiner (emit any buffered final line) on rotation/removal.
    fn flush_joiner(&mut self, path: &Path, idx: usize, joiner: Option<MultilineJoiner>) {
        if let Some(mut j) = joiner
            && let Some(line) = j.flush()
        {
            self.forward(path, idx, line);
        }
    }

    /// Build + forward one line per the overflow policy, updating stats. Returns
    /// true if the channel closed.
    fn forward(&self, path: &Path, idx: usize, line: String) -> bool {
        IngestStats::inc(&self.stats.received);
        if !self.limiter.allow(Instant::now()) {
            IngestStats::inc(&self.stats.dropped);
            return false;
        }
        let received = build_message(&self.sources[idx], path, &self.host, line);
        IngestStats::inc(&self.stats.parsed);
        match self.overflow {
            OverflowPolicy::Block => self.tx.blocking_send(received).is_err(),
            OverflowPolicy::DropNewest => match self.tx.try_send(received) {
                Ok(()) => false,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    IngestStats::inc(&self.stats.dropped);
                    false
                }
                Err(mpsc::error::TrySendError::Closed(_)) => true,
            },
        }
    }

    /// Persist current offsets atomically for restart resumption.
    fn persist(&self) {
        let Some(path) = &self.offsets_path else {
            return;
        };
        let map: HashMap<String, Offset> = self
            .files
            .iter()
            .map(|(p, f)| {
                (
                    p.to_string_lossy().into_owned(),
                    Offset {
                        dev: f.dev,
                        inode: f.inode,
                        offset: f.offset,
                    },
                )
            })
            .collect();
        if let Ok(json) = serde_json::to_vec(&map) {
            let _ = write_atomic(path, &json);
        }
    }
}

/// Open a file for tailing, resuming from `resume` when its identity matches.
fn open_file(
    path: &Path,
    source_idx: usize,
    src: &CompiledSource,
    resume: Option<&Offset>,
) -> Option<TailedFile> {
    let mut file = File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    let (dev, inode, size) = (meta.dev(), meta.ino(), meta.len());
    // Resume only when the file identity is unchanged and hasn't shrunk.
    let offset = match resume {
        Some(o) if o.dev == dev && o.inode == inode && o.offset <= size => o.offset,
        _ => 0,
    };
    file.seek(SeekFrom::Start(offset)).ok()?;
    Some(TailedFile {
        source_idx,
        file,
        dev,
        inode,
        offset,
        pending: Vec::new(),
        joiner: src
            .cfg
            .multiline
            .then(|| MultilineJoiner::new(&crate::config::MultilineConfig::default())),
    })
}

/// Resolve the offsets state-file path (same scheme as the journald cursor).
fn resolve_offsets_path(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    if let Ok(state) = std::env::var("STATE_DIRECTORY") {
        let first = state.split(':').next().unwrap_or(&state);
        if !first.is_empty() {
            return Some(Path::new(first).join("file-offsets.json"));
        }
    }
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        return Some(Path::new(&xdg).join("zensight/file-offsets.json"));
    }
    if let Ok(home) = std::env::var("HOME") {
        return Some(Path::new(&home).join(".local/state/zensight/file-offsets.json"));
    }
    None
}

fn load_offsets(path: Option<&Path>) -> HashMap<PathBuf, Offset> {
    let Some(path) = path else {
        return HashMap::new();
    };
    let Ok(bytes) = std::fs::read(path) else {
        return HashMap::new();
    };
    serde_json::from_slice::<HashMap<String, Offset>>(&bytes)
        .map(|m| m.into_iter().map(|(k, v)| (PathBuf::from(k), v)).collect())
        .unwrap_or_default()
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn src(cfg: FileSourceConfig) -> CompiledSource {
        CompiledSource::new(cfg)
    }

    #[test]
    fn plain_line_uses_config_severity_and_labels() {
        let mut labels = HashMap::new();
        labels.insert("env".to_string(), "prod".to_string());
        let s = src(FileSourceConfig {
            paths: vec![],
            labels,
            unit: Some("myapp.service".into()),
            app: Some("myapp".into()),
            format: FileFormat::Plain,
            severity: Some("warning".into()),
            severity_regex: None,
            multiline: true,
        });
        let rm = build_message(&s, Path::new("/var/log/x.log"), "host1", "boom".into());
        assert_eq!(rm.message.severity, Severity::Warning);
        assert_eq!(rm.message.app_name.as_deref(), Some("myapp"));
        assert_eq!(rm.message.message, "boom");
        assert_eq!(rm.resolved_hostname, "host1");
        assert_eq!(rm.source.to_string(), "file");
        let jd = rm.message.structured_data.get("journald").unwrap();
        assert_eq!(jd.get("unit").map(String::as_str), Some("myapp.service"));
        let file = rm.message.structured_data.get("file").unwrap();
        assert_eq!(file.get("env").map(String::as_str), Some("prod"));
        assert_eq!(file.get("path").map(String::as_str), Some("/var/log/x.log"));
    }

    #[test]
    fn severity_regex_extracts_level() {
        let s = src(FileSourceConfig {
            paths: vec![],
            severity_regex: Some(r"^\[(\w+)\]".into()),
            ..Default::default()
        });
        let rm = build_message(&s, Path::new("/l"), "h", "[ERROR] bad thing".into());
        assert_eq!(rm.message.severity, Severity::Error);
        // A line with no level word falls back to the default (info).
        let rm2 = build_message(&s, Path::new("/l"), "h", "no level here".into());
        assert_eq!(rm2.message.severity, Severity::Informational);
    }

    #[test]
    fn syslog_format_parses_pri() {
        let s = src(FileSourceConfig {
            paths: vec![],
            format: FileFormat::Syslog,
            ..Default::default()
        });
        let rm = build_message(
            &s,
            Path::new("/l"),
            "h",
            "<34>Oct 11 22:14:15 mymachine su: failure".into(),
        );
        assert_eq!(rm.message.severity, Severity::Critical);
        assert_eq!(rm.message.hostname.as_deref(), Some("mymachine"));
    }

    /// A tailer over a temp file picks up appended lines, then survives a
    /// rotate+recreate without losing or re-reading lines (#549 acceptance).
    #[test]
    fn tail_reads_appends_and_survives_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        std::fs::write(&path, b"line1\nline2\n").unwrap();

        let (tx, mut rx) = mpsc::channel::<ReceivedMessage>(1000);
        let cfg = FileTailingConfig {
            sources: vec![FileSourceConfig {
                paths: vec![path.to_string_lossy().into_owned()],
                multiline: false, // no join: assert line-for-line
                ..Default::default()
            }],
            offsets_path: Some(dir.path().join("offsets.json")),
            ..Default::default()
        };
        let mut tailer = FileTailer::new(
            cfg,
            "h".into(),
            tx,
            Arc::new(IngestStats::default()),
            Arc::new(SharedRateLimiter::new(None, 1, Instant::now())),
            OverflowPolicy::DropNewest,
        );
        tailer.rescan(&HashMap::new());
        tailer.poll_once();

        let drain = |rx: &mut mpsc::Receiver<ReceivedMessage>| -> Vec<String> {
            let mut out = Vec::new();
            while let Ok(m) = rx.try_recv() {
                out.push(m.message.message);
            }
            out
        };
        assert_eq!(drain(&mut rx), vec!["line1", "line2"]);

        // Append more, poll again → only the new lines.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(b"line3\n").unwrap();
        }
        tailer.poll_once();
        assert_eq!(drain(&mut rx), vec!["line3"]);

        // Rotate: rename away (handle keeps draining it), create a fresh file.
        std::fs::rename(&path, dir.path().join("app.log.1")).unwrap();
        std::fs::write(&path, b"line4\n").unwrap();
        // First poll drains the old handle (nothing new there) and detects the
        // inode change, reopening the new file; second poll reads it.
        tailer.poll_once();
        tailer.poll_once();
        assert_eq!(drain(&mut rx), vec!["line4"], "no lost/duplicated lines");
    }

    /// After a restart, tailing resumes from the persisted offset (no re-ingest).
    #[test]
    fn restart_resumes_from_persisted_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        let offsets = dir.path().join("offsets.json");
        std::fs::write(&path, b"a\nb\n").unwrap();

        let make = || -> FileTailingConfig {
            FileTailingConfig {
                sources: vec![FileSourceConfig {
                    paths: vec![path.to_string_lossy().into_owned()],
                    multiline: false,
                    ..Default::default()
                }],
                offsets_path: Some(offsets.clone()),
                ..Default::default()
            }
        };
        // First run: read a,b; persist.
        {
            let (tx, mut rx) = mpsc::channel::<ReceivedMessage>(100);
            let mut t = FileTailer::new(
                make(),
                "h".into(),
                tx,
                Arc::new(IngestStats::default()),
                Arc::new(SharedRateLimiter::new(None, 1, Instant::now())),
                OverflowPolicy::DropNewest,
            );
            t.rescan(&HashMap::new());
            t.poll_once();
            let mut n = 0;
            while rx.try_recv().is_ok() {
                n += 1;
            }
            assert_eq!(n, 2);
            t.persist();
        }
        // Append after "restart", then a fresh tailer resuming from offsets.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(b"c\n").unwrap();
        }
        {
            let (tx, mut rx) = mpsc::channel::<ReceivedMessage>(100);
            let mut t = FileTailer::new(
                make(),
                "h".into(),
                tx,
                Arc::new(IngestStats::default()),
                Arc::new(SharedRateLimiter::new(None, 1, Instant::now())),
                OverflowPolicy::DropNewest,
            );
            let resume = load_offsets(Some(&offsets));
            t.rescan(&resume);
            t.poll_once();
            let mut got = Vec::new();
            while let Ok(m) = rx.try_recv() {
                got.push(m.message.message);
            }
            assert_eq!(got, vec!["c"], "resumes past a,b — no re-ingest");
        }
    }
}
