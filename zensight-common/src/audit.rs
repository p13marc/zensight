//! Write procedures record what they did, in the host's own audit trail (#957).
//!
//! **Not to be confused with [`crate::registry_audit`]**, two lines above this
//! one in `lib.rs`: that one audits the *registry* (registered ⊆ emittable).
//! This one is the operator's trail — who asked this producer to change
//! something, and what happened.
//!
//! # Why the OS trail and not a store of our own
//!
//! Before this module the only trail in the tree was the `systemd` sensor's
//! bounded in-memory ring: per-sensor, volatile, and covering one write surface
//! out of a dozen. The alternative to a bespoke store is the one the host
//! already runs. `auditd` is tamper-evident, survives our restarts, is already
//! collected wherever a fleet collects anything — and its records land in
//! journald, which the `logs` sensor already ingests and already security-tags
//! (`_AUDIT_TYPE_NAME` → `sd.journald.audit_type`, `category=security`, #107).
//! So the trail comes back onto the bus and into the GUI Logs view **with no
//! new transport**.
//!
//! # What this does NOT say
//!
//! It records *what was requested and what happened on this host*. It does
//! **not** say *who asked*. The bus caller is anonymous:
//!
//! - [`AuditRecord::caller_zid`] identifies a Zenoh **session**, not a person,
//!   and only when the querier's session filled its source info in.
//! - [`AuditRecord::actor`] and [`AuditRecord::request_id`] are whatever the
//!   caller put in the selector. They are **unauthenticated claims**.
//!
//! Real attribution needs a caller identity — Zenoh mTLS (certificate CN) plus
//! Zenoh's ACL — which is a scope question named in epic #952 and deliberately
//! not answered here. Do not present these fields as authentication.
//!
//! # Reads are not recorded
//!
//! Only procedures the registry declares `kind = "write"` reach this module.
//! A trail that also carries every `introspect` is a trail nobody reads.
//!
//! # Delivery
//!
//! With the `linux-audit` feature, one `AUDIT_USYS_CONFIG` netlink datagram
//! per record — findable with `ausearch -m USYS_CONFIG`. Without it — or with
//! it, on a host that has no audit socket (a container, an unprivileged run,
//! no `CAP_AUDIT_WRITE`) — the same fields go to `tracing` under the target
//! `zensight::audit`. The fallback is loud on purpose: a silent audit path is
//! worse than none, because it looks like one, and [`is_delivering`] is how a
//! sensor says which of the two it has.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock, RwLock};

/// What happened to the request. Both are recorded; a refusal is at least as
/// interesting as an execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Executed,
    Refused,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Executed => "executed",
            Verdict::Refused => "refused",
        }
    }
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A gate's refusal: which switch said no, and the sentence to show a human.
///
/// A refusal used to be a bare `String` that happened to *contain* the switch
/// name, so the audit trail could only be grepped, and #866's "name the switch"
/// contract was enforced by a test reading English. The switch is a field now.
/// [`Display`](std::fmt::Display) yields `message`, so a refusal still reads as
/// its own sentence wherever one was expected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The config key that refused, e.g. `actions.enabled`. `&'static str`
    /// on purpose: a switch is a name in this build, never a runtime value.
    pub switch: &'static str,
    /// The sentence for the operator — which switch, and what to change.
    pub message: String,
}

