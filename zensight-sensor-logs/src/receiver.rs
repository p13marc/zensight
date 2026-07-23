//! Syslog message receivers (UDP, TCP, and Unix socket).

use crate::config::{
    Framing, ListenerConfig, ListenerProtocol, MultilineConfig, OverflowPolicy, SyslogConfig,
};
use crate::ingest::{FrameReader, IngestStats, SharedRateLimiter, forward_parsed};
use crate::multiline::MultilineJoiner;
use crate::parser::{self, RfcTimeCtx, SyslogMessage};
use anyhow::{Context, Result};
use chrono::Utc;
use chrono_tz::Tz;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tokio::net::{TcpListener, UdpSocket, UnixListener};
use tokio::sync::mpsc;
use tokio::time::Duration;
use zensight_common::telemetry::{Protocol, TelemetryPoint, TelemetryValue};

/// Received syslog message with source information.
#[derive(Debug)]
pub struct ReceivedMessage {
    /// Parsed syslog message.
    pub message: SyslogMessage,
    /// Source address (for network protocols) or path (for Unix socket).
    pub source: MessageSource,
    /// Resolved hostname (from aliases or reverse DNS).
    pub resolved_hostname: String,
}

/// Source of a received message.
#[derive(Debug, Clone)]
pub enum MessageSource {
    /// Network source (UDP or TCP).
    Network(SocketAddr),
    /// Unix socket source.
    Unix,
    /// systemd-journald (local journal, read via libsystemd).
    /// Only constructed by the feature-gated journald reader.
    #[cfg_attr(not(feature = "journald"), allow(dead_code))]
    Journald,
    /// A tailed log file (#549). The originating path rides a label, not the
    /// enum, so this stays a plain tag.
    File,
}

impl std::fmt::Display for MessageSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MessageSource::Network(addr) => write!(f, "{}", addr),
            MessageSource::Unix => write!(f, "unix"),
            MessageSource::Journald => write!(f, "journald"),
            // `source_type=file` per #549; the path rides a separate label.
            MessageSource::File => write!(f, "file"),
        }
    }
}

/// journald reader throughput + loss accounting (#62), shared between the reader
/// thread and the async health/telemetry tasks. Plain relaxed atomics —
/// monotonic counters read for periodic snapshots, no cross-counter invariant.
/// Lives here (not in the feature-gated `journald` module) so the channel/health
/// wiring can name it unconditionally.
#[derive(Debug, Default)]
pub struct JournaldStats {
    /// Entries read from the journal (post server-side match).
    pub read: AtomicU64,
    /// Entries handed to the telemetry channel.
    pub published: AtomicU64,
    /// Entries dropped because the channel was full (`drop_newest`).
    pub dropped: AtomicU64,
    /// Entries shed by the rate limiter (over `max_eps`).
    pub sampled_out: AtomicU64,
    /// Entries we failed to read/decode (tolerated, not fatal).
    pub decode_errors: AtomicU64,
    /// `wait()` invalidations (journal rotation / files added-removed).
    pub invalidations: AtomicU64,
}

impl JournaldStats {
    /// Increment a counter (relaxed). Used by the feature-gated reader module.
    #[cfg_attr(not(feature = "journald"), allow(dead_code))]
    pub(crate) fn inc(field: &AtomicU64) {
        field.fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot the counters for a point-in-time read (health / telemetry).
    pub fn snapshot(&self) -> JournaldStatsSnapshot {
        JournaldStatsSnapshot {
            read: self.read.load(Ordering::Relaxed),
            published: self.published.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            sampled_out: self.sampled_out.load(Ordering::Relaxed),
            decode_errors: self.decode_errors.load(Ordering::Relaxed),
            invalidations: self.invalidations.load(Ordering::Relaxed),
        }
    }
}

/// A plain (non-atomic) copy of [`JournaldStats`] for a point-in-time read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JournaldStatsSnapshot {
    pub read: u64,
    pub published: u64,
    pub dropped: u64,
    pub sampled_out: u64,
    pub decode_errors: u64,
    pub invalidations: u64,
}

impl JournaldStatsSnapshot {
    /// Fraction of read entries dropped or sampled-out over the delta since
    /// `prev`. `0.0` when nothing was read in the window.
    pub fn loss_ratio_since(&self, prev: &JournaldStatsSnapshot) -> f64 {
        let read = self.read.saturating_sub(prev.read);
        if read == 0 {
            return 0.0;
        }
        let lost = self.dropped.saturating_sub(prev.dropped)
            + self.sampled_out.saturating_sub(prev.sampled_out);
        lost as f64 / read as f64
    }
}

