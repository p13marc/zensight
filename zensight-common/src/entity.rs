//! Host-entity wire types — the `zensight/v1/@catalog/state/entity/*` keyspace (#305).
//!
//! The **correlator** (single writer) merges [`HostEvidence`](crate::HostEvidence)
//! claims from every sensor into [`HostEntity`] docs: one entity per real host,
//! carrying the union of its identifying fields plus a reversible list of the
//! `(sensor, source)` [`MemberClaim`]s that were merged into it and which rule
//! bound each. Entities are a **materialized view** — a pure, deterministic
//! function of the current (TTL-live) evidence set — so a restarted correlator
//! rebuilds identical docs from the evidence caches with no local state.
//!
//! Telemetry keys are deliberately *not* re-keyed on entity ids (they stay
//! `zensight/v1/<origin>/telemetry/<producer>/<subject...>`, scoped by the
//! publishing origin); the entity layer provides the join via `members[]`. See
//! `docs/KEYSPACE.md`.

use serde::{Deserialize, Serialize};

/// One provenance-tagged name bound to an entity (or served for an arbitrary IP
/// via the names queryable). Distinct from the wire-level
/// [`NameObservation`](crate::NameObservation): the correlator accumulates
/// *multiple* names per IP into these, since #307 publishes only one
/// observation per IP key (last-writer-wins on the wire).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NameVal {
    /// The canonical name (lowercased, no trailing dot).
    pub name: String,
    /// Provenance slug from the passive-DNS source (`dns_a`, `dns_ptr`, `sni`,
    /// `mdns`, ...).
    pub provenance: String,
    /// Unix epoch millis of the most recent sighting.
    pub last_seen: i64,
}

/// A durable **historical** passive-DNS record, published by the
/// **correlator** on `zensight/v1/@catalog/state/pdns/<ip-slug>` (#310)
/// whenever it learns or updates the names for an IP.
///
/// Unlike the live [`NameObservation`](crate::NameObservation) wire claim (one
/// name per sample, last-writer-wins) this carries the correlator's *full
/// accumulated* [`NameVal`] set for the IP, so a router-hosted storage backend
/// (filesystem snapshot or InfluxDB time series) capturing the
/// `zensight/v1/@catalog/state/pdns/**` tier records the complete IP↔name
/// history off a single key. The catalog pdns state tier stays off the
/// telemetry bus because `@catalog` is a verbatim origin the `*` selectors
/// never match (D4) — see [`crate::keyexpr::pdns_key`].
/// An old-id → entity-id re-pointing record (RFC 06 §5.4), published at
/// `zensight/v1/@catalog/state/alias/<old_id>` when a merge/upgrade retires
/// an entity id. Long-TTL so a consumer holding a stale id can re-point after
/// arbitrarily long offline periods.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AliasRecord {
    /// The retired entity id.
    pub old_id: String,
    /// The current entity id it merged into.
    pub entity_id: String,
    /// Unix epoch millis this record was published.
    pub last_updated: i64,
}

/// What an operator asserted about two origins (#473, RFC 06 §5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AssertionKind {
    /// "These are the same host." The canonical case is a **reinstall**: the
    /// machine minted a new origin, evidence for the old one is still live, and
    /// the conflicting-strong-ids rule correctly refuses to merge them — the
    /// catalog cannot tell a reinstall from two machines. Only an operator can.
    Link,
    /// "These are **not** the same host." Retires a [`AssertionKind::Link`], and
    /// stands on its own as a veto: it forbids the merge rules from ever putting
    /// the two origins in one entity, including transitively through a third
    /// node.
    Unlink,
}

