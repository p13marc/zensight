//! Control-channel command primitives shared by sensors and the frontend.
//!
//! Sensors expose runtime control via two conventional channels under their
//! key prefix:
//! - commands (pub/sub): `zensight/<protocol>/@/commands/<topic>`
//! - status (queryable): `zensight/<protocol>/@/status/<topic>`
//!
//! A "topic" namespaces a control surface — e.g. `filter` (syslog),
//! `expectations` (the sentinel), `detectors` (netring). The payload type is
//! topic-specific; wrap it in [`Command`] when you want an optional correlation
//! id for matching an async reply.

use serde::{Deserialize, Serialize};

/// Build the command key for a sensor `prefix` and `topic`.
///
/// # Example
/// ```
/// use zensight_common::command::command_key;
/// assert_eq!(command_key("zensight/syslog", "filter"), "zensight/syslog/@/commands/filter");
/// ```
pub fn command_key(prefix: &str, topic: &str) -> String {
    format!("{}/@/commands/{}", prefix, topic)
}

/// Build the status (queryable) key for a sensor `prefix` and `topic`.
///
/// # Example
/// ```
/// use zensight_common::command::status_key;
/// assert_eq!(status_key("zensight/syslog", "filter"), "zensight/syslog/@/status/filter");
/// ```
pub fn status_key(prefix: &str, topic: &str) -> String {
    format!("{}/@/status/{}", prefix, topic)
}

/// Build the on-demand detail-query (queryable) key for a sensor `prefix` and
/// `topic`. High-cardinality detail (flow tables, socket lists, …) is served
/// here on request, never streamed onto the telemetry bus.
///
/// # Example
/// ```
/// use zensight_common::command::query_key;
/// assert_eq!(query_key("zensight/netring", "flows"), "zensight/netring/@/query/flows");
/// ```
pub fn query_key(prefix: &str, topic: &str) -> String {
    format!("{}/@/query/{}", prefix, topic)
}

/// Optional envelope carrying a correlation id alongside a command body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Command<T> {
    /// Optional correlation id, echoed in a reply for request/response matching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The topic-specific command payload.
    pub body: T,
}

impl<T> Command<T> {
    pub fn new(body: T) -> Self {
        Self { id: None, body }
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_builders() {
        assert_eq!(
            command_key("zensight/netlink", "expectations"),
            "zensight/netlink/@/commands/expectations"
        );
        assert_eq!(
            status_key("zensight/netring", "detectors"),
            "zensight/netring/@/status/detectors"
        );
    }

    #[test]
    fn command_envelope_roundtrip() {
        let cmd = Command::new(serde_json::json!({"a": 1})).with_id("x");
        let bytes = crate::encode(&cmd, crate::Format::Json).unwrap();
        let back: Command<serde_json::Value> = crate::decode(&bytes, crate::Format::Json).unwrap();
        assert_eq!(back.id.as_deref(), Some("x"));
        assert_eq!(back.body["a"], 1);
    }
}
