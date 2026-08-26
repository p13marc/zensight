//! The one place a ZenSight producer name becomes a [`V1Context`].
//!
//! zenkey 0.7 made `V1Context::for_producer` fallible (`Result<Self,
//! KeyError>`): 0.6 slugged an illegal producer name and, failing that, fell
//! back to the literal `sensor`, so a misconfigured producer published its
//! entire keyspace under a *different identity* — silently, and colliding
//! with every other misconfigured producer in the fleet. Making that a
//! `Result` is right, and it is also 47 call sites deep in this workspace.
//!
//! So the validation happens **once, here**, and the rest of the tree keeps
//! an infallible builder. That is honest rather than lazy because of what a
//! ZenSight producer name actually is: a compile-time constant of the sensor
//! that owns it, one of the names in `zensight-common/registry/*.toml`, never
//! foreign data and never operator input. A name that is not a legal chunk is
//! a *build* mistake, and
//! [`every_registered_producer_name_is_a_legal_chunk`] below is the test that
//! catches it before a binary ever runs — a new registry TOML whose file name
//! is not chunk-legal fails `cargo test`, not a customer's fleet.
//!
//! A name that genuinely *is* foreign data must not come through here: cross
//! that boundary explicitly with `Producer::new(Chunk::slug(name).as_str())`,
//! because slugging an identity is a decision, not a fallback.

use zenkey::Key;
pub use zenkey::V1Context;
use zenkey::grammar::{Origin, Producer};

/// The [`V1Context`] for one producer on the local host origin.
///
/// # Panics
/// If `producer` is not a legal producer chunk (RFC 03 §1.5). See the module
/// docs: producer names are compile-time constants, and the test below pins
/// every registered one.
#[must_use]
pub fn for_producer(producer: &str) -> V1Context {
    V1Context::with_producer(
        Origin::Host(crate::PROFILE.host_id().clone()),
        Producer::new(producer).unwrap_or_else(|e| {
            panic!(
                "{producer:?} is not a legal producer chunk (RFC 03 §1.5): {e}. Producer names \
                 are compile-time constants from zensight-common/registry/; if this name is \
                 foreign data, slug it deliberately at its boundary instead"
            )
        }),
    )
}

/// The narrow escape hatch for zenkey 0.7's newly-fallible key builders.
///
/// `V1Context::state_key` and `rpc_key` now return `Result` for exactly one
/// reason: a subject or procedure chunk that is literally `alive`, the
/// reserved liveliness leaf (RFC 03 §3). Every other malformed chunk is
/// slugged, as it always was. So the error is reachable only from a
/// *dynamic* chunk, and most of this workspace's subjects are compile-time
/// constants (`["health"]`, `["artifact", "request"]`) or hex digests
/// (`["alert", <16 hex>]`), where it is not reachable at all.
///
/// These two methods say that in the type name instead of scattering
/// `.expect("…")` across two dozen sites with two dozen different messages.
/// A builder whose chunks *are* foreign data — a device name, a stream name —
/// must NOT use them: call `state_key` and handle the refusal, because
/// refusing a device called `alive` is the point of the check.
pub trait V1ContextExt {
    /// [`V1Context::state_key`] for a subject that cannot be `alive`.
    ///
    /// # Panics
    /// If any chunk is the reserved `alive` token — i.e. if the caller was
    /// wrong about its subject being constant.
    fn const_state_key(&self, subject: &[&str]) -> Key;

    /// [`V1Context::rpc_key`] for a procedure path that cannot be `alive`.
    ///
    /// # Panics
    /// As [`const_state_key`](V1ContextExt::const_state_key).
    fn const_rpc_key(&self, procedure: &[&str]) -> Key;
}

impl V1ContextExt for V1Context {
    fn const_state_key(&self, subject: &[&str]) -> Key {
        self.state_key(subject)
            .unwrap_or_else(|e| panic!("{subject:?} is not a constant state subject: {e}"))
    }

    fn const_rpc_key(&self, procedure: &[&str]) -> Key {
        self.rpc_key(procedure)
            .unwrap_or_else(|e| panic!("{procedure:?} is not a constant procedure path: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The claim `for_producer`'s `panic!` rests on: every name the registry
    /// ships is chunk-legal, so the panic is unreachable for the producers
    /// this build knows. Adding a registry TOML with an illegal name fails
    /// here rather than at that sensor's first publish.
    #[test]
    fn every_registered_producer_name_is_a_legal_chunk() {
        for (name, _) in crate::registry::REGISTRIES {
            // `@catalog` is a service origin, not a producer chunk, and
            // carries no producer position at all (RFC 06 §5).
            if name.starts_with('@') {
                continue;
            }
            assert!(
                Producer::new(name).is_ok(),
                "registry producer {name:?} is not a legal producer chunk (RFC 03 §1.5)"
            );
            // And the shim itself builds for it.
            let _ = for_producer(name);
        }
    }

    /// The context this builds is the local host origin's — the same one
    /// `V1Context::for_producer` would have built, which is the whole claim
    /// of the shim.
    #[test]
    fn the_shim_matches_the_fallible_constructor() {
        let shimmed = for_producer("netring");
        let direct = V1Context::for_producer(&crate::PROFILE, "netring").expect("legal name");
        assert_eq!(shimmed.telemetry_prefix(), direct.telemetry_prefix());
    }

    /// The precondition `const_state_key` names is real: the reserved token
    /// is refused, not slugged into a colliding liveliness key.
    #[test]
    fn the_reserved_alive_token_is_still_refused() {
        let ctx = for_producer("netring");
        assert!(ctx.state_key(&["alive"]).is_err());
        assert!(ctx.rpc_key(&["alive"]).is_err());
        // And the non-reserved case is the same key either way.
        assert_eq!(
            ctx.const_state_key(&["alert", "0123456789abcdef"]),
            ctx.state_key(&["alert", "0123456789abcdef"]).unwrap()
        );
    }
}
