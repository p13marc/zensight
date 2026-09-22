//! Producer-agnostic intake (#1256): the pure judgements the fold makes about
//! a sample from a producer this GUI was **not** compiled with.
//!
//! The compiled registry (`zensight_common::registry`) answers "what type is
//! this subject" only for the producers it was built from. Everything else on
//! the bus — a newer sensor, a third party's, the system-view fixture — used
//! to be dropped at decode, silently, with the registry, the conformance
//! judges and the bus explorer all agreeing the key was fine. The fleet's own
//! `introspect` replies (the runtime [`SliceSet`]) and its `describe` replies
//! (the per-producer [`SchemaSet`]) say the same things at runtime, for every
//! producer that is alive; this module judges a sample against those, and
//! says *which* it could not judge and why, rather than pretending.
//!
//! Nothing here touches Iced or Zenoh. The verdicts render in
//! [`crate::view::device`]; the app folds documents in `update`, at fold time
//! rather than decode time, so a document that arrives before its slice is
//! re-judged when the slice lands (the late-joiner case is the normal one:
//! subscriptions are up before the first sweep answers).

use std::collections::{BTreeSet, HashMap};

use zenkey::CommonFamily;
use zenkey_fleet::SliceSet;
use zensight_common::schema::{NotValidated, SchemaSet, Verdict};

/// Whether a producer's slice declares a subject. Three states, never a
/// boolean: "not declared" is a finding about the producer, "no slice" is a
/// finding about the fleet (nobody answered `introspect` for it), and a view
/// that showed the two alike would blame the wrong party.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Declared {
    /// The slice lists a pattern the subject matches.
    Yes,
    /// The producer served a slice, and the subject matches nothing in it.
    No,
    /// No slice for this producer in the last sweep.
    NoSlice,
}

/// A state document held for a device — the `(type, verdict, declared)`
/// triple beside the value, so the view can say what it knows.
pub struct DocumentState {
    /// The subject tail, as published (chunks 5.. of the key, joined).
    pub subject: String,
    /// The slice's declared type for the subject, when declared.
    pub type_name: Option<String>,
    pub value: serde_json::Value,
    pub verdict: Verdict,
    pub declared: Declared,
    /// Our clock at fold, for the "as of" caption.
    pub received_ms: i64,
    /// The typed projection a view asked for, decoded once (#1261): a view
    /// borrows `&state` and hands Iced elements that borrow the rows, so the
    /// decoded document must live here, beside the value, not on a stack.
    pub(crate) typed: std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>,
}

impl std::fmt::Debug for DocumentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocumentState")
            .field("subject", &self.subject)
            .field("type_name", &self.type_name)
            .field("value", &self.value)
            .field("verdict", &self.verdict)
            .field("declared", &self.declared)
            .field("received_ms", &self.received_ms)
            .finish_non_exhaustive()
    }
}

impl Clone for DocumentState {
    /// The wire value clones; the typed projection is decoded again on
    /// first use — it is a cache, not state.
    fn clone(&self) -> Self {
        DocumentState {
            subject: self.subject.clone(),
            type_name: self.type_name.clone(),
            value: self.value.clone(),
            verdict: self.verdict.clone(),
            declared: self.declared,
            received_ms: self.received_ms,
            typed: std::sync::OnceLock::new(),
        }
    }
}

impl DocumentState {
    pub fn new(
        subject: String,
        type_name: Option<String>,
        value: serde_json::Value,
        verdict: Verdict,
        declared: Declared,
        received_ms: i64,
    ) -> Self {
        DocumentState {
            subject,
            type_name,
            value,
            verdict,
            declared,
            received_ms,
            typed: std::sync::OnceLock::new(),
        }
    }

    /// The document as a type, decoded once and borrowed from then on —
    /// what a bespoke view reads a state document as (snmp's
    /// `InterfaceTable`), while the generic view keeps rendering the value
    /// as it came. One document, one type: a second type asked of the same
    /// document is an error, not a second decode.
    pub fn decoded<T>(&self) -> Result<&T, String>
    where
        T: serde::de::DeserializeOwned + std::any::Any + Send + Sync,
    {
        let slot = self.typed.get_or_init(|| {
            Box::new(serde_json::from_value::<T>(self.value.clone()).map_err(|e| e.to_string()))
                as Box<dyn std::any::Any + Send + Sync>
        });
        match slot.downcast_ref::<Result<T, String>>() {
            Some(Ok(t)) => Ok(t),
            Some(Err(e)) => Err(e.clone()),
            None => Err(format!(
                "document already decoded as another type than {}",
                std::any::type_name::<T>()
            )),
        }
    }
}

