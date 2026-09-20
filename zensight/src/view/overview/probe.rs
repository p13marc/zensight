//! Probe fleet overview — failing targets and the certificates about to expire.
//!
//! #1126: the fleet-wide half of the probe view. A certificate expiring in
//! eleven days is not a fact about one vantage point; it is a fact about the
//! fleet, and the only place it can be seen at all is a list that spans every
//! probe host.
//!
//! **Expired is not "0 days".** `tls_days_to_expiry` goes negative once
//! `notAfter` has passed, deliberately — the registry says clamping "would make
//! 'expired an hour ago' and 'expires in a month' look equally survivable". So
//! negatives sort to the top and are labelled as expired, never floored.
//!
//! **A target checked from two vantages is two rows.** Deduplicating on the
//! target would hide the case the sensor exists for: a certificate that
//! validates from inside the network and not from outside it is a split-horizon
//! misconfiguration, and one row would report whichever vantage was folded
//! last.

use std::collections::HashMap;

use iced::widget::{Column, column, row, text};
use iced::{Alignment, Element, Theme};

use zensight_common::TelemetryValue;
use zensight_common::registry::probe::Subject;

use crate::message::{DeviceId, Message};
use crate::view::dashboard::DeviceState;
use crate::view::specialized::probe::EXPIRY_SOON_DAYS;
use crate::view::theme;
use crate::view::tokens::{font, space};

/// One certificate, as one vantage point sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct CertRow {
    pub target: String,
    pub vantage: String,
    pub days: f64,
    /// `None` when the sensor published no `tls_chain_valid` for this target.
    pub chain_valid: Option<bool>,
}

impl CertRow {
    #[must_use]
    pub fn label(&self) -> String {
        if self.days < 0.0 {
            format!("EXPIRED {:.0} days ago", -self.days)
        } else {
            format!("{:.0} days left", self.days)
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ProbeAgg {
    pub targets: usize,
    pub failing: usize,
    /// Timed out, counted separately from `failing` — a hang and a refusal are
    /// different diagnoses, and the sensor separates them on the wire.
    pub timed_out: usize,
    pub vantages: usize,
}

fn num(value: &TelemetryValue) -> Option<f64> {
    match value {
        TelemetryValue::Counter(c) => Some(*c as f64),
        TelemetryValue::Gauge(g) => Some(*g),
        TelemetryValue::Boolean(b) => Some(f64::from(u8::from(*b))),
        _ => None,
    }
}

/// Every certificate the fleet can see, soonest-to-expire first.
///
/// Filtered to [`EXPIRY_SOON_DAYS`], which is this list's window and not a
/// threshold any publisher declared — the real day count always rides the row.
#[must_use]
pub fn expiring_certificates(devices: &HashMap<&DeviceId, &DeviceState>) -> Vec<CertRow> {
    let mut rows: Vec<CertRow> = Vec::new();

    for (id, state) in devices {
        // `source` is the vantage point (`probe/src/poller.rs:528`).
        let vantage = id.source.clone();
        let mut valid: HashMap<String, bool> = HashMap::new();
        for (key, point) in &state.metrics {
            if let Some(Subject::TlsChainValid { target }) = Subject::parse_metric(key)
                && let Some(v) = num(&point.value)
            {
                valid.insert(target.to_string(), v != 0.0);
            }
        }
        for (key, point) in &state.metrics {
            if let Some(Subject::TlsDaysToExpiry { target }) = Subject::parse_metric(key)
                && let Some(days) = num(&point.value)
                && days < EXPIRY_SOON_DAYS
            {
                rows.push(CertRow {
                    target: target.to_string(),
                    vantage: vantage.clone(),
                    days,
                    chain_valid: valid.get(target.as_str()).copied(),
                });
            }
        }
    }

    // Soonest first — and because expired is negative rather than clamped,
    // "expired three days ago" sorts above "expires tomorrow" for free.
    rows.sort_by(|a, b| {
        a.days
            .total_cmp(&b.days)
            .then_with(|| a.target.cmp(&b.target))
            .then_with(|| a.vantage.cmp(&b.vantage))
    });
    rows
}

/// Fleet counts across every vantage.
#[must_use]
pub fn aggregate(devices: &HashMap<&DeviceId, &DeviceState>) -> ProbeAgg {
    let mut agg = ProbeAgg {
        vantages: devices.len(),
        ..ProbeAgg::default()
    };
    for state in devices.values() {
        let mut timed_out: HashMap<String, bool> = HashMap::new();
        for (key, point) in &state.metrics {
            if let Some(Subject::Timeout { target }) = Subject::parse_metric(key)
                && let Some(v) = num(&point.value)
            {
                timed_out.insert(target.to_string(), v != 0.0);
            }
        }
        for (key, point) in &state.metrics {
            if let Some(Subject::Up { target }) = Subject::parse_metric(key) {
                agg.targets += 1;
                if num(&point.value).is_some_and(|v| v == 0.0) {
                    if timed_out.get(target.as_str()).copied().unwrap_or(false) {
                        agg.timed_out += 1;
                    } else {
                        agg.failing += 1;
                    }
                }
            }
        }
    }
    agg
}

/// Render the probe overview.
pub fn probe_overview<'a>(devices: &HashMap<&DeviceId, &DeviceState>) -> Element<'a, Message> {
    if devices.is_empty() {
        return muted("No probe vantage points available");
    }

    let agg = aggregate(devices);
    let certs = expiring_certificates(devices);

    let mut col = Column::new().spacing(space::SM);
    col = col.push(
        row![
            stat("Vantages", agg.vantages.to_string()),
            stat("Targets", agg.targets.to_string()),
            stat("Failing", agg.failing.to_string()),
            // Its own stat, never added to "failing": eight days of an outage
            // went into learning that a hang is not a refusal.
            stat("Timed out", agg.timed_out.to_string()),
        ]
        .spacing(space::LG)
        .align_y(Alignment::Center),
    );

    col = col.push(text("Certificates expiring or expired").size(font::EMPHASIS));
    if certs.is_empty() {
        col = col.push(muted_owned(format!(
            "No certificate the fleet can see expires within {EXPIRY_SOON_DAYS:.0} days"
        )));
    } else {
        for c in certs.iter().take(25) {
            let expired = c.days < 0.0;
            let days = text(c.label())
                .size(font::DENSE)
                .style(move |t: &Theme| text::Style {
                    color: Some(if expired {
                        theme::colors(t).danger()
                    } else {
                        theme::colors(t).warning()
                    }),
                });
            let mut line = row![
                text(c.target.clone()).size(font::DENSE),
                days,
                // The vantage is on every row, not in a heading: the same
                // target from two hosts is two rows, and which host saw it is
                // half the finding.
                text(format!("from {}", c.vantage)).size(font::MICRO),
            ]
            .spacing(space::MD)
            .align_y(Alignment::Center);
            if c.chain_valid == Some(false) {
                line = line.push(text("chain INVALID").size(font::MICRO));
            }
            col = col.push(line);
        }
        if certs.len() > 25 {
            col = col.push(muted_owned(format!("… and {} more", certs.len() - 25)));
        }
    }

    col.into()
}

fn muted<'a>(s: &'a str) -> Element<'a, Message> {
    text(s)
        .size(font::CAPTION)
        .style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).text_muted()),
        })
        .into()
}

