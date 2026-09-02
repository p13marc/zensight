//! Publishing relationship evidence (#916), once, for every sensor that has
//! any.
//!
//! Four sensors publish relations — pve, container, probe, netlink — and the
//! bookkeeping around them is identical in all four and easy to get subtly
//! wrong in each. What varies is *what* a sensor observed; what does not vary
//! is that a claim must be refreshed while it holds, retired when it stops
//! holding, and never allowed past the cardinality its registry TOML declares.
//!
//! # Why a tombstone and not just a TTL
//!
//! The subject declares `ttl_s = 900`, so a claim nobody refreshes does
//! eventually disappear. Relying on that alone would leave a guest that
//! migrated to another node showing on the old one for up to fifteen minutes,
//! and — worse — showing on *both* while the new node's claim and the old
//! node's stale one overlap. A migration is exactly when an operator looks at
//! the map. So [`RelationSet::sync`] retires what it stopped seeing, in the
//! same pass that refreshes what it still sees, and the TTL becomes the
//! backstop it should be rather than the mechanism.
//!
//! # Why this owns its publisher
//!
//! [`RelationSet::new`] declares its own registry at [`QosClass::Evidence`]
//! rather than borrowing a sensor's existing one, and that is not tidiness.
//!
//! Every sensor's `STATE_QOS` is [`QosClass::HealthLiveness`] — best-effort,
//! congestion-drop. That is right for a document republished every few
//! seconds, and wrong for a **tombstone**, which is published exactly once and
//! whose loss leaves a retired edge on the map until the TTL expires: the one
//! outcome the retire exists to prevent.
//!
//! The alternative — reusing the identity-`evidence` registry, which does have
//! the right QoS — would tie the topology graph to a flag about *identity*
//! republishing (`container.evidence`, `netlink.evidence.enabled`). An
//! operator turning that off for privacy has no reason to expect the map to
//! empty, and a sensor without such a flag at all (probe) would have had to do
//! something different anyway.
//!
//! # Why the cap is here and not left to the registry
//!
//! The registry's `cardinality` is a *declaration*, and the conformance judge
//! compares it against what is observed on the bus. Nothing stops a sensor
//! exceeding it; the judge simply fails afterwards, on someone else's CI run,
//! naming a number rather than a cause. Refusing to publish past the cap at
//! the seam turns that into a bounded, logged, local event.

use std::collections::BTreeMap;

use std::sync::Arc;

use zenoh::Session;
use zensight_common::serialization::Format;

use zensight_common::relation::RelationshipEvidence;

use crate::advanced_publisher::AdvancedPublisherRegistry;

/// The declared cardinality of `evidence/relation/{relation_id}` in every
/// sensor's registry TOML.
///
/// Duplicated here as a constant rather than read from the generated registry
/// because the check has to happen before a key is built, and because a
/// mismatch is worth catching: [`relation_cardinality_matches_the_registry`]
/// asserts the two agree, so raising one without the other fails the suite
/// rather than the fleet.
pub const MAX_RELATIONS: usize = 1024;

/// One sensor's live relation claims, and what it published last time.
///
/// Not a cache of documents — only of the keys, because that is all a retire
/// needs and holding the payloads would mean a second copy of the graph in
/// every sensor.
#[derive(Debug)]
pub struct RelationSet {
    producer: &'static str,
    registry: AdvancedPublisherRegistry,
    /// `relation_id -> key`, as published on the previous [`RelationSet::sync`].
    published: BTreeMap<String, String>,
}

/// What one `sync` did, for the caller's log line and metrics.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SyncOutcome {
    /// Claims published or refreshed.
    pub published: usize,
    /// Claims retired because they were no longer observed.
    pub retired: usize,
    /// Claims dropped because the set exceeded [`MAX_RELATIONS`].
    pub dropped: usize,
    /// Publishes that failed. Their keys stay in the set, so the next sync
    /// retries rather than silently forgetting them.
    pub failed: usize,
}

impl RelationSet {
    pub fn new(producer: &'static str, session: Arc<Session>, format: Format) -> Self {
        RelationSet {
            producer,
            registry: AdvancedPublisherRegistry::new(
                session,
                crate::v1::for_producer(producer).telemetry_prefix(),
                format,
                // One cached sample: a late joiner needs the current claim,
                // not its history. The history of a relationship is the
                // catalog's edge document, not this feed.
                crate::AdvancedPublisherConfig::cache_only(1),
            )
            .with_qos(zensight_common::QosClass::Evidence),
            published: BTreeMap::new(),
        }
    }

