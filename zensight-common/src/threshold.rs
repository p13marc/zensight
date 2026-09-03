//! Threshold rules a sensor owns (#928, epic #901).
//!
//! # Why this type exists
//!
//! ZenSight had **two alerting authorities**. One runs in every sensor: it has
//! `for`, adopt-on-restart, a seed queryable, and it publishes to the bus. The
//! other ran in one GUI's memory, had a 60-second cooldown, persisted to a
//! JSON file on one laptop, and its alerts reached *nothing* — not the bus,
//! not the exporters, not the notifier. An operator who set a threshold there
//! had made a note to themselves that looked like monitoring.
//!
//! This is the vocabulary that lets the *sensor* own the rule instead. It is
//! deliberately the smallest thing that can express what the GUI engine
//! expressed, plus the two kinds of hysteresis the tree had nowhere:
//!
//! - **Value hysteresis** ([`ThresholdRule::clear`]) is numeric and belongs to
//!   the rule: fire above 90, recover below 80.
//! - **Time hysteresis** (`recover_after_secs`) is generic and belongs to the
//!   reporter, so *every* expectation kind gets it (#929) — not just these.
//!
//! # What a rule matches
//!
//! A metric-name glob plus an optional map of label globs. `source` is just a
//! label, which is what lets a **proxy** sensor — snmp, gnmi, modbus — write
//! one rule per polled device, or one rule for all of them, without the
//! vocabulary knowing that proxies exist.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::alert::AlertSeverity;
use crate::comparison::ComparisonOp;

/// The whole set of threshold rules one producer evaluates.
///
/// This is a `@desired` document: it is set **wholesale**, never appended to,
/// which is why there is no `thresholds/add` procedure. The set you send is
/// the set that runs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ThresholdsConfig {
    /// How long a rule with no `for_secs` of its own must be violated
    /// continuously before it fires.
    ///
    /// `0` — the default — fires on the first sample. That is the right
    /// default for a set an operator is writing by hand: a rule that does
    /// nothing for five minutes after you save it looks broken.
    pub default_for_secs: u64,
    /// How long a rule with no `recover_after_secs` of its own must be clear
    /// before it resolves. `0` resolves immediately.
    pub default_recover_after_secs: u64,
    pub rules: Vec<ThresholdRule>,
}

/// One rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ThresholdRule {
    /// Stable identity. It becomes the alert's rule as `threshold:<name>`, so
    /// renaming a rule retires the old alerts and raises new ones — which is
    /// the honest behaviour, since a renamed rule is a different assertion.
    pub name: String,
    /// Glob over the metric name as the sensor publishes it, e.g.
    /// `cpu/usage_percent` or `if/*/in_errors.rate`.
    pub metric: String,
    /// Label globs, all of which must match. `source` is a label here, so a
    /// proxy sensor keys per device with `{"source": "pdu-a"}` — or matches
    /// every device by leaving it out.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub op: ComparisonOp,
    pub value: f64,
    /// Value hysteresis: the level the metric must cross back through before
    /// the alert resolves. Absent means "resolve as soon as the comparison is
    /// false", which for a value sitting on the threshold is a flap.
    ///
    /// It must be on the **quiet** side of `value` — below it for `>`/`>=`,
    /// above it for `<`/`<=` — and [`ThresholdsConfig::validate`] refuses it
    /// otherwise, because a `clear` on the wrong side produces a rule that can
    /// fire and never recover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clear: Option<f64>,
    /// Per-rule override of [`ThresholdsConfig::default_for_secs`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub for_secs: Option<u64>,
    /// Per-rule override of [`ThresholdsConfig::default_recover_after_secs`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recover_after_secs: Option<u64>,
    #[serde(default)]
    pub severity: AlertSeverity,
    /// Summary template. See [`render_summary`] for the placeholders; absent
    /// gives a generated sentence naming the metric, the value and the rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