impl Refusal {
    pub fn new(switch: &'static str, message: impl Into<String>) -> Self {
        Self {
            switch,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<Refusal> for String {
    fn from(r: Refusal) -> String {
        r.message
    }
}

/// One write procedure's outcome.
///
/// Absent fields are **omitted** from the record, never emitted as an empty
/// string: "not supplied" and "supplied as empty" are different facts, and the
/// reader of an audit trail is exactly the person who cares which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRecord {
    /// The registry key of the procedure, `<producer>/<path>`, e.g.
    /// `systemd/action/set`.
    pub procedure: String,
    /// The producer that served it.
    pub producer: String,
    /// The origin the producer publishes under.
    pub origin: String,
    /// What was acted on — unit, outlet, topic, entity.
    pub target: Option<String>,
    pub verdict: Verdict,
    /// The switch that refused, for a refusal (#866). Structured, so the trail
    /// can be filtered on it rather than grepped.
    pub refused_by: Option<String>,
    /// The querier's Zenoh session id, when its source info carried one.
    /// A session, **not** a person — see the module docs.
    pub caller_zid: Option<String>,
    /// `?actor=` from the selector: an unauthenticated claim.
    pub actor: Option<String>,
    /// `?request_id=` from the selector, for correlating with the caller's log.
    pub request_id: Option<String>,
    /// Why an *executed* action did not achieve what it was asked to.
    ///
    /// Not in the same column as [`Self::refused_by`], and deliberately: a
    /// refusal is the gate saying no, this is the gate saying yes and the
    /// system failing anyway. "Journal every user action" is not served by a
    /// trail that records an attempt the operator watched fail as a success.
    pub error: Option<String>,
    pub ts_unix: u64,
}

impl AuditRecord {
    /// A record for `procedure`, taking producer and origin from [`init`].
    pub fn new(procedure: impl Into<String>, verdict: Verdict) -> Self {
        let (producer, origin) = identity();
        Self {
            procedure: procedure.into(),
            producer,
            origin,
            target: None,
            verdict,
            refused_by: None,
            caller_zid: None,
            actor: None,
            request_id: None,
            error: None,
            ts_unix: now_unix(),
        }
    }

    /// The request was carried out.
    pub fn executed(procedure: impl Into<String>) -> Self {
        Self::new(procedure, Verdict::Executed)
    }

    /// The request was refused, by the named switch.
    pub fn refused(procedure: impl Into<String>, refused_by: impl Into<String>) -> Self {
        let mut r = Self::new(procedure, Verdict::Refused);
        r.refused_by = Some(refused_by.into());
        r
    }

    /// The request was refused by a gate, which already knows its switch.
    pub fn refused_by_gate(procedure: impl Into<String>, refusal: &Refusal) -> Self {
        Self::refused(procedure, refusal.switch)
    }

    pub fn with_target(mut self, target: impl Into<String>) -> Self {
        self.target = Some(target.into());
        self
    }

    pub fn with_caller_zid(mut self, zid: Option<String>) -> Self {
        self.caller_zid = zid;
        self
    }

    pub fn with_actor(mut self, actor: Option<String>) -> Self {
        self.actor = cap(actor);
        self
    }

    pub fn with_request_id(mut self, id: Option<String>) -> Self {
        self.request_id = cap(id);
        self
    }

    /// An executed action that did not achieve its aim.
    pub fn with_error(mut self, error: Option<String>) -> Self {
        self.error = error;
        self
    }

    /// Fill `caller_zid`, `actor` and `request_id` from one incoming call.
    pub fn with_request(self, req: &crate::rpc::RpcRequest) -> Self {
        self.with_caller_zid(req.caller_zid.clone())
            .with_actor(req.actor())
            .with_request_id(req.request_id())
    }

    /// The record as an auditd `key=value` line.
    ///
    /// Field order is fixed so the text is diffable and greppable, and absent
    /// fields are omitted. Values that are not plain tokens are hex-encoded,
    /// which is auditd's own convention for untrusted strings — a unit name
    /// with a space in it must not be able to forge a second field.
    pub fn message(&self) -> String {
        let mut s = String::with_capacity(160);
        s.push_str("op=zensight-write");
        push_field(&mut s, "procedure", Some(&self.procedure));
        push_field(&mut s, "producer", Some(&self.producer));
        push_field(&mut s, "origin", Some(&self.origin));
        push_field(&mut s, "target", self.target.as_deref());
        push_field(&mut s, "verdict", Some(self.verdict.as_str()));
        push_field(&mut s, "refused_by", self.refused_by.as_deref());
        push_field(&mut s, "caller_zid", self.caller_zid.as_deref());
        push_field(&mut s, "actor", self.actor.as_deref());
        push_field(&mut s, "request_id", self.request_id.as_deref());
        push_field(&mut s, "error", self.error.as_deref());
        // `res` is the field `ausearch --success` and every audit report reads.
        // Success means "asked for, permitted, and achieved": a refusal is 0,
        // and so is a permitted action that failed.
        let res = u8::from(self.verdict == Verdict::Executed && self.error.is_none());
        s.push_str(&format!(" res={res} ts={}", self.ts_unix));
        s
    }
}

fn push_field(out: &mut String, key: &str, value: Option<&str>) {
    // An empty value is an ABSENT value. `?actor=` with nothing after it would
    // otherwise emit `actor=`, which reads as "the caller claimed to be
    // nobody" — a different and wrong statement from "the caller said nothing".
    let Some(value) = value.filter(|v| !v.is_empty()) else {
        return;
    };
    out.push(' ');
    out.push_str(key);
    out.push('=');
    out.push_str(&encode(value));
}

/// The longest a single caller-supplied value may be.
///
/// The kernel drops a datagram over `MAX_AUDIT_MESSAGE_LENGTH` (8970) without
/// telling anyone — the exact failure that looks like a working audit path. A
/// caller must not be able to reach that by padding `?actor=`.
const MAX_CALLER_VALUE: usize = 256;

fn cap(value: Option<String>) -> Option<String> {
    value.map(|v| {
        if v.len() <= MAX_CALLER_VALUE {
            v
        } else {
            let mut t: String = v.chars().take(MAX_CALLER_VALUE).collect();
            t.push_str("...");
            t
        }
    })
}

/// auditd's encoding for a value: a plain token as-is, anything else as
/// uppercase hex. Quoting is deliberately not used — a value containing a
/// quote would then need escaping, and an escape that a parser gets wrong is
/// how a log line becomes two.
fn encode(value: &str) -> String {
    let plain = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | ':' | '/' | '@'));
    if plain {
        value.to_string()
    } else {
        value.bytes().map(|b| format!("{b:02X}")).collect()
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── process identity ─────────────────────────────────────────────────────────

static IDENTITY: RwLock<Option<(String, String)>> = RwLock::new(None);

/// Register this process as an audit client. Call once, at startup, from a
/// producer that serves at least one write procedure.
///
/// A producer that never writes never calls this and pays nothing: no socket
/// is opened until the first [`record`].
pub fn init(producer: impl Into<String>) {
    let producer = producer.into();
    let origin = crate::PROFILE.local_origin().host_id().as_str().to_string();
    if let Ok(mut slot) = IDENTITY.write() {
        *slot = Some((producer, origin));
    }
}

fn identity() -> (String, String) {
    IDENTITY
        .read()
        .ok()
        .and_then(|s| s.clone())
        .unwrap_or_else(|| ("unknown".to_string(), "unknown".to_string()))
}

// ── which procedures are audited ─────────────────────────────────────────────

/// Whether `producer` declares `path` as a **write** procedure.
///
/// The registry is the authority — the same `kind = "write"` column
/// `introspect` hands the fleet and the conformance judges read. Asking it,
/// rather than keeping a list here, means a new write surface is audited by
/// the act of declaring it.
///
/// Unknown producer, unparseable slice, or an undeclared path all answer
/// `false`: this decides whether to *record*, and a record naming a procedure
/// the registry has never heard of would be noise in the one log that must not
/// have any. So does anything on [`NOT_AUDITED`].
pub fn is_write_procedure(producer: &str, path: &str) -> bool {
    static WRITES: OnceLock<Mutex<HashMap<String, HashSet<String>>>> = OnceLock::new();
    let cache = WRITES.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut cache) = cache.lock() else {
        return false;
    };
    let set = cache.entry(producer.to_string()).or_insert_with(|| {
        crate::registry::registry_toml(producer)
            .and_then(|toml| zenkey::parse_slice(toml).ok())
            .map(|slice| {
                slice
                    .procedures
                    .iter()
                    .filter(|p| {
                        p.kind
                            .as_ref()
                            .and_then(|k| k.known())
                            .is_some_and(|k| matches!(k, zenkey::slice::ProcedureKind::Write))
                    })
                    .map(|p| p.path.clone())
                    .collect()
            })
            .unwrap_or_default()
    });
    set.contains(path)
        && !NOT_AUDITED
            .iter()
            .any(|(p, path_, _)| *p == producer && *path_ == path)
}

/// Write procedures deliberately kept out of the trail, with the reason.
///
/// The shape of `CONDITIONAL_FAMILIES` and `deny.toml`'s per-crate licence
/// exceptions: a short, named list that a test proves still resolves, so an
/// exemption cannot outlive what it excuses or be granted by silence.
pub const NOT_AUDITED: &[(&str, &str, &str)] = &[(
    "parallax",
    "stream/report",
    "receiver feedback, not a command: RFC 07 §1.2 forbids it from changing anything, and it \
     arrives once per interval per (consumer, stream, tier). Auditing it would bury every real \
     action under records of a measurement.",
)];

/// The producer and procedure path a served `@rpc` key belongs to.
///
/// Handles both real shapes — the host form
/// `[base/]v1/<origin>/@rpc/<producer>/<path...>` and the service form
/// `[base/]v1/@catalog/@rpc/<path...>`, whose producer *is* the service origin
/// and which therefore has no producer chunk. Anchored on the `@rpc` segment
/// rather than on a position, because a deployment namespace prefixes the key.
pub fn rpc_route(key: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = key.split('/').collect();
    let at = parts.iter().position(|p| *p == "@rpc")?;
    let origin = parts.get(at.checked_sub(1)?)?;
    if let Some(service) = origin.strip_prefix('@') {
        // `v1/@catalog/@rpc/link` — the service is the producer.
        let path = parts.get(at + 1..)?.join("/");
        (!path.is_empty()).then(|| (service.to_string(), path))
    } else {
        let producer = parts.get(at + 1)?;
        let path = parts.get(at + 2..)?.join("/");
        (!path.is_empty()).then(|| (producer.to_string(), path))
    }
}

// ── delivery ─────────────────────────────────────────────────────────────────

/// Record one outcome. Never fails and never blocks the caller's reply: a
/// trail that can refuse a legitimate action is a worse bug than a missing
/// line.
pub fn record(rec: &AuditRecord) {
    let message = rec.message();
    if emit(&message) {
        return;
    }
    // The fallback carries the SAME fields, structured, so a log pipeline can
    // reconstruct the record when the audit socket is not reachable.
    tracing::warn!(
        target: "zensight::audit",
        procedure = %rec.procedure,
        producer = %rec.producer,
        origin = %rec.origin,
        target = rec.target.as_deref(),
        verdict = %rec.verdict,
        refused_by = rec.refused_by.as_deref(),
        caller_zid = rec.caller_zid.as_deref(),
        actor = rec.actor.as_deref(),
        request_id = rec.request_id.as_deref(),
        "{message}"
    );
}

/// Whether records are actually reaching the kernel's audit subsystem.
///
/// `false` in a build without the feature, on a host with no audit socket, on
/// one that refused the first record — and also *before the first record*,
/// because until one has been acknowledged nothing has been established.
/// Asking opens nothing and changes nothing.
pub fn is_delivering() -> bool {
    delivering()
}

#[cfg(not(feature = "linux-audit"))]
fn emit(_message: &str) -> bool {
    false
}

#[cfg(not(feature = "linux-audit"))]
fn delivering() -> bool {
    false
}

#[cfg(feature = "linux-audit")]
mod netlink {
    //! One record per datagram, written straight to `NETLINK_AUDIT`.
    //!
    //! Deliberately not the C `libaudit`: it is LGPL-2.1+, and `deny.toml`
    //! grants LGPL only as a named per-crate exception. What we send is one
    //! header and one line of text, so the C library would buy nothing and
    //! cost a licence exception plus a build-time system dependency.
    //! `netlink-sys` is MIT and already in the lock file.

    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::{Mutex, OnceLock};

