//! Syslog message receivers (UDP, TCP, and Unix socket).

use crate::config::{ListenerConfig, ListenerProtocol, SyslogConfig};
use crate::parser::{self, SyslogMessage};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UdpSocket, UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio::time::{Duration, timeout};
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
}

impl std::fmt::Display for MessageSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MessageSource::Network(addr) => write!(f, "{}", addr),
            MessageSource::Unix => write!(f, "unix"),
        }
    }
}

/// Start all configured listeners and return a channel for receiving messages.
pub async fn start_listeners(config: &SyslogConfig) -> Result<mpsc::Receiver<ReceivedMessage>> {
    let (tx, rx) = mpsc::channel(1000);
    let hostname_aliases = Arc::new(config.hostname_aliases.clone());

    for listener_config in &config.listeners {
        let tx = tx.clone();
        let aliases = hostname_aliases.clone();
        let config = listener_config.clone();

        match config.protocol {
            ListenerProtocol::Udp => {
                tokio::spawn(async move {
                    if let Err(e) = run_udp_listener(&config, tx, aliases).await {
                        tracing::error!("UDP listener error: {}", e);
                    }
                });
            }
            ListenerProtocol::Tcp => {
                tokio::spawn(async move {
                    if let Err(e) = run_tcp_listener(&config, tx, aliases).await {
                        tracing::error!("TCP listener error: {}", e);
                    }
                });
            }
            ListenerProtocol::Unix => {
                tokio::spawn(async move {
                    if let Err(e) = run_unix_listener(&config, tx, aliases).await {
                        tracing::error!("Unix listener error: {}", e);
                    }
                });
            }
        }
    }

    Ok(rx)
}

