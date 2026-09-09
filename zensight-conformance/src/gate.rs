//! The gate: a [`DoctorReport`] in, an RFC 13 [`Judgement`] out.
//!
//! `run_doctor` reports; it does not decide whether a deployment passes. That
//! decision is policy, it belongs to whoever runs the harness, and it is the
//! whole of this module — kept separate from `main.rs` so it can be tested
//! without a bus.
//!
//! # The judged claim
//!
//! zenkey-fleet's judgement core (RFC 13 v1.24) states the convention: **the
//! judged claim is the finding**. So the claim here is *"this deployment has
//! conformance findings the gate cares about"*, and
//! [`zenkey_fleet::judgement_exit_code`] projects it:
//!
//! | judgement | exit | meaning |
//! |---|---|---|
//! | `NotEstablished` | 0 | the checks ran and found nothing gated — pass |
//! | `Established`    | 1 | gated findings — fail |
//! | `Unobservable`   | 2 | the checks ran and cannot carry the claim |
//! | `NotAsked`       | 2 | the checks never ran |
//!
//! The polarity looks inverted at a glance and is not: exit 0 is
//! "the finding was *not* established". `run_why` has the same shape for the
//! same reason.
//!
//! Note that [`DoctorReport`] carries **no** `judgement` field of its own, and
//! there is no `DoctorReport::to_judgement()` — unlike `ExpectVerdict`,
//! `CutoverVerdict` and `WhyVerdict`, which each own their mapping upstream.
//! A doctor report is a bag of findings at three severities; folding that into
//! one pole is exactly the policy call above, so the fold lives here.

use zenkey_fleet::Judgement;
// `CheckId`, `DoctorSeverity` and `DoctorFinding` are the *rendering*
// vocabulary, which upstream deliberately keeps behind `report::` rather than
// lifting to the crate root (see the "supported surface" block in
// zenkey-fleet's lib.rs: only the documents the verbs **return** are lifted).
// `zenkey_fleet::report::CheckId` is therefore the root-sanctioned spelling,
// not a reach into a private module path.
use zenkey_fleet::report::{CheckId, DoctorFinding, DoctorReport, DoctorSeverity};

/// Which severities the gate is willing to fail on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum FailOn {
    /// Only `error` findings fail the run.
    Error,
    /// `error` and `warning` findings fail the run.
    Warning,
}

impl FailOn {
    fn admits(self, severity: DoctorSeverity) -> bool {
        match self {
            FailOn::Error => severity == DoctorSeverity::Error,
            FailOn::Warning => {
                severity == DoctorSeverity::Error || severity == DoctorSeverity::Warning
            }
        }
    }
}

/// Checks the gate ignores **by default**, each with the reason it is here.
///
/// This list is a liability, not a convenience: every entry is a check whose
/// findings we have decided not to act on, and every entry needs a reason that
/// names when it goes away. Keep it short and keep it explained.
///
/// ## `field-new` — zenkey#384, the `oneOf` blind spot
///
/// `schema_drift`'s declared-field-path walker descends `properties` and does
/// **not** descend `oneOf`/`anyOf`. Every ZenSight telemetry payload carries a
/// `TelemetryValue`, an adjacently-tagged enum
/// (`#[serde(tag = "type", content = "value")]`), which schemars renders as a
/// `oneOf` whose branches each declare `type` and `value` as required. The
/// walker never reaches those branches, concludes the two paths were never
/// declared, and emits `field-new` at **warning** severity for
/// `<key> · value.type` and `<key> · value.value` on every telemetry key it
/// observes. A 4-producer deployment over a 15 s window produced 141 of them,
/// all false.
///
/// The served schema really does declare both — confirmed off the bus with
/// `zenctl interface show TelemetryPoint --schema --full`. So this is an
/// upstream walker bug, filed as **zenkey#384**, and not something ZenSight
/// can fix by re-shaping a payload.
///
/// **Empty since #845.** The one entry it ever held — `field-new`, excluded
/// while upstream's declared-path walker could not descend `oneOf`/`$ref`
/// (zenkey#384) — lifted when the fix shipped in the pinned zenkey-fleet
/// 0.11.1 (`judge/field.rs`, "Both additions fix the same defect (#384)").
/// The exclusion then sat here stale for a release: its lift-condition had
/// happened and nothing noticed, which is the exact failure mode this
/// comment's predecessor warned about. If an entry ever returns, name the
/// upstream issue AND add a re-check that can notice the lift.
///
/// A caller can override in both directions: `--allow <id>` adds an
/// exclusion for a run, `--deny <id>` puts a default exclusion back under
/// the gate.
pub const DEFAULT_EXCLUDED: &[CheckId] = &[];