    use netlink_sys::{Socket, SocketAddr, protocols::NETLINK_AUDIT};

    /// `AUDIT_USYS_CONFIG` — "userspace system configuration change"
    /// (`linux/audit.h`). In the 1100–1199 trusted-application range, so it
    /// needs only `CAP_AUDIT_WRITE`, and auditd's default rules keep it.
    ///
    /// **Not 1107.** That is `AUDIT_USER_AVC`, which SELinux tooling reads as
    /// an access-vector denial — every ZenSight record would have been filed
    /// as a policy violation.
    const AUDIT_USYS_CONFIG: u16 = 1111;
    const NLM_F_REQUEST: u16 = 1;
    const NLM_F_ACK: u16 = 4;
    const NLMSG_ERROR: u16 = 2;
    const NLMSG_HDR_LEN: usize = 16;
    /// `MSG_DONTWAIT` on Linux. Spelled out rather than pulling `libc` in for
    /// one integer.
    const MSG_DONTWAIT: i32 = 0x40;

    /// What we know about delivery. Latched: the capability check is not going
    /// to change under a running process, and re-probing per record would put
    /// a syscall storm on the serial `@rpc` handler loop.
    const UNKNOWN: u8 = 0;
    const DELIVERING: u8 = 1;
    const DEGRADED: u8 = 2;

