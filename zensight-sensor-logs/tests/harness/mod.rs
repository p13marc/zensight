//! Integration test harness for the logs sensor (#548).
//!
//! Drives the real listener stack (`receiver::start_listeners`) over localhost
//! sockets and asserts that received lines reach the `@rpc/logs/events` ring on
//! an in-process, isolated Zenoh session — no external services, no root.
//!
//! The harness reproduces the sensor's intake bridge (filter → uid →
//! `to_telemetry_point` → `LogRecord::from_point` → `query::push`) faithfully
//! but minimally: the transport, framing, parsing, filtering, and ring/query
//! paths are the crate's own code; only the ~10-line glue that `main.rs`
//! inlines is restated here. Later epic issues extend it (durable store, TLS,
//! file source).

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use zensight_common::LogRecord;
use zensight_sensor_logs::config::{
    Framing, IngestConfig, ListenerConfig, ListenerProtocol, MultilineConfig, OverflowPolicy,
    SyslogConfig,
};
use zensight_sensor_logs::filter::FilterManager;
use zensight_sensor_logs::query::{self, EventRing};
use zensight_sensor_logs::receiver::{self, ReceivedMessage};

/// A standalone Zenoh config: scouting off so concurrent test peers (and any
/// live fleet on the host) never cross-contaminate — a test that scouts is a
/// participant, not a test (RFC 09 §0.1).
pub fn isolated_config() -> zenoh::Config {
    let mut config = zenoh::Config::default();
    config
        .insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    config
        .insert_json5("scouting/gossip/enabled", "false")
        .unwrap();
    config
}

/// A free localhost TCP port (bind :0, read it, drop). Small TOCTOU window,
/// fine for CI on localhost.
pub fn free_tcp_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A free localhost UDP port.
pub fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Builder for a `SyslogConfig` with one listener and tuned ingest/multiline.
pub struct RigBuilder {
    listener: ListenerConfig,
    overflow: OverflowPolicy,
    flush_timeout_ms: u64,
    /// When false, the intake loop is NOT spawned — the channel fills, so
    /// overflow/backpressure accounting can be exercised.
    drain: bool,
}

impl RigBuilder {
    pub fn udp(port: u16) -> Self {
        Self::with(listener(ListenerProtocol::Udp, format!("127.0.0.1:{port}")))
    }
    pub fn tcp(port: u16, framing: Framing) -> Self {
        let mut l = listener(ListenerProtocol::Tcp, format!("127.0.0.1:{port}"));
        l.framing = framing;
        Self::with(l)
    }
    pub fn unix(path: &std::path::Path) -> Self {
        Self::with(listener(
            ListenerProtocol::Unix,
            path.to_string_lossy().into_owned(),
        ))
    }

    fn with(listener: ListenerConfig) -> Self {
        Self {
            listener,
            overflow: OverflowPolicy::DropNewest,
            flush_timeout_ms: 100,
            drain: true,
        }
    }

    pub fn overflow(mut self, policy: OverflowPolicy) -> Self {
        self.overflow = policy;
        self
    }
    /// Park the intake loop (don't drain the channel) — for backpressure tests.
    pub fn no_drain(mut self) -> Self {
        self.drain = false;
        self
    }