/// Per-listener shared ingest context (#106): drop/parse accounting, the global
/// rate limiter, and the overflow policy, threaded into every network listener.
#[derive(Clone)]
struct IngestCtx {
    stats: Arc<IngestStats>,
    limiter: Arc<SharedRateLimiter>,
    overflow: OverflowPolicy,
    /// Multiline-join settings for the stream paths (#107). Copied per
    /// connection into a fresh [`MultilineJoiner`].
    multiline: MultilineConfig,
    /// Default RFC 3164 sender timezone for this listener (#545). UTC unless the
    /// listener sets `timezone`. Overridden per-sender by `host_tz`.
    default_tz: Tz,
    /// Per-sender RFC 3164 timezone overrides, keyed by sender IP (#545).
    host_tz: Arc<HashMap<String, Tz>>,
}

impl IngestCtx {
    /// Resolve the timezone for an RFC 3164 sender: per-IP override first, then
    /// the listener default (#545). Unix senders have no IP → listener default.
    fn tz_for(&self, source: &MessageSource) -> Tz {
        if let MessageSource::Network(addr) = source
            && let Some(tz) = self.host_tz.get(&addr.ip().to_string())
        {
            return *tz;
        }
        self.default_tz
    }
}

/// Start all configured listeners and return the message channel, the shared
/// network [`IngestStats`] (#106), and — when the journald source is enabled —
/// its [`JournaldStats`] for health/telemetry (`None` otherwise). Both stats
/// handles are `Arc`-shared with their producers.
pub async fn start_listeners(
    config: &SyslogConfig,
) -> Result<(
    mpsc::Receiver<ReceivedMessage>,
    Option<Arc<JournaldStats>>,
    Arc<IngestStats>,
)> {
    // Central intake channel: capacity is configurable (#546) so operators can
    // trade memory for burst absorption instead of the old hardcoded 1000.
    let (tx, rx) = mpsc::channel(config.ingest.channel_capacity.max(1));
    let hostname_aliases = Arc::new(config.hostname_aliases.clone());

    // Per-sender RFC 3164 timezone overrides, resolved once (#545). Validated in
    // `validate_config`, so `parse_tz` cannot fail here.
    let mut host_tz = HashMap::new();
    for (host, tz) in &config.host_timezones {
        host_tz.insert(host.clone(), crate::config::parse_tz(tz)?);
    }
    let host_tz = Arc::new(host_tz);

    // Shared network-ingest context: one stats block + one global rate limiter
    // across all network listeners (the `logs/ingest/*` series is sensor-wide).
    let ingest_stats = Arc::new(IngestStats::default());
    let ctx = IngestCtx {
        stats: ingest_stats.clone(),
        limiter: Arc::new(SharedRateLimiter::new(
            config.ingest.max_eps,
            config.ingest.sample_ratio,
            Instant::now(),
        )),
        overflow: config.ingest.overflow,
        multiline: config.multiline,
        // Per-listener default filled in below.
        default_tz: Tz::UTC,
        host_tz: host_tz.clone(),
    };

    for listener_config in &config.listeners {
        let tx = tx.clone();
        let aliases = hostname_aliases.clone();
        let config = listener_config.clone();
        let mut ctx = ctx.clone();
        ctx.default_tz = match &config.timezone {
            Some(tz) => crate::config::parse_tz(tz)?,
            None => Tz::UTC,
        };

        match config.protocol {
            ListenerProtocol::Udp => {
                tokio::spawn(async move {
                    if let Err(e) = run_udp_listener(&config, tx, aliases, ctx).await {
                        tracing::error!("UDP listener error: {}", e);
                    }
                });
            }
            ListenerProtocol::Tcp => {
                tokio::spawn(async move {
                    if let Err(e) = run_tcp_listener(&config, tx, aliases, ctx).await {
                        tracing::error!("TCP listener error: {}", e);
                    }
                });
            }
            ListenerProtocol::Unix => {
                tokio::spawn(async move {
                    if let Err(e) = run_unix_listener(&config, tx, aliases, ctx).await {
                        tracing::error!("Unix listener error: {}", e);
                    }
                });
            }
            ListenerProtocol::Tls => {
                tokio::spawn(async move {
                    if let Err(e) = run_tls_listener(&config, tx, aliases, ctx).await {
                        tracing::error!("TLS listener error: {}", e);
                    }
                });
            }
        }
    }

    // systemd-journald source (#57): a dedicated OS thread feeds the same
    // channel as the network listeners. `systemd::journal::Journal` is
    // `!Send + !Sync`, so it cannot live on a tokio task.
    // `mut` is used only when the journald feature is compiled in.
    #[cfg_attr(not(feature = "journald"), allow(unused_mut))]
    let mut journald_stats: Option<Arc<JournaldStats>> = None;
    if let Some(journald) = &config.journald
        && journald.enabled
    {
        #[cfg(feature = "journald")]
        {
            let (_handle, stats) = crate::journald::spawn_reader(journald.clone(), tx.clone());
            journald_stats = Some(stats);
            tracing::info!(
                scope = ?journald.scope,
                overflow = ?journald.overflow,
                max_eps = ?journald.max_eps,
                "journald source enabled"
            );
        }
        #[cfg(not(feature = "journald"))]
        tracing::warn!(
            "journald.enabled is set but this binary was built without the \
             `journald` feature; ignoring"
        );
    }

    // File tailing source (#549): a dedicated OS thread tails glob-matched files
    // into the same channel with blocking I/O off the runtime. Shares the network
    // ingest stats + rate limiter (it's another ingress into the same pipeline).
    if !config.files.sources.is_empty() {
        let tailer = crate::file_source::FileTailer::new(
            config.files.clone(),
            config.resolved_source(),
            tx.clone(),
            ingest_stats.clone(),
            ctx.limiter.clone(),
            config.ingest.overflow,
        );
        let _handle = tailer.spawn();
        tracing::info!(
            sources = config.files.sources.len(),
            "file tailing source enabled"
        );
    }

    Ok((rx, journald_stats, ingest_stats))
}

