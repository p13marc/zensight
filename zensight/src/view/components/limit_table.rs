//! Readings rendered **beside the limits their publisher declared** (#1127).
//!
//! A fan at 12 000 RPM is either fine or about to fail, and the number alone
//! does not say which — only the limit does. The bmc sensor publishes
//! `thermal/{s}/upper_warning_c` and `upper_critical_c` as sibling subjects of
//! the reading precisely so a consumer can show both, and nothing read them:
//! 12 000 against a 14 000 limit rendered exactly like 3 000 against the same
//! limit.
//!
//! Three honesty rules, all of them load-bearing and all of them inherited
//! from `specialized/sysinfo.rs`'s "Fans & power" panel, which is where this
//! vocabulary was written:
//!
//! - **absent is not zero.** A bay with no supply in it renders "absent", not
//!   `0 W`. A zero there reads as a supply drawing nothing, which is a
//!   different and wrong statement.
//! - **unmetered is not zero either.** A BMC that reports a supply's capacity
//!   and not its draw renders "not metered". So does a fan reported as a
//!   percentage of maximum rather than in RPM.
//! - **no limit, no verdict.** A reading with no declared threshold is shown
//!   plainly, in the ordinary text colour. Colouring it would mean this GUI
//!   had invented a threshold for somebody else's hardware, which is the one
//!   thing the bmc sensor spends its whole design refusing to do.

use iced::widget::{Column, row, text};
use iced::{Alignment, Element, Length, Theme};

use crate::message::Message;
use crate::view::components::kit::empty_state;
use crate::view::theme;
use crate::view::tokens::{font, space};

/// One reading and what its publisher said about it.
#[derive(Debug, Clone, PartialEq)]
pub struct LimitRow {
    /// What the row is about, already scoped — `chassis-1/psu/0`,
    /// `coretemp/Package id 0`.
    pub label: String,
    /// The measurement. `None` means **not metered**: the publisher said
    /// nothing, which is not zero.
    pub reading: Option<f64>,
    /// Suffix for the reading, e.g. `"°C"`, `" RPM"`, `" W"`. Written with its
    /// own leading space where one belongs, so the caller decides.
    pub unit: &'static str,
    /// The publisher's own warning threshold, if it declared one.
    pub warning: Option<f64>,
    /// The publisher's own critical threshold, if it declared one.
    pub critical: Option<f64>,
    /// Whether the thing exists at all. `false` renders "absent" and no
    /// reading — an empty bay is not a reading of zero.
    pub present: bool,
    /// Decimal places for the reading. Fans want 0, temperatures 1.
    pub precision: usize,
}

impl LimitRow {
    /// A present row with a reading and no declared limits.
    #[must_use]
    pub fn new(label: impl Into<String>, reading: Option<f64>, unit: &'static str) -> Self {
        Self {
            label: label.into(),
            reading,
            unit,
            warning: None,
            critical: None,
            present: true,
            precision: 0,
        }
    }

    #[must_use]
    pub fn with_limits(mut self, warning: Option<f64>, critical: Option<f64>) -> Self {
        self.warning = warning;
        self.critical = critical;
        self
    }

    #[must_use]
    pub fn with_present(mut self, present: bool) -> Self {
        self.present = present;
        self
    }

    #[must_use]
    pub fn with_precision(mut self, precision: usize) -> Self {
        self.precision = precision;
        self
    }

    /// Where the reading sits against the **publisher's** thresholds.
    ///
    /// `None` whenever a verdict would have to be invented: no reading, no
    /// threshold, or nothing present to measure.
    #[must_use]
    pub fn verdict(&self) -> Option<LimitVerdict> {
        if !self.present {
            return None;
        }
        let v = self.reading?;
        if let Some(c) = self.critical
            && v >= c
        {
            return Some(LimitVerdict::Critical);
        }
        if let Some(w) = self.warning
            && v >= w
        {
            return Some(LimitVerdict::Warning);
        }
        // Inside a declared limit is a verdict; with no limit declared there
        // is nothing to be inside of.
        (self.warning.is_some() || self.critical.is_some()).then_some(LimitVerdict::Ok)
    }

    /// The reading as it is shown: the number, "absent", or "not metered".
    #[must_use]
    pub fn reading_label(&self) -> String {
        if !self.present {
            return "absent".to_string();
        }
        match self.reading {
            Some(v) => format!("{v:.*}{}", self.precision, self.unit),
            None => "not metered".to_string(),
        }
    }

    /// The limits as they are shown, or `None` when the publisher declared
    /// none — in which case nothing is rendered rather than a placeholder,
    /// because "limit: —" reads as a limit of nothing.
    #[must_use]
    pub fn limit_label(&self) -> Option<String> {
        match (self.warning, self.critical) {
            (Some(w), Some(c)) => Some(format!(
                "warn {w:.*}{} · crit {c:.*}{}",
                self.precision, self.unit, self.precision, self.unit
            )),
            (Some(w), None) => Some(format!("warn {w:.*}{}", self.precision, self.unit)),
            (None, Some(c)) => Some(format!("crit {c:.*}{}", self.precision, self.unit)),
            (None, None) => None,
        }
    }
}

/// Where a reading sits against its publisher's own thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitVerdict {
    Ok,
    Warning,
    Critical,
}

