//! The `@desired` reconcile contract's shared types (#816).
//!
//! A controller publishes per-host runtime POLICY under the `@desired`
//! service origin (`v1/@desired/state/<host>/<producer>/<topic>` — RFC 07
//! §3, RFC 12 §3); the target sensor reconciles it over its file config and
//! publishes an [`AppliedConfig`] marker saying what is actually in force,
//! so drift between desired and effective is visible rather than assumed.
//!
//! **The never-list** (the single most important constraint in #816):
//! nothing under `@desired` may carry secrets or anything a sensor needs to
//! REACH THE BUS — endpoints, TLS material, the namespace. One bad desired
//! publish must never lock the fleet out of its own supervision. The
//! consumer enforces this structurally: the reconciler only deserializes a
//! sentinel's own config type and only writes that sentinel's handle.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}
fn default_refresh_secs() -> u64 {
    300
}

/// Per-sensor file-config block for the `@desired` path: the kill switch and
/// the reconcile cadence. Lives in FILE config on purpose — the mechanism
/// that could misbehave must be disarmable from outside itself.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DesiredConfig {
    /// `false` makes this sensor ignore `@desired` entirely (the marker then
    /// says `source: file`). The kill switch for the case where the
    /// mechanism itself is the problem.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Periodic re-seed GET interval, seconds. The GET against the
    /// deployment's `@desired` storage is the PRIMARY convergence path —
    /// level-triggered, it survives any missed sample, reconnect or router
    /// restart; the live subscription is the accelerator.
    #[serde(default = "default_refresh_secs")]
    pub refresh_secs: u64,
}

impl Default for DesiredConfig {
    fn default() -> Self {
        DesiredConfig {
            enabled: true,
            refresh_secs: default_refresh_secs(),
        }
    }
}

/// Where the config actually in force came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AppliedSource {
    /// The file/bootstrap baseline (also: kill switch on, or a desired
    /// delete reverted to baseline).
    File,
    /// A `@desired` document.
    Desired,
    /// An operator's `@rpc/<producer>/<topic>/set`. Two writers exist and
    /// the rule is LWW by arrival; this marker is what says who won last.
    Rpc,
}

/// A desired document the sensor REFUSED, kept on the marker until a good
/// one supersedes it — the rejection is on the bus, not only in a log.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RejectedDesired {
    /// Millis since epoch when the rejection happened.
    pub at: i64,
    /// The rejected sample's HLC timestamp, when it carried one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// The validation/decode error, verbatim.
    pub error: String,
}

/// The effective-config marker (`state/<producer>/applied/<topic>`): what is
/// actually in force for one reconcilable topic, and why. Published on every
/// change of the answer — including at startup, so drift is visible before
/// any desired doc exists.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AppliedConfig {
    pub topic: String,
    pub source: AppliedSource,
    /// Millis since epoch when this config took effect.
    pub applied_at: i64,
    /// The applied desired sample's HLC timestamp (`source: desired` only) —
    /// the drift-correlation handle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired_timestamp: Option<String>,
    /// The document actually in force, JSON-encoded. A string, deliberately:
    /// its real schema is the TOPIC's own type (named by `topic` and served
    /// by the producer's `describe`), and a `Value` field here would be the
    /// anything-goes stub the #815 schema gate exists to refuse. Consumers
    /// wanting structure parse it with the topic type.
    pub effective_json: String,
    /// The most recent rejected desired doc, until superseded by a good one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_rejected: Option<RejectedDesired>,
}