/// Run a UDP syslog listener. Each datagram is exactly one frame (no stream
/// framing); ingest accounting + rate-limit + overflow mirror journald (#106).
async fn run_udp_listener(
    config: &ListenerConfig,
    tx: mpsc::Sender<ReceivedMessage>,
    aliases: Arc<HashMap<String, String>>,
    ctx: IngestCtx,
) -> Result<()> {
    let socket = UdpSocket::bind(&config.bind)
        .await
        .with_context(|| format!("Failed to bind UDP socket to {}", config.bind))?;

    tracing::info!("UDP syslog listener started on {}", config.bind);

    let mut buf = vec![0u8; config.max_message_size];

    loop {
        match socket.recv_from(&mut buf).await {
            Ok((len, addr)) => {
                let data = &buf[..len];

                // Try to parse as UTF-8
                let text = match std::str::from_utf8(data) {
                    Ok(s) => s,
                    Err(_) => {
                        // Try lossy conversion for non-UTF8 messages
                        &String::from_utf8_lossy(data)
                    }
                };

                IngestStats::inc(&ctx.stats.received);
                let time_ctx =
                    RfcTimeCtx::new(ctx.tz_for(&MessageSource::Network(addr)), Utc::now());
                if let Some(message) = parser::parse_with_time(text, &time_ctx) {
                    IngestStats::inc(&ctx.stats.parsed);
                    let resolved_hostname = resolve_hostname_network(&addr, &message, &aliases);

                    let received = ReceivedMessage {
                        message,
                        source: MessageSource::Network(addr),
                        resolved_hostname,
                    };

                    if !forward_parsed(received, &tx, &ctx.stats, &ctx.limiter, ctx.overflow).await
                    {
                        tracing::warn!("Receiver channel closed");
                        break;
                    }
                } else {
                    IngestStats::inc(&ctx.stats.parse_failed);
                    tracing::debug!("Failed to parse syslog message from {}: {:?}", addr, text);
                }
            }
            Err(e) => {
                tracing::error!("UDP receive error: {}", e);
            }
        }
    }

    Ok(())
}