impl ThresholdRule {
    /// A minimal rule, for tests and for the GUI's promote form.
    pub fn new(
        name: impl Into<String>,
        metric: impl Into<String>,
        op: ComparisonOp,
        value: f64,
    ) -> Self {
        Self {
            name: name.into(),
            metric: metric.into(),
            labels: BTreeMap::new(),
            op,
            value,
            clear: None,
            for_secs: None,
            recover_after_secs: None,
            severity: AlertSeverity::default(),
            summary: None,
        }
    }

    /// The alert rule slug this threshold raises.
    ///
    /// Namespaced so a threshold alert is never mistaken for one of a sensor's
    /// own semantic rules (`interface_down`, `backup-failed`) — those encode
    /// domain knowledge a number cannot, and the two should not be reconciled
    /// against each other.
    pub fn alert_rule(&self) -> String {
        format!("threshold:{}", self.name)
    }

    /// Whether this rule's metric glob and label globs match a point.
    pub fn matches(&self, metric: &str, labels: &BTreeMap<String, String>) -> bool {
        if !glob_matches(&self.metric, metric) {
            return false;
        }
        self.labels.iter().all(|(key, pattern)| {
            labels
                .get(key)
                .is_some_and(|actual| glob_matches(pattern, actual))
        })
    }

    /// Whether `value` violates this rule.
    pub fn fires(&self, value: f64) -> bool {
        self.op.evaluate(value, self.value)
    }

    /// Whether `value` has recovered — crossed back through `clear` where one
    /// is set, or merely stopped violating where one is not.
    ///
    /// Note this is **not** `!fires(value)`: between `clear` and `value` a
    /// firing alert stays firing. That gap is the whole point.
    pub fn recovered(&self, value: f64) -> bool {
        match self.clear {
            // The quiet side of `clear`, which validate() has already checked
            // is the quiet side of `value`.
            Some(clear) => match self.op {
                ComparisonOp::GreaterThan | ComparisonOp::GreaterOrEqual => value < clear,
                ComparisonOp::LessThan | ComparisonOp::LessOrEqual => value > clear,
                // validate() refuses `clear` on an equality operator, so this
                // is unreachable through a validated config.
                ComparisonOp::Equal | ComparisonOp::NotEqual => !self.fires(value),
            },
            None => !self.fires(value),
        }
    }
}

impl ThresholdsConfig {
    /// Every problem at once.
    ///
    /// Reporting the first and stopping means an operator fixes one rule, runs
    /// it again, and finds the next — which is how a five-minute edit takes
    /// twenty.
    pub fn validate(&self) -> Result<(), String> {
        let mut problems = Vec::new();
        let mut seen = std::collections::HashSet::new();

        for rule in &self.rules {
            let label = if rule.name.trim().is_empty() {
                problems.push("a rule has an empty name".to_string());
                "<unnamed>".to_string()
            } else {
                if !seen.insert(rule.name.as_str()) {
                    // The name is the alert's rule slug: two rules sharing one
                    // would reconcile each other's alerts away every sweep.
                    problems.push(format!("two rules are both named {:?}", rule.name));
                }
                rule.name.clone()
            };

            if rule.metric.trim().is_empty() {
                problems.push(format!("rule {label}: metric glob is empty"));
            } else if glob::Pattern::new(&rule.metric).is_err() {
                problems.push(format!(
                    "rule {label}: {:?} is not a valid metric glob",
                    rule.metric
                ));
            }
            for (key, pattern) in &rule.labels {
                if glob::Pattern::new(pattern).is_err() {
                    problems.push(format!(
                        "rule {label}: label {key} pattern {pattern:?} is not a valid glob"
                    ));
                }
            }

            // A NaN threshold makes every comparison false, so the rule is
            // silently inert — the worst outcome for something an operator
            // wrote down to be told about.
            if !rule.value.is_finite() {
                problems.push(format!(
                    "rule {label}: value must be a finite number, not {}",
                    rule.value
                ));
            }

            if let Some(clear) = rule.clear {
                if !clear.is_finite() {
                    problems.push(format!("rule {label}: clear must be a finite number"));
                }
                match rule.op {
                    ComparisonOp::GreaterThan | ComparisonOp::GreaterOrEqual => {
                        if clear > rule.value {
                            problems.push(format!(
                                "rule {label}: clear ({clear}) is above value ({}) for `{}` — \
                                 the recovery level must be on the QUIET side of the firing \
                                 level, or the rule can fire and never recover",
                                rule.value, rule.op
                            ));
                        }
                    }
                    ComparisonOp::LessThan | ComparisonOp::LessOrEqual => {
                        if clear < rule.value {
                            problems.push(format!(
                                "rule {label}: clear ({clear}) is below value ({}) for `{}` — \
                                 the recovery level must be on the QUIET side of the firing \
                                 level, or the rule can fire and never recover",
                                rule.value, rule.op
                            ));
                        }
                    }
                    ComparisonOp::Equal | ComparisonOp::NotEqual => problems.push(format!(
                        "rule {label}: clear has no meaning with `{}` — there is no quiet \
                         side of an equality test to recover through",
                        rule.op
                    )),
                }
            }
        }

        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }

