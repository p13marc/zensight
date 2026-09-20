//! Outside-in synthetic checks, from one vantage point (#1126).
//!
//! `probe.toml`'s own header is an eight-day outage post-mortem whose thesis is
//! that **"timeout, 20 s" said once would have ended it**. Since #820 the
//! sensor has published `{target}/up`, `duration_ms`, `timeout`, `http_status`,
//! `tls_days_to_expiry`, `tls_chain_valid`, the burst series and the NTP set —
//! and `specialized/mod.rs` answered `Protocol::Probe => None`. The sentence
//! that would have ended the outage was on the bus and rendered nowhere.
//!
//! A probe **device is a vantage point**: the sensor puts the reporting host in
//! the payload's `source` (`poller.rs:528`, `source: r.vantage.clone()`), so
//! one device card is one host's view of its targets. The same target checked
//! from two hosts is two rows in two tables, which is the entire point — "is it
//! down, or is it down *from here*" is the question a synthetic check answers.
//!
//! Four renderings here are the sensor's own doctrine, and each is wrong in a
//! specific way if taken naively. They are listed against their registry
//! descriptions in [`outcome_label`], [`latency_label`] and
//! [`expiry_label`].

use std::collections::BTreeMap;

use iced::widget::{Column, column, row, scrollable, text};
use iced::{Alignment, Element, Length, Theme};

use zensight_common::TelemetryValue;
use zensight_common::registry::probe::Subject;

use crate::message::Message;
use crate::view::components::{card, section_header};
use crate::view::device::DeviceDetailState;
use crate::view::theme;
use crate::view::tokens::{font, space};

/// How one target's last check came out.
///
/// `Timeout` is a **state of its own**, not a flavour of failure. The registry
/// says so in as many words — *"a connection that hangs is a different
/// diagnosis from one that is refused"* — and the outage that motivated the
/// sensor was eight days of reading one as the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Outcome {
    /// `timeout = 1`. Worst, because it is the most often misread.
    Timeout,
    /// `up = 0` without a timeout — refused, or answered wrongly.
    Failed,
    Up,
    /// No `{target}/up` sample yet.
    Unknown,
}

/// One row of the per-vantage target table.
#[derive(Debug, Clone)]
pub struct TargetRow {
    pub target: String,
    pub outcome: Outcome,
    pub duration_ms: Option<f64>,
    pub http_status: Option<f64>,
    /// 95th-percentile RTT. **Absent at 100 % loss** rather than zero.
    pub rtt_p95_ms: Option<f64>,
    pub loss_pct: Option<f64>,
    pub jitter_ms: Option<f64>,
    /// Days until `notAfter`. **Negative once expired.**
    pub tls_days_to_expiry: Option<f64>,
    pub tls_chain_valid: Option<bool>,
    pub ntp_offset_ms: Option<f64>,
    pub ntp_stratum: Option<f64>,
}

impl TargetRow {
    /// The check's outcome in words.
    ///
    /// **A timeout is named as one, with its duration.** `up = 0` alone reads
    /// "down"; `timeout = 1` with `duration_ms = 20000` reads "timed out after
    /// 20.0 s", which is the sentence the sensor exists to print. The registry
    /// is explicit that `duration_ms` is published on a timeout *because* "the
    /// duration IS the diagnosis".
    #[must_use]
    pub fn outcome_label(&self) -> String {
        match self.outcome {
            Outcome::Timeout => match self.duration_ms {
                Some(ms) => format!("timed out after {:.1}s", ms / 1000.0),
                None => "timed out".to_string(),
            },
            Outcome::Failed => match self.http_status {
                Some(s) => format!("failed (HTTP {s:.0})"),
                None => "failed".to_string(),
            },
            Outcome::Up => match self.duration_ms {
                Some(ms) => format!("up · {ms:.0} ms"),
                None => "up".to_string(),
            },
            Outcome::Unknown => "no result yet".to_string(),
        }
    }

    /// Burst latency.
    ///
    /// **100 % loss is not 0 ms.** The registry: `loss_pct` is "published even
    /// at 100 %, where the RTT series are absent rather than zero". A p95 of
    /// `—` beside `100 % loss` is the truth; a p95 of `0 ms` reads as the
    /// fastest target in the fleet.
    #[must_use]
    pub fn latency_label(&self) -> Option<String> {
        let loss = self.loss_pct?;
        Some(match self.rtt_p95_ms {
            Some(p95) => format!("p95 {p95:.1} ms · {loss:.0}% loss"),
            // Not "0 ms": nothing came back to measure.
            None if loss >= 100.0 => format!("no probe answered · {loss:.0}% loss"),
            None => format!("{loss:.0}% loss"),
        })
    }

