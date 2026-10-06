//! What this instance can do *here* — the registration document's
//! `capabilities` member (RFC 04 §5, v1.41).
//!
//! The registry says which surfaces are conditional and on what (`when =
//! ["capability:CAP_BPF", …]`, RFC 08 §2). This says which of those
//! `capability:` names actually hold on this host, per device: a map from the
//! producer's device chunks to names, with the producer's own, device-less
//! capabilities under [`PRODUCER`]. A consumer reads a gated subject's silence
//! as honest by it, and `zenctl check conform` holds a `gated` reply to it —
//! a procedure whose `when` names only `capability:` predicates and answers
//! `error/gated` while the device claims every one of them contradicts itself.
//!
//! **Claim only what was established.** The member is optional and its
//! absence means *not asked*, never "no capability holds" (RFC 13 §3 O4). A
//! sensor claims a name when the thing that needed it succeeded — the eBPF
//! programs attached, the vendor library initialised — not when a
//! configuration asked for it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

/// The key for the producer's own, device-less capabilities.
pub const PRODUCER: &str = "*";

/// A shared, runtime-updatable set of claims. Cloning shares it: the sensor
/// keeps one handle and claims or withdraws as its collectors come and go,
/// and the runner's registration task reads it on each re-emission.
#[derive(Debug, Clone, Default)]
pub struct CapabilityClaims(Arc<RwLock<BTreeMap<String, BTreeSet<String>>>>);

impl CapabilityClaims {
    /// Claim `capability` on `device` ([`PRODUCER`] for the producer's own).
    pub fn claim(&self, device: &str, capability: &str) {
        let mut map = self.0.write().unwrap_or_else(|e| e.into_inner());
        map.entry(device.to_string())
            .or_default()
            .insert(capability.to_string());
    }

    /// Withdraw a claim — the capability stopped holding (a device went away,
    /// a program was detached). A device left with no claims is dropped.
    pub fn withdraw(&self, device: &str, capability: &str) {
        let mut map = self.0.write().unwrap_or_else(|e| e.into_inner());
        if let Some(set) = map.get_mut(device) {
            set.remove(capability);
            if set.is_empty() {
                map.remove(device);
            }
        }
    }

    /// The member as it goes on the wire: device → sorted names. Empty when
    /// nothing was claimed, which the document then omits (not asked).
    pub fn snapshot(&self) -> BTreeMap<String, Vec<String>> {
        let map = self.0.read().unwrap_or_else(|e| e.into_inner());
        map.iter()
            .map(|(d, names)| (d.clone(), names.iter().cloned().collect()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_are_per_device_and_withdraw_cleanly() {
        let c = CapabilityClaims::default();
        assert!(c.snapshot().is_empty(), "nothing claimed is not asked");
        c.claim(PRODUCER, "CAP_BPF");
        c.clone().claim("card0", "ecc");
        c.claim("card0", "nvidia-driver");
        let snap = c.snapshot();
        assert_eq!(snap[PRODUCER], vec!["CAP_BPF"]);
        assert_eq!(snap["card0"], vec!["ecc", "nvidia-driver"]);
        c.withdraw("card0", "ecc");
        c.withdraw("card0", "nvidia-driver");
        assert!(
            !c.snapshot().contains_key("card0"),
            "an empty device is dropped"
        );
    }
}