    /// The effective `for` duration for one rule.
    pub fn for_secs(&self, rule: &ThresholdRule) -> u64 {
        rule.for_secs.unwrap_or(self.default_for_secs)
    }

    /// The effective recovery duration for one rule.
    pub fn recover_after_secs(&self, rule: &ThresholdRule) -> u64 {
        rule.recover_after_secs
            .unwrap_or(self.default_recover_after_secs)
    }
}

/// Glob match over a `/`-separated name, with shell semantics.
///
/// **`*` does not cross a `/`**, and `**` does — which is what someone writing
/// `if/*/in_errors.rate` means, and what stops `cpu/*` quietly matching a
/// deeper family it was never aimed at. The `glob` crate's default is the
/// opposite (`require_literal_separator: false`), so this is set explicitly
/// rather than inherited.
///
/// An invalid pattern matches **nothing**. `validate()` refuses one at the
/// door, so a bad pattern only reaches here through a config nobody validated
/// — and a pattern that cannot compile matching *everything* would turn a typo
/// into a fleet-wide alert.
fn glob_matches(pattern: &str, value: &str) -> bool {
    const OPTIONS: glob::MatchOptions = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        // A metric name has no hidden files; a leading dot is just a
        // character, and treating it specially would be a rule about
        // filesystems in a rule about numbers.
        require_literal_leading_dot: false,
    };
    glob::Pattern::new(pattern).is_ok_and(|p| p.matches_with(value, OPTIONS))
}

/// Render a rule's summary template.
///
/// Placeholders: `{metric}`, `{value}`, `{source}`, `{op}`, `{threshold}`, and
/// `{label.<name>}` for any label on the point.
///
/// **An unknown placeholder is left verbatim**, not blanked. `{lable.if_name}`
/// appearing in the alert text is how an operator finds their typo; an empty
/// gap where a value should be reads as a missing measurement.
pub fn render_summary(
    template: &str,
    metric: &str,
    value: f64,
    source: &str,
    op: ComparisonOp,
    threshold: f64,
    labels: &BTreeMap<String, String>,
) -> String {
    let mut out = String::with_capacity(template.len() + 32);
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            // An unclosed brace is just text.
            out.push_str(&rest[start..]);
            return out;
        };
        let name = &after[..end];
        let replacement = match name {
            "metric" => Some(metric.to_string()),
            "value" => Some(format_number(value)),
            "source" => Some(source.to_string()),
            "op" => Some(op.symbol().to_string()),
            "threshold" => Some(format_number(threshold)),
            _ => name
                .strip_prefix("label.")
                .and_then(|key| labels.get(key).cloned()),
        };
        match replacement {
            Some(text) => out.push_str(&text),
            None => {
                out.push('{');
                out.push_str(name);
                out.push('}');
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// A number for a human: no trailing `.0`, and no fifteen decimal places.
fn format_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{value:.0}")
    } else {
        format!("{value:.2}")
    }
}