/// Run a UDP syslog listener.
async fn run_udp_listener(
    config: &ListenerConfig,
    tx: mpsc::Sender<ReceivedMessage>,
    aliases: Arc<HashMap<String, String>>,
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

                if let Some(message) = parser::parse(text) {
                    let resolved_hostname = resolve_hostname_network(&addr, &message, &aliases);

                    let received = ReceivedMessage {
                        message,
                        source: MessageSource::Network(addr),
                        resolved_hostname,
                    };

                    if tx.send(received).await.is_err() {
                        tracing::warn!("Receiver channel closed");
                        break;
                    }
                } else {
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
) -> Result<()> {
    let listener = TcpListener::bind(&config.bind)
        .await
        .with_context(|| format!("Failed to bind TCP socket to {}", config.bind))?;

    tracing::info!("TCP syslog listener started on {}", config.bind);

    let semaphore = Arc::new(tokio::sync::Semaphore::new(config.max_connections));
    let connection_timeout = Duration::from_secs(config.connection_timeout_secs);

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                let tx = tx.clone();
                let aliases = aliases.clone();
                let permit = semaphore.clone().try_acquire_owned();

                match permit {
                    Ok(permit) => {
                        tokio::spawn(async move {
                            let _permit = permit; // Hold permit until connection closes
                            if let Err(e) =
                                handle_tcp_connection(stream, addr, tx, aliases, connection_timeout)
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

/// Handle a single TCP connection.
async fn handle_tcp_connection(
    stream: TcpStream,
    addr: SocketAddr,
    tx: mpsc::Sender<ReceivedMessage>,
    aliases: Arc<HashMap<String, String>>,
    connection_timeout: Duration,
) -> Result<()> {
    tracing::debug!("TCP connection from {}", addr);

    let reader = BufReader::new(stream);
    let mut lines = reader.lines();

    loop {
        // Apply timeout to each line read
        let line_result = timeout(connection_timeout, lines.next_line()).await;

        match line_result {
            Ok(Ok(Some(line))) => {
                if let Some(message) = parser::parse(&line) {
                    let resolved_hostname = resolve_hostname_network(&addr, &message, &aliases);

                    let received = ReceivedMessage {
                        message,
                        source: MessageSource::Network(addr),
                        resolved_hostname,
                    };

                    if tx.send(received).await.is_err() {
                        break;
                    }
                }
            }
            Ok(Ok(None)) => {
                // Connection closed
                break;
            }
            Ok(Err(e)) => {
                return Err(e.into());
            }
            Err(_) => {
                // Timeout
                tracing::debug!("TCP connection timeout from {}", addr);
                break;
            }
        }
    }

    tracing::debug!("TCP connection closed from {}", addr);
    Ok(())
}

/// Run a Unix socket syslog listener.
async fn run_unix_listener(
    config: &ListenerConfig,
    tx: mpsc::Sender<ReceivedMessage>,
    aliases: Arc<HashMap<String, String>>,
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

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let tx = tx.clone();
                let aliases = aliases.clone();
                let permit = semaphore.clone().try_acquire_owned();

                match permit {
                    Ok(permit) => {
                        tokio::spawn(async move {
                            let _permit = permit;
                            if let Err(e) =
                                handle_unix_connection(stream, tx, aliases, connection_timeout)
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

/// Handle a single Unix socket connection.
async fn handle_unix_connection(
    stream: UnixStream,
    tx: mpsc::Sender<ReceivedMessage>,
    aliases: Arc<HashMap<String, String>>,
    connection_timeout: Duration,
) -> Result<()> {
    tracing::debug!("Unix connection accepted");

    let reader = BufReader::new(stream);
    let mut lines = reader.lines();

    loop {
        let line_result = timeout(connection_timeout, lines.next_line()).await;

        match line_result {
            Ok(Ok(Some(line))) => {
                if let Some(message) = parser::parse(&line) {
                    let resolved_hostname = resolve_hostname_unix(&message, &aliases);

                    let received = ReceivedMessage {
                        message,
                        source: MessageSource::Unix,
                        resolved_hostname,
                    };

                    if tx.send(received).await.is_err() {
                        break;
                    }
                }
            }
            Ok(Ok(None)) => {
                break;
            }
            Ok(Err(e)) => {
                return Err(e.into());
            }
            Err(_) => {
                tracing::debug!("Unix connection timeout");
                break;
            }
        }
    }

    tracing::debug!("Unix connection closed");
    Ok(())
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

/// Convert a syslog message to a TelemetryPoint.
pub fn to_telemetry_point(received: &ReceivedMessage, include_raw: bool) -> TelemetryPoint {
    let msg = &received.message;

    let mut labels = HashMap::new();

    // Add facility and severity as labels
    labels.insert("facility".to_string(), msg.facility.as_str().to_string());
    labels.insert("severity".to_string(), msg.severity.as_str().to_string());

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

    // Add raw message if configured
    if include_raw {
        labels.insert("raw".to_string(), msg.raw.clone());
    }

    let timestamp = msg
        .timestamp
        .map(|dt| dt.timestamp_millis())
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());

    TelemetryPoint {
        timestamp,
        source: received.resolved_hostname.clone(),
        protocol: Protocol::Syslog,
        metric: format!("{}/{}", msg.facility.as_str(), msg.severity.as_str()),
        value: TelemetryValue::Text(msg.message.clone()),
        labels,
    }
}

/// Build the key expression for a syslog message.
pub fn build_key_expr(prefix: &str, received: &ReceivedMessage) -> String {
    let msg = &received.message;
    format!(
        "{}/{}/{}/{}",
        prefix,
        received.resolved_hostname,
        msg.facility.as_str(),
        msg.severity.as_str()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let point = to_telemetry_point(&received, false);

        assert_eq!(point.source, "myhost");
        assert_eq!(point.protocol, Protocol::Syslog);
        assert_eq!(point.metric, "auth/crit");
        assert!(matches!(point.value, TelemetryValue::Text(_)));
        assert_eq!(point.labels.get("facility"), Some(&"auth".to_string()));
        assert_eq!(point.labels.get("severity"), Some(&"crit".to_string()));
        assert_eq!(point.labels.get("app"), Some(&"sshd".to_string()));
        assert_eq!(point.labels.get("pid"), Some(&"1234".to_string()));
    }

    #[test]
    fn test_to_telemetry_point_unix() {
        let msg = parser::parse("<14>Jan  5 14:30:00 localhost app: test message").unwrap();

        let received = ReceivedMessage {
            message: msg,
            source: MessageSource::Unix,
            resolved_hostname: "localhost".to_string(),
        };

        let point = to_telemetry_point(&received, false);

        assert_eq!(point.source, "localhost");
        assert_eq!(point.labels.get("source_type"), Some(&"unix".to_string()));
    }

    #[test]
    fn test_build_key_expr() {
        let addr: SocketAddr = "192.168.1.1:514".parse().unwrap();
        let msg = parser::parse("<34>Jan  5 14:30:00 myhost sshd: test").unwrap();

        let received = ReceivedMessage {
            message: msg,
            source: MessageSource::Network(addr),
            resolved_hostname: "myhost".to_string(),
        };

        let key = build_key_expr("zensight/syslog", &received);
        assert_eq!(key, "zensight/syslog/myhost/auth/crit");
    }

    #[test]
    fn test_message_source_display() {
        let network = MessageSource::Network("192.168.1.1:514".parse().unwrap());
        assert_eq!(network.to_string(), "192.168.1.1:514");

        let unix = MessageSource::Unix;
        assert_eq!(unix.to_string(), "unix");
    }
}
