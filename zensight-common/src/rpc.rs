//! The `@rpc` call contract (RFC 05 §3): what a call carries, and what a
//! failure looks like.
//!
//! These types live in `zensight-common`, not in `zensight-sensor-core`, for the
//! same reason every other reply type does: **a caller is not a sensor.** The
//! correlator serves `@catalog/@rpc/link` and does not (and must not) depend on
//! the sensor framework; the GUI and `zenctl` call procedures and depend on
//! neither. A shared wire contract that lives in one participant's framework is
//! not shared — it is that participant's private type that everyone else has to
//! reach through it to reach (a lesson the #477 cash-in already paid for once).
//!
//! `zensight-sensor-core::rpc` re-exports these, so a sensor's imports are
//! unchanged; it keeps the *serving* machinery (declare-before-alive, reply on
//! the concrete key), which genuinely is sensor-framework business.

use serde::{Deserialize, Serialize};

/// Convention-reserved error names (RFC 05 §3).
pub const ERR_INVALID_ARGS: &str = "error/invalid-args";
pub const ERR_UNAUTHORIZED: &str = "error/unauthorized";
pub const ERR_NOT_FOUND: &str = "error/not-found";
pub const ERR_UNSUPPORTED: &str = "error/unsupported";
pub const ERR_BUSY: &str = "error/busy";
pub const ERR_GATED: &str = "error/gated";

/// Reserved selector parameters every write procedure understands (#957).
///
/// Both are **caller-supplied and unauthenticated** — see [`crate::audit`].
/// They are named here so the audit record and the call sites cannot spell
/// them differently.
pub const PARAM_ACTOR: &str = "actor";
pub const PARAM_REQUEST_ID: &str = "request_id";

/// A procedure failure: namespaced name + human message. Serialized as the
/// `reply_err` payload — a value reply always means success.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub error: String,
    pub message: String,
    /// The config switch that refused, when a gate refused (#866, #957).
    ///
    /// Additive on the wire and skipped when absent, so an older caller is
    /// unaffected. It exists so the refusal is machine-readable for the
    /// *caller* too, not only for the host's audit trail — a GUI can name the
    /// switch without parsing an English sentence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused_by: Option<String>,
}

impl RpcError {
    pub fn new(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: name.into(),
            message: message.into(),
            refused_by: None,
        }
    }

    /// Name the switch that refused this call.
    pub fn with_refused_by(mut self, switch: impl Into<String>) -> Self {
        self.refused_by = Some(switch.into());
        self
    }

    pub fn invalid_args(message: impl Into<String>) -> Self {
        Self::new(ERR_INVALID_ARGS, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ERR_NOT_FOUND, message)
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(ERR_UNSUPPORTED, message)
    }

    pub fn gated(message: impl Into<String>) -> Self {
        Self::new(ERR_GATED, message)
    }

    /// The caller asked too often, or asked for something already in flight.
    ///
    /// `ERR_BUSY` has been in the RFC 05 vocabulary since the start and had no
    /// constructor until a rate-limited procedure needed one (#715). A refusal
    /// carrying the limit lets a caller back off machine-readably instead of
    /// guessing.
    pub fn busy(message: impl Into<String>) -> Self {
        Self::new(ERR_BUSY, message)
    }

    /// A producer-specific failure: `error/<producer>/<slug>`.
    pub fn producer(producer: &str, slug: &str, message: impl Into<String>) -> Self {
        Self::new(format!("error/{producer}/{slug}"), message)
    }
}

/// One incoming call.
#[derive(Debug, Clone)]
pub struct RpcRequest {
    /// The query payload (request body), empty when none.
    pub payload: Vec<u8>,
    /// Zenoh selector parameters (`?a=1;b=2`), raw.
    pub parameters: String,
    /// The querier's Zenoh session id, when its source info carried one.
    ///
    /// This is the only caller fact the bus provides, and it identifies a
    /// **session, not a person** — see [`crate::audit`]. It is `None` whenever
    /// the querier did not fill its source info in, and an absent value is
    /// recorded as absent rather than guessed at.
    pub caller_zid: Option<String>,
}

impl RpcRequest {
    /// A request with no caller information — the shape every call site used
    /// before `caller_zid` existed, kept so a test or a synthetic call does not
    /// have to spell `None`.
    pub fn new(payload: Vec<u8>, parameters: impl Into<String>) -> Self {
        Self {
            payload,
            parameters: parameters.into(),
            caller_zid: None,
        }
    }

    /// Build a request from one incoming Zenoh query, carrying whatever the
    /// bus can say about the caller.
    ///
    /// `caller_zid` comes from the query's source info, which the *querier's*
    /// session fills in. A querier that did not leaves it `None`, and that is
    /// recorded as absent — never as a placeholder that would read like an
    /// identification.
    pub fn from_query(query: &zenoh::query::Query) -> Self {
        Self {
            payload: query
                .payload()
                .map(|p| p.to_bytes().to_vec())
                .unwrap_or_default(),
            parameters: query.parameters().as_str().to_string(),
            caller_zid: query.source_info().map(|s| s.source_id().zid().to_string()),
        }
    }