fn muted_owned<'a>(s: String) -> Element<'a, Message> {
    text(s)
        .size(font::CAPTION)
        .style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).text_muted()),
        })
        .into()
}

fn stat<'a>(label: &'a str, value: String) -> Element<'a, Message> {
    column![
        text(label)
            .size(font::MICRO)
            .style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).text_muted()),
            }),
        text(value).size(font::EMPHASIS)
    ]
    .spacing(space::XS)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zensight_common::TelemetryPoint;

    fn dev(vantage: &str, metrics: &[(&str, f64)]) -> (DeviceId, DeviceState) {
        let id = DeviceId::fixture("probe", vantage);
        let mut state = DeviceState::new(id.clone());
        for (metric, v) in metrics {
            state.metrics.insert(
                (*metric).to_string(),
                TelemetryPoint::new(vantage, (*metric).to_string(), TelemetryValue::Gauge(*v)),
            );
        }
        (id, state)
    }

    fn fleet(pairs: &[(DeviceId, DeviceState)]) -> HashMap<&DeviceId, &DeviceState> {
        pairs.iter().map(|(i, s)| (i, s)).collect()
    }

    /// A certificate that validates from inside the network and not from
    /// outside it is a split-horizon misconfiguration — the exact thing an
    /// outside-in check exists to catch. Deduplicating on the target would
    /// report whichever vantage happened to be folded last.
    #[test]
    fn one_target_seen_from_two_vantages_is_two_rows() {
        let pairs = [
            dev(
                "probe-inside",
                &[
                    ("www-example-com/tls_days_to_expiry", 11.0),
                    ("www-example-com/tls_chain_valid", 1.0),
                ],
            ),
            dev(
                "probe-outside",
                &[
                    ("www-example-com/tls_days_to_expiry", 11.0),
                    ("www-example-com/tls_chain_valid", 0.0),
                ],
            ),
        ];
        let rows = expiring_certificates(&fleet(&pairs));
        assert_eq!(rows.len(), 2);
        let outside = rows.iter().find(|r| r.vantage == "probe-outside").unwrap();
        assert_eq!(outside.chain_valid, Some(false));
        let inside = rows.iter().find(|r| r.vantage == "probe-inside").unwrap();
        assert_eq!(inside.chain_valid, Some(true));
    }

    /// Because expiry is negative rather than clamped, "expired three days
    /// ago" sorts above "expires tomorrow" with no special case.
    #[test]
    fn an_already_expired_certificate_sorts_above_one_expiring_tomorrow() {
        let pairs = [dev(
            "probe01",
            &[
                ("soon-example-com/tls_days_to_expiry", 1.0),
                ("gone-example-com/tls_days_to_expiry", -3.0),
            ],
        )];
        let rows = expiring_certificates(&fleet(&pairs));
        assert_eq!(rows[0].target, "gone-example-com");
        assert_eq!(rows[0].label(), "EXPIRED 3 days ago");
        assert_eq!(rows[1].target, "soon-example-com");
        assert_eq!(rows[1].label(), "1 days left");
    }

    /// Only certificates inside the window are listed; a year of validity is
    /// not a finding.
    #[test]
    fn a_healthy_certificate_is_not_in_the_expiry_list() {
        let pairs = [dev(
            "probe01",
            &[("ok-example-com/tls_days_to_expiry", 300.0)],
        )];
        assert!(expiring_certificates(&fleet(&pairs)).is_empty());
    }

    /// A hang and a refusal are different diagnoses, so they are different
    /// numbers. Folding both into "failing" is the reading that cost eight
    /// days.
    #[test]
    fn timed_out_targets_are_counted_apart_from_failing_ones() {
        let pairs = [dev(
            "probe01",
            &[
                ("hung/up", 0.0),
                ("hung/timeout", 1.0),
                ("refused/up", 0.0),
                ("refused/timeout", 0.0),
                ("fine/up", 1.0),
            ],
        )];
        let agg = aggregate(&fleet(&pairs));
        assert_eq!(agg.targets, 3);
        assert_eq!(agg.timed_out, 1);
        assert_eq!(agg.failing, 1, "the hung target is not counted here too");
    }
}