    static STATE: AtomicU8 = AtomicU8::new(UNKNOWN);
    static SOCKET: OnceLock<Option<Mutex<Socket>>> = OnceLock::new();

    /// Whether records are reaching the kernel. Answers `false` until the
    /// first record has been sent *and acknowledged* — it never opens a socket
    /// of its own, so asking does not change the answer.
    pub(super) fn delivering() -> bool {
        STATE.load(Ordering::Relaxed) == DELIVERING
    }

    /// The socket, opened at most once.
    fn socket() -> Option<&'static Mutex<Socket>> {
        SOCKET
            .get_or_init(|| {
                let mut sock = match Socket::new(NETLINK_AUDIT) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!(
                            target: "zensight::audit",
                            error = %e,
                            "no NETLINK_AUDIT socket — write-procedure outcomes will be LOGGED, \
                             not audited. Grant the unit AmbientCapabilities=CAP_AUDIT_WRITE."
                        );
                        STATE.store(DEGRADED, Ordering::Relaxed);
                        return None;
                    }
                };
                // Non-blocking throughout: the `@rpc` handler loop is serial by
                // design (see `zensight_sensor_core::rpc`), and auditd backlog
                // pressure can otherwise park a send for `audit_backlog_wait_time`
                // — a minute by default — with every queued call behind it.
                if let Err(e) = sock.set_non_blocking(true) {
                    tracing::warn!(target: "zensight::audit", error = %e, "audit socket stays blocking");
                }
                if let Err(e) = sock.bind_auto() {
                    tracing::error!(
                        target: "zensight::audit",
                        error = %e,
                        "could not bind the audit socket — outcomes will be logged, not audited"
                    );
                    STATE.store(DEGRADED, Ordering::Relaxed);
                    return None;
                }
                Some(Mutex::new(sock))
            })
            .as_ref()
    }

    /// The datagram: a 16-byte `nlmsghdr` in native byte order, then the text.
    pub(super) fn frame(message: &str, seq: u32) -> Vec<u8> {
        let len = NLMSG_HDR_LEN + message.len();
        let mut buf = Vec::with_capacity(len);
        buf.extend_from_slice(&(len as u32).to_ne_bytes());
        buf.extend_from_slice(&AUDIT_USYS_CONFIG.to_ne_bytes());
        buf.extend_from_slice(&(NLM_F_REQUEST | NLM_F_ACK).to_ne_bytes());
        buf.extend_from_slice(&seq.to_ne_bytes());
        buf.extend_from_slice(&0u32.to_ne_bytes()); // pid: the kernel fills it in
        buf.extend_from_slice(message.as_bytes());
        buf
    }

    /// The error code out of an `NLMSGERR` payload; `Some(0)` is a plain ack.
    pub(super) fn ack_code(buf: &[u8]) -> Option<i32> {
        if buf.len() < NLMSG_HDR_LEN + 4 {
            return None;
        }
        let kind = u16::from_ne_bytes(buf[4..6].try_into().ok()?);
        if kind != NLMSG_ERROR {
            return None;
        }
        Some(i32::from_ne_bytes(buf[16..20].try_into().ok()?))
    }

    pub(super) fn send(message: &str) -> bool {
        if STATE.load(Ordering::Relaxed) == DEGRADED {
            return false;
        }
        let Some(sock) = socket() else { return false };
        let Ok(sock) = sock.lock() else { return false };
        let seq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(1)
            .max(1);
        let buf = frame(message, seq);
        // The kernel is port 0, no multicast group.
        if sock
            .send_to(&buf, &SocketAddr::new(0, 0), MSG_DONTWAIT)
            .is_err()
        {
            // A full send buffer is a dropped record, not a broken path: stay
            // in whatever state we were in and let the caller log this one.
            return false;
        }
        if STATE.load(Ordering::Relaxed) == DELIVERING {
            return true;
        }
        // FIRST record only: read the ack. A netlink permission failure comes
        // back as an NLMSGERR datagram, NOT as a `sendmsg` error — so without
        // this, a process with no CAP_AUDIT_WRITE reports every record as
        // delivered and the trail silently does not exist. The kernel handles
        // an audit message in the sender's own context, so the reply is already
        // queued by the time `send_to` returns and one non-blocking read is
        // enough.
        match sock.recv_from_full() {
            Ok((reply, _)) => match ack_code(&reply) {
                Some(0) | None => {
                    STATE.store(DELIVERING, Ordering::Relaxed);
                    true
                }
                Some(code) => {
                    tracing::error!(
                        target: "zensight::audit",
                        errno = -code,
                        "the kernel refused an audit record (CAP_AUDIT_WRITE?) — write-procedure \
                         outcomes will be LOGGED, not audited, for the life of this process. \
                         Add AmbientCapabilities=CAP_AUDIT_WRITE to the unit."
                    );
                    STATE.store(DEGRADED, Ordering::Relaxed);
                    false
                }
            },
            // Nothing to read yet: the send went out and we have no evidence
            // against it. Do not latch either way — the next record asks again.
            Err(_) => true,
        }
    }
}