    /// Certificate expiry.
    ///
    /// **Negative days mean already expired**, and are rendered as such rather
    /// than clamped. The registry says why: clamping "would make 'expired an
    /// hour ago' and 'expires in a month' look equally survivable".
    #[must_use]
    pub fn expiry_label(&self) -> Option<String> {
        let days = self.tls_days_to_expiry?;
        Some(if days < 0.0 {
            format!("EXPIRED {:.0} days ago", -days)
        } else {
            format!("{days:.0} days left")
        })
    }

    /// Whether this row should draw the eye.
    #[must_use]
    pub fn is_finding(&self) -> bool {
        matches!(self.outcome, Outcome::Timeout | Outcome::Failed)
            || self.tls_chain_valid == Some(false)
            || self.tls_days_to_expiry.is_some_and(|d| d < EXPIRY_SOON_DAYS)
            || self.loss_pct.is_some_and(|l| l > 0.0)
            // Stratum 0 is a kiss-o'-death refusal, never a valid time source
            // (the registry's words). A server answering with stratum 0 has
            // told us not to use it, which is not the same as being down.
            || self.ntp_stratum == Some(0.0)
    }
}

/// The window the certificate list uses.
///
/// This is **the GUI's list threshold, not a publisher's limit** — no probe
/// declares one, and none is invented as a verdict. Rows are filtered by it and
/// shown with their real day counts; nothing is coloured as though the sensor
/// had graded it.
pub const EXPIRY_SOON_DAYS: f64 = 30.0;

fn value(state: &DeviceDetailState, metric: &str) -> Option<f64> {
    state.metrics.get(metric).and_then(|p| match &p.value {
        TelemetryValue::Counter(v) => Some(*v as f64),
        TelemetryValue::Gauge(v) => Some(*v),
        TelemetryValue::Boolean(b) => Some(f64::from(u8::from(*b))),
        _ => None,
    })
}

/// Whether this device has anything a probe table would show.
#[must_use]
pub fn has_probe_results(state: &DeviceDetailState) -> bool {
    state
        .metrics
        .keys()
        .any(|k| Subject::parse_metric(k).is_some())
}

#[derive(Default)]
struct Raw {
    up: Option<bool>,
    timeout: Option<bool>,
    duration_ms: Option<f64>,
    http_status: Option<f64>,
    rtt_p95_ms: Option<f64>,
    loss_pct: Option<f64>,
    jitter_ms: Option<f64>,
    tls_days_to_expiry: Option<f64>,
    tls_chain_valid: Option<bool>,
    ntp_offset_ms: Option<f64>,
    ntp_stratum: Option<f64>,
}

/// One row per target, findings first.
#[must_use]
pub fn target_rows(state: &DeviceDetailState) -> Vec<TargetRow> {
    let mut raw: BTreeMap<String, Raw> = BTreeMap::new();

    for key in state.metrics.keys() {
        let Some(subject) = Subject::parse_metric(key) else {
            continue;
        };
        let v = value(state, key);
        macro_rules! at {
            ($t:expr) => {
                raw.entry($t.to_string()).or_default()
            };
        }
        match subject {
            Subject::Up { target } => at!(target).up = v.map(|n| n != 0.0),
            Subject::Timeout { target } => at!(target).timeout = v.map(|n| n != 0.0),
            Subject::DurationMs { target } => at!(target).duration_ms = v,
            Subject::HttpStatus { target } => at!(target).http_status = v,
            Subject::RttP95Ms { target } => at!(target).rtt_p95_ms = v,
            Subject::LossPct { target } => at!(target).loss_pct = v,
            Subject::JitterMs { target } => at!(target).jitter_ms = v,
            Subject::TlsDaysToExpiry { target } => at!(target).tls_days_to_expiry = v,
            Subject::TlsChainValid { target } => {
                at!(target).tls_chain_valid = v.map(|n| n != 0.0);
            }
            Subject::NtpOffsetMs { target } => at!(target).ntp_offset_ms = v,
            Subject::NtpStratum { target } => at!(target).ntp_stratum = v,
            _ => {}
        }
    }

    let mut rows: Vec<TargetRow> = raw
        .into_iter()
        .map(|(target, r)| TargetRow {
            target,
            // Timeout is read FIRST. A timed-out check also publishes `up = 0`,
            // so testing `up` first would collapse the two states the sensor
            // went to the trouble of separating.
            outcome: match (r.timeout, r.up) {
                (Some(true), _) => Outcome::Timeout,
                (_, Some(false)) => Outcome::Failed,
                (_, Some(true)) => Outcome::Up,
                (_, None) => Outcome::Unknown,
            },
            duration_ms: r.duration_ms,
            http_status: r.http_status,
            rtt_p95_ms: r.rtt_p95_ms,
            loss_pct: r.loss_pct,
            jitter_ms: r.jitter_ms,
            tls_days_to_expiry: r.tls_days_to_expiry,
            tls_chain_valid: r.tls_chain_valid,
            ntp_offset_ms: r.ntp_offset_ms,
            ntp_stratum: r.ntp_stratum,
        })
        .collect();

    rows.sort_by(|a, b| {
        a.outcome
            .cmp(&b.outcome)
            .then_with(|| {
                // Then by nearest expiry, expired first (negatives sort low).
                a.tls_days_to_expiry
                    .unwrap_or(f64::MAX)
                    .total_cmp(&b.tls_days_to_expiry.unwrap_or(f64::MAX))
            })
            .then_with(|| a.target.cmp(&b.target))
    });
    rows
}