    /// How many claims are currently published.
    pub fn len(&self) -> usize {
        self.published.len()
    }

    pub fn is_empty(&self) -> bool {
        self.published.is_empty()
    }

    /// Publish `claims` as the complete current set, retiring anything
    /// published before that is not in it.
    ///
    /// `claims` is the whole truth, not a delta. That is deliberate: a sensor
    /// that computes its relations fresh each poll cannot forget to retire
    /// something, whereas a delta API makes forgetting the default. Duplicate
    /// `relation_id`s collapse — two observations of one relationship are one
    /// claim, which is the same property `relation_id` gives on the wire.
    pub async fn sync(&mut self, claims: &[RelationshipEvidence]) -> SyncOutcome {
        let mut out = SyncOutcome::default();

        let mut current: BTreeMap<String, &RelationshipEvidence> = BTreeMap::new();
        for c in claims {
            current.insert(c.relation_id(), c);
        }
        if current.len() > MAX_RELATIONS {
            // Keep a deterministic prefix rather than an arbitrary one: which
            // claims survive must not depend on iteration order, or two polls
            // of an unchanged over-cap fleet would churn the whole family.
            out.dropped = current.len() - MAX_RELATIONS;
            let keep: Vec<String> = current.keys().take(MAX_RELATIONS).cloned().collect();
            current.retain(|k, _| keep.binary_search(k).is_ok());
            tracing::warn!(
                producer = %self.producer,
                dropped = out.dropped,
                cap = MAX_RELATIONS,
                "relation evidence exceeds the declared cardinality; publishing a \
                 deterministic prefix. The graph is incomplete until the cap or the \
                 claim set changes."
            );
        }

        for (id, claim) in &current {
            let Some(key) = zensight_common::keyexpr::relation_evidence_key(self.producer, id)
            else {
                // Unregistered subject: the family is not in this producer's
                // TOML. A warning rather than silence, because the sensor
                // believes it is publishing a graph and is not.
                tracing::warn!(
                    producer = %self.producer,
                    "relation evidence is not a registered subject for this producer"
                );
                out.failed += 1;
                continue;
            };
            match self.registry.publish_serializable(&key, claim).await {
                Ok(()) => {
                    out.published += 1;
                    self.published.insert(id.clone(), key);
                }
                Err(e) => {
                    out.failed += 1;
                    tracing::debug!(producer = %self.producer, key = %key, error = %e,
                        "relation evidence publish failed");
                }
            }
        }

        let gone: Vec<(String, String)> = self
            .published
            .iter()
            .filter(|(id, _)| !current.contains_key(*id))
            .map(|(id, key)| (id.clone(), key.clone()))
            .collect();
        for (id, key) in gone {
            match self.registry.tombstone(&key).await {
                Ok(()) => {
                    out.retired += 1;
                    self.published.remove(&id);
                }
                Err(e) => {
                    // Left in `published` on purpose: the next sync retries.
                    // Dropping it here would leave a claim on the bus that
                    // nothing ever retires except the TTL.
                    out.failed += 1;
                    tracing::debug!(producer = %self.producer, key = %key, error = %e,
                        "relation evidence tombstone failed; will retry next cycle");
                }
            }
        }
        out
    }

    /// Retire everything, for a clean shutdown.
    pub async fn retire_all(&mut self) -> SyncOutcome {
        self.sync(&[]).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cap here and the cap in every registry TOML must be the same
    /// number. If they drift, the sensor either publishes past a declared
    /// budget (and the conformance judge fails, elsewhere, later) or refuses
    /// claims it was allowed to make.
    #[test]
    fn relation_cardinality_matches_the_registry() {
        use zensight_common::registry;
        // Each producer's generated `Family` carries what its TOML declared.
        let declared = [
            (
                "container",
                registry::container::Subject::evidence_relation("r-0").cardinality(),
            ),
            (
                "pve",
                registry::pve::Subject::evidence_relation("r-0").cardinality(),
            ),
            (
                "probe",
                registry::probe::Subject::evidence_relation("r-0").cardinality(),
            ),
            (
                "netlink",
                registry::netlink::Subject::evidence_relation("r-0").cardinality(),
            ),
        ];
        for (producer, c) in declared {
            assert_eq!(
                c,
                Some(MAX_RELATIONS as u64),
                "{producer} declares a different cardinality than MAX_RELATIONS"
            );
        }
    }
}