/// An events-class record held in the ring — wire facts only.
#[derive(Debug, Clone)]
pub struct EventState {
    pub origin: String,
    pub producer: String,
    pub subject: String,
    pub value: serde_json::Value,
    pub received_ms: i64,
}

/// The bound on undeclared subjects remembered per device. A producer with
/// a firehose of unique undeclared subjects is itself the finding; listing
/// every one of them would make the finding unreadable and the map unbounded.
pub const UNDECLARED_CAP: usize = 256;

/// Undeclared-subject bookkeeping for one device: the subjects seen that the
/// producer's slice does not declare, plus the count that fell off the cap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Undeclared {
    /// Subject tails, as the wire has them — what the banner lists.
    pub subjects: BTreeSet<String>,
    /// The metric names those subjects carried — what the metric table is
    /// keyed by, so a row can wear the marker.
    pub metrics: BTreeSet<String>,
    pub overflow: usize,
}

impl Undeclared {
    pub fn insert(&mut self, subject: &str, metric: &str) {
        if self.subjects.contains(subject) {
            return;
        }
        if self.subjects.len() >= UNDECLARED_CAP {
            self.overflow += 1;
        } else {
            self.subjects.insert(subject.to_string());
            self.metrics.insert(metric.to_string());
        }
    }

    pub fn is_empty(&self) -> bool {
        self.subjects.is_empty() && self.overflow == 0
    }

    pub fn len(&self) -> usize {
        self.subjects.len() + self.overflow
    }
}

/// Is `subject` (a tail, `a/b/c`) declared by `producer`'s slice for `class`?
pub fn declared(slices: &SliceSet, producer: &str, class: &str, subject: &str) -> Declared {
    if slices.get(producer).is_none() {
        return Declared::NoSlice;
    }
    let tail: Vec<&str> = subject.split('/').collect();
    match slices.refine(producer, class, &tail) {
        Some(_) => Declared::Yes,
        None => Declared::No,
    }
}

/// The declared type name for a state subject, when the slice declares it.
pub fn declared_type(slices: &SliceSet, producer: &str, subject: &str) -> Option<String> {
    let tail: Vec<&str> = subject.split('/').collect();
    slices
        .refine(producer, "state", &tail)
        .map(|(decl, _)| decl.type_name.clone())
}

/// Judge a state document: the declared type, the schema verdict against the
/// producer's served set, and whether the subject was declared at all.
///
/// The not-validated reasons are distinct on purpose (RFC 09 §5.1 O4):
/// `NoRegistry` when the producer served no slice, `NoSchema` when the slice
/// names a type the `describe` reply does not carry (or declares no such
/// subject), and whatever [`zensight_common::schema::verdict_against`] says
/// once there is a schema to check against.
pub fn judge(
    slices: &SliceSet,
    schemas: &HashMap<String, SchemaSet>,
    producer: &str,
    subject: &str,
    value: &serde_json::Value,
) -> (Option<String>, Verdict, Declared) {
    let declared = declared(slices, producer, "state", subject);
    let type_name = match declared {
        Declared::Yes => declared_type(slices, producer, subject),
        Declared::No | Declared::NoSlice => None,
    };
    let verdict = match (&declared, &type_name) {
        (Declared::NoSlice, _) => Verdict::NotValidated(NotValidated::NoRegistry),
        (_, None) => Verdict::NotValidated(NotValidated::NoSchema),
        (_, Some(ty)) => match schemas.get(producer).and_then(|set| set.get(ty)) {
            Some(schema) => zensight_common::schema::verdict_against(schema, value),
            None => Verdict::NotValidated(NotValidated::NoSchema),
        },
    };
    (type_name, verdict, declared)
}

