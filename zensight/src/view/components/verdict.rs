//! The three-state payload-verdict chip (#791).
//!
//! **`NotValidated` must not render as a passing check.** A green tick for
//! "I did not check" is the same lie the fleet view's `Silent` used to tell
//! (#746), in a different pane. So the chip resolves its colour through the
//! verdict's *pole*, never a boolean:
//!
//! | Verdict | Swatch | Reads as |
//! |---|---|---|
//! | `Valid` | `STATUS_ONLINE` | checked, conformant |
//! | `Invalid` | `STATUS_OFFLINE` | checked, non-conformant |
//! | `NotValidated(FeatureOff \| NoRegistry)` | `STATUS_UNKNOWN` | nobody looked — *chose not to* |
//! | `NotValidated(NoSchema \| KindUnsupported \| Undecodable \| BadSchema)` | `JUDGEMENT_UNOBSERVABLE` | asked, unanswerable — *could not* |
//!
//! Six reasons, two visual groups — "we could not" and "we chose not to"
//! are different facts (the issue's one hard rule) — with the full reason
//! always in the label via upstream's `Display`, because meaning is never
//! carried by colour alone (`kit::badge`). The wording tracks
//! zensight-conformance's `PayloadUndecodable`/`PayloadInvalid` findings:
//! the same verdict reaching a human instead of CI.

use iced::{Color, Element};

use zensight_common::schema::{NotValidated, Verdict};

use crate::message::Message;
use crate::view::components::kit::badge;
use crate::view::theme;

/// The chip's swatch for a verdict — the pole, never the label.
pub fn verdict_color(verdict: &Verdict) -> Color {
    match verdict {
        Verdict::Valid => theme::STATUS_ONLINE,
        Verdict::Invalid(_) => theme::STATUS_OFFLINE,
        Verdict::NotValidated(NotValidated::FeatureOff | NotValidated::NoRegistry) => {
            theme::STATUS_UNKNOWN
        }
        Verdict::NotValidated(
            NotValidated::NoSchema
            | NotValidated::KindUnsupported
            | NotValidated::Undecodable
            | NotValidated::BadSchema,
        ) => theme::JUDGEMENT_UNOBSERVABLE,
    }
}

/// The chip's label. The count rides the label for `Invalid` (the violations
/// themselves are the surrounding pane's to list); the reason rides it for
/// `NotValidated`.
pub fn verdict_label(verdict: &Verdict) -> String {
    match verdict {
        Verdict::Valid => "valid".to_string(),
        Verdict::Invalid(violations) => format!(
            "invalid · {} violation{}",
            violations.len(),
            if violations.len() == 1 { "" } else { "s" }
        ),
        Verdict::NotValidated(r @ (NotValidated::FeatureOff | NotValidated::NoRegistry)) => {
            format!("not checked — {r}")
        }
        Verdict::NotValidated(r) => format!("could not check — {r}"),
    }
}

/// The chip: a coloured dot plus the words (`kit::badge` — meaning never by
/// colour alone).
pub fn verdict_badge(verdict: &Verdict) -> Element<'static, Message> {
    badge(verdict_color(verdict), verdict_label(verdict))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_verdict() -> Vec<Verdict> {
        vec![
            Verdict::Valid,
            Verdict::Invalid(vec!["a sentence".into()]),
            Verdict::NotValidated(NotValidated::NoSchema),
            Verdict::NotValidated(NotValidated::NoRegistry),
            Verdict::NotValidated(NotValidated::FeatureOff),
            Verdict::NotValidated(NotValidated::KindUnsupported),
            Verdict::NotValidated(NotValidated::Undecodable),
            Verdict::NotValidated(NotValidated::BadSchema),
        ]
    }

    /// The issue's one hard rule as a property: no `NotValidated` reason may
    /// ever wear the passing swatch — "I did not check" must read as absent,
    /// not as green.
    #[test]
    fn not_validated_is_never_green() {
        for v in every_verdict() {
            if matches!(v, Verdict::NotValidated(_)) {
                assert_ne!(
                    verdict_color(&v),
                    theme::STATUS_ONLINE,
                    "{v:?} rendered as a pass"
                );
                let label = verdict_label(&v);
                assert!(
                    label.starts_with("not checked") || label.starts_with("could not check"),
                    "{v:?} label does not state its absence: {label:?}"
                );
            }
        }
    }

    /// "Could not" and "chose not to" are different facts and get different
    /// swatches — the same split the fleet view's four poles draw (#746).
    #[test]
    fn could_not_and_chose_not_to_differ() {
        let chose = verdict_color(&Verdict::NotValidated(NotValidated::FeatureOff));
        assert_eq!(
            chose,
            verdict_color(&Verdict::NotValidated(NotValidated::NoRegistry))
        );
        for r in [
            NotValidated::NoSchema,
            NotValidated::KindUnsupported,
            NotValidated::Undecodable,
            NotValidated::BadSchema,
        ] {
            assert_eq!(
                verdict_color(&Verdict::NotValidated(r)),
                theme::JUDGEMENT_UNOBSERVABLE
            );
            assert_ne!(verdict_color(&Verdict::NotValidated(r)), chose);
        }
    }

    /// The two real answers keep their own swatches and never collide with
    /// the non-answers.
    #[test]
    fn answers_are_distinct_from_non_answers() {
        let valid = verdict_color(&Verdict::Valid);
        let invalid = verdict_color(&Verdict::Invalid(vec![]));
        assert_ne!(valid, invalid);
        for v in every_verdict() {
            if matches!(v, Verdict::NotValidated(_)) {
                assert_ne!(verdict_color(&v), valid);
                assert_ne!(verdict_color(&v), invalid);
            }
        }
    }
}