/// Run a TCP syslog listener.
async fn run_tcp_listener(
    config: &ListenerConfig,
    tx: mpsc::Sender<ReceivedMessage>,
    aliases: Arc<HashMap<String, String>>,
    ctx: IngestCtx,
) -> Result<()> {
    let listener = TcpListener::bind(&config.bind)
        .await
        .with_context(|| format!("Failed to bind TCP socket to {}", config.bind))?;

    tracing::info!(
        framing = ?config.framing,
        "TCP syslog listener started on {}",
        config.bind
    );

    let semaphore = Arc::new(tokio::sync::Semaphore::new(config.max_connections));
    let connection_timeout = Duration::from_secs(config.connection_timeout_secs);
    let framing = config.framing;
    let max_frame_len = config.max_message_size;

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                let tx = tx.clone();
                let aliases = aliases.clone();
                let ctx = ctx.clone();
                let permit = semaphore.clone().try_acquire_owned();

                match permit {
                    Ok(permit) => {
                        tokio::spawn(async move {
                            let _permit = permit; // Hold permit until connection closes
                            let mut reader = FrameReader::new(framing, max_frame_len);
                            if let Err(e) = handle_stream_connection(
                                stream,
                                &mut reader,
                                connection_timeout,
                                MessageSource::Network(addr),
                                &tx,
                                &aliases,
                                &ctx,
                                None,
                            )
                            .await
                            {
                                tracing::debug!("TCP connection error from {}: {}", addr, e);
                            }
                        });
                    }
                    Err(_) => {
                        tracing::warn!("Max connections reached, rejecting {}", addr);
                        drop(stream);
                    }
                }
            }
            Err(e) => {
                tracing::error!("TCP accept error: {}", e);
            }
        }
    }
}

