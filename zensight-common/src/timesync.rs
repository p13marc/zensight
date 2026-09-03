//! The host's own clock discipline (#959, SYS-SUP-013).
//!
//! The payload of `state/sysinfo/timesync`. It lives here rather than in the
//! sensor for the reason #816 established for the desired-state payloads: a
//! state-class subject must serve a **generated** schema (RFC 08 §7, enforced
//! by #815's `every_state_family_serves_a_generated_schema`), and
//! `zensight-common` cannot depend on a sensor — so a type defined in one can
//! only ever get a summary stub.
//!
//! The *reading* of it stays in `zensight-sensor-sysinfo::timesync`, which is
//! where the chrony and `timedatectl` parsers belong.

use serde::{Deserialize, Serialize};

/// The host's clock discipline, published on `state/sysinfo/timesync`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TimesyncStatus {
    /// Whether the local time daemon considers the clock disciplined. This is
    /// the daemon's own statement, never a threshold applied here.
    pub synchronised: bool,
    /// Which daemon answered: `"chrony"` or `"systemd-timesyncd"`.
    pub source: String,
    /// Estimated offset from true time, in milliseconds, as the daemon
    /// reports it. Absent when the daemon does not report one — `timedatectl`
    /// does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset_ms: Option<f64>,
    /// Distance from a reference clock, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stratum: Option<u8>,
    /// The upstream currently being followed, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// Seconds since the last update from the upstream, when reported.
    ///
    /// The field that catches the failure mode a "synchronised: true" flag
    /// alone does not: a daemon that lost its upstream an hour ago may still
    /// report itself synchronised, because it *was*.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_update_age_s: Option<f64>,
}