/// How to turn a report into a verdict.
#[derive(Debug, Clone)]
pub struct Gate {
    /// The severity floor a finding must reach to count.
    pub fail_on: FailOn,
    /// Check ids that do not count however severe they are.
    pub excluded: Vec<CheckId>,
    /// Honour RFC 09 §5.1 O6 strictly: a listen window that dropped samples
    /// cannot carry a *clean* verdict, so promote it to `Unobservable`.
    ///
    /// Off by default, deliberately. O6 taints the **listen-phase** findings
    /// (`payload-*`, `qos-observed-mismatch`, `unregistered-traffic`,
    /// `rate-over-declared`, `field-*`), and a drop is a false-*negative*
    /// risk — it means the window may have missed a finding, not that it
    /// invented one. Turning a busy-runner drop into a red build would make
    /// the gate flap on load rather than on conformance. The drop counts are
    /// printed unconditionally either way, so nothing is hidden; a caller who
    /// wants completeness to be a hard claim asks for it.
    pub strict_window: bool,
}

impl Default for Gate {
    fn default() -> Self {
        Gate {
            fail_on: FailOn::Warning,
            excluded: DEFAULT_EXCLUDED.to_vec(),
            strict_window: false,
        }
    }
}

/// What the gate made of a report.
#[derive(Debug)]
pub struct Verdict {
    /// The RFC 13 pole; feed it to [`zenkey_fleet::judgement_exit_code`].
    pub judgement: Judgement,
    /// The findings that counted, in report order.
    pub gated: Vec<DoctorFinding>,
    /// Findings suppressed by [`Gate::excluded`], whatever their severity.
    pub excluded: Vec<DoctorFinding>,
    /// Findings below the severity floor (and not excluded) — informational.
    pub below_floor: Vec<DoctorFinding>,
}