/// The framework family a state subject tail belongs to, by the token
/// vocabulary alone — the question a consumer has to answer for a producer it
/// has no registry for, since `health`/`errors`/`sensor`/`alert/{key}` mean
/// the same thing under every producer (RFC 06 §3).
///
/// Returns the family and its variable binding, if the family takes one.
/// A tail with extra chunks past the family's shape is not that family.
pub fn common_family_of(tail: &[&str]) -> Option<(CommonFamily, Option<String>)> {
    CommonFamily::ALL.iter().copied().find_map(|family| {
        let prefix = family.prefix();
        if !tail.starts_with(prefix) {
            return None;
        }
        let rest = &tail[prefix.len()..];
        match (family.var(), rest) {
            (None, []) => Some((family, None)),
            (Some(_), [var]) => Some((family, Some((*var).to_string()))),
            _ => None,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::fake_sensor::{PRODUCER, SLICE};

    fn slices() -> SliceSet {
        SliceSet::from_slices(vec![
            zenkey::slice::parse_slice(SLICE).expect("fixture slice parses"),
        ])
    }

    /// The fixture slice declares seven of the eight telemetry subjects the
    /// fixture publishes; `humidity/pct` is the one it does not (#1254 gate 4).
    #[test]
    fn declared_over_the_fixture_slice() {
        let s = slices();
        for subject in [
            "rack7/temp/inlet/celsius",
            "rack7/temp/inlet/upper_critical_c",
            "rack7/temp/outlet/celsius",
            "rack7/temp/outlet/upper_critical_c",
            "rack7/temp/exhaust/celsius",
            "rack7/uplink/rx_bytes",
        ] {
            assert_eq!(
                declared(&s, PRODUCER, "telemetry", subject),
                Declared::Yes,
                "{subject}"
            );
        }
        assert_eq!(
            declared(&s, PRODUCER, "telemetry", "rack7/humidity/pct"),
            Declared::No
        );
        assert_eq!(
            declared(&s, PRODUCER, "state", "rack7/status"),
            Declared::Yes
        );
        // A class the slice declares nothing for is undeclared, not absent.
        assert_eq!(declared(&s, PRODUCER, "events", "rack7/boot"), Declared::No);
    }

    /// No slice is a different answer from not declared.
    #[test]
    fn no_slice_is_not_the_same_as_not_declared() {
        let s = slices();
        assert_eq!(
            declared(&s, "nobody", "telemetry", "x/y"),
            Declared::NoSlice
        );
        assert_eq!(
            declared(
                &SliceSet::default(),
                PRODUCER,
                "telemetry",
                "rack7/temp/inlet/celsius"
            ),
            Declared::NoSlice
        );
    }

    /// The three-state judgement: the declared type resolves, the schema is
    /// consulted when the producer served one, and each absence names itself.
    #[test]
    fn judge_is_three_state() {
        let s = slices();
        let doc = serde_json::json!({"unit": "rack7", "mode": "run", "uptime_s": 86400});
        // Declared, schema served.
        let mut schemas = HashMap::new();
        schemas.insert(
            PRODUCER.to_string(),
            SchemaSet::parse(crate::mock::fake_sensor::SCHEMAS).expect("schemas parse"),
        );
        let (ty, verdict, declared) = judge(&s, &schemas, PRODUCER, "rack7/status", &doc);
        assert_eq!(ty.as_deref(), Some("FakeUnitStatus"));
        assert_eq!(declared, Declared::Yes);
        #[cfg(feature = "validate")]
        assert_eq!(verdict, Verdict::Valid, "a conformant document validates");
        #[cfg(not(feature = "validate"))]
        assert_eq!(verdict, Verdict::NotValidated(NotValidated::FeatureOff));

        // Declared, no schema served: NoSchema — asked, and the type has none.
        let (ty, verdict, _) = judge(&s, &HashMap::new(), PRODUCER, "rack7/status", &doc);
        assert_eq!(ty.as_deref(), Some("FakeUnitStatus"));
        assert_eq!(verdict, Verdict::NotValidated(NotValidated::NoSchema));

        // Undeclared subject: no type, NoSchema.
        let (ty, verdict, declared) = judge(&s, &schemas, PRODUCER, "rack7/mystery", &doc);
        assert_eq!(ty, None);
        assert_eq!(declared, Declared::No);
        assert_eq!(verdict, Verdict::NotValidated(NotValidated::NoSchema));

        // No slice: NoRegistry.
        let (_, verdict, declared) = judge(&s, &schemas, "nobody", "rack7/status", &doc);
        assert_eq!(declared, Declared::NoSlice);
        assert_eq!(verdict, Verdict::NotValidated(NotValidated::NoRegistry));
    }

    /// A non-conformant document is `Invalid` with the violation named — the
    /// case a boolean would have hidden (#791's three states).
    #[cfg(feature = "validate")]
    #[test]
    fn judge_reports_a_violation() {
        let s = slices();
        let mut schemas = HashMap::new();
        schemas.insert(
            PRODUCER.to_string(),
            SchemaSet::parse(crate::mock::fake_sensor::SCHEMAS).expect("schemas parse"),
        );
        let bad = serde_json::json!({"unit": "rack7", "mode": "melting", "uptime_s": -1});
        let (_, verdict, _) = judge(&s, &schemas, PRODUCER, "rack7/status", &bad);
        match verdict {
            Verdict::Invalid(problems) => assert!(!problems.is_empty()),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    /// The framework vocabulary is recognised by its tokens alone.
    #[test]
    fn common_family_by_tokens() {
        assert_eq!(
            common_family_of(&["health"]),
            Some((CommonFamily::Health, None))
        );
        assert_eq!(
            common_family_of(&["alert", "cpu-hot"]),
            Some((CommonFamily::Alert, Some("cpu-hot".to_string())))
        );
        assert_eq!(
            common_family_of(&["evidence", "device", "sw1"]),
            Some((CommonFamily::EvidenceDevice, Some("sw1".to_string())))
        );
        // Wrong shape: a bare `alert`, a `health` with a tail, a foreign token.
        assert_eq!(common_family_of(&["alert"]), None);
        assert_eq!(common_family_of(&["health", "x"]), None);
        assert_eq!(common_family_of(&["rack7", "status"]), None);
    }

    /// The undeclared set is bounded and counts what it dropped.
    #[test]
    fn undeclared_is_bounded() {
        let mut u = Undeclared::default();
        for i in 0..(UNDECLARED_CAP + 10) {
            u.insert(&format!("s/{i}"), &format!("s/{i}"));
        }
        assert_eq!(u.subjects.len(), UNDECLARED_CAP);
        assert_eq!(u.overflow, 10);
        assert_eq!(u.len(), UNDECLARED_CAP + 10);
        // A repeat is not an overflow.
        u.insert("s/0", "s/0");
        assert_eq!(u.overflow, 10);
    }

    /// A document decodes once as the type a view asks for, and is borrowed
    /// from then on (#1261); the wrong shape is an error, never a default.
    #[test]
    fn a_document_decodes_once_as_a_type() {
        let doc = DocumentState::new(
            "router01/interfaces".into(),
            Some("InterfaceTable".into()),
            serde_json::to_value(crate::mock::snmp::interface_table("router01", 1)).unwrap(),
            Verdict::NotValidated(NotValidated::NoSchema),
            Declared::Yes,
            0,
        );
        let a: &zensight_common::InterfaceTable = doc.decoded().expect("decodes");
        let b: &zensight_common::InterfaceTable = doc.decoded().expect("the same");
        assert!(std::ptr::eq(a, b), "decoded once, borrowed twice");
        assert_eq!(a.device, "router01");
        assert!(
            doc.decoded::<Vec<u8>>().is_err(),
            "a second type is refused"
        );
        let wrong = DocumentState::new(
            "x".into(),
            None,
            serde_json::json!([1, 2]),
            Verdict::NotValidated(NotValidated::NoSchema),
            Declared::No,
            0,
        );
        assert!(wrong.decoded::<zensight_common::InterfaceTable>().is_err());
        // A clone starts over: a cache, not state.
        assert!(doc.clone().decoded::<Vec<u8>>().is_err());
        assert!(
            doc.clone()
                .decoded::<zensight_common::InterfaceTable>()
                .is_ok()
        );
    }
}
