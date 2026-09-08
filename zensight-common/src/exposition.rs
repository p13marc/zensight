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

// ===========================================================================
// Registry-driven metric identity (#764)
// ===========================================================================

/// What kind of series a telemetry value becomes, backend-neutral.
///
/// Prometheus maps `Text` to an info-style gauge; OTel drops it. Neither
/// exports `Unsupported`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    /// A cumulative total.
    Counter,
    /// A current level.
    Gauge,
    /// A string value.
    Text,
    /// Nothing an exporter can represent (binary blobs).
    Unsupported,
}

impl MetricKind {
    /// The kind a telemetry value would naturally take, before any
    /// [`KIND_OVERRIDE`] correction.
    pub fn of(value: &crate::telemetry::TelemetryValue) -> Self {
        use crate::telemetry::TelemetryValue as V;
        match value {
            V::Counter(_) => MetricKind::Counter,
            V::Gauge(_) | V::Boolean(_) => MetricKind::Gauge,
            V::Text(_) => MetricKind::Text,
            V::Binary(_) => MetricKind::Unsupported,
        }
    }
}

/// Why a key could not be refined through the registry.
///
/// Carried rather than swallowed so the caller can count it: "a subject that is
/// not registered does not exist" is worth nothing if the exporter quietly
/// invents a name for it anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unrefined {
    /// Not a v1 key at all.
    NotAV1Key,
    /// A v1 key, but not in the telemetry class.
    NotTelemetryClass,
    /// A v1 telemetry key whose subject the registry does not know.
    SubjectNotRegistered,
}

impl Unrefined {
    /// A stable token for a `reason` label on the self-metric.
    pub fn reason(&self) -> &'static str {
        match self {
            Unrefined::NotAV1Key => "not_a_v1_key",
            Unrefined::NotTelemetryClass => "not_telemetry_class",
            Unrefined::SubjectNotRegistered => "subject_not_registered",
        }
    }
}

/// A metric's identity, derived from the key and the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricIdentity {
    /// Name chunks. Prometheus joins with `_` under its prefix; OTel with `.`.
    pub name: Vec<String>,
    /// True when the name came from the semconv table, in which case it is
    /// already a complete dotted name and must not gain a producer chunk.
    pub semconv: bool,
    /// Merged, sorted, unique by name.
    pub labels: Vec<(String, String)>,
    /// Label candidates dropped because a higher-precedence stage held the name.
    pub shadowed: u32,
    /// UCUM-ish unit, when the registry or the point supplies one.
    pub unit: Option<String>,
    /// The registry's own sentence for this subject, for `# HELP` (#768).
    pub description: Option<String>,
    pub kind: MetricKind,
}

/// Producers whose telemetry tail is defined by the polled device, registered
/// as a rest-var catch-all (`{device}/{metric...}`).
///
/// Their pattern has no literal chunks at all, so the family rule would give an
/// empty name. The rest variable's **value** becomes the name instead, which
/// preserves exactly what these producers already exported while promoting the
/// leading `{device}` chunk from "buried in the name" to a real label.
///
/// This is a property of the registry, not a preference — see
/// `registry_audit::has_catchall_telemetry`. Fixing it properly means changing
/// what the *producer* publishes (#769 does that for SNMP's interface index),
/// not adding a rule here.
const REST_VAR_PRODUCERS: &[&str] = &["snmp", "modbus", "gnmi", "netflow"];

/// Patterns whose family name would lose its discriminator under the plain
/// rule, because their leading chunk is a variable.
///
/// `{cpu}/times/{component}` would become `times`, orphaned from its
/// `cpu/times/{component}` sibling. The alias reunites them into one family
/// told apart by the `cpu` label — the same shape as `cpu/usage` versus
/// `cpu/{core}/usage`, and therefore an *intended* collision (see
/// `INTENDED_COLLISIONS` in the naming tests).
const NAME_ALIASES: &[(&str, &str, &str)] = &[
    ("sysinfo", "{cpu}/times/{component}", "cpu/times"),
    (
        "sysinfo",
        "{cpu}/schedstat/run_delay_ns_total",
        "cpu/schedstat/run_delay_ns_total",
    ),
];

