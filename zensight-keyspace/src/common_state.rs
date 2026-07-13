//! The framework state subjects, shared by every producer (issue #475).
//!
//! RFC 08 §1 makes the codegen contract *two* directions, and the **parse**
//! direction exists for one reason: to delete positional `split('/')` from
//! consumers. But a consumer that subscribes to the whole `state` class sees
//! eleven different producers' `Subject` enums, and matching each one
//! separately would trade a positional parse for a combinatorial one.
//!
//! Almost all of the state class is the *same* small set of subjects on every
//! producer — health, errors, the registration doc, alerts, evidence, artifact
//! progress. [`CommonState`] is that set, and [`crate::registry::AnySubject::common_state`]
//! (generated from the registry, so it cannot drift from it) refines any
//! producer's subject into it.
//!
//! A consumer therefore writes one match over a dozen typed variants, with the
//! variables already extracted and named, and gets a compile error if the
//! registry moves under it — which is what the parse direction was for.
//!
//! Telemetry deliberately has no equivalent: a subscriber decoding a
//! `TelemetryPoint` does not need to know *which* metric it is, and a consumer
//! that does (a view) already knows its producer and matches that producer's
//! `Subject` directly.

/// A framework state subject, refined from any producer's `Subject`.
///
/// Borrows from the subject it was refined from — no allocation on the decode
/// path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommonState<'a> {
    /// `health` — the sensor health document.
    Health,
    /// `errors` — the rolling error window.
    Errors,
    /// `sensor` — the registration document (identity, version, capabilities).
    Sensor,
    /// `alert/{alert_key}` — firing→resolved on one key; a delete is a tombstone.
    Alert { alert_key: &'a str },
    /// `artifact/{kind}` — per-kind artifact progress.
    Artifact { kind: &'a str },
    /// `evidence/self` — the producer's own identity claim (RFC 06 §4).
    EvidenceSelf,
    /// `evidence/device/{device}` — an observed device's identity claim.
    EvidenceDevice { device: &'a str },
    /// `evidence/names/{ip_slug}` — a passive-DNS name observation.
    EvidenceNames { ip_slug: &'a str },
    /// `stream/{stream}` — a parallax per-stream status document.
    Stream { stream: &'a str },
    /// `@catalog` `entity/{entity_id}` — the merged entity document.
    CatalogEntity { entity_id: &'a str },
    /// `@catalog` `alias/{old_id}` — old-id → entity-id re-pointing.
    CatalogAlias { old_id: &'a str },
    /// `@catalog` `pdns/{ip_slug}` — the accumulated IP↔name record.
    CatalogPdns { ip_slug: &'a str },
}