/// The sentence a rule with no template gets.
pub fn default_summary(rule: &ThresholdRule, metric: &str, value: f64, source: &str) -> String {
    format!(
        "{source}: {metric} is {} ({} {})",
        format_number(value),
        rule.op.symbol(),
        format_number(rule.value)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn rule() -> ThresholdRule {
        ThresholdRule::new(
            "cpu-hot",
            "cpu/usage_percent",
            ComparisonOp::GreaterThan,
            90.0,
        )
    }

    #[test]
    fn a_metric_glob_matches_the_way_a_shell_glob_does() {
        let mut r = rule();
        r.metric = "if/*/in_errors.rate".to_string();
        assert!(r.matches("if/3/in_errors.rate", &BTreeMap::new()));
        assert!(!r.matches("if/3/out_errors.rate", &BTreeMap::new()));
        // `*` does NOT cross a `/` — shell semantics, set explicitly, because
        // the crate's default is the opposite and `cpu/*` quietly matching a
        // deeper family is exactly the surprise to avoid.
        assert!(!r.matches("if/3/sub/in_errors.rate", &BTreeMap::new()));

        // `**` is there for someone who does mean the whole subtree.
        r.metric = "if/**".to_string();
        assert!(r.matches("if/3/sub/in_errors.rate", &BTreeMap::new()));

        // …and a bare `*` still matches a whole flat family.
        r.metric = "cpu/*".to_string();
        assert!(r.matches("cpu/usage_percent", &BTreeMap::new()));
        assert!(!r.matches("cpu/core/0/usage_percent", &BTreeMap::new()));
    }

    /// `source` is a label, which is what lets a proxy sensor write one rule
    /// per polled device without the vocabulary knowing proxies exist.
    #[test]
    fn label_globs_scope_a_rule_to_devices() {
        let mut r = rule();
        r.labels = labels(&[("source", "pdu-*")]);
        assert!(r.matches("cpu/usage_percent", &labels(&[("source", "pdu-a")])));
        assert!(!r.matches("cpu/usage_percent", &labels(&[("source", "switch01")])));
        // A label the point does not carry cannot match.
        assert!(!r.matches("cpu/usage_percent", &BTreeMap::new()));

        // Every label pattern must match, not just one.
        r.labels = labels(&[("source", "pdu-*"), ("outlet", "3")]);
        assert!(r.matches(
            "cpu/usage_percent",
            &labels(&[("source", "pdu-a"), ("outlet", "3")])
        ));
        assert!(!r.matches(
            "cpu/usage_percent",
            &labels(&[("source", "pdu-a"), ("outlet", "4")])
        ));
    }

    /// **The gap between `clear` and `value` is the whole point**: in it, a
    /// firing alert stays firing. `recovered` is not `!fires`.
    #[test]
    fn value_hysteresis_keeps_a_firing_alert_firing_in_the_band() {
        let mut r = rule();
        r.clear = Some(80.0);

        assert!(r.fires(95.0));
        assert!(!r.recovered(95.0));

        // In the band: no longer violating, and not yet recovered.
        assert!(!r.fires(85.0), "85 is not above 90");
        assert!(
            !r.recovered(85.0),
            "…but it has not crossed back through 80"
        );

        assert!(r.recovered(75.0));
    }

    /// The same, the other way up — the case a `<` rule on free space needs.
    #[test]
    fn value_hysteresis_works_below_as_well_as_above() {
        let mut r = ThresholdRule::new("space-low", "disk/free_pct", ComparisonOp::LessThan, 10.0);
        r.clear = Some(20.0);
        assert!(r.fires(5.0));
        assert!(!r.recovered(15.0), "in the band");
        assert!(r.recovered(25.0));
    }

    /// With no `clear`, a value sitting on the threshold flaps — which is the
    /// behaviour to keep, because it is what an operator who set no hysteresis
    /// asked for.
    #[test]
    fn without_clear_recovery_is_simply_not_firing() {
        let r = rule();
        assert!(r.recovered(90.0));
        assert!(!r.recovered(91.0));
    }

    /// A `clear` on the loud side of `value` produces a rule that can fire and
    /// never recover. Refused at the door.
    #[test]
    fn a_clear_on_the_wrong_side_is_refused_and_says_why() {
        let mut r = rule();
        r.clear = Some(95.0); // above 90 on a `>` rule
        let mut cfg = ThresholdsConfig {
            rules: vec![r],
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("QUIET side"), "{err}");
        assert!(err.contains("never recover"), "{err}");

        // …and the other direction.
        let mut r = ThresholdRule::new("low", "m", ComparisonOp::LessThan, 10.0);
        r.clear = Some(5.0);
        cfg.rules = vec![r];
        assert!(cfg.validate().unwrap_err().contains("QUIET side"));
    }

    /// There is no quiet side of an equality test.
    #[test]
    fn clear_is_meaningless_on_an_equality_operator() {
        let mut r = ThresholdRule::new("eq", "m", ComparisonOp::Equal, 1.0);
        r.clear = Some(0.0);
        let cfg = ThresholdsConfig {
            rules: vec![r],
            ..Default::default()
        };
        assert!(cfg.validate().unwrap_err().contains("no meaning"));
    }

    /// A NaN threshold makes every comparison false, so the rule is silently
    /// inert — the worst outcome for something written down to be told about.
    #[test]
    fn a_non_finite_threshold_is_refused_rather_than_silently_inert() {
        let cfg = ThresholdsConfig {
            rules: vec![ThresholdRule::new(
                "nan",
                "m",
                ComparisonOp::GreaterThan,
                f64::NAN,
            )],
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("finite"), "{err}");
    }

    /// The name is the alert's rule slug, so two the same reconcile each
    /// other's alerts away every sweep.
    #[test]
    fn duplicate_and_empty_names_are_refused() {
        let mut cfg = ThresholdsConfig {
            rules: vec![rule(), rule()],
            ..Default::default()
        };
        assert!(cfg.validate().unwrap_err().contains("both named"));

        cfg.rules = vec![ThresholdRule::new("", "m", ComparisonOp::GreaterThan, 1.0)];
        assert!(cfg.validate().unwrap_err().contains("empty name"));
    }

    /// An invalid glob is refused — and if one ever reaches the matcher, it
    /// matches NOTHING. A pattern that cannot compile matching everything
    /// would turn a typo into a fleet-wide alert.
    #[test]
    fn an_invalid_glob_is_refused_and_matches_nothing_if_it_slips_through() {
        let mut r = rule();
        r.metric = "cpu/[".to_string();
        let cfg = ThresholdsConfig {
            rules: vec![r.clone()],
            ..Default::default()
        };
        assert!(
            cfg.validate()
                .unwrap_err()
                .contains("not a valid metric glob")
        );
        assert!(!r.matches("cpu/[", &BTreeMap::new()), "not even itself");
    }

    /// Every problem at once, so one run fixes the whole document.
    #[test]
    fn validate_reports_every_problem_together() {
        let mut bad = ThresholdRule::new("", "cpu/[", ComparisonOp::GreaterThan, f64::INFINITY);
        bad.labels = labels(&[("source", "[")]);
        let cfg = ThresholdsConfig {
            rules: vec![bad],
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        for expected in [
            "empty name",
            "not a valid metric glob",
            "not a valid glob",
            "finite",
        ] {
            assert!(err.contains(expected), "missing {expected:?} in {err}");
        }
    }

    #[test]
    fn a_healthy_config_validates() {
        let mut cfg = ThresholdsConfig {
            default_for_secs: 120,
            default_recover_after_secs: 60,
            rules: vec![rule()],
        };
        cfg.rules[0].clear = Some(80.0);
        cfg.validate().unwrap();
        assert_eq!(cfg.for_secs(&cfg.rules[0]), 120, "inherits the default");
        cfg.rules[0].for_secs = Some(30);
        assert_eq!(cfg.for_secs(&cfg.rules[0]), 30, "the rule wins");
        assert_eq!(cfg.recover_after_secs(&cfg.rules[0]), 60);
    }

    #[test]
    fn the_alert_rule_slug_is_namespaced() {
        // So a threshold alert is never reconciled against a sensor's own
        // semantic rules, which encode domain knowledge a number cannot.
        assert_eq!(rule().alert_rule(), "threshold:cpu-hot");
    }

    #[test]
    fn a_summary_template_renders_every_placeholder() {
        let l = labels(&[("if_name", "eth0"), ("source", "web01")]);
        let rendered = render_summary(
            "{source}: {metric} on {label.if_name} is {value} ({op} {threshold})",
            "if/3/in_errors.rate",
            12.5,
            "web01",
            ComparisonOp::GreaterThan,
            10.0,
            &l,
        );
        assert_eq!(
            rendered,
            "web01: if/3/in_errors.rate on eth0 is 12.50 (> 10)"
        );
    }

    /// **A typo is left visible.** `{lable.if_name}` in the alert text is how
    /// an operator finds it; an empty gap reads as a missing measurement.
    #[test]
    fn an_unknown_placeholder_survives_verbatim() {
        let rendered = render_summary(
            "{lable.if_name} and {label.missing} and {nonsense}",
            "m",
            1.0,
            "s",
            ComparisonOp::GreaterThan,
            0.0,
            &BTreeMap::new(),
        );
        assert_eq!(
            rendered,
            "{lable.if_name} and {label.missing} and {nonsense}"
        );
    }

    #[test]
    fn an_unclosed_brace_is_just_text() {
        assert_eq!(
            render_summary(
                "100% of {metric",
                "m",
                1.0,
                "s",
                ComparisonOp::GreaterThan,
                0.0,
                &BTreeMap::new()
            ),
            "100% of {metric"
        );
    }

    #[test]
    fn the_default_summary_names_the_metric_the_value_and_the_test() {
        let s = default_summary(&rule(), "cpu/usage_percent", 97.4, "web01");
        assert_eq!(s, "web01: cpu/usage_percent is 97.40 (> 90)");
    }

    /// Round-trips in both encodings — it is a `@desired` document and an
    /// `@rpc` reply, so both matter (#815).
    #[test]
    fn json_and_cbor_round_trip() {
        let mut cfg = ThresholdsConfig {
            default_for_secs: 60,
            default_recover_after_secs: 30,
            rules: vec![rule()],
        };
        cfg.rules[0].clear = Some(80.0);
        cfg.rules[0].labels = labels(&[("source", "web*")]);
        cfg.rules[0].summary = Some("{metric} at {value}".to_string());

        let json = serde_json::to_vec(&cfg).unwrap();
        assert_eq!(
            serde_json::from_slice::<ThresholdsConfig>(&json).unwrap(),
            cfg
        );

        let mut cbor = Vec::new();
        ciborium::into_writer(&cfg, &mut cbor).unwrap();
        let back: ThresholdsConfig = ciborium::from_reader(&cbor[..]).unwrap();
        assert_eq!(back, cfg);
    }

    /// An empty document is the default, and it is valid: a sensor with no
    /// rules asserts nothing, which is what every existing deployment gets.
    #[test]
    fn the_empty_document_is_valid_and_is_the_default() {
        let cfg = ThresholdsConfig::default();
        cfg.validate().unwrap();
        assert!(cfg.rules.is_empty());
        assert_eq!(cfg.default_for_secs, 0, "a hand-written rule fires at once");

        // …and it round-trips from `{}`, so an operator can start from nothing.
        let from_empty: ThresholdsConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(from_empty, cfg);
    }
}
