//! The container runtime's socket (#819) — HTTP/1.1 over a UNIX socket.
//!
//! `reqwest` cannot speak a UNIX socket, so this drives `hyper` directly. The
//! surface is tiny and deliberately so: **two GETs and nothing else**. There
//! is no method here that can start, stop, or change anything, which is the
//! sensor's security posture expressed in code rather than in a comment.
//!
//! Podman's REST API serves two dialects on the same socket: the Docker
//! compatibility API and the richer `libpod` one. This uses `libpod`, because
//! the compatibility API omits the two fields the audit's findings turn on —
//! the healthcheck *log* (which is how "never ran" is distinguishable from
//! "failing") and the image digest.

use std::path::{Path, PathBuf};
use std::time::Duration;

use http_body_util::BodyExt;
use hyper_util::rt::TokioIo;
use serde_json::Value;

#[derive(Debug)]
pub enum RuntimeError {
    Connect(String),
    Http { code: u16, body: String },
    Transport(String),
    Malformed(String),
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RuntimeError::Connect(e) => write!(f, "cannot reach the container socket: {e}"),
            RuntimeError::Http { code, body } => write!(f, "HTTP {code}: {body}"),
            RuntimeError::Transport(e) => write!(f, "transport: {e}"),
            RuntimeError::Malformed(e) => write!(f, "malformed reply: {e}"),
        }
    }
}

impl std::error::Error for RuntimeError {}

pub type Result<T> = std::result::Result<T, RuntimeError>;

/// A read-only client for one container runtime socket.
pub struct RuntimeClient {
    socket: PathBuf,
    timeout: Duration,
    /// `true` when the socket is a user-session one — the container's cgroup
    /// then lives under `user.slice`, not `machine.slice`, and the sensor
    /// cannot see other users' containers at all.
    pub rootless: bool,
}

impl RuntimeClient {
    pub fn new(socket: impl Into<PathBuf>, timeout: Duration, rootless: bool) -> Self {
        Self {
            socket: socket.into(),
            timeout,
            rootless,
        }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// One GET. A fresh connection per request: the poll interval is tens of
    /// seconds, so pooling would save nothing and a stale half-closed
    /// connection to a restarted podman would cost a whole cycle.
    async fn get(&self, path: &str) -> Result<Value> {
        let stream =
            tokio::time::timeout(self.timeout, tokio::net::UnixStream::connect(&self.socket))
                .await
                .map_err(|_| RuntimeError::Connect("connect timed out".into()))?
                .map_err(|e| RuntimeError::Connect(e.to_string()))?;

        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| RuntimeError::Transport(e.to_string()))?;
        // The connection task ends when the response is done; nothing else
        // holds it, so a failure here is not worth surfacing twice.
        tokio::spawn(async move {
            let _ = conn.await;
        });

        let req = hyper::Request::builder()
            .method("GET")
            .uri(path)
            // hyper/1 requires a Host header on HTTP/1.1; the socket has no
            // authority, and podman ignores the value.
            .header("Host", "d")
            .body(String::new())
            .map_err(|e| RuntimeError::Transport(e.to_string()))?;

        let resp = tokio::time::timeout(self.timeout, sender.send_request(req))
            .await
            .map_err(|_| RuntimeError::Transport("request timed out".into()))?
            .map_err(|e| RuntimeError::Transport(e.to_string()))?;

        let status = resp.status().as_u16();
        let body = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| RuntimeError::Transport(e.to_string()))?
            .to_bytes();
        if !(200..300).contains(&status) {
            return Err(RuntimeError::Http {
                code: status,
                body: String::from_utf8_lossy(&body).chars().take(200).collect(),
            });
        }
        serde_json::from_slice(&body).map_err(|e| RuntimeError::Malformed(e.to_string()))
    }

    /// Every container, running or not. A container that exited is exactly the
    /// one worth reporting, so `all=true` is not optional.
    pub async fn list(&self) -> Result<Vec<Value>> {
        match self.get("/v4.0.0/libpod/containers/json?all=true").await? {
            Value::Array(a) => Ok(a),
            other => Err(RuntimeError::Malformed(format!(
                "expected an array of containers, got {}",
                kind_of(&other)
            ))),
        }
    }

    /// One container's full inspect document — where the healthcheck log, the
    /// restart count, the image digest and the cgroup path live.
    pub async fn inspect(&self, id: &str) -> Result<Value> {
        self.get(&format!("/v4.0.0/libpod/containers/{id}/json"))
            .await
    }

    /// Whether the socket is there at all. Distinguishes "no containers" from
    /// "no runtime", which are different answers and must not both be silence.
    pub fn socket_exists(&self) -> bool {
        self.socket.exists()
    }
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// The conventional socket paths, in the order worth trying.
///
/// Rootful first: a sensor running as root on a host with both sees the system
/// containers, which is what a fleet cares about. A rootless-only deployment
/// finds its own socket second.
pub fn default_sockets() -> Vec<(PathBuf, bool)> {
    let mut out = vec![(PathBuf::from("/run/podman/podman.sock"), false)];
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        out.push((PathBuf::from(xdg).join("podman/podman.sock"), true));
    }
    // The Docker socket speaks the compatibility API only; it is listed so a
    // Docker host is not silently unsupported, and the caller degrades to the
    // fields that dialect carries.
    out.push((PathBuf::from("/run/docker.sock"), false));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_socket_order_prefers_rootful() {
        let s = default_sockets();
        assert_eq!(s[0].0, PathBuf::from("/run/podman/podman.sock"));
        assert!(!s[0].1, "the rootful socket is not rootless");
    }

    #[test]
    fn a_missing_socket_is_detectable_without_a_request() {
        let c = RuntimeClient::new("/nonexistent/podman.sock", Duration::from_secs(1), false);
        assert!(!c.socket_exists());
    }

    /// "No runtime" and "no containers" are different answers, and a sensor
    /// that renders both as an empty list is lying about one of them.
    #[tokio::test]
    async fn a_missing_socket_errors_rather_than_reporting_zero_containers() {
        let c = RuntimeClient::new(
            "/nonexistent/podman.sock",
            Duration::from_millis(200),
            false,
        );
        let e = c.list().await.unwrap_err();
        assert!(matches!(e, RuntimeError::Connect(_)), "{e}");
    }
}
