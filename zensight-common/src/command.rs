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
/// assert_eq!(command_key("zensight/logs", "filter"), "zensight/logs/@/commands/filter");
/// ```
pub fn command_key(prefix: &str, topic: &str) -> String {
    format!("{}/@/commands/{}", prefix, topic)
}

/// Build the status (queryable) key for a sensor `prefix` and `topic`.
///
/// # Example
/// ```
/// use zensight_common::command::status_key;
/// assert_eq!(status_key("zensight/logs", "filter"), "zensight/logs/@/status/filter");
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

/// Build the report-request (subscriber) key: PUT a `ReportRequest` here to ask a
/// sensor to generate a debug report.
///
/// # Example
/// ```
/// use zensight_common::command::report_request_key;
/// assert_eq!(report_request_key("zensight/netlink"), "zensight/netlink/@/report/request");
/// ```
pub fn report_request_key(prefix: &str) -> String {
    format!("{prefix}/@/report/request")
}

/// Build the report-status (queryable) key: GET a `ReportStatus` to track a
/// report's lifecycle.
pub fn report_status_key(prefix: &str) -> String {
    format!("{prefix}/@/report/status")
}

/// Build the report-cancel (subscriber) key: PUT a report id (ULID string) to
/// free the sensor's temp artifact early.
pub fn report_cancel_key(prefix: &str) -> String {
    format!("{prefix}/@/report/cancel")
}

/// Build the key prefix of the `zenoh-blob` server that serves report bytes.
/// The blob lives under `<prefix>/@/report/blob/<id>/…` (kept under its own
/// `blob/` segment so the blob queryable on `…/blob/**` cannot collide with the
/// `…/report/status` or `…/report/request` channels).
pub fn report_blob_prefix(prefix: &str) -> String {
    format!("{prefix}/@/report/blob")
}

/// Build the snapshot-request (subscriber) key: PUT a `SnapshotRequest` here to
/// ask a sensor to package an allowlisted directory (Tier-2).
///
/// # Example
/// ```
/// use zensight_common::command::snapshot_request_key;
/// assert_eq!(snapshot_request_key("zensight/sysinfo"), "zensight/sysinfo/@/snapshot/request");
/// ```
pub fn snapshot_request_key(prefix: &str) -> String {
    format!("{prefix}/@/snapshot/request")
}

/// Build the snapshot-status (queryable) key: GET a `SnapshotStatus` to track a
/// snapshot's lifecycle and learn the advertised directories.
pub fn snapshot_status_key(prefix: &str) -> String {
    format!("{prefix}/@/snapshot/status")
}

/// Build the snapshot-cancel (subscriber) key: PUT a snapshot id (ULID string) to
/// free the sensor's temp chunk store early.
pub fn snapshot_cancel_key(prefix: &str) -> String {
    format!("{prefix}/@/snapshot/cancel")
}

/// Build the key prefix of the content-addressed chunk queryable (Tier-2). Chunks
/// live at `<prefix>/@/store/<algo>/<hash>` — immutable, so cacheable fleet-wide.
pub fn snapshot_store_prefix(prefix: &str) -> String {
    format!("{prefix}/@/store")
}

/// Build the key prefix of the tree-index queryable (Tier-2). An index lives at
/// `<prefix>/@/tree/<id>`.
pub fn snapshot_tree_prefix(prefix: &str) -> String {
    format!("{prefix}/@/tree")
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
    fn report_key_builders() {
        let p = "zensight/netlink";
        assert_eq!(report_request_key(p), "zensight/netlink/@/report/request");
        assert_eq!(report_status_key(p), "zensight/netlink/@/report/status");
        assert_eq!(report_cancel_key(p), "zensight/netlink/@/report/cancel");
        assert_eq!(report_blob_prefix(p), "zensight/netlink/@/report/blob");
        // The blob server (queryable on `…/blob/**`) must not collide with the
        // status/request channels.
        assert!(report_status_key(p).starts_with(&format!("{p}/@/report/")));
        assert!(!report_status_key(p).starts_with(&report_blob_prefix(p)));
    }

    #[test]
    fn snapshot_key_builders() {
        let p = "zensight/sysinfo";
        assert_eq!(
            snapshot_request_key(p),
            "zensight/sysinfo/@/snapshot/request"
        );
        assert_eq!(snapshot_status_key(p), "zensight/sysinfo/@/snapshot/status");
        assert_eq!(snapshot_cancel_key(p), "zensight/sysinfo/@/snapshot/cancel");
        assert_eq!(snapshot_store_prefix(p), "zensight/sysinfo/@/store");
        assert_eq!(snapshot_tree_prefix(p), "zensight/sysinfo/@/tree");
        // The store/tree queryables (declared on `…/**`) must not swallow the
        // request/status/cancel control channels.
        assert!(!snapshot_request_key(p).starts_with(&snapshot_store_prefix(p)));
        assert!(!snapshot_request_key(p).starts_with(&snapshot_tree_prefix(p)));
        // …nor collide with the Tier-1 report surface.
        assert_ne!(snapshot_store_prefix(p), report_blob_prefix(p));
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
