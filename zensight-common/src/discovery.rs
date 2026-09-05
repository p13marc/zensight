//! Subnet-discovery report (#541): the SNMP sensor's "propose, don't
//! auto-add" state doc — unconfigured responders found by an opt-in sweep,
//! published LWW on `state/snmp/discovery` for the GUI/zenctl to list.

use std::collections::BTreeMap;

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

// ── Stream discovery (#410) ─────────────────────────────────────────────────
//
// The parallax analogue of the SNMP sweep above, and deliberately a separate
// shape rather than a reuse of it: `credentials` and `sys_object_id` mean
// nothing to a camera found over mDNS, and a report whose fields are mostly
// absent teaches a consumer nothing about what discovery actually found.
//
// The stance is identical, and it is the load-bearing part: **propose, never
// auto-add.** A responder appearing here is a suggestion for an operator to
// accept, not a stream the sensor has started serving. Nothing in this document
// causes anything to be captured, encoded or published.

/// One round of camera discovery (#410): responders on the local network that
/// are not already configured streams. Published LWW on
/// `state/parallax/discovery`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StreamDiscoveryReport {
    /// Unix epoch millis when the round finished.
    pub timestamp: i64,
    /// Which probes ran this round — `mdns`, `ws-discovery`. Named rather than
    /// counted: "nothing found" means something different when only one of two
    /// enabled probes actually ran.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<String>,
    /// Responders that are not configured streams.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub discovered: Vec<DiscoveredStream>,
}

/// One discovered camera or streaming endpoint.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DiscoveredStream {
    /// How it was found: `mdns` or `ws-discovery`.
    pub via: String,
    /// Where it answered, `ip:port`.
    pub address: String,
    /// Instance or device name as advertised.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The RTSP URL, when the protocol handed one over outright — an mDNS TXT
    /// `path`, say.
    ///
    /// **Absent is the common case and is not a defect.** WS-Discovery returns
    /// a device's *service* address, not a stream URI; getting the URI means an
    /// ONVIF Media `GetStreamUri` call, which is a different protocol surface.
    /// So the operator supplies the URL, which is what "propose" means here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// What the responder advertised about itself — mDNS TXT records,
    /// WS-Discovery scopes. Kept as it arrived rather than parsed into fields
    /// this sensor would then have to keep true for every vendor.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, String>,
    /// A copy-pasteable JSON5 `parallax.rtsp[]` entry. With no URL it carries a
    /// `rtsp://<address>/<path>` placeholder, so what has to be filled in is
    /// visible rather than implied.
    pub suggested: String,
}