/// An operator's explicit statement about identity, carried on the bus as
/// ordinary catalog state (#473).
///
/// **Why this is a wire record and not a config file or a side table.** RFC 06
/// §5 says the catalog is a pure function of live evidence — "no private
/// database, no migration state". An operator override *is* state, and it is not
/// evidence. Putting it in a registered state subject
/// ([`crate::keyexpr::assertion_key`]) is what preserves the property: a
/// restarted correlator, a replica, or a second catalog re-seeds the assertion
/// through the same path as every other document, and a storage-backed router
/// makes it durable for free. A side table would be invisible to all three.
///
/// The two ids are **origin ids** (`h-<12hex>`), never the weaker
/// evidence-derived entity ids: an entity id computed from a hostname or MAC
/// changes when the set it names changes, so an assertion keyed on one would
/// dangle the moment it took effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OperatorAssertion {
    /// Stable key chunk, derived from the pair — see [`OperatorAssertion::id`].
    pub id: String,
    pub kind: AssertionKind,
    /// The retired/older origin (`old` in `@rpc/link?old=…;new=…`).
    pub old: String,
    /// The current origin the entity should be known by (`new`).
    pub new: String,
    /// Unix epoch millis the operator made the call.
    pub asserted_at: i64,
    /// Free-text operator note ("reinstalled 2026-07-14, same box").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl OperatorAssertion {
    /// The key chunk for a `(kind, old, new)` triple: stable, collision-free,
    /// and re-derivable, so re-asserting the same thing is an idempotent LWW
    /// overwrite rather than a second record.
    pub fn id(kind: AssertionKind, old: &str, new: &str) -> String {
        let k = match kind {
            AssertionKind::Link => "link",
            AssertionKind::Unlink => "unlink",
        };
        format!("{k}-{old}-{new}")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PdnsRecord {
    /// The IP these names are bound to (canonical, *un*-slugified — the key is
    /// slugged, the payload keeps the real address).
    pub ip: String,
    /// The accumulated provenance-tagged names for the IP, ranked
    /// most-recent-first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<NameVal>,
    /// Unix epoch millis this record was published.
    pub last_updated: i64,
}

/// One reversible membership claim: an evidence `(sensor, source)` that the
/// correlator merged into an entity, the rule that bound it, and how confident
/// that binding is. Un-merging is dropping a claim — `DeviceId`s are never
/// rewritten.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MemberClaim {
    /// The publishing sensor of the merged evidence (e.g. `"sysinfo"`).
    pub sensor: String,
    /// The evidence `source` — the same value used as `source` in that sensor's
    /// telemetry keys (the join key back to per-protocol devices).
    pub source: String,
    /// The strongest rule that attached this member to the set:
    /// `"host_id"` | `"mac_ip"` | `"fqdn"` | `"hostname"`.
    pub rule: String,
    /// Confidence of the binding rule (after any observer down-weight).
    pub confidence: f32,
    /// Unix epoch millis of the merged evidence's last refresh.
    pub last_seen: i64,
}

/// A resolved host entity, published on
/// `zensight/v1/@catalog/state/entity/<entity_id>` by the correlator (cached,
/// re-emitted ~60 s, tombstoned on retire/merge).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HostEntity {
    /// Stable id `"h_<12hex>"`: the 12-hex prefix of the host's hashed
    /// machine-id when known, else of `sha256(best evidence key)`. Prior ids
    /// after an upgrade/merge move into `aliases`.
    pub entity_id: String,
    /// Prior `entity_id`s this entity subsumed (after a weak→host_id upgrade or
    /// a merge). Ids never silently swap; the old id is tombstoned and re-pointed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,

    // --- identifying (joins allowed) ---
    /// Hashed machine-id, when any member reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    /// Kernel boot id, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<String>,
    /// Union of known IPs across members (sorted, deduped).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ips: Vec<String>,
    /// Union of known MACs across members (sorted, deduped).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub macs: Vec<String>,
    /// Containers the host's sensors run in (#311), unioned from member
    /// evidence (sorted, deduped). Descriptive: container ids are host-scoped
    /// and never a merge key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub container_ids: Vec<String>,

    // --- descriptive (display only) ---
    /// Representative hostname (self-report preferred over third-party claim).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// Representative fully-qualified domain name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fqdn: Option<String>,
    /// Provenance-tagged names attached from the name map (passive DNS).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<NameVal>,
    /// Hardware vendor, when observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    /// OS/platform hint, when observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// OS display name (self-report preferred over wire observation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_name: Option<String>,
    /// os-release `VERSION_ID`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,
    /// Kernel release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<String>,
    /// CPU architecture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,

    // --- membership + status ---
    /// The evidence claims merged into this entity (sorted by `(sensor, source)`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<MemberClaim>,
    /// Rolled-up device status (`"online"` | `"degraded"` | `"offline"` |
    /// `"unknown"`), worst-of-members from device liveness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Unix epoch millis this doc was (re)computed.
    pub last_updated: i64,
}

