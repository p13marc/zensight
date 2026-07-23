//! Subnet-discovery report (#541): the SNMP sensor's "propose, don't
//! auto-add" state doc — unconfigured responders found by an opt-in sweep,
//! published LWW on `state/snmp/discovery` for the GUI/zenctl to list.

use serde::{Deserialize, Serialize};

/// One subnet-discovery report (#541): devices that answered the sweep but
/// are not in the configured fleet — proposed, never auto-added.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DiscoveryReport {
    /// Unix epoch millis when the sweep finished.
    pub timestamp: i64,
    /// Addresses probed this sweep.
    pub scanned: u32,
    /// Unconfigured responders.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub discovered: Vec<DiscoveredDevice>,
}

/// One unconfigured SNMP responder found by the sweep.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DiscoveredDevice {
    /// `ip:port` that answered.
    pub address: String,
    /// Named credential set that worked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sys_object_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sys_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sys_descr: Option<String>,
    /// Device profiles that would apply (#531).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matched_profiles: Vec<String>,
    /// Copy-pasteable JSON5 `devices[]` snippet.
    pub suggested: String,
}
