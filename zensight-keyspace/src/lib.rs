//! Executable form of the keyspace-v2 convention.
//!
//! The convention is specified in `docs/rfcs/keyspace-v2/` (v1). This crate is
//! its enforcement layer: everything a producer or consumer needs to emit and
//! parse conforming keys without ever spelling a raw key string.
//!
//! Canonical grammar (base-relative — the deployment base is the session
//! *namespace*, RFC 03 §1.1, so no key built here contains it):
//!
//! ```text
//! v1/<origin>/<class>/<producer>/<subject...>
//! ```
//!
//! Layer map:
//! - [`grammar`] — chunk lexical rules, reserved tokens, structural key
//!   assembly and parsing (RFC 03).
//! - [`origin`] — `h-<12hex>` host-origin minting (RFC 06 §1).
//! - [`slug`] — canonical, injective slugging of foreign values (RFC 03 §2).
//! - [`qos`] — the five named QoS profiles (RFC 04 §3).
//!
//! The subject vocabulary itself is governed by the registry (RFC 08); the
//! generated per-subject builders/parsers sit on top of [`grammar`] and are
//! produced by this crate's build script (issue #455).
//!
//! The RFC's design properties D1–D6 are pinned as executable guard tests in
//! `tests/guard.rs` — run by CI, as RFC 03 §4 requires.

pub mod common_state;
pub mod context;
pub mod grammar;
pub mod origin;
pub mod qos;
pub mod registry;
pub mod slice;
pub mod slug;

/// The default deployment base (RFC 03 §1.1).
///
/// **It is not part of any key this crate builds.** It is the value a
/// participant sets as its Zenoh session **`namespace`**, which prefixes it
/// onto every keyexpr the session emits, strips it on delivery, and *filters*
/// ingress from outside it — so the base is an isolation boundary, not a string
/// convention (RFC 09 §0, issue #466).
///
/// The only legitimate readers are therefore:
/// - session configuration ([`zensight_common::session`] — the one place it is
///   spelled in a running application);
/// - router-side artifacts (storage selectors, ACL rules), which see **full**
///   keys because a router has no namespace;
/// - deliberately un-namespaced debug tools (`zenctl`, the `v1_probe` example),
///   which spell full keys because that is the honest view of the wire
///   (RFC 09 §5).
///
/// Application code that reaches for this to *build a key* has made a mistake:
/// the session adds the base.
pub const DEFAULT_BASE: &str = "zensight";

pub use common_state::CommonState;
pub use context::V1Context;
pub use grammar::{
    Class, ClassOrPlane, KeyError, Origin, Plane, Producer, StructuralKey, VERSION_CHUNK,
};
pub use origin::HostId;
pub use qos::QosProfile;
pub use slice::{RegistrySlice, SliceFinding, parse_slice};