/// Handle a single stream (TCP or Unix) connection: pull RFC 6587 frames off the
/// wire via [`FrameReader`], then parse + account + forward each (#106). Shared
/// by the TCP and Unix listeners — only the [`MessageSource`] and hostname
/// resolution differ (resolved from the per-frame `source`).
/// Run a TLS syslog listener (#550, RFC 5425): TLS over TCP with octet-counting
/// framing. Certs are hot-reloaded on mtime change (rotation without restart);
/// with `client_ca_file` set, clients must present a cert verified against it,
/// and its CN is attached as `sd.tls.peer_cn`.
async fn run_tls_listener(
    config: &ListenerConfig,
    tx: mpsc::Sender<ReceivedMessage>,
    aliases: Arc<HashMap<String, String>>,
    ctx: IngestCtx,
) -> Result<()> {
    use tokio_rustls::TlsAcceptor;

    let tls_cfg = config
        .tls
        .as_ref()
        .context("tls listener has no tls config")?
        .clone();
    let listener = TcpListener::bind(&config.bind)
        .await
        .with_context(|| format!("Failed to bind TLS socket to {}", config.bind))?;

    // Reloadable server config: built once, swapped by a background mtime watcher
    // so cert rotation needs no restart.
    let server_config = Arc::new(std::sync::Mutex::new(crate::tls::load_server_config(
        &tls_cfg,
    )?));
    tracing::info!(
        mtls = tls_cfg.client_ca_file.is_some(),
        min_version = %tls_cfg.min_version,
        "TLS syslog listener started on {}",
        config.bind
    );
    {
        let shared = server_config.clone();
        let watch_cfg = tls_cfg.clone();
        tokio::spawn(async move {
            let mut mtimes = crate::tls::cert_mtimes(&watch_cfg);
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            loop {
                tick.tick().await;
                let current = crate::tls::cert_mtimes(&watch_cfg);
                if current != mtimes {
                    match crate::tls::load_server_config(&watch_cfg) {
                        Ok(new) => {
                            *shared.lock().unwrap_or_else(|p| p.into_inner()) = new;
                            mtimes = current;
                            tracing::info!("TLS listener reloaded rotated certificate");
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "TLS cert reload failed; keeping old cert");
                        }
                    }
                }
            }
        });
    }

    let semaphore = Arc::new(tokio::sync::Semaphore::new(config.max_connections));
    let connection_timeout = Duration::from_secs(config.connection_timeout_secs);
    let max_frame_len = config.max_message_size;

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                let Ok(permit) = semaphore.clone().try_acquire_owned() else {
                    tracing::warn!("Max connections reached, rejecting {}", addr);
                    drop(stream);
                    continue;
                };
                let acceptor = TlsAcceptor::from(
                    server_config
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .clone(),
                );
                let tx = tx.clone();
                let aliases = aliases.clone();
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let tls_stream = match acceptor.accept(stream).await {
                        Ok(s) => s,
                        Err(e) => {
                            // Cleartext / bad-cert / wrong-version connections land
                            // here — rejected, not ingested.
                            tracing::debug!("TLS handshake failed from {}: {}", addr, e);
                            return;
                        }
                    };
                    let peer_cn = {
                        let (_io, conn) = tls_stream.get_ref();
                        crate::tls::peer_cn(conn.peer_certificates())
                    };
                    // RFC 5425 mandates octet-counting framing over TLS.
                    let mut reader = FrameReader::new(Framing::Octet, max_frame_len);
                    if let Err(e) = handle_stream_connection(
                        tls_stream,
                        &mut reader,
                        connection_timeout,
                        MessageSource::Network(addr),
                        &tx,
                        &aliases,
                        &ctx,
                        peer_cn.as_deref(),
                    )
                    .await
                    {
                        tracing::debug!("TLS connection error from {}: {}", addr, e);
                    }
                });
            }
            Err(e) => tracing::error!("TLS accept error: {}", e),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_stream_connection<R>(
    stream: R,
    reader: &mut FrameReader,
    connection_timeout: Duration,
    source: MessageSource,
    tx: &mpsc::Sender<ReceivedMessage>,
    aliases: &HashMap<String, String>,
    ctx: &IngestCtx,
    peer_cn: Option<&str>,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut stream = stream;
    // Stacktrace joiner (#107): folds continuation lines into the head record.
    // When a record is buffered, race the next frame against an idle flush so
    // the last line of a burst isn't held past `flush_timeout_ms`.
    let mut joiner = MultilineJoiner::new(&ctx.multiline);
    let flush_after = Duration::from_millis(ctx.multiline.flush_timeout_ms.max(1));
    loop {
        let frame = if joiner.has_pending() {
            tokio::select! {
                r = reader.next_frame(&mut stream, connection_timeout) => Some(r),
                _ = tokio::time::sleep(flush_after) => None,
            }
        } else {
            Some(reader.next_frame(&mut stream, connection_timeout).await)
        };
        match frame {
            // Idle flush: no new frame within the window → emit the buffer.
            None => {
                if let Some(raw) = joiner.flush()
                    && !process_record(raw, &source, tx, aliases, ctx, peer_cn).await
                {
                    break;
                }
            }
            Some(Ok(Some(bytes))) => {
                IngestStats::inc(&ctx.stats.received);
                let text = String::from_utf8_lossy(&bytes).into_owned();
                if let Some(raw) = joiner.push(text)
                    && !process_record(raw, &source, tx, aliases, ctx, peer_cn).await
                {
                    break;
                }
            }
            // EOF: flush any buffered record before stopping so a trailing
            // stack trace isn't lost.
            Some(Ok(None)) => {
                if let Some(raw) = joiner.flush() {
                    let _ = process_record(raw, &source, tx, aliases, ctx, peer_cn).await;
                }
                break;
            }
            Some(Err(e)) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Parse one completed (possibly multi-line) raw record, account it, and forward
/// it downstream. Returns `false` only when the telemetry channel has closed
/// (the caller should stop the connection). Shared by the live-frame and
/// flush/EOF paths of [`handle_stream_connection`].
async fn process_record(
    raw: String,
    source: &MessageSource,
    tx: &mpsc::Sender<ReceivedMessage>,
    aliases: &HashMap<String, String>,
    ctx: &IngestCtx,
    peer_cn: Option<&str>,
) -> bool {
    let time_ctx = RfcTimeCtx::new(ctx.tz_for(source), Utc::now());
    let Some(mut message) = parser::parse_with_time(&raw, &time_ctx) else {
        IngestStats::inc(&ctx.stats.parse_failed);
        return true;
    };
    // mTLS peer identity (#550): surface the client-cert CN as `sd.tls.peer_cn`
    // for sender attribution / observer evidence.
    if let Some(cn) = peer_cn {
        message
            .structured_data
            .entry("tls".to_string())
            .or_default()
            .insert("peer_cn".to_string(), cn.to_string());
    }
    IngestStats::inc(&ctx.stats.parsed);
    let resolved_hostname = match source {
        MessageSource::Network(addr) => resolve_hostname_network(addr, &message, aliases),
        _ => resolve_hostname_unix(&message, aliases),
    };
    let received = ReceivedMessage {
        message,
        source: source.clone(),
        resolved_hostname,
    };
    forward_parsed(received, tx, &ctx.stats, &ctx.limiter, ctx.overflow).await
}

/// Run a Unix socket syslog listener.
async fn run_unix_listener(
    config: &ListenerConfig,
    tx: mpsc::Sender<ReceivedMessage>,
    aliases: Arc<HashMap<String, String>>,
    ctx: IngestCtx,
) -> Result<()> {
    let socket_path = Path::new(&config.bind);

    // Remove existing socket if configured
    if config.remove_existing_socket && socket_path.exists() {
        std::fs::remove_file(socket_path).with_context(|| {
            format!(
                "Failed to remove existing socket at {}",
                socket_path.display()
            )
        })?;
    }

    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("Failed to bind Unix socket to {}", socket_path.display()))?;

    // Set socket permissions
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = std::fs::Permissions::from_mode(config.socket_mode);
        std::fs::set_permissions(socket_path, permissions).with_context(|| {
            format!(
                "Failed to set permissions on socket {}",
                socket_path.display()
            )
        })?;
    }

    tracing::info!("Unix syslog listener started on {}", socket_path.display());

    let semaphore = Arc::new(tokio::sync::Semaphore::new(config.max_connections));
    let connection_timeout = Duration::from_secs(config.connection_timeout_secs);
    let framing = config.framing;
    let max_frame_len = config.max_message_size;

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let tx = tx.clone();
                let aliases = aliases.clone();
                let ctx = ctx.clone();
                let permit = semaphore.clone().try_acquire_owned();

                match permit {
                    Ok(permit) => {
                        tokio::spawn(async move {
                            let _permit = permit;
                            let mut reader = FrameReader::new(framing, max_frame_len);
                            if let Err(e) = handle_stream_connection(
                                stream,
                                &mut reader,
                                connection_timeout,
                                MessageSource::Unix,
                                &tx,
                                &aliases,
                                &ctx,
                                None,
                            )
                            .await
                            {
                                tracing::debug!("Unix connection error: {}", e);
                            }
                        });
                    }
                    Err(_) => {
                        tracing::warn!("Max connections reached, rejecting Unix connection");
                        drop(stream);
                    }
                }
            }
            Err(e) => {
                tracing::error!("Unix accept error: {}", e);
            }
        }
    }
}