    pub async fn start(self) -> LogRig {
        let mut cfg = SyslogConfig::default();
        cfg.listeners = vec![self.listener.clone()];
        cfg.ingest = IngestConfig {
            overflow: self.overflow,
            ..cfg.ingest
        };
        cfg.multiline = MultilineConfig {
            flush_timeout_ms: self.flush_timeout_ms,
            ..MultilineConfig::default()
        };

        let (rx, _journald, ingest_stats) = receiver::start_listeners(&cfg)
            .await
            .expect("start listeners");

        let session = Arc::new(zenoh::open(isolated_config()).await.expect("open zenoh"));
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // Unique producer prefix so parallel rigs don't share keys on one origin.
        let producer = format!("test_{nanos}/logs");
        let filter = Arc::new(FilterManager::pass_all());
        let (ring, capacity) = query::new_ring(10_000);

        // Serve @rpc/<producer>/events.
        tokio::spawn(query::run_events(
            session.clone(),
            producer.clone(),
            ring.clone(),
        ));

        // Intake bridge (faithful to main.rs's inlined loop, minus analytics).
        if self.drain {
            let ring = ring.clone();
            let filter = filter.clone();
            tokio::spawn(intake_loop(rx, filter, ring, capacity));
        } else {
            // Keep rx alive but never drain it, so the channel back-pressures.
            std::mem::forget(rx);
        }

        // Let listeners bind and the queryable declare.
        tokio::time::sleep(Duration::from_millis(200)).await;

        LogRig {
            session,
            producer,
            ring,
            capacity,
            filter,
            ingest_stats,
            listener: self.listener,
        }
    }
}

async fn intake_loop(
    mut rx: tokio::sync::mpsc::Receiver<ReceivedMessage>,
    filter: Arc<FilterManager>,
    ring: EventRing,
    capacity: usize,
) {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    while let Some(received) = rx.recv().await {
        if !filter
            .matches(&received.message, &received.resolved_hostname)
            .await
        {
            continue;
        }
        let ts = received
            .message
            .timestamp
            .map(|d| d.timestamp_millis())
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        let uid = receiver::make_log_uid(ts, SEQ.fetch_add(1, Ordering::Relaxed));
        let point = receiver::to_telemetry_point(&received, false, &uid);
        if let Some(record) = LogRecord::from_point(&point) {
            query::push(&ring, capacity, record);
        }
    }
}

/// A running logs-sensor rig: listener(s) + intake + `events` queryable.
pub struct LogRig {
    pub session: Arc<zenoh::Session>,
    pub producer: String,
    pub ring: EventRing,
    pub capacity: usize,
    pub filter: Arc<FilterManager>,
    pub ingest_stats: Arc<zensight_sensor_logs::ingest::IngestStats>,
    pub listener: ListenerConfig,
}

impl LogRig {
    /// GET `@rpc/<producer>/events` with optional `;`-separated params.
    pub async fn events(&self, params: &str) -> Vec<LogRecord> {
        let key = zensight_common::command::query_key(&self.producer, "events");
        let selector = if params.is_empty() {
            key
        } else {
            format!("{key}?{params}")
        };
        let replies = self
            .session
            .get(&selector)
            .timeout(Duration::from_secs(5))
            .await
            .expect("get events");
        let reply = replies.recv_async().await.expect("one reply");
        let sample = reply.result().expect("ok reply");
        serde_json::from_slice(&sample.payload().to_bytes()).expect("decode Vec<LogRecord>")
    }

