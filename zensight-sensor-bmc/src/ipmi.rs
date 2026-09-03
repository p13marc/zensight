//! IPMI, for BMCs that predate Redfish — **not implemented**, and the gate
//! says so out loud (#953).
//!
//! The feature exists as a flag before it exists as a client, deliberately:
//! the config shape, the startup refusal and the CI leg are the parts that get
//! got wrong later, and settling them first means a protocol client lands into
//! a slot that is already shaped and already type-checked. The alternative —
//! adding the flag together with the client — is how a feature ships with a
//! config field nothing validates.
//!
//! What matters either way is that an `ipmi` endpoint is **refused at
//! startup**, naming the flag and naming the working alternative, rather than
//! reported as permanently unreachable. A check that did not run is not
//! evidence about the target.

/// Why this transport cannot answer, phrased for a startup refusal.
///
/// One function with two `cfg` arms, so the message lives in one place and the
/// difference between "not built" and "built but not implemented" is visible
/// where someone will read it.
pub fn unavailable_reason() -> &'static str {
    if cfg!(feature = "ipmi") {
        "this build has the `ipmi` feature, but the IPMI client is not implemented yet (#953) \
         — use a `redfish` endpoint, which every BMC made since roughly 2016 serves"
    } else {
        "this build has no `ipmi` feature — build with --features ipmi, or use a `redfish` \
         endpoint, which every BMC made since roughly 2016 serves"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The refusal has to name the flag AND the alternative. A refusal that
    /// says only "unsupported" sends an operator to the source.
    #[test]
    fn the_refusal_names_the_flag_and_what_to_use_instead() {
        let why = unavailable_reason();
        assert!(why.contains("redfish"), "{why}");
        if cfg!(feature = "ipmi") {
            assert!(why.contains("not implemented"), "{why}");
        } else {
            assert!(why.contains("--features ipmi"), "{why}");
        }
    }
}