#[cfg(feature = "linux-audit")]
fn emit(message: &str) -> bool {
    netlink::send(message)
}

#[cfg(feature = "linux-audit")]
fn delivering() -> bool {
    netlink::delivering()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec() -> AuditRecord {
        AuditRecord {
            procedure: "systemd/action/set".to_string(),
            producer: "systemd".to_string(),
            origin: "host-abc".to_string(),
            target: Some("nginx.service".to_string()),
            verdict: Verdict::Refused,
            refused_by: Some("actions.enabled".to_string()),
            caller_zid: Some("a1b2c3".to_string()),
            actor: None,
            request_id: None,
            error: None,
            ts_unix: 1_756_000_000,
        }
    }

    /// The text is the record. If it moves, every downstream filter someone
    /// wrote against it breaks silently, so it is pinned here rather than
    /// described.
    #[test]
    fn a_refusal_is_field_complete_and_stable() {
        assert_eq!(
            rec().message(),
            "op=zensight-write procedure=systemd/action/set producer=systemd origin=host-abc \
             target=nginx.service verdict=refused refused_by=actions.enabled caller_zid=a1b2c3 \
             res=0 ts=1756000000"
        );
    }

    /// A refusal must carry the switch that refused it (#866) — that is the
    /// field an operator filters on when asking "what is turned off here".
    #[test]
    fn a_refusal_names_the_switch_and_an_execution_has_nothing_to_name() {
        let refused = rec();
        assert!(refused.message().contains("refused_by=actions.enabled"));

        let done = AuditRecord {
            verdict: Verdict::Executed,
            refused_by: None,
            ..rec()
        };
        assert!(done.message().contains("verdict=executed"));
        assert!(
            !done.message().contains("refused_by"),
            "an executed action has no switch to name: {}",
            done.message()
        );
    }

    /// Absent is absent. An empty `actor=` would read as "the caller claimed to
    /// be nobody", which is a different and wrong statement.
    #[test]
    fn unsupplied_fields_are_omitted_not_emptied() {
        let m = rec().message();
        assert!(!m.contains("actor="), "{m}");
        assert!(!m.contains("request_id="), "{m}");

        let claimed = rec().with_actor(Some("alice".to_string()));
        assert!(claimed.message().contains("actor=alice"));
    }

    /// A target with a space in it must not be able to forge a second field.
    #[test]
    fn a_value_that_could_forge_a_field_is_hex_encoded() {
        let sneaky = AuditRecord {
            target: Some("nginx verdict=executed".to_string()),
            ..rec()
        };
        let m = sneaky.message();
        assert!(
            !m.contains("nginx verdict=executed"),
            "the raw value reached the record: {m}"
        );
        // "n" == 0x6E, and exactly one `verdict=` field survives.
        assert!(m.contains("target=6E"), "{m}");
        assert_eq!(m.matches("verdict=").count(), 1, "{m}");
    }

    #[test]
    fn plain_tokens_stay_readable() {
        assert_eq!(encode("nginx.service"), "nginx.service");
        assert_eq!(encode("pdu-a/outlet:3"), "pdu-a/outlet:3");
        assert_eq!(encode("a b"), "612062");
        assert_eq!(encode(""), "");
    }

    /// The constructors are what call sites use; they must produce the same
    /// record as the struct literal the tests above pin.
    #[test]
    fn the_constructors_agree_with_the_fields() {
        let r =
            AuditRecord::refused("snmp/action/set", "actions.allow_outlets").with_target("pdu-a/3");
        assert_eq!(r.verdict, Verdict::Refused);
        assert_eq!(r.refused_by.as_deref(), Some("actions.allow_outlets"));
        assert_eq!(r.target.as_deref(), Some("pdu-a/3"));

        let r = AuditRecord::executed("snmp/action/set");
        assert_eq!(r.verdict, Verdict::Executed);
        assert_eq!(r.refused_by, None);
    }

    /// `init` is what makes producer and origin real; without it a record is
    /// still emitted, marked `unknown`, rather than being dropped. A dropped
    /// audit record is the one outcome this module must never have.
    #[test]
    fn a_record_without_init_is_still_a_record() {
        let r = AuditRecord::executed("hostspec/expectations/set");
        assert!(!r.producer.is_empty());
        assert!(!r.message().is_empty());
    }

    /// Without the feature there is no socket, and `is_delivering` says so
    /// rather than implying a trail that does not exist.
    #[cfg(not(feature = "linux-audit"))]
    #[test]
    fn a_build_without_the_feature_admits_it_is_not_delivering() {
        assert!(!is_delivering());
    }

    // ── the classifier and the parser it depends on ──────────────────────

    /// Every `kind = "write"` in the workspace's registries, and nothing else.
    ///
    /// `is_write_procedure` filters on `kind.known()`, so a spelling this build
    /// of zenkey does not recognise silently makes *everything* a read: nothing
    /// would be audited, and nothing would fail. This is the test that notices.
    #[test]
    fn every_declared_write_is_classified_as_one_and_no_read_is() {
        let mut writes = 0usize;
        for (producer, toml) in crate::registry::REGISTRIES {
            let slice = zenkey::parse_slice(toml).expect("a shipped registry slice parses");
            for p in &slice.procedures {
                let declared_write = toml.contains(&format!("path = \"{}\"", p.path))
                    && p.kind
                        .as_ref()
                        .and_then(|k| k.known())
                        .is_some_and(|k| matches!(k, zenkey::slice::ProcedureKind::Write));
                let exempt = NOT_AUDITED
                    .iter()
                    .any(|(pr, path, _)| pr == producer && *path == p.path);
                assert_eq!(
                    is_write_procedure(producer, &p.path),
                    declared_write && !exempt,
                    "{producer}/{} classified wrongly (declared_write={declared_write}, \
                     exempt={exempt})",
                    p.path
                );
                if declared_write {
                    writes += 1;
                }
            }
        }
        assert!(
            writes >= 30,
            "only {writes} write procedures found — the registry walk is not seeing the slices"
        );
    }

    /// Every exemption names a procedure that is still declared as a write.
    /// Rename `stream/report` and this fails, rather than the exemption quietly
    /// excusing nothing while the real procedure goes unaudited.
    #[test]
    fn every_exemption_still_names_a_live_write_procedure() {
        for (producer, path, reason) in NOT_AUDITED {
            let toml = crate::registry::registry_toml(producer)
                .unwrap_or_else(|| panic!("NOT_AUDITED names unknown producer {producer}"));
            let slice = zenkey::parse_slice(toml).expect("slice parses");
            let decl = slice
                .procedures
                .iter()
                .find(|p| p.path == *path)
                .unwrap_or_else(|| panic!("NOT_AUDITED names unknown procedure {producer}/{path}"));
            assert!(
                decl.kind
                    .as_ref()
                    .and_then(|k| k.known())
                    .is_some_and(|k| matches!(k, zenkey::slice::ProcedureKind::Write)),
                "{producer}/{path} is exempted from the audit trail but is not a write \
                 procedure — the exemption excuses nothing"
            );
            assert!(
                reason.len() > 40,
                "{producer}/{path} is exempted with no real reason"
            );
        }
    }

    /// The parser has to survive both key shapes, including `@catalog`'s, which
    /// has no producer chunk at all.
    #[test]
    fn a_served_key_resolves_to_its_producer_and_path() {
        assert_eq!(
            rpc_route("v1/h-abc/@rpc/systemd/action/set"),
            Some(("systemd".into(), "action/set".into()))
        );
        assert_eq!(
            rpc_route("v1/@catalog/@rpc/link"),
            Some(("catalog".into(), "link".into()))
        );
        // A deployment namespace prefixes the key; the anchor is `@rpc`.
        assert_eq!(
            rpc_route("site-a/v1/h-abc/@rpc/snmp/artifact/request"),
            Some(("snmp".into(), "artifact/request".into()))
        );
        // Serve-side wildcards keep their shape.
        assert_eq!(
            rpc_route("v1/*/@rpc/historian/range"),
            Some(("historian".into(), "range".into()))
        );
        assert_eq!(rpc_route("v1/h-abc/state/systemd/health"), None);
        assert_eq!(rpc_route("v1/h-abc/@rpc/systemd"), None);
    }

    /// Every write path resolves through the parser it will be classified by.
    /// A `{var}` in a write path would need `*` widening on the serve side; none
    /// exists today, and this is where a new one gets noticed.
    #[test]
    fn every_write_path_round_trips_through_the_parser() {
        for (producer, toml) in crate::registry::REGISTRIES {
            let slice = zenkey::parse_slice(toml).expect("slice parses");
            for p in slice.procedures.iter().filter(|p| {
                p.kind
                    .as_ref()
                    .and_then(|k| k.known())
                    .is_some_and(|k| matches!(k, zenkey::slice::ProcedureKind::Write))
            }) {
                assert!(
                    !p.path.contains('{'),
                    "{producer}/{} is a write procedure with a variable chunk — the serve-side \
                     spelling widens it to `*`, so rpc_route and is_write_procedure need to as \
                     well before this can land",
                    p.path
                );
                let key = format!("v1/h-abc/@rpc/{producer}/{}", p.path);
                assert_eq!(
                    rpc_route(&key),
                    Some((producer.to_string(), p.path.clone())),
                    "{key} did not resolve back to itself"
                );
            }
        }
    }

    /// A permitted action that failed is not a success, and `res=` is the field
    /// every audit report reads.
    #[test]
    fn res_is_zero_for_a_refusal_and_for_an_execution_that_failed() {
        assert!(rec().message().contains(" res=0 "));

        let ok = AuditRecord {
            verdict: Verdict::Executed,
            refused_by: None,
            ..rec()
        };
        assert!(ok.message().contains(" res=1 "), "{}", ok.message());

        let failed = AuditRecord {
            verdict: Verdict::Executed,
            refused_by: None,
            error: Some("D-Bus proxy failed".to_string()),
            ..rec()
        };
        let m = failed.message();
        assert!(
            m.contains(" res=0 "),
            "a failed action is not a success: {m}"
        );
        assert!(m.contains("verdict=executed"), "{m}");
    }

    /// `?actor=` with nothing after it is not a claim.
    #[test]
    fn an_empty_caller_value_is_absent_not_empty() {
        let r = rec().with_actor(Some(String::new()));
        assert!(!r.message().contains("actor="), "{}", r.message());
    }

    /// A caller must not be able to push the datagram past the kernel's
    /// `MAX_AUDIT_MESSAGE_LENGTH`, where it is dropped without a word.
    #[test]
    fn a_caller_value_cannot_grow_without_bound() {
        let r = rec().with_actor(Some("a".repeat(10_000)));
        assert!(r.actor.as_ref().unwrap().len() < 300);
        assert!(r.message().len() < 1_000, "{}", r.message().len());
    }

    /// Asking whether we are delivering must not be what makes us try.
    #[test]
    fn asking_about_delivery_opens_nothing() {
        let first = is_delivering();
        assert_eq!(first, is_delivering());
    }

    /// The kernel reports an audit permission failure as an `NLMSGERR`
    /// datagram, not as a `sendmsg` error. Reading it is the difference between
    /// a real trail and one that only looks like one.
    #[cfg(feature = "linux-audit")]
    #[test]
    fn a_negative_ack_is_read_out_of_the_reply() {
        let mut ok = netlink::frame("", 1);
        ok[4..6].copy_from_slice(&2u16.to_ne_bytes()); // NLMSG_ERROR
        ok.extend_from_slice(&0i32.to_ne_bytes());
        assert_eq!(netlink::ack_code(&ok), Some(0));

        let mut denied = netlink::frame("", 1);
        denied[4..6].copy_from_slice(&2u16.to_ne_bytes());
        denied.extend_from_slice(&(-1i32).to_ne_bytes()); // -EPERM
        assert_eq!(netlink::ack_code(&denied), Some(-1));

        // Anything that is not an NLMSGERR carries no verdict.
        assert_eq!(netlink::ack_code(&netlink::frame("hello", 1)), None);
        assert_eq!(netlink::ack_code(&[0u8; 4]), None);
    }

    /// The header the kernel parses: length first, then type, then flags. A
    /// wrong length is silently dropped by the kernel, which is exactly the
    /// failure that looks like a working audit path.
    #[cfg(feature = "linux-audit")]
    #[test]
    fn the_datagram_is_a_well_formed_nlmsghdr() {
        let buf = netlink::frame("op=zensight-write verdict=executed", 7);
        assert_eq!(buf.len(), 16 + "op=zensight-write verdict=executed".len());
        assert_eq!(
            u32::from_ne_bytes(buf[0..4].try_into().unwrap()),
            buf.len() as u32
        );
        // 1111 = AUDIT_USYS_CONFIG. NOT 1107 (AUDIT_USER_AVC), which SELinux
        // tooling reads as an access-vector denial.
        assert_eq!(u16::from_ne_bytes(buf[4..6].try_into().unwrap()), 1111);
        // REQUEST | ACK — without the ack we cannot tell a delivered record
        // from a silently refused one.
        assert_eq!(u16::from_ne_bytes(buf[6..8].try_into().unwrap()), 1 | 4);
        assert_eq!(u32::from_ne_bytes(buf[8..12].try_into().unwrap()), 7);
        assert_eq!(&buf[16..], b"op=zensight-write verdict=executed");
    }
}