/// The probe device view — one vantage point's targets.
pub fn probe_vantage_view(state: &DeviceDetailState) -> Element<'_, Message> {
    let rows = target_rows(state);
    let vantage = state.device_id.source.clone();

    let mut body = Column::new().spacing(space::SM);
    body = body.push(section_header("Synthetic checks", None));
    // Named, always. The same target checked from two hosts is two answers,
    // and a result without its vantage cannot be acted on.
    body = body.push(muted_owned(format!("Vantage point: {vantage}")));

    if rows.is_empty() {
        body = body.push(muted("No check results yet from this vantage"));
        let body: Element<'_, Message> = body.into();
        return card(body);
    }

    for r in &rows {
        let finding = r.is_finding();
        let outcome = text(r.outcome_label())
            .size(font::DENSE)
            .style(move |t: &Theme| text::Style {
                color: Some(if finding {
                    theme::colors(t).danger()
                } else {
                    theme::colors(t).text_muted()
                }),
            });

        let mut line = row![text(r.target.clone()).size(font::DENSE), outcome,]
            .spacing(space::MD)
            .align_y(Alignment::Center);

        if let Some(l) = r.latency_label() {
            line = line.push(text(l).size(font::MICRO));
            // The burst kinds' shape over time: a target at a steady 2 % loss
            // and one that lost 40 % once read identically as a single
            // number, and they are not the same fault.
            line = line.push(crate::view::specialized::metric_sparkline(
                state,
                &format!("{}/loss_pct", r.target),
            ));
        }
        if let Some(j) = r.jitter_ms {
            line = line.push(text(format!("jitter {j:.1} ms")).size(font::MICRO));
            line = line.push(crate::view::specialized::metric_sparkline(
                state,
                &format!("{}/jitter_ms", r.target),
            ));
        }
        if let Some(e) = r.expiry_label() {
            line = line.push(text(e).size(font::MICRO));
        }
        if r.tls_chain_valid == Some(false) {
            line = line.push(text("chain INVALID").size(font::MICRO));
        }
        if let Some(off) = r.ntp_offset_ms {
            line = line.push(text(format!("offset {off:+.0} ms")).size(font::MICRO));
        }
        if r.ntp_stratum == Some(0.0) {
            // Not "stratum 0" — the number reads like a good one.
            line = line.push(
                text("kiss-o'-death: this server refuses to be a time source").size(font::MICRO),
            );
        }

        body = body.push(line);
    }

    let body: Element<'_, Message> = body.into();
    scrollable(column![card(body)]).height(Length::Fill).into()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::DeviceId;
    use iced_test::simulator;
    use zensight_common::{Protocol, TelemetryPoint};

    fn vantage(host: &str, metrics: &[(&str, f64)]) -> DeviceDetailState {
        let mut state = DeviceDetailState::new(DeviceId::fixture(Protocol::Probe, host));
        for (metric, v) in metrics {
            state.metrics.insert(
                (*metric).to_string(),
                TelemetryPoint::new(host, (*metric).to_string(), TelemetryValue::Gauge(*v)),
            );
        }
        state
    }

    /// The sentence the sensor exists to print, and the one the eight-day
    /// outage needed.
    ///
    /// A timed-out check publishes **both** `up = 0` and `timeout = 1`, so a
    /// reading that tests `up` first collapses the two states the sensor went
    /// to the trouble of separating, and prints "down" where "timed out after
    /// 20.0 s" was available.
    #[test]
    fn a_timeout_is_its_own_state_and_says_how_long_it_hung() {
        let state = vantage(
            "probe01",
            &[
                ("api-example-com/up", 0.0),
                ("api-example-com/timeout", 1.0),
                ("api-example-com/duration_ms", 20_000.0),
            ],
        );
        let rows = target_rows(&state);
        assert_eq!(rows[0].outcome, Outcome::Timeout);
        assert_ne!(rows[0].outcome, Outcome::Failed);
        assert_eq!(rows[0].outcome_label(), "timed out after 20.0s");

        let mut ui = simulator(probe_vantage_view(&state));
        assert!(ui.find("timed out after 20.0s").is_ok());
    }

    /// A refusal is not a hang. Same `up = 0`, different diagnosis, and the
    /// HTTP status is what distinguishes them.
    #[test]
    fn a_refusal_reads_differently_from_a_hang() {
        let state = vantage(
            "probe01",
            &[
                ("api-example-com/up", 0.0),
                ("api-example-com/timeout", 0.0),
                ("api-example-com/http_status", 503.0),
            ],
        );
        let rows = target_rows(&state);
        assert_eq!(rows[0].outcome, Outcome::Failed);
        assert_eq!(rows[0].outcome_label(), "failed (HTTP 503)");
    }

    /// The registry: `loss_pct` is published at 100 %, "where the RTT series
    /// are absent rather than zero". A p95 of 0 ms would make the deadest
    /// target in the fleet read as the fastest.
    #[test]
    fn total_loss_is_not_a_p95_of_zero_milliseconds() {
        let state = vantage("probe01", &[("gw-lan/up", 0.0), ("gw-lan/loss_pct", 100.0)]);
        let rows = target_rows(&state);
        assert_eq!(rows[0].rtt_p95_ms, None);
        assert_eq!(
            rows[0].latency_label().as_deref(),
            Some("no probe answered · 100% loss")
        );

        let mut ui = simulator(probe_vantage_view(&state));
        assert!(ui.find("no probe answered · 100% loss").is_ok());
        assert!(
            ui.find("p95 0.0 ms").is_err(),
            "nothing answered, so there is no p95 to show"
        );
    }

    /// The registry: days go negative on purpose, because clamping "would make
    /// 'expired an hour ago' and 'expires in a month' look equally
    /// survivable".
    #[test]
    fn an_expired_certificate_is_not_zero_days_left() {
        let state = vantage("probe01", &[("old-example-com/tls_days_to_expiry", -3.0)]);
        let rows = target_rows(&state);
        assert_eq!(
            rows[0].expiry_label().as_deref(),
            Some("EXPIRED 3 days ago")
        );
        assert!(rows[0].is_finding());

        let mut ui = simulator(probe_vantage_view(&state));
        assert!(ui.find("EXPIRED 3 days ago").is_ok());
        assert!(ui.find("0 days left").is_err());
    }

    /// Stratum 0 is a kiss-o'-death refusal, never a valid time source — and
    /// "stratum 0" reads like the best possible number to anyone who does not
    /// know that.
    #[test]
    fn stratum_zero_is_named_as_a_refusal_not_shown_as_a_number() {
        let state = vantage(
            "probe01",
            &[
                ("ntp-example-com/up", 1.0),
                ("ntp-example-com/ntp_stratum", 0.0),
            ],
        );
        let rows = target_rows(&state);
        assert!(rows[0].is_finding(), "a refusing time server is a finding");

        let mut ui = simulator(probe_vantage_view(&state));
        assert!(
            ui.find("kiss-o'-death: this server refuses to be a time source")
                .is_ok()
        );
    }

    /// Findings first: a timeout outranks a plain failure, which outranks an
    /// up target.
    #[test]
    fn the_worst_outcome_leads_the_table() {
        let state = vantage(
            "probe01",
            &[
                ("a-ok/up", 1.0),
                ("b-failed/up", 0.0),
                ("c-hung/up", 0.0),
                ("c-hung/timeout", 1.0),
            ],
        );
        let rows = target_rows(&state);
        assert_eq!(rows[0].target, "c-hung");
        assert_eq!(rows[1].target, "b-failed");
        assert_eq!(rows[2].target, "a-ok");
    }

    /// A vantage that has published nothing gets no tab.
    #[test]
    fn the_tab_needs_at_least_one_probe_subject() {
        assert!(!has_probe_results(&vantage("probe01", &[])));
        assert!(has_probe_results(&vantage("probe01", &[("a/up", 1.0)])));
    }
}