/// Resolve hostname from message, aliases, or source address (network).
fn resolve_hostname_network(
    addr: &SocketAddr,
    message: &SyslogMessage,
    aliases: &HashMap<String, String>,
) -> String {
    let ip = addr.ip().to_string();

    // First check aliases
    if let Some(alias) = aliases.get(&ip) {
        return alias.clone();
    }

    // Use hostname from message if available
    if let Some(ref hostname) = message.hostname {
        return hostname.clone();
    }

    // Fall back to IP address
    ip
}

/// Resolve hostname from message or aliases (Unix socket).
fn resolve_hostname_unix(message: &SyslogMessage, aliases: &HashMap<String, String>) -> String {
    // Use hostname from message if available
    if let Some(ref hostname) = message.hostname {
        // Check aliases for the hostname
        if let Some(alias) = aliases.get(hostname) {
            return alias.clone();
        }
        return hostname.clone();
    }

    // Check for localhost alias
    if let Some(alias) = aliases.get("localhost") {
        return alias.clone();
    }

    // Default to localhost for Unix socket connections
    "localhost".to_string()
}

/// A unique, time-sortable id for one log line (#104). `<timestamp_ms><seq>`,
/// fixed-width zero-padded so it sorts chronologically, with a per-sensor monotonic
/// sequence guaranteeing uniqueness even within the same millisecond. This is the
/// `log.record.uid` and the `events/<uid>` key suffix — a ULID-style identifier
/// that kills the old last-writer-wins keying without needing a `rand` dependency.
pub fn make_log_uid(timestamp_ms: i64, seq: u64) -> String {
    format!("{:013}{:012}", timestamp_ms.max(0), seq)
}