    /// Serve a minimal `filter`/`filter/set` command pair on this rig's
    /// producer, mutating the shared `FilterManager` — the live dynamic-filter
    /// path (#548). Mirrors `main.rs`'s handler with the public
    /// `FilterCommand`/`FilterStatus` protocol.
    pub fn serve_filters(&self) {
        use zensight_sensor_logs::commands::{
            FilterCommand, FilterStatus, command_key, status_key,
        };

        let filter = self.filter.clone();
        let write_key = command_key(&self.producer);
        let read_key = status_key(&self.producer);
        let session = self.session.clone();

        // Write: @rpc/<producer>/filter/set
        let f = filter.clone();
        let s = session.clone();
        tokio::spawn(async move {
            let q = s
                .declare_queryable(&write_key)
                .await
                .expect("filter/set queryable");
            while let Ok(query) = q.recv_async().await {
                let payload = query
                    .payload()
                    .map(|p| p.to_bytes().to_vec())
                    .unwrap_or_default();
                let reply: Result<(), String> =
                    match serde_json::from_slice::<FilterCommand>(&payload) {
                        Ok(FilterCommand::AddFilter { id, filter: cfg }) => f
                            .add_filter(id.unwrap_or_else(|| "dyn".into()), &cfg)
                            .await
                            .map_err(|e| e.to_string()),
                        Ok(FilterCommand::RemoveFilter { id }) => {
                            f.remove_filter(&id).await;
                            Ok(())
                        }
                        Ok(FilterCommand::ClearFilters) => {
                            f.clear_filters().await;
                            Ok(())
                        }
                        Ok(FilterCommand::GetStatus) => Ok(()),
                        Err(e) => Err(e.to_string()),
                    };
                let body = serde_json::to_vec(&reply).unwrap();
                let _ = query.reply(write_key.clone(), body).await;
            }
        });

        // Read: @rpc/<producer>/filter
        tokio::spawn(async move {
            let q = session
                .declare_queryable(&read_key)
                .await
                .expect("filter queryable");
            while let Ok(query) = q.recv_async().await {
                let status = FilterStatus {
                    base_filter: filter.base_config().clone(),
                    dynamic_filters: filter.dynamic_filter_info().await,
                    stats: filter.stats(),
                };
                let body = serde_json::to_vec(&status).unwrap();
                let _ = query.reply(read_key.clone(), body).await;
            }
        });
    }

    /// PUT a `FilterCommand` on `filter/set` and await the ack.
    pub async fn set_filter(&self, cmd: &zensight_sensor_logs::commands::FilterCommand) {
        let key = zensight_sensor_logs::commands::command_key(&self.producer);
        let body = serde_json::to_vec(cmd).unwrap();
        let replies = self
            .session
            .get(&key)
            .payload(body)
            .timeout(Duration::from_secs(5))
            .await
            .expect("filter/set get");
        let reply = replies.recv_async().await.expect("filter/set reply");
        reply.result().expect("filter/set ok");
    }

    /// Poll `events` until at least `min` records are present or `deadline`.
    pub async fn events_until(&self, min: usize, deadline: Duration) -> Vec<LogRecord> {
        let start = tokio::time::Instant::now();
        loop {
            let records = self.events("").await;
            if records.len() >= min || start.elapsed() >= deadline {
                return records;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn listener(protocol: ListenerProtocol, bind: String) -> ListenerConfig {
    ListenerConfig {
        protocol,
        bind,
        max_message_size: 65535,
        max_connections: 100,
        connection_timeout_secs: 5,
        socket_mode: 0o666,
        remove_existing_socket: true,
        framing: Framing::Auto,
    }
}

// --- senders --------------------------------------------------------------

pub async fn send_udp(port: u16, msg: &str) {
    let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sock.send_to(msg.as_bytes(), ("127.0.0.1", port))
        .await
        .unwrap();
}

/// Send framed messages over one TCP connection using RFC 6587 octet-counting.
pub async fn send_tcp_octet(port: u16, msgs: &[&str]) {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    for m in msgs {
        let frame = format!("{} {m}", m.len());
        stream.write_all(frame.as_bytes()).await.unwrap();
    }
    stream.flush().await.unwrap();
    // Leave the caller to drop the stream (EOF) by returning the connection.
    let _ = stream.shutdown().await;
}

/// Open a TCP connection, send LF-framed lines, and return the live stream so
/// the caller controls when EOF/idle happens (multiline idle-flush test).
pub async fn tcp_connect(port: u16) -> tokio::net::TcpStream {
    tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap()
}

pub async fn send_line(stream: &mut tokio::net::TcpStream, line: &str) {
    stream.write_all(line.as_bytes()).await.unwrap();
    if !line.ends_with('\n') {
        stream.write_all(b"\n").await.unwrap();
    }
    stream.flush().await.unwrap();
}

pub async fn send_unix(path: &std::path::Path, msg: &str) {
    let mut stream = tokio::net::UnixStream::connect(path).await.unwrap();
    let frame = format!("{} {msg}", msg.len());
    stream.write_all(frame.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let _ = stream.shutdown().await;
}
