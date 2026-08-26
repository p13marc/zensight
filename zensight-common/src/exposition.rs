//! One label merge, shared by both exporters (#753).
//!
//! # Why this exists
//!
//! Both exporters used to assemble a label set by pushing sources in order and
//! de-duplicating against a hard-coded two-name list (`source`, `protocol`).
//! That is not a de-duplication rule, it is a list of the two names somebody
//! remembered — and it produced an invalid series the moment a third name came
//! from two places at once:
//!
//! ```text
//! zensight_system_disk_io{device="sda",device="sda",direction="read",…}
//! ```
//!
//! `device` arrived from the semconv table (which computed it by re-splitting
//! the metric path) *and* from the sysinfo sensor's own point labels. Duplicate
//! label names are invalid in both the exposition format and remote-write:
//! Prometheus rejects the sample, and a remote-write receiver rejects the whole
//! `WriteRequest`. On the OTel side there was no de-duplication at all.
//!
//! The fix is not a longer reserved-name list. It is one merge with a **stated
//! precedence**, used by both exporters, that cannot express a duplicate.
//!
//! # Precedence
//!
//! Later never overwrites earlier, and a collision is **dropped and counted**,
//! never appended:
//!
//! | | Source | Trust |
//! |---|---|---|
//! | 1 | structural, from the key — `origin`, `protocol`, `source` | the wire |
//! | 2 | semconv constants — `state`, `direction`, … | the compiled table |
//! | 3 | registry pattern vars — `device`, `iface`, `mount`, … | the compiled registry |
//! | 4 | the point's own labels | the sensor |
//! | 5 | operator `default_labels` from config | the deployment |
//!
//! Stages 1–3 are structural truth derived from the key and the registry;
//! 4–5 are the mutable layers. Dropping rather than overwriting is what stops a
//! sensor forging `origin` — a label a consumer is entitled to trust, because
//! it is the RFC 06 minted host id and not something a payload can assert.
//!
//! Appending rather than dropping is the original bug, so it is the one
//! behaviour this module structurally cannot have.

use std::collections::BTreeMap;

/// Where a label came from. Ordered: lower stages win.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LabelSource {
    /// Derived from the key: `origin`, `protocol`, `source`.
    Structural,
    /// A semconv constant: `state`, `direction`, `type`.
    SemconvConstant,
    /// A registry pattern variable: `device`, `iface`, `mount`, `core`.
    PatternVar,
    /// A label the sensor attached to the point.
    PointLabel,
    /// An operator label from `default_labels`.
    ConfigDefault,
}

/// The outcome of a merge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergedLabels {
    /// Merged labels, sorted by name, unique by name.
    pub labels: Vec<(String, String)>,
    /// How many candidates were dropped because their name was already taken
    /// by a higher-precedence stage.
    ///
    /// This is deliberately surfaced rather than silently swallowed: a sensor
    /// whose labels are being dropped is a real condition an operator should be
    /// able to see, and it is exactly the signal that would have made the
    /// duplicate-`device` bug visible years earlier.
    pub shadowed: u32,
}

/// Accumulates labels under the precedence rule above.
///
/// Names are normalised through the caller's sanitizer *before* the collision
/// check, so two names that differ only in punctuation (`if-name` and
/// `if.name`, both `if_name` to Prometheus) collide correctly rather than
/// producing a duplicate.
#[derive(Debug)]
pub struct LabelMerger<F> {
    seen: BTreeMap<String, (LabelSource, String)>,
    shadowed: u32,
    sanitize: F,
}

