//! GPU inventory (#954, SYS-SUP-009/012).
//!
//! The payload of `state/sysinfo/gpu/{card}`. It lives here rather than in the
//! sensor for the reason #816 established and #959 repeated: a state-class
//! subject must serve a **generated** schema (RFC 08 §7, enforced by #815), and
//! `zensight-common` cannot depend on a sensor, so a type defined in one could
//! only ever get a summary stub.
//!
//! The *reading* stays in `zensight-sensor-sysinfo::gpu`.

use serde::{Deserialize, Serialize};

/// One GPU, as the kernel's DRM sysfs describes it.
///
/// Everything except `card` and `vendor` is optional, because what a driver
/// exposes varies: amdgpu publishes a product name, i915 does not, and a
/// vendor's next release may publish neither. **An absent field means the
/// driver did not say**, never a default that looks like an answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GpuInfo {
    /// The DRM card node, e.g. `"card0"`. The key chunk.
    pub card: String,
    /// PCI vendor, resolved to a name where known (`"AMD"`, `"Intel"`,
    /// `"NVIDIA"`), else the raw id.
    pub vendor: String,
    /// The kernel driver bound to it: `"amdgpu"`, `"i915"`, `"xe"`, `"nvidia"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    /// The raw `vendor:device` PCI id, e.g. `"0x1002:0x73ff"` — the thing to
    /// paste into a search when the product name is absent or unhelpful.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pci_id: Option<String>,
    /// The driver's own product name, where it publishes one (amdgpu does).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The PCI address the card sits at, e.g. `"0000:01:00.0"` — what you
    /// paste into `lspci -s`, and the key that joins a DRM card to a vendor
    /// library's view of the same device (#954). Absent when
    /// `/sys/class/drm/<card>/device` is not a symlink into the PCI tree,
    /// which is the case for a virtual DRM node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pci_addr: Option<String>,
}

/// Resolve a PCI vendor id to a name, or hand back the id.
///
/// Three vendors, because those are the three that ship a DRM driver anyone
/// runs a supervision platform against. An unknown id is returned verbatim
/// rather than mapped to `"unknown"`: the id is the useful thing.
pub fn vendor_name(id: &str) -> String {
    match id.trim().to_ascii_lowercase().as_str() {
        "0x1002" => "AMD".to_string(),
        "0x8086" => "Intel".to_string(),
        "0x10de" => "NVIDIA".to_string(),
        other => other.to_string(),
    }
}