    /// Decode the request body, **JSON or CBOR** (#1148).
    ///
    /// `docs/data-model.md` says "every consumer decodes via `decode_auto`",
    /// and the write half of the generic `<topic>/set` seam did not: it was
    /// `serde_json::from_slice`, so a caller whose session serialises CBOR —
    /// which is this tree's default — got `error/invalid-args` from
    /// `logs rules/set`, `netlink expectations/set` and `collection/set`, and
    /// `hostspec` and `systemd` `expectations/set`. A read procedure answered
    /// them and the write beside it did not.
    pub fn decode<T: serde::de::DeserializeOwned>(&self) -> std::result::Result<T, RpcError> {
        crate::serialization::decode_auto(&self.payload)
            .map_err(|e| RpcError::invalid_args(format!("bad request body: {e}")))
    }

    /// Decode the request body as JSON, specifically.
    ///
    /// Prefer [`decode`](Self::decode) — a caller's format is the caller's
    /// business. This stays for a body whose contract really is JSON and
    /// nothing else.
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> std::result::Result<T, RpcError> {
        serde_json::from_slice(&self.payload)
            .map_err(|e| RpcError::invalid_args(format!("bad request body: {e}")))
    }

    /// The caller's claimed actor (`?actor=`), when it made one.
    ///
    /// Unauthenticated: this is whatever the caller typed. See
    /// [`crate::audit`] for why that is recorded anyway and how it must not be
    /// used.
    pub fn actor(&self) -> Option<String> {
        self.param(PARAM_ACTOR)
    }

    /// The caller's correlation id (`?request_id=`), when it supplied one.
    pub fn request_id(&self) -> Option<String> {
        self.param(PARAM_REQUEST_ID)
    }

    /// One selector parameter by name, **percent-decoded** (#1122).
    ///
    /// A selector's parameters are `;`-separated `k=v` pairs, so a value
    /// carrying `;`, `=`, `?` or a space silently splits into something else —
    /// which is why callers encode. They were not decoded here, so an
    /// operator's name went into the audit record as `Ada%20Lovelace` and a
    /// search for `foo bar` reached the matcher as `foo%20bar`.
    ///
    /// A value with no `%` in it decodes to itself, so an unencoded caller is
    /// unaffected.
    pub fn param(&self, name: &str) -> Option<String> {
        self.parameters.split(';').find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            (k == name).then(|| percent_decode(v))
        })
    }
}

/// Percent-decode one selector-parameter value (#1122).
///
/// **A malformed escape is left exactly as it arrived**, never guessed at.
/// `100%` is a perfectly ordinary thing to search a log for, and turning it
/// into a decode error — or into some other byte — would break a query that an
/// operator has every right to make. Only a complete, valid `%XX` is consumed.
#[must_use]
pub fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        // The overwhelmingly common case, and the one that keeps an unencoded
        // caller working unchanged.
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(b) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode one selector-parameter value (#1122).
///
/// The inverse of [`percent_decode`], and the same table the GUI's ack path
/// has used since #925 — unreserved characters through, everything else
/// `%XX`.
#[must_use]
pub fn percent_encode(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for b in v.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Successful reply bytes (already encoded — typically JSON).
pub type RpcResult = std::result::Result<Vec<u8>, RpcError>;

#[cfg(test)]
mod tests {

    /// The encoder and the decoder are inverses, and a malformed escape
    /// survives (#1122).
    #[test]
    fn selector_values_round_trip_and_malformed_escapes_survive() {
        for v in [
            "plain",
            "foo bar", // the space that used to go in raw
            "a;b",     // the `;` that used to disable the filter
            "a?b",
            "k=v",
            "100%", // a perfectly ordinary thing to search for
            "Ada Lovelace",
            "%zz",
            "a%2",
            "üñïçø∂é",
        ] {
            assert_eq!(percent_decode(&percent_encode(v)), v, "round trip: {v:?}");
        }

        // A value that was never encoded decodes to itself — which is what
        // keeps an older caller working against a decoding sensor.
        assert_eq!(percent_decode("foo bar"), "foo bar");
        assert_eq!(percent_decode("a;b"), "a;b");

        // Malformed escapes are left alone rather than guessed at.
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("a%2"), "a%2");
    }

    /// A parameter value is decoded on the way out (#1122).
    ///
    /// It was not, so an operator's name reached the audit record as
    /// `Ada%20Lovelace` and a log search for `foo bar` reached the matcher as
    /// `foo%20bar`.
    #[test]
    fn a_parameter_is_decoded_when_it_is_read() {
        let req = RpcRequest::new(
            Vec::new(),
            format!(
                "actor={};pattern={}",
                percent_encode("Ada Lovelace"),
                percent_encode("foo; bar")
            ),
        );
        assert_eq!(req.actor().as_deref(), Some("Ada Lovelace"));
        assert_eq!(req.param("pattern").as_deref(), Some("foo; bar"));
    }

    use super::*;

    #[test]
    fn params_are_semicolon_separated() {
        let req = RpcRequest::new(Vec::new(), "old=h-aaa;new=h-bbb");
        assert_eq!(req.param("old").as_deref(), Some("h-aaa"));
        assert_eq!(req.param("new").as_deref(), Some("h-bbb"));
        assert_eq!(req.param("missing"), None);
    }
}