/// Fold a doctor report into one judgement.
///
/// The `Unobservable` cases come first, because a run that could not observe
/// the fleet must never render as a clean one (RFC 09 §5.1 O6 / O4):
///
/// * an **empty roster** means nothing was judged. In CI that is the harness
///   failing to stand the deployment up, which is precisely the failure a
///   green "no findings" would hide.
/// * a **lossy listen window**, under [`Gate::strict_window`].
pub fn judge(report: &DoctorReport, gate: &Gate) -> Verdict {
    let mut gated = Vec::new();
    let mut excluded = Vec::new();
    let mut below_floor = Vec::new();
    for f in &report.findings {
        if gate.excluded.contains(&f.check) {
            excluded.push(f.clone());
        } else if gate.fail_on.admits(f.severity) {
            gated.push(f.clone());
        } else {
            below_floor.push(f.clone());
        }
    }

    let judgement = if report.live_producers == 0 {
        Judgement::Unobservable {
            reason: "the liveliness roster was empty — no producer was judged, so a \
                     clean report says nothing about the deployment (RFC 05 §3.1)"
                .into(),
        }
    } else if !gated.is_empty() {
        Judgement::Established
    } else if gate.strict_window && report.observation.is_none() {
        // A window that never happened is not a clean window (#1112). The
        // dropped count used to come from `map_or(0, …)`, so an ABSENT
        // observation section — which `--for 0` produces by design — read as
        // "zero samples dropped" and the run passed green. `--for 0
        // --strict-window` is a caller asking for completeness to be a hard
        // claim and being told it holds, having observed nothing at all.
        Judgement::Unobservable {
            reason: "--strict-window was asked for but no listen window ran (--for 0), \
                     so there is no window to call clean (RFC 09 §5.1 O6)"
                .into(),
        }
    } else if gate.strict_window && report.observation.as_ref().is_some_and(|o| o.dropped > 0) {
        let dropped = report.observation.as_ref().map_or(0, |o| o.dropped);
        Judgement::Unobservable {
            reason: format!(
                "the listen window dropped {dropped} sample(s); under --strict-window \
                 a clean verdict over a lossy window is not a clean fleet \
                 (RFC 09 §5.1 O6)"
            ),
        }
    } else {
        Judgement::NotEstablished {
            reason: format!(
                "{} producer(s) judged, {} finding(s) reported, none of them gated",
                report.live_producers,
                report.findings.len()
            ),
        }
    };

    Verdict {
        judgement,
        gated,
        excluded,
        below_floor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zenkey_fleet::judgement_exit_code;
    use zenkey_fleet::report::{Asked, ObservationSummary};

    fn finding(severity: DoctorSeverity, check: CheckId) -> DoctorFinding {
        DoctorFinding {
            severity,
            check,
            subject: "s".into(),
            evidence: "e".into(),
            citation: None,
        }
    }

    fn report(findings: Vec<DoctorFinding>) -> DoctorReport {
        DoctorReport {
            findings,
            synced: Asked::Asked(vec!["h-1/sysinfo (registry 1.0)".into()]),
            introspect_answered: 1,
            live_producers: 1,
            describe_served: 1,
            describe_missing: 0,
            routers: 0,
            router_version: None,
            deep: true,
            observation: None,
        }
    }

    /// The polarity, pinned. A clean report is `NotEstablished` and exits 0;
    /// getting this backwards would make the gate pass exactly when it should
    /// fail.
    #[test]
    fn clean_is_not_established_and_exits_zero() {
        let v = judge(&report(vec![]), &Gate::default());
        assert!(matches!(v.judgement, Judgement::NotEstablished { .. }));
        assert_eq!(judgement_exit_code(&v.judgement), 0);
    }

    #[test]
    fn a_gated_finding_is_established_and_exits_one() {
        let v = judge(
            &report(vec![finding(DoctorSeverity::Error, CheckId::SliceSync)]),
            &Gate::default(),
        );
        assert_eq!(v.gated.len(), 1);
        assert_eq!(judgement_exit_code(&v.judgement), 1);
    }

    /// The #743 baseline calibration: `admin-unreachable`, `storage-coverage`
    /// and `describe-missing` all fire at **info** in a dev deployment with no
    /// router, no storage and only some producers up. They are facts about the
    /// deployment, not defects in it, and a gate that reddens on them is a
    /// gate nobody will keep.
    #[test]
    fn info_findings_never_fail_the_gate() {
        let v = judge(
            &report(vec![
                finding(DoctorSeverity::Info, CheckId::AdminUnreachable),
                finding(DoctorSeverity::Info, CheckId::StorageCoverage),
                finding(DoctorSeverity::Info, CheckId::DescribeMissing),
            ]),
            &Gate::default(),
        );
        assert_eq!(judgement_exit_code(&v.judgement), 0);
        assert_eq!(v.below_floor.len(), 3);
        assert!(v.gated.is_empty());
    }

    /// zenkey#384 landed (fleet 0.11.1), so `field-new` is a real warning
    /// again (#845) — the flipped form of the test that used to pin the
    /// exclusion. If this fails after a fleet bump, upstream regressed the
    /// walker; exclude it again WITH a re-check this time.
    #[test]
    fn field_new_gates_now_that_zenkey_384_landed() {
        assert!(DEFAULT_EXCLUDED.is_empty());
        let v = judge(
            &report(vec![finding(DoctorSeverity::Warning, CheckId::FieldNew)]),
            &Gate::default(),
        );
        assert_eq!(judgement_exit_code(&v.judgement), 1);
        assert!(v.excluded.is_empty());
        // …and `--allow field-new` is still available as the operator's
        // per-run override.
        let lenient = Gate {
            excluded: vec![CheckId::FieldNew],
            ..Gate::default()
        };
        let v = judge(
            &report(vec![finding(DoctorSeverity::Warning, CheckId::FieldNew)]),
            &lenient,
        );
        assert_eq!(judgement_exit_code(&v.judgement), 0);
    }

    /// An exclusion suppresses the *check*, not the severity floor: an
    /// excluded `error` is still excluded.
    #[test]
    fn an_exclusion_outranks_the_severity_floor() {
        // The default list is empty (#845), so the exclusion under test is an
        // explicit --allow.
        let gate = Gate {
            excluded: vec![CheckId::FieldNew],
            ..Gate::default()
        };
        let v = judge(
            &report(vec![finding(DoctorSeverity::Error, CheckId::FieldNew)]),
            &gate,
        );
        assert_eq!(judgement_exit_code(&v.judgement), 0);
        assert_eq!(v.excluded.len(), 1);
    }

    #[test]
    fn fail_on_error_lets_warnings_through() {
        let warn = vec![finding(DoctorSeverity::Warning, CheckId::StaleState)];
        assert_eq!(
            judgement_exit_code(&judge(&report(warn.clone()), &Gate::default()).judgement),
            1,
            "the default floor is `warning`"
        );
        let lenient = Gate {
            fail_on: FailOn::Error,
            ..Gate::default()
        };
        assert_eq!(
            judgement_exit_code(&judge(&report(warn), &lenient).judgement),
            0
        );
    }

    /// The failure a green build must never hide: the harness stood nothing
    /// up, so there was nothing to judge.
    #[test]
    fn an_empty_roster_is_unobservable_not_clean() {
        let empty = DoctorReport {
            live_producers: 0,
            ..report(vec![])
        };
        let v = judge(&empty, &Gate::default());
        assert!(v.judgement.is_unobservable());
        assert_eq!(judgement_exit_code(&v.judgement), 2);
    }

    #[test]
    fn a_lossy_window_is_clean_by_default_and_unobservable_under_strict() {
        let lossy = DoctorReport {
            observation: Some(ObservationSummary {
                window_s: 10.0,
                scopes: vec!["v1/*/telemetry/**".into()],
                samples: 100,
                keys_seen: 7,
                dropped: 3,
                synthetic_marked: 0,
                field_paths_dropped: 0,
                facts_evicted: 0,
            }),
            ..report(vec![])
        };
        assert_eq!(
            judgement_exit_code(&judge(&lossy, &Gate::default()).judgement),
            0
        );
        let strict = Gate {
            strict_window: true,
            ..Gate::default()
        };
        assert_eq!(judgement_exit_code(&judge(&lossy, &strict).judgement), 2);
    }

    /// #1112: `--for 0 --strict-window` is a caller asking for completeness to
    /// be a hard claim, and being told it holds having observed nothing.
    ///
    /// `--for 0` produces no observation section by design (`docs/checks.md`),
    /// and `dropped` came from `observation.map_or(0, …)` — so an *absent*
    /// window read as "zero samples dropped", the strict arm never fired, and
    /// the run exited 0. A window that never happened is not a clean window.
    #[test]
    fn strict_window_without_a_window_is_unobservable() {
        let no_window = DoctorReport {
            observation: None,
            ..report(vec![])
        };
        // Without --strict-window, a run that did not listen is still a normal
        // clean run: the caller did not ask for the window.
        assert_eq!(
            judgement_exit_code(&judge(&no_window, &Gate::default()).judgement),
            0
        );
        let strict = Gate {
            strict_window: true,
            ..Gate::default()
        };
        let v = judge(&no_window, &strict);
        assert!(
            v.judgement.is_unobservable(),
            "a strict run that never listened must not read as clean: {:?}",
            v.judgement
        );
        assert_eq!(judgement_exit_code(&v.judgement), 2);
    }

    /// The contrast that keeps the arm honest: a window that *did* run and
    /// dropped nothing is clean under `--strict-window`, as it always was.
    #[test]
    fn strict_window_over_a_complete_window_is_clean() {
        let complete = DoctorReport {
            observation: Some(ObservationSummary {
                window_s: 10.0,
                scopes: vec!["v1/*/telemetry/**".into()],
                samples: 100,
                keys_seen: 7,
                dropped: 0,
                synthetic_marked: 0,
                field_paths_dropped: 0,
                facts_evicted: 0,
            }),
            ..report(vec![])
        };
        let strict = Gate {
            strict_window: true,
            ..Gate::default()
        };
        assert_eq!(judgement_exit_code(&judge(&complete, &strict).judgement), 0);
    }

    /// `field_paths_dropped` is the #223 per-path table hitting its own bound
    /// (the baseline saw 11128 refusals at four producers over 15 s). It taints
    /// the field-intelligence checks only — all three of which are either
    /// excluded or below the floor here — so it is reported, never gated, not
    /// even under `--strict-window`.
    #[test]
    fn a_full_field_path_table_alone_does_not_taint_the_verdict() {
        let bounded = DoctorReport {
            observation: Some(ObservationSummary {
                window_s: 15.0,
                scopes: vec!["v1/*/telemetry/**".into()],
                samples: 900,
                keys_seen: 141,
                dropped: 0,
                synthetic_marked: 0,
                field_paths_dropped: 11128,
                facts_evicted: 0,
            }),
            ..report(vec![])
        };
        let strict = Gate {
            strict_window: true,
            ..Gate::default()
        };
        assert_eq!(judgement_exit_code(&judge(&bounded, &strict).judgement), 0);
    }
}
