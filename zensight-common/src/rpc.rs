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

/// A procedure failure: namespaced name + human message. Serialized as the
/// `reply_err` payload — a value reply always means success.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub error: String,
    pub message: String,
}

impl RpcError {
    pub fn new(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: name.into(),
            message: message.into(),
        }
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
}

impl RpcRequest {
    /// Decode the JSON request body.
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> std::result::Result<T, RpcError> {
        serde_json::from_slice(&self.payload)
            .map_err(|e| RpcError::invalid_args(format!("bad request body: {e}")))
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
        let req = RpcRequest {
            payload: Vec::new(),
            parameters: "old=h-aaa;new=h-bbb".to_string(),
        };
        assert_eq!(req.param("old").as_deref(), Some("h-aaa"));
        assert_eq!(req.param("new").as_deref(), Some("h-bbb"));
        assert_eq!(req.param("missing"), None);
    }
}
