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

    /// Decode the JSON request body.
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

    /// One selector parameter by name.
    pub fn param(&self, name: &str) -> Option<String> {
        self.parameters.split(';').find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            (k == name).then(|| v.to_string())
        })
    }
}

/// Successful reply bytes (already encoded — typically JSON).
pub type RpcResult = std::result::Result<Vec<u8>, RpcError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_are_semicolon_separated() {
        let req = RpcRequest::new(Vec::new(), "old=h-aaa;new=h-bbb");
        assert_eq!(req.param("old").as_deref(), Some("h-aaa"));
        assert_eq!(req.param("new").as_deref(), Some("h-bbb"));
        assert_eq!(req.param("missing"), None);
    }
}
