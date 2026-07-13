//! Flow ↔ process display join (#309): "this beacon is `curl` run by uid 1000".
//!
//! A netring flow carries a 5-tuple but no process; the endpoint host's netlink
//! sensor knows its sockets *with* process attribution (#304). This module is
//! the pure join: match a flow's endpoints against fetched [`SocketRecord`]s
//! (either direction — the flow may have been observed from either side) and
//! reduce the hit to a display [`AttributedProcess`], labelled with where the
//! attribution came from. Query-time only — no new bus traffic, no new keys;
//! the fetch narrows server-side via the `?ip=` sockets selector.

use std::net::SocketAddr;

use zensight_common::SocketRecord;

/// Where a flow's process attribution came from — labelled in the UI so an
/// analyst can weigh it ("live socket" is a now-snapshot; a completed
/// connection would be event-time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttributionSource {
    /// Matched a currently-open socket from `@rpc/netlink/sockets` (#304).
    LiveSocket,
    // CompletedConnection: the eBPF ConnRecord tier (event-time, catches
    // short-lived flows) lands with the post-0.7.0 eBPF frontier (#168/#114).
}

impl AttributionSource {
    pub fn label(self) -> &'static str {
        match self {
            AttributionSource::LiveSocket => "live socket",
        }
    }
}

/// The owning process of a flow endpoint, reduced for display.
#[derive(Debug, Clone, PartialEq)]
pub struct AttributedProcess {
    /// Owning pid, when the sensor could attribute the socket.
    pub pid: Option<i32>,
    /// Owning comm, when attributed.
    pub comm: Option<String>,
    /// Socket owner uid (always known from sock_diag).
    pub uid: u32,
    /// The matched socket's state (`established`, …).
    pub state: String,
    /// Which endpoint side matched: the socket's host owns this side.
    pub endpoint: String,
    pub source: AttributionSource,
}

impl AttributedProcess {
    /// One display line: `curl (4242) · uid 1000 · live socket`.
    pub fn display(&self) -> String {
        let who = match (self.comm.as_deref(), self.pid) {
            (Some(c), Some(p)) => format!("{c} ({p})"),
            (Some(c), None) => c.to_string(),
            (None, Some(p)) => format!("pid {p}"),
            (None, None) => "unknown process".to_string(),
        };
        format!("{who} · uid {} · {}", self.uid, self.source.label())
    }
}

/// A stable, displayable key for one flow row's attribution slot.
pub fn flow_key(src: &str, dst: &str) -> String {
    format!("{src} → {dst}")
}

/// Normalize an `ip:port` endpoint for comparison ([`SocketAddr`] when it
/// parses, so `[::1]:22` and `::1:22` spellings can't miscompare; verbatim
/// otherwise).
fn norm(endpoint: &str) -> String {
    endpoint
        .parse::<SocketAddr>()
        .map(|sa| sa.to_string())
        .unwrap_or_else(|_| endpoint.to_string())
}

/// Match a flow's `(src, dst)` endpoints against fetched sockets and reduce the
/// first hit. The socket's `(local, remote)` must equal the tuple in either
/// direction: the netring capture point and the socket-owning host can sit on
/// either side of the flow.
pub fn match_flow_socket(
    sockets: &[SocketRecord],
    flow_src: &str,
    flow_dst: &str,
) -> Option<AttributedProcess> {
    let (src, dst) = (norm(flow_src), norm(flow_dst));
    sockets
        .iter()
        .find_map(|s| {
            let (local, remote) = (norm(&s.local), norm(&s.remote));
            let endpoint = if local == src && remote == dst {
                Some(flow_src)
            } else if local == dst && remote == src {
                Some(flow_dst)
            } else {
                None
            };
            endpoint.map(|ep| (s, ep))
        })
        .map(|(s, endpoint)| AttributedProcess {
            pid: s.pid,
            comm: s.process.clone(),
            uid: s.uid,
            state: s.state.clone(),
            endpoint: endpoint.to_string(),
            source: AttributionSource::LiveSocket,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sock(local: &str, remote: &str, pid: Option<i32>, comm: Option<&str>) -> SocketRecord {
        SocketRecord {
            local: local.into(),
            remote: remote.into(),
            state: "established".into(),
            uid: 1000,
            pid,
            process: comm.map(String::from),
            ..Default::default()
        }
    }

    #[test]
    fn matches_exact_tuple_and_reversed_direction() {
        let sockets = vec![
            sock("10.0.0.5:44444", "1.1.1.1:443", Some(4242), Some("curl")),
            sock("10.0.0.5:22", "10.0.0.9:55555", Some(1), Some("sshd")),
        ];
        // Flow observed client→server: socket local == flow src.
        let a = match_flow_socket(&sockets, "10.0.0.5:44444", "1.1.1.1:443").unwrap();
        assert_eq!(a.pid, Some(4242));
        assert_eq!(a.comm.as_deref(), Some("curl"));
        assert_eq!(a.endpoint, "10.0.0.5:44444");
        assert_eq!(a.display(), "curl (4242) · uid 1000 · live socket");
        // Flow observed from the other side: reversed tuple still matches.
        let a = match_flow_socket(&sockets, "1.1.1.1:443", "10.0.0.5:44444").unwrap();
        assert_eq!(a.pid, Some(4242));
        assert_eq!(a.endpoint, "10.0.0.5:44444");
        // Same IPs, different port → no match (exact tuple, not endpoint-only).
        assert!(match_flow_socket(&sockets, "10.0.0.5:44445", "1.1.1.1:443").is_none());
        assert!(match_flow_socket(&[], "10.0.0.5:44444", "1.1.1.1:443").is_none());
    }

    #[test]
    fn unattributed_socket_still_reports_uid() {
        // The sensor couldn't resolve the owner (other user's process) — the
        // socket match still yields the uid, with an honest "pid unknown".
        let sockets = vec![sock("10.0.0.5:44444", "1.1.1.1:443", None, None)];
        let a = match_flow_socket(&sockets, "10.0.0.5:44444", "1.1.1.1:443").unwrap();
        assert_eq!(a.pid, None);
        assert_eq!(a.display(), "unknown process · uid 1000 · live socket");
    }
}