/// The family name for a registered pattern, as chunks, without the producer.
///
/// Public so the naming conformance tests can walk every registry pattern
/// through exactly the code the exporters use.
pub fn family_chunks(
    producer: &str,
    pattern: &str,
    vars: &[(&'static str, String)],
) -> Vec<String> {
    // A rest-var producer's name comes from the rest variable's value.
    if REST_VAR_PRODUCERS.contains(&producer)
        && let Some((_, tail)) = vars
            .iter()
            .find(|(n, _)| pattern.contains(&format!("{{{n}...}}")))
    {
        return tail.split('/').map(str::to_string).collect();
    }

    let effective = NAME_ALIASES
        .iter()
        .find(|(p, pat, _)| *p == producer && *pat == pattern)
        .map(|(_, _, alias)| *alias)
        .unwrap_or(pattern);

    effective
        .split('/')
        .filter(|c| !c.starts_with('{'))
        .map(str::to_string)
        .collect()
}

/// Derive a metric's identity from its **key** and payload.
///
/// `key` is base-relative: the Zenoh session namespace already stripped the
/// deployment base (#466), which is what every sample an exporter receives
/// looks like.
///
/// This is the direction #475 mandates — decode keys through the registry, not
/// with `split('/')`. The exporters were the last consumers still naming from
/// the payload, which is why per-entity subjects ended up baked into metric
/// *names* and `sum by (iface)` was impossible for every producer the semconv
/// table did not hand-map.
pub fn identify<F>(
    key: &str,
    point: &crate::telemetry::TelemetryPoint,
    defaults: &std::collections::HashMap<String, String>,
    sanitize: F,
) -> Result<MetricIdentity, Unrefined>
where
    F: Fn(&str) -> String,
{
    use zenkey::grammar::{Class, ClassOrPlane};

    let parsed = crate::keyexpr::parse_key(key).ok_or(Unrefined::NotAV1Key)?;
    if !matches!(parsed.class, ClassOrPlane::Class(Class::Telemetry)) {
        return Err(Unrefined::NotTelemetryClass);
    }
    let (parsed, producer, subject) =
        crate::keyexpr::refine_key(key).ok_or(Unrefined::SubjectNotRegistered)?;

    let vars = subject.vars();
    let pattern = subject.pattern();
    let sc = crate::semconv::semconv_of(&subject);

    // ---- name -------------------------------------------------------------
    let (name, is_semconv) = match &sc {
        Some(sc) => (
            sc.name.split('.').map(str::to_string).collect::<Vec<_>>(),
            true,
        ),
        None => {
            let mut chunks = vec![producer.clone()];
            chunks.extend(family_chunks(&producer, pattern, &vars));
            (chunks, false)
        }
    };

    // ---- labels -----------------------------------------------------------
    let mut merger = LabelMerger::new(&sanitize);

    // Stage 1: structural, straight off the key. `origin` is the RFC 06 minted
    // host id — the thing a hostname cannot be trusted to be.
    merger.offer("origin", parsed.origin.to_string(), LabelSource::Structural);
    merger.offer("source", point.source.clone(), LabelSource::Structural);
    merger.offer("protocol", producer.clone(), LabelSource::Structural);
    if let Some(p) = parsed.producer()
        && let Some(instance) = p.instance()
    {
        merger.offer(
            "producer_instance",
            instance.to_string(),
            LabelSource::Structural,
        );
    }

    // Stage 2/3: the semconv table names an attribute, the registry supplies
    // its value. When there is no semconv entry the pattern variables ride
    // under their own registry names.
    match &sc {
        Some(sc) => merger.offer_all(
            sc.attributes.iter().map(|(k, v)| (*k, v.clone())),
            LabelSource::SemconvConstant,
        ),
        None => merger.offer_all(
            vars.iter()
                .filter(|(n, _)| !pattern.contains(&format!("{{{n}...}}")))
                .map(|(n, v)| (*n, v.clone())),
            LabelSource::PatternVar,
        ),
    }
    // A rest-var producer's leading variables are labels even though the rest
    // variable itself became the name. Only when the semconv arm ran: the
    // `None` arm above already offered exactly this set, and offering it
    // twice counted every pattern variable as *shadowed* on every point of
    // the four rest-var producers — the diagnostic that exists to make a
    // dropped label visible read permanently non-zero and meant nothing.
    if sc.is_some() && REST_VAR_PRODUCERS.contains(&producer.as_str()) {
        merger.offer_all(
            vars.iter()
                .filter(|(n, _)| !pattern.contains(&format!("{{{n}...}}")))
                .map(|(n, v)| (*n, v.clone())),
            LabelSource::PatternVar,
        );
    }

    // ---- unit -------------------------------------------------------------
    //
    // A label literally named `unit` is a unit-of-measure annotation on every
    // producer except systemd, where `{unit}` is the systemd unit name and has
    // already been claimed at a stronger stage. Consume it rather than emitting
    // a dimension that is not one.
    //
    // The generated `AnySubject::unit()` reads the registry's `unit =` column.
    // Only one subject in the whole registry declares one today (netring's
    // `ms`), so this is plumbing ahead of data — populate the TOMLs and every
    // exporter gets it for free, because the registry is the only place that
    // value belongs (#767).
    let docs = crate::registry_audit::telemetry_subject_docs(&producer, pattern);
    let mut unit = subject
        .unit()
        .map(str::to_string)
        .or_else(|| docs.as_ref().and_then(|d| d.unit.clone()))
        .or_else(|| point.unit.clone());
    for (k, v) in &point.labels {
        if k == "unit" && !merger.holds("unit") && unit.is_none() {
            unit = Some(v.clone());
            continue;
        }
        merger.offer(k, v.clone(), LabelSource::PointLabel);
    }

    merger.offer_all(defaults, LabelSource::ConfigDefault);
    let merged = merger.finish();

    // ---- kind -------------------------------------------------------------
    //
    // A `.rate` sibling (docs/KEYSPACE.md) is a derived per-second gauge that
    // rides inside its family as a dot-suffix on the leaf. It is recognised —
    // kind and unit are corrected — but the suffix is deliberately NOT
    // stripped: `in_octets.rate` and `in_octets` are different series, and
    // collapsing them would put a gauge and a counter in one family.
    let leaf_is_rate = point.metric.ends_with(".rate");
    let mut kind = MetricKind::of(&point.value);
    if leaf_is_rate {
        kind = MetricKind::Gauge;
    }
    if let Some(over) = kind_override(&producer, pattern) {
        kind = over;
    }

    Ok(MetricIdentity {
        name,
        semconv: is_semconv,
        labels: merged.labels,
        shadowed: merged.shadowed,
        unit,
        description: docs.and_then(|d| d.description),
        kind,
    })
}

/// Patterns that are a LEVEL however the sensor typed them.
///
/// #766 fixed the sysinfo sensor: `memory/used`, the filesystem levels, the TCP
/// state counts and friends were published as `TelemetryValue::Counter` even
/// though they go **down**. This is the backstop for the same keys arriving
/// from a sensor that has not been upgraded — an exporter has to be right
/// against whatever is on the bus, and a level exported as a monotonic Sum is a
/// contract violation on the OTel side, not merely a bad panel on the
/// Prometheus one.
///
/// The tell is a doubled suffix: a level typed as a counter renders as
/// `..._bytes_total`, which is how these were found.
///
/// The principled fix is a `monotonic = true|false` column in the registry, so
/// the answer lives with the subject instead of in a table here. That needs a
/// `zenkey`/`zenkey-build` release (`zenkey-build` lints unknown keys, so it
/// cannot be added to the TOMLs unilaterally) and is filed upstream.
pub const KIND_OVERRIDE: &[(&str, &str, MetricKind)] = &[
    ("sysinfo", "system/uptime", MetricKind::Gauge),
    ("sysinfo", "system/boot_time", MetricKind::Gauge),
    ("sysinfo", "memory/total", MetricKind::Gauge),
    ("sysinfo", "memory/used", MetricKind::Gauge),
    ("sysinfo", "memory/available", MetricKind::Gauge),
    ("sysinfo", "memory/swap_total", MetricKind::Gauge),
    ("sysinfo", "memory/swap_used", MetricKind::Gauge),
    ("sysinfo", "disk/{mount}/total", MetricKind::Gauge),
    ("sysinfo", "disk/{mount}/used", MetricKind::Gauge),
    ("sysinfo", "disk/{mount}/available", MetricKind::Gauge),
    // `process/{rank}/memory` was here until #1070 retired the family. The
    // correction had nothing left to correct, and `kind_overrides_name_real_patterns`
    // is the test that says so — an override for a subject that no longer
    // exists is a rule nobody can find their way back from.
];

fn kind_override(producer: &str, pattern: &str) -> Option<MetricKind> {
    KIND_OVERRIDE
        .iter()
        .find(|(p, pat, _)| *p == producer && *pat == pattern)
        .map(|(_, _, k)| *k)
}