impl<F> LabelMerger<F>
where
    F: Fn(&str) -> String,
{
    /// Create a merger that normalises label names with `sanitize`.
    ///
    /// Prometheus passes its `[a-zA-Z_][a-zA-Z0-9_]*` sanitizer; OTel, whose
    /// attribute keys are unconstrained, can pass an identity function and
    /// still get the collision guarantee.
    pub fn new(sanitize: F) -> Self {
        Self {
            seen: BTreeMap::new(),
            shadowed: 0,
            sanitize,
        }
    }

    /// Offer one label at a given precedence stage.
    ///
    /// Kept if the (sanitized) name is free, or if `source` is strictly
    /// stronger than whatever holds the name. Otherwise dropped and counted.
    /// An empty name or an empty value is dropped without counting — neither is
    /// a label, and remote-write drops empty values anyway.
    pub fn offer(&mut self, name: &str, value: impl Into<String>, source: LabelSource) {
        let key = (self.sanitize)(name);
        let value = value.into();
        if key.is_empty() || value.is_empty() {
            return;
        }
        match self.seen.get(&key) {
            None => {
                self.seen.insert(key, (source, value));
            }
            Some((held, _)) if source < *held => {
                // A stronger stage arriving late still wins, so callers are not
                // forced into a fixed call order to be correct.
                self.seen.insert(key, (source, value));
                self.shadowed += 1;
            }
            Some(_) => {
                self.shadowed += 1;
            }
        }
    }

    /// Offer many labels at one stage.
    pub fn offer_all<'a, I, K, V>(&mut self, labels: I, source: LabelSource)
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str> + 'a,
        V: Into<String>,
    {
        for (k, v) in labels {
            self.offer(k.as_ref(), v, source);
        }
    }

    /// Whether a (sanitized) name is already held.
    pub fn holds(&self, name: &str) -> bool {
        self.seen.contains_key(&(self.sanitize)(name))
    }

    /// Finish, yielding labels sorted by name and unique by name.
    pub fn finish(self) -> MergedLabels {
        MergedLabels {
            labels: self.seen.into_iter().map(|(k, (_, v))| (k, v)).collect(),
            shadowed: self.shadowed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident(s: &str) -> String {
        s.to_string()
    }

    /// The exact shape of #753: `device` offered by both the semconv table and
    /// the sensor's own labels.
    #[test]
    fn the_duplicate_device_bug_cannot_be_expressed() {
        let mut m = LabelMerger::new(ident);
        m.offer("source", "host01", LabelSource::Structural);
        m.offer("direction", "read", LabelSource::SemconvConstant);
        m.offer("device", "sda", LabelSource::PatternVar);
        m.offer("device", "sda", LabelSource::PointLabel);
        let out = m.finish();

        let devices: Vec<_> = out.labels.iter().filter(|(k, _)| k == "device").collect();
        assert_eq!(devices.len(), 1, "exactly one `device`: {:?}", out.labels);
        assert_eq!(out.shadowed, 1, "the dropped candidate is counted");
    }

    /// A sensor must not be able to overwrite a label derived from the key.
    /// `origin` is the RFC 06 minted host id; a payload asserting its own
    /// origin would be a consumer-visible lie.
    #[test]
    fn a_point_label_cannot_forge_structural_truth() {
        let mut m = LabelMerger::new(ident);
        m.offer("origin", "h-abcdef123456", LabelSource::Structural);
        m.offer("protocol", "sysinfo", LabelSource::Structural);
        m.offer_all(
            [("origin", "h-000000000000"), ("protocol", "evil")],
            LabelSource::PointLabel,
        );
        let out = m.finish();

        assert_eq!(out.labels[0], ("origin".into(), "h-abcdef123456".into()));
        assert_eq!(out.labels[1], ("protocol".into(), "sysinfo".into()));
        assert_eq!(out.shadowed, 2);
    }

    /// Two names that sanitize to the same thing are one name. Without
    /// normalising before the check, this is a duplicate on the wire.
    #[test]
    fn names_collide_after_sanitization_not_before() {
        let sanitize = |s: &str| s.replace(['-', '.'], "_");
        let mut m = LabelMerger::new(sanitize);
        m.offer("if-name", "eth0", LabelSource::PointLabel);
        m.offer("if.name", "eth1", LabelSource::PointLabel);
        let out = m.finish();

        assert_eq!(
            out.labels,
            vec![("if_name".to_string(), "eth0".to_string())]
        );
        assert_eq!(out.shadowed, 1);
    }

    /// Precedence is by stage, not by call order — a caller should not have to
    /// know the right order to be correct.
    #[test]
    fn a_stronger_stage_wins_even_when_it_arrives_late() {
        let mut m = LabelMerger::new(ident);
        m.offer("device", "from-sensor", LabelSource::PointLabel);
        m.offer("device", "from-registry", LabelSource::PatternVar);
        let out = m.finish();

        assert_eq!(
            out.labels,
            vec![("device".to_string(), "from-registry".to_string())]
        );
    }

    /// Config defaults fill gaps but never override.
    #[test]
    fn config_defaults_are_the_weakest_stage() {
        let mut m = LabelMerger::new(ident);
        m.offer("env", "prod", LabelSource::PointLabel);
        m.offer_all(
            [("env", "staging"), ("region", "eu-west")],
            LabelSource::ConfigDefault,
        );
        let out = m.finish();

        assert_eq!(
            out.labels,
            vec![
                ("env".to_string(), "prod".to_string()),
                ("region".to_string(), "eu-west".to_string()),
            ]
        );
        assert_eq!(out.shadowed, 1);
    }

    #[test]
    fn empty_names_and_values_are_not_labels() {
        let mut m = LabelMerger::new(ident);
        m.offer("", "x", LabelSource::PointLabel);
        m.offer("k", "", LabelSource::PointLabel);
        let out = m.finish();
        assert!(out.labels.is_empty());
        assert_eq!(out.shadowed, 0, "a non-label is not a shadowed label");
    }

    #[test]
    fn output_is_sorted_and_unique() {
        let mut m = LabelMerger::new(ident);
        for (k, v) in [("z", "1"), ("a", "2"), ("m", "3"), ("a", "4")] {
            m.offer(k, v, LabelSource::PointLabel);
        }
        let out = m.finish();
        let names: Vec<_> = out.labels.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, vec!["a", "m", "z"]);
    }
}