impl HostEntity {
    /// Sort the multi-valued fields (`ips`, `macs`, `container_ids`,
    /// `members`, `aliases`, `names`) into a canonical order so two entities
    /// built from the same evidence in different input orders serialize
    /// byte-identically.
    ///
    /// The correlator calls this before publishing; determinism tests pin it.
    pub fn canonicalize(&mut self) {
        self.ips.sort();
        self.ips.dedup();
        self.macs.sort();
        self.macs.dedup();
        self.container_ids.sort();
        self.container_ids.dedup();
        self.aliases.sort();
        self.aliases.dedup();
        self.members.sort_by(|a, b| {
            a.sensor
                .cmp(&b.sensor)
                .then_with(|| a.source.cmp(&b.source))
        });
        self.names.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| a.provenance.cmp(&b.provenance))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_entity_roundtrip() {
        let ent = HostEntity {
            entity_id: "h_0123456789ab".into(),
            aliases: vec!["h_ffffffffffff".into()],
            host_id: Some("ab".repeat(32)),
            boot_id: None,
            ips: vec!["10.0.0.5".into()],
            macs: vec![],
            container_ids: vec![],
            hostname: Some("host1".into()),
            fqdn: None,
            names: vec![NameVal {
                name: "host1.example.com".into(),
                provenance: "dns_a".into(),
                last_seen: 1,
            }],
            vendor: None,
            platform: None,
            os_name: None,
            os_version: None,
            kernel: None,
            arch: None,
            members: vec![MemberClaim {
                sensor: "sysinfo".into(),
                source: "host1".into(),
                rule: "host_id".into(),
                confidence: 1.0,
                last_seen: 1,
            }],
            status: Some("online".into()),
            last_updated: 1_700_000_000_000,
        };
        let json = serde_json::to_string(&ent).unwrap();
        let back: HostEntity = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ent);
    }

    #[test]
    fn pdns_record_roundtrips_and_skips_empty_names() {
        let rec = PdnsRecord {
            ip: "10.0.0.9".into(),
            names: vec![NameVal {
                name: "printer.example.com".into(),
                provenance: "dns_ptr".into(),
                last_seen: 123,
            }],
            last_updated: 1_700_000_000_000,
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: PdnsRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, rec);

        // An empty name set is elided on the wire and decodes back to empty.
        let empty = PdnsRecord {
            ip: "10.0.0.9".into(),
            names: vec![],
            last_updated: 1,
        };
        let json = serde_json::to_string(&empty).unwrap();
        assert!(!json.contains("names"));
        let back: PdnsRecord = serde_json::from_str(&json).unwrap();
        assert!(back.names.is_empty());
    }

    #[test]
    fn empty_optionals_are_skipped_on_the_wire() {
        let ent = HostEntity {
            entity_id: "h_0123456789ab".into(),
            aliases: vec![],
            host_id: None,
            boot_id: None,
            ips: vec![],
            macs: vec![],
            container_ids: vec![],
            hostname: None,
            fqdn: None,
            names: vec![],
            vendor: None,
            platform: None,
            os_name: None,
            os_version: None,
            kernel: None,
            arch: None,
            members: vec![],
            status: None,
            last_updated: 1,
        };
        let json = serde_json::to_string(&ent).unwrap();
        // Only entity_id + last_updated survive; everything else is skipped.
        assert!(!json.contains("aliases"));
        assert!(!json.contains("container_ids"));
        assert!(!json.contains("host_id"));
        assert!(!json.contains("members"));
        assert!(!json.contains("status"));
        assert!(json.contains("entity_id"));
        assert!(json.contains("last_updated"));
        let back: HostEntity = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ent);
    }

    #[test]
    fn canonicalize_sorts_and_dedups() {
        let mut ent = HostEntity {
            entity_id: "h_0123456789ab".into(),
            aliases: vec![],
            host_id: None,
            boot_id: None,
            ips: vec!["10.0.0.9".into(), "10.0.0.1".into(), "10.0.0.9".into()],
            macs: vec!["bb:bb".into(), "aa:aa".into()],
            container_ids: vec!["cccc".into(), "aaaa".into(), "cccc".into()],
            hostname: None,
            fqdn: None,
            names: vec![],
            vendor: None,
            platform: None,
            os_name: None,
            os_version: None,
            kernel: None,
            arch: None,
            members: vec![
                MemberClaim {
                    sensor: "netlink".into(),
                    source: "b".into(),
                    rule: "mac_ip".into(),
                    confidence: 0.8,
                    last_seen: 1,
                },
                MemberClaim {
                    sensor: "netlink".into(),
                    source: "a".into(),
                    rule: "mac_ip".into(),
                    confidence: 0.8,
                    last_seen: 1,
                },
            ],
            status: None,
            last_updated: 1,
        };
        ent.canonicalize();
        assert_eq!(ent.ips, vec!["10.0.0.1", "10.0.0.9"]);
        assert_eq!(ent.macs, vec!["aa:aa", "bb:bb"]);
        assert_eq!(ent.container_ids, vec!["aaaa", "cccc"]);
        assert_eq!(ent.members[0].source, "a");
        assert_eq!(ent.members[1].source, "b");
    }
}