impl LimitVerdict {
    fn color(self, t: &Theme) -> iced::Color {
        let c = theme::colors(t);
        match self {
            LimitVerdict::Ok => c.success(),
            LimitVerdict::Warning => c.warning(),
            LimitVerdict::Critical => c.danger(),
        }
    }
}

/// Render `rows` as a labelled block, or an honest empty state.
///
/// `empty` is what to say when there is nothing — and it should say *why*
/// there is nothing, not merely that there is. "No fans reported by hwmon" is
/// a fact about this host; "No fans" reads as a fault.
pub fn limit_table<'a>(rows: &[LimitRow], empty: &'a str) -> Element<'a, Message> {
    if rows.is_empty() {
        return empty_state(empty, None);
    }
    let mut col = Column::new().spacing(space::XS);
    for r in rows {
        let verdict = r.verdict();
        let reading = text(r.reading_label())
            .size(font::CAPTION)
            .style(move |t: &Theme| iced::widget::text::Style {
                // No verdict, no colour: this GUI does not invent a threshold
                // for somebody else's hardware.
                color: Some(match verdict {
                    Some(v) => v.color(t),
                    None => theme::colors(t).text(),
                }),
            });
        let mut line = row![
            text(r.label.clone())
                .size(font::CAPTION)
                .width(Length::Fixed(220.0))
                .style(|t: &Theme| iced::widget::text::Style {
                    color: Some(theme::colors(t).text_muted()),
                }),
            reading,
        ]
        .spacing(space::SM)
        .align_y(Alignment::Center);
        if let Some(limits) = r.limit_label() {
            line = line.push(text(limits).size(font::DENSE).style(|t: &Theme| {
                iced::widget::text::Style {
                    color: Some(theme::colors(t).text_dimmed()),
                }
            }));
        }
        col = col.push(line);
    }
    col.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The bug this exists for** (#1127): a reading with a limit beside it is
    /// not the same as a reading alone, and 12 000 RPM against a 14 000 limit
    /// used to render exactly like 3 000 against the same limit.
    #[test]
    fn a_reading_is_graded_against_its_publishers_limits() {
        let fan = |rpm: f64| {
            LimitRow::new("chassis-1/fan/0", Some(rpm), " RPM")
                .with_limits(Some(13000.0), Some(14000.0))
        };
        assert_eq!(fan(3_000.0).verdict(), Some(LimitVerdict::Ok));
        assert_eq!(fan(13_000.0).verdict(), Some(LimitVerdict::Warning));
        assert_eq!(fan(14_000.0).verdict(), Some(LimitVerdict::Critical));
        assert_eq!(fan(20_000.0).verdict(), Some(LimitVerdict::Critical));
        // At the threshold is over it: a vendor's "upper critical" is the
        // value at which the hardware says it is in trouble, not the value
        // above which it is.
        assert_eq!(
            fan(13_000.0).limit_label().as_deref(),
            Some("warn 13000 RPM · crit 14000 RPM")
        );
    }

    /// **No limit, no verdict.** Colouring an ungraded reading would mean this
    /// GUI had invented a threshold for somebody else's hardware — the one
    /// thing the bmc sensor spends its whole design refusing to do.
    #[test]
    fn a_reading_with_no_declared_limit_gets_no_verdict() {
        let row = LimitRow::new("coretemp/Package id 0", Some(99.0), "°C");
        assert_eq!(
            row.verdict(),
            None,
            "99 °C is alarming and is not a verdict"
        );
        assert_eq!(row.limit_label(), None, "and nothing is rendered for it");

        // One threshold is enough to grade against.
        let warned = row.clone().with_limits(Some(90.0), None);
        assert_eq!(warned.verdict(), Some(LimitVerdict::Warning));
        assert_eq!(warned.limit_label().as_deref(), Some("warn 90°C"));
    }

    /// **Absent is not zero, and unmetered is not zero either.**
    ///
    /// The two failures the sysinfo panel's vocabulary was written to avoid,
    /// and the two a BMC produces constantly: an empty supply bay, and a
    /// supply whose capacity is reported without its draw.
    #[test]
    fn absent_and_unmetered_are_not_readings_of_zero() {
        let absent = LimitRow::new("chassis-1/psu/2", None, " W").with_present(false);
        assert_eq!(absent.reading_label(), "absent");
        assert_eq!(absent.verdict(), None);

        let unmetered = LimitRow::new("chassis-1/psu/1", None, " W");
        assert_eq!(unmetered.reading_label(), "not metered");
        assert_eq!(unmetered.verdict(), None);

        // And a genuine zero still reads as zero — a stopped fan is a
        // measurement, not a missing one.
        let stopped = LimitRow::new("chassis-1/fan/3", Some(0.0), " RPM");
        assert_eq!(stopped.reading_label(), "0 RPM");
    }

    /// An absent row carries no verdict **even with limits declared** — there
    /// is nothing there to be over a threshold.
    #[test]
    fn an_absent_row_is_never_graded() {
        let row = LimitRow::new("chassis-1/psu/2", Some(0.0), " W")
            .with_limits(Some(500.0), Some(750.0))
            .with_present(false);
        assert_eq!(row.verdict(), None);
        assert_eq!(row.reading_label(), "absent");
    }
}
