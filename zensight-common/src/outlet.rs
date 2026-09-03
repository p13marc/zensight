//! Gated PDU outlet control — the wire types (#956).
//!
//! ZenSight could not power-cycle anything. The only write surface in the
//! platform was the `systemd` sensor's gated action set (#283), and a PDU
//! outlet cycle is SYS-SUP-003's shape for "secure remote restart".
//!
//! It is a bigger step than restarting a unit, and this file exists because
//! that difference is worth making visible: **a monitor that can cut power is
//! a different threat model** — the sentence `zensight-sensor-pve` uses to
//! justify having no action surface at all, and `zensight-sensor-bmc` after
//! it. So this is its own issue with its own decision, separate from the read
//! side in #955, and the gate is the strictest in the tree.
//!
//! # Why these types live in `zensight-common`
//!
//! The same reason every other reply type does: a caller is not a sensor. The
//! frontend renders the gate *before* anyone clicks, from
//! [`OutletCapability`], and it must be reading the same struct the sensor
//! answers with — two definitions of "what is permitted here" would drift, and
//! the one that drifts is the one that says a button is safe.
//!
//! # The honest description of the authorisation
//!
//! Unlike `systemd`, there is **no polkit here**. A PDU speaks SNMP; there is
//! no local policy engine between us and it. The allowlist is the only gate,
//! and the bus caller is anonymous — #957 makes the attempt *auditable*, not
//! *attributable*. Until a caller identity exists (Zenoh mTLS certificate CN
//! plus Zenoh's ACL — a scope question named in epic #952), the true sentence
//! is: **anyone who can reach the bus and whose target is on the allowlist**.
//! That sentence is in the crate docs too, where an operator will meet it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What may be asked of an outlet.
///
/// **`Cycle` only**, deliberately. The requirement asks for *restart*, and
/// `off` and `on` as separate verbs — each behind its own switch, the
/// `allow_unit_files` / `allow_daemon_reload` shape — are a follow-up. A verb
/// that can leave a load powered *off* indefinitely is a different promise
/// from one that returns it, and shipping both at once would mean the
/// narrower decision was never taken on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum OutletVerb {
    Cycle,
}

impl OutletVerb {
    pub fn as_str(self) -> &'static str {
        match self {
            OutletVerb::Cycle => "cycle",
        }
    }

    pub fn all() -> Vec<OutletVerb> {
        vec![OutletVerb::Cycle]
    }
}

impl std::fmt::Display for OutletVerb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OutletAction {
    /// The configured device name, not an address.
    pub device: String,
    /// The outlet's table index as the PDU reports it.
    pub outlet: String,
    pub verb: OutletVerb,
}

impl OutletAction {
    /// The allowlist form: `<device>/<outlet>`.
    pub fn target(&self) -> String {
        format!("{}/{}", self.device, self.outlet)
    }
}

/// One outcome — executed or refused.
///
/// Deliberately has **no `Default`**: an all-empty status reads exactly like a
/// refusal, and a caller cannot tell the two apart. The `actions` ring answers
/// `null` when nothing has run, the way `systemd`'s does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OutletStatus {
    pub device: String,
    pub outlet: String,
    pub verb: OutletVerb,
    /// Whether the gate permitted it. `false` means nothing was sent.
    pub accepted: bool,
    /// The switch that refused, when one did (#866/#957).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused_by: Option<String>,
    /// The sentence for a human.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The outlet's state immediately before the SET, when it could be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_before: Option<String>,
    /// Its state immediately after. A cycle is asynchronous inside the PDU, so
    /// this is usually still `on` — it is evidence the SET was accepted, not
    /// evidence the load restarted, and the docs say so rather than letting a
    /// reader infer more from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_after: Option<String>,
    /// The PDU's own configured reboot duration, where it publishes one. Not
    /// ours: the delay belongs to the device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reboot_duration_secs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub ts_unix: i64,
}

/// The gate state this build advertises, so a caller can render the answer
/// before anyone clicks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OutletCapability {
    pub enabled: bool,
    /// The `<device>/<outlet>` globs that are permitted. **Empty accepts
    /// nothing**, even with `enabled` true — a distinct fact from "switched
    /// off", and one an operator would otherwise diagnose by trying it.
    pub allow_outlets: Vec<String>,
    pub verbs: Vec<OutletVerb>,
    /// Why nothing is permitted, when nothing is (#866). A greyed button with
    /// no reason is indistinguishable from a broken one, and the reason
    /// otherwise lives only in a log on a machine the operator is not reading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl OutletCapability {
    /// Nothing is permitted, and here is why.
    pub fn disabled_because(reason: impl Into<String>) -> Self {
        Self {
            enabled: false,
            allow_outlets: Vec::new(),
            verbs: Vec::new(),
            reason: Some(reason.into()),
        }
    }

    /// Whether this capability permits `target` at all.
    ///
    /// The *preview* of the gate. It shares
    /// [`crate::action::allows`] with the sensor's own gate, so the button and
    /// the decision cannot disagree.
    pub fn permits(&self, target: &str) -> bool {
        self.enabled && crate::action::allows(&self.allow_outlets, target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_is_device_slash_outlet() {
        let a = OutletAction {
            device: "pdu-a".into(),
            outlet: "3".into(),
            verb: OutletVerb::Cycle,
        };
        assert_eq!(a.target(), "pdu-a/3");
    }

    /// An empty allowlist accepts nothing, even switched on. It reads as a
    /// working gate until you try it, which is why it is a distinct advertised
    /// state rather than the same as "disabled".
    #[test]
    fn an_empty_allowlist_permits_nothing_even_when_enabled() {
        let cap = OutletCapability {
            enabled: true,
            allow_outlets: Vec::new(),
            verbs: OutletVerb::all(),
            reason: None,
        };
        assert!(!cap.permits("pdu-a/3"));
        assert!(cap.enabled, "the switch really is on");
    }

    /// The preview shares its matcher with the gate, so a wildcard means the
    /// same thing in the button and in the decision.
    #[test]
    fn the_preview_matches_the_way_the_gate_does() {
        let cap = OutletCapability {
            enabled: true,
            allow_outlets: vec!["pdu-a/*".into()],
            verbs: OutletVerb::all(),
            reason: None,
        };
        assert!(cap.permits("pdu-a/3"));
        assert!(cap.permits("pdu-a/12"));
        assert!(!cap.permits("pdu-b/3"), "a different PDU is not covered");
    }

    #[test]
    fn a_disabled_capability_says_why() {
        let cap = OutletCapability::disabled_because("actions.enabled is false");
        assert!(!cap.permits("pdu-a/3"));
        assert!(cap.reason.unwrap().contains("actions.enabled"));
    }

    /// There is exactly one verb, and that is a decision rather than an
    /// oversight: `off` on its own can leave a load dark indefinitely, which
    /// is a different promise from a restart.
    #[test]
    fn cycle_is_the_only_verb() {
        assert_eq!(OutletVerb::all(), vec![OutletVerb::Cycle]);
        assert_eq!(OutletVerb::Cycle.to_string(), "cycle");
    }
}
