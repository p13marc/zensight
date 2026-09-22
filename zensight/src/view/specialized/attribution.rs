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

use crate::call::{CallSurface, Calls, Request};
use crate::message::Message;
use crate::view::specialized::fetch::Fetch;

/// The netlink read procedure the join asks (#304): `@rpc/netlink/sockets`.
pub const PROCEDURE: &str = "sockets";

/// What a flow endpoint's call is filed under in a surface's [`Calls`]
/// (#1261): the flow, then the endpoint — two answers to one procedure.
pub fn call_key(flow_key: &str, endpoint: &str) -> String {
    format!("attribution:{flow_key}#{endpoint}")
}

/// The IP of an `ip:port` endpoint (`[::1]:22` included), or the endpoint
/// itself when it is not one.
pub fn endpoint_ip(endpoint: &str) -> String {
    if let Ok(sa) = endpoint.parse::<SocketAddr>() {
        return sa.ip().to_string();
    }
    if let Ok(ip) = endpoint.parse::<std::net::IpAddr>() {
        return ip.to_string();
    }
    match endpoint.rsplit_once(':') {
        Some((host, _port)) => host.trim_matches(['[', ']']).to_string(),
        None => endpoint.to_string(),
    }
}

/// The endpoints a flow's join asks about: both, or one when they share an
/// IP (a flow between two ports of one host).
fn endpoints<'a>(src: &'a str, dst: &'a str) -> Vec<&'a str> {
    if endpoint_ip(src) == endpoint_ip(dst) {
        vec![src]
    } else {
        vec![src, dst]
    }
}

/// The message a "who?" press sends (#309, #1261): one `netlink/sockets`
/// call per endpoint IP, fleet-wide — only the host that owns an endpoint
/// can hold a matching socket, so the tuple match is itself
/// host-discriminating — each filed under its [`call_key`] on `surface`.
pub fn ask(surface: CallSurface, src: &str, dst: &str) -> Message {
    let flow = flow_key(src, dst);
    let calls: Vec<Message> = endpoints(src, dst)
        .into_iter()
        .map(|endpoint| {
            Message::Call(
                Request::new(PROCEDURE, format!("ip={}", endpoint_ip(endpoint)))
                    .of("netlink")
                    .on(surface)
                    .keyed(call_key(&flow, endpoint)),
            )
        })
        .collect();
    match calls.len() {
        1 => calls.into_iter().next().expect("one"),
        _ => Message::Batch(calls),
    }
}

/// Where a flow's join stands, read from the surface's [`Calls`].
#[derive(Debug, Clone, PartialEq)]
pub enum Attribution {
    /// Nobody pressed "who?" for this flow.
    NotAsked,
    /// At least one endpoint's call is in flight.
    Looking,
    /// No endpoint's call answered — no netlink sensor replied at all.
    Unavailable(String),
    /// Every endpoint that answered was matched: the owning process, or
    /// none when no socket matched.
    Ready(Option<AttributedProcess>),
}

/// Reduce a flow's endpoint calls to one outcome (#1261): the join runs at
/// render time over the replies, each decoded as `Vec<SocketRecord>` once.
pub fn lookup(calls: &Calls, src: &str, dst: &str) -> Attribution {
    let flow = flow_key(src, dst);
    let states: Vec<&Fetch<crate::call::Reply>> = endpoints(src, dst)
        .into_iter()
        .map(|endpoint| calls.fetch(&call_key(&flow, endpoint)))
        .collect();
    if states.iter().all(|f| matches!(f, Fetch::Idle)) {
        return Attribution::NotAsked;
    }
    if states.iter().any(|f| f.is_loading()) {
        return Attribution::Looking;
    }
    let mut first_error = None;
    let mut answered = false;
    let mut matched = None;
    for state in states {
        match state {
            Fetch::Ready(reply) => match reply.decoded::<Vec<SocketRecord>>() {
                Ok(sockets) => {
                    answered = true;
                    if matched.is_none() {
                        matched = match_flow_socket(sockets, src, dst);
                    }
                }
                Err(e) => {
                    first_error.get_or_insert(e);
                }
            },
            Fetch::Error(e) => {
                first_error.get_or_insert(e.clone());
            }
            Fetch::Idle | Fetch::Loading => {}
        }
    }
    if answered {
        Attribution::Ready(matched)
    } else {
        Attribution::Unavailable(
            first_error.unwrap_or_else(|| "no netlink sensor responded".to_string()),
        )
    }
}

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

    /// The join over the surface's calls (#1261): not asked, looking while
    /// either endpoint's call is in flight, unavailable when nobody answered,
    /// and matched from whichever endpoint's host held the socket.
    #[test]
    fn lookup_reduces_the_endpoint_calls() {
        let (src, dst) = ("10.0.0.5:44444", "1.1.1.1:443");
        let flow = flow_key(src, dst);
        let mut calls = Calls::default();
        assert_eq!(lookup(&calls, src, dst), Attribution::NotAsked);

        calls.loading_as(&call_key(&flow, src), PROCEDURE, "ip=10.0.0.5");
        assert_eq!(lookup(&calls, src, dst), Attribution::Looking);
        calls.loading_as(&call_key(&flow, dst), PROCEDURE, "ip=1.1.1.1");
        assert_eq!(lookup(&calls, src, dst), Attribution::Looking);

        // Neither host answered: unavailable, in the first failure's words.
        calls.set_failed(&call_key(&flow, src), "no netlink sensor responded");
        calls.set_failed(&call_key(&flow, dst), "no netlink sensor responded");
        assert_eq!(
            lookup(&calls, src, dst),
            Attribution::Unavailable("no netlink sensor responded".into())
        );

        // One host answered with the socket, the other with nothing.
        calls.set_ready(
            &call_key(&flow, src),
            "ip=10.0.0.5",
            serde_json::to_value(vec![sock(src, dst, Some(4242), Some("curl"))]).unwrap(),
        );
        calls.set_ready(&call_key(&flow, dst), "ip=1.1.1.1", serde_json::json!([]));
        match lookup(&calls, src, dst) {
            Attribution::Ready(Some(a)) => {
                assert_eq!(a.pid, Some(4242));
                assert_eq!(a.endpoint, src);
            }
            other => panic!("expected a match, got {other:?}"),
        }
        // One answered, one failed: still an answer — absence on one host is
        // not evidence, and the socket lives on exactly one of them.
        calls.set_failed(&call_key(&flow, dst), "timed out");
        assert!(matches!(
            lookup(&calls, src, dst),
            Attribution::Ready(Some(_))
        ));
    }

    /// A flow between two ports of one host asks once, not twice.
    #[test]
    fn ask_sends_one_call_per_distinct_endpoint_ip() {
        match ask(CallSurface::Security, "10.0.0.5:1", "10.0.0.5:2") {
            Message::Call(r) => {
                assert_eq!(r.params, "ip=10.0.0.5");
                assert_eq!(r.producer.as_deref(), Some("netlink"));
                assert_eq!(r.surface, CallSurface::Security);
                assert_eq!(
                    r.key(),
                    call_key(&flow_key("10.0.0.5:1", "10.0.0.5:2"), "10.0.0.5:1")
                );
            }
            other => panic!("expected one call, got {other:?}"),
        }
        match ask(CallSurface::Topology, "10.0.0.5:1", "[::1]:2") {
            Message::Batch(calls) => assert_eq!(calls.len(), 2),
            other => panic!("expected two calls, got {other:?}"),
        }
        assert_eq!(endpoint_ip("[::1]:22"), "::1");
        assert_eq!(endpoint_ip("10.0.0.5"), "10.0.0.5");
    }
}