/// Convert a syslog message to a per-line event TelemetryPoint (#104).
///
/// The metric is `events/<uid>` (unique per line — no last-writer-wins), the value
/// is the message text, and the labels carry the OpenTelemetry logs data model
/// (`severity_number` 1–24, `severity_text`, `log.record.uid`, and — when
/// `include_raw` — `log.record.original`) alongside facility/severity/app/etc.
///
/// Since #358 the point is no longer published on the telemetry bus: it feeds
/// the bounded `@rpc/logs/events` ring (via `LogRecord::from_point`), from which
/// consumers pull on demand.
pub fn to_telemetry_point(
    received: &ReceivedMessage,
    include_raw: bool,
    uid: &str,
) -> TelemetryPoint {
    let msg = &received.message;

    let mut labels = HashMap::new();

    // Add facility and severity as labels
    labels.insert("facility".to_string(), msg.facility.as_str().to_string());
    labels.insert("severity".to_string(), msg.severity.as_str().to_string());

    // OTel logs data model (#104): numeric severity + coarse text + record uid.
    labels.insert(
        "severity_number".to_string(),
        msg.severity.otel_severity_number().to_string(),
    );
    labels.insert(
        "severity_text".to_string(),
        msg.severity.otel_severity_text().to_string(),
    );
    labels.insert("log.record.uid".to_string(), uid.to_string());

    // Add app name if available
    if let Some(ref app) = msg.app_name {
        labels.insert("app".to_string(), app.clone());
    }

    // Add process ID if available
    if let Some(ref pid) = msg.proc_id {
        labels.insert("pid".to_string(), pid.clone());
    }

    // Add message ID if available (RFC 5424)
    if let Some(ref msgid) = msg.msg_id {
        labels.insert("msgid".to_string(), msgid.clone());
    }

    // Add structured data as flattened labels
    for (sd_id, params) in &msg.structured_data {
        for (key, value) in params {
            labels.insert(format!("sd.{}.{}", sd_id, key), value.clone());
        }
    }

    // Add source information
    labels.insert("source_type".to_string(), received.source.to_string());

    // Mark records whose timestamp we had to substitute (#545) so consumers can
    // tell a receive-clock stamp from a sender stamp.
    if msg.ts_source == crate::parser::TsSource::Receiver {
        labels.insert(
            "ts_source".to_string(),
            msg.ts_source.as_label().to_string(),
        );
    }

    // Add raw message if configured. This doubles as OTel `log.record.original`.
    if include_raw {
        labels.insert("raw".to_string(), msg.raw.clone());
        labels.insert("log.record.original".to_string(), msg.raw.clone());
    }

    let timestamp = msg
        .timestamp
        .map(|dt| dt.timestamp_millis())
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());

    TelemetryPoint {
        timestamp,
        source: received.resolved_hostname.clone(),
        protocol: Protocol::Logs,
        // Per-line event key (#104): unique uid kills last-writer-wins so every
        // line survives. Facility/severity now travel in labels, not the metric.
        metric: format!("events/{uid}"),
        value: TelemetryValue::Text(msg.message.clone()),
        labels,
        unit: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loss_ratio_is_delta_over_window() {
        let prev = JournaldStatsSnapshot {
            read: 1000,
            published: 990,
            dropped: 10,
            ..Default::default()
        };
        // Next window: +1000 read, +100 dropped, +50 sampled → 15% loss.
        let cur = JournaldStatsSnapshot {
            read: 2000,
            published: 1840,
            dropped: 110,
            sampled_out: 50,
            ..Default::default()
        };
        assert!((cur.loss_ratio_since(&prev) - 0.15).abs() < 1e-9);
        // Idle window (no reads) → 0.0, not NaN.
        assert_eq!(cur.loss_ratio_since(&cur), 0.0);
    }

    #[test]
    fn test_resolve_hostname_network_alias() {
        let addr: SocketAddr = "192.168.1.1:514".parse().unwrap();
        let msg = parser::parse("<14>test message").unwrap();
        let mut aliases = HashMap::new();
        aliases.insert("192.168.1.1".to_string(), "router01".to_string());

        let hostname = resolve_hostname_network(&addr, &msg, &aliases);
        assert_eq!(hostname, "router01");
    }

    #[test]
    fn test_resolve_hostname_network_from_message() {
        let addr: SocketAddr = "192.168.1.1:514".parse().unwrap();
        let msg = parser::parse("<34>Jan  5 14:30:00 myhost sshd: test").unwrap();
        let aliases = HashMap::new();

        let hostname = resolve_hostname_network(&addr, &msg, &aliases);
        assert_eq!(hostname, "myhost");
    }

    #[test]
    fn test_resolve_hostname_network_fallback_ip() {
        let addr: SocketAddr = "192.168.1.1:514".parse().unwrap();
        let msg = parser::parse("<14>test message").unwrap();
        let aliases = HashMap::new();

        let hostname = resolve_hostname_network(&addr, &msg, &aliases);
        assert_eq!(hostname, "192.168.1.1");
    }

    #[test]
    fn test_resolve_hostname_unix() {
        let msg = parser::parse("<34>Jan  5 14:30:00 myhost sshd: test").unwrap();
        let aliases = HashMap::new();

        let hostname = resolve_hostname_unix(&msg, &aliases);
        assert_eq!(hostname, "myhost");
    }

    #[test]
    fn test_resolve_hostname_unix_default() {
        let msg = parser::parse("<14>test message").unwrap();
        let aliases = HashMap::new();

        let hostname = resolve_hostname_unix(&msg, &aliases);
        assert_eq!(hostname, "localhost");
    }

    #[test]
    fn test_resolve_hostname_unix_with_alias() {
        let msg = parser::parse("<14>test message").unwrap();
        let mut aliases = HashMap::new();
        aliases.insert("localhost".to_string(), "server01".to_string());

        let hostname = resolve_hostname_unix(&msg, &aliases);
        assert_eq!(hostname, "server01");
    }

    #[test]
    fn test_to_telemetry_point() {
        let addr: SocketAddr = "192.168.1.1:514".parse().unwrap();
        let msg = parser::parse("<34>Jan  5 14:30:00 myhost sshd[1234]: Connection from 10.0.0.1")
            .unwrap();

        let received = ReceivedMessage {
            message: msg,
            source: MessageSource::Network(addr),
            resolved_hostname: "myhost".to_string(),
        };

        let uid = make_log_uid(point_ts(&received), 7);
        let point = to_telemetry_point(&received, false, &uid);

        assert_eq!(point.source, "myhost");
        assert_eq!(point.protocol, Protocol::Logs);
        assert_eq!(point.metric, format!("events/{uid}"));
        assert!(matches!(point.value, TelemetryValue::Text(_)));
        assert_eq!(point.labels.get("facility"), Some(&"auth".to_string()));
        assert_eq!(point.labels.get("severity"), Some(&"crit".to_string()));
        assert_eq!(point.labels.get("app"), Some(&"sshd".to_string()));
        assert_eq!(point.labels.get("pid"), Some(&"1234".to_string()));
        // OTel logs data model (#104).
        assert_eq!(point.labels.get("severity_number"), Some(&"22".to_string()));
        assert_eq!(
            point.labels.get("severity_text"),
            Some(&"FATAL".to_string())
        );
        assert_eq!(point.labels.get("log.record.uid"), Some(&uid));
    }

    // Mirror the timestamp logic in `to_telemetry_point` for deterministic uid tests.
    fn point_ts(received: &ReceivedMessage) -> i64 {
        received
            .message
            .timestamp
            .map(|dt| dt.timestamp_millis())
            .unwrap_or(0)
    }

    #[test]
    fn test_to_telemetry_point_unix() {
        let msg = parser::parse("<14>Jan  5 14:30:00 localhost app: test message").unwrap();

        let received = ReceivedMessage {
            message: msg,
            source: MessageSource::Unix,
            resolved_hostname: "localhost".to_string(),
        };

        let point = to_telemetry_point(&received, false, "0000000000000000000000001");

        assert_eq!(point.source, "localhost");
        assert_eq!(point.labels.get("source_type"), Some(&"unix".to_string()));
    }

    #[test]
    fn make_log_uid_is_sortable_and_unique() {
        // Same ms, increasing seq → lexicographically increasing.
        let a = make_log_uid(1000, 1);
        let b = make_log_uid(1000, 2);
        assert!(a < b);
        // Later ms always sorts after earlier ms regardless of seq.
        let c = make_log_uid(2000, 0);
        assert!(b < c);
        // Negative timestamps are clamped to 0 (never panics / no minus sign).
        assert_eq!(make_log_uid(-5, 0), make_log_uid(0, 0));
        assert!(!make_log_uid(-5, 0).contains('-'));
    }

    #[test]
    fn test_message_source_display() {
        let network = MessageSource::Network("192.168.1.1:514".parse().unwrap());
        assert_eq!(network.to_string(), "192.168.1.1:514");

        let unix = MessageSource::Unix;
        assert_eq!(unix.to_string(), "unix");
    }
}
