//! Honest data-freshness indicators (Plan v3-04 §B, #23).
//!
//! Research's #1 real-time-dashboard UX rule: prefer an honest "data as of
//! 10:42" over a fake "live" that silently goes stale. This module provides:
//!
//! - A global [`Freshness`] verdict — **Live / Stale / Paused** — derived from
//!   the age of the most recently received telemetry and the connection state.
//! - A top-bar [`freshness_indicator`] widget (colored dot + label + "as of …").
//! - A per-panel [`age_label`] ("5s ago" / "2m ago"), fading to muted once a
//!   panel's data passes its stale threshold.
//!
//! All verdict logic is pure (`now`/`last_update` passed in) so it unit-tests
//! without a clock.

use iced::widget::{row, text};
use iced::{Alignment, Element, Theme};

use crate::message::Message;
use crate::view::theme;
use crate::view::tokens::{font, space};

/// How long after the last telemetry point we still consider the feed "Live".
/// Past this the global indicator flips to "Stale" (data is aging).
pub const LIVE_WINDOW_MS: i64 = 10_000;

/// The global data-freshness verdict shown in the top bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Connected and telemetry arrived within [`LIVE_WINDOW_MS`].
    Live,
    /// Connected but no telemetry within the live window — data is aging.
    Stale,
    /// Not connected / not subscribed — the feed is paused, not just slow.
    Paused,
}

impl Freshness {
    /// Compute the verdict from connection state + the age of the newest point.
    ///
    /// - Not connected ⇒ `Paused` (honest: we are not receiving anything).
    /// - Connected, never received anything ⇒ `Stale` (nothing to be live about).
    /// - Connected, last point within the live window ⇒ `Live`, else `Stale`.
    pub fn compute(connected: bool, last_update_ms: Option<i64>, now_ms: i64) -> Self {
        if !connected {
            return Freshness::Paused;
        }
        match last_update_ms {
            Some(ts) if now_ms.saturating_sub(ts) <= LIVE_WINDOW_MS => Freshness::Live,
            _ => Freshness::Stale,
        }
    }

    /// Short label for the indicator.
    pub fn label(self) -> &'static str {
        match self {
            Freshness::Live => "Live",
            Freshness::Stale => "Stale",
            Freshness::Paused => "Paused",
        }
    }

    /// Theme color for the indicator dot/label.
    pub fn color(self, theme: &Theme) -> iced::Color {
        let c = theme::colors(theme);
        match self {
            Freshness::Live => c.status_connected(),
            Freshness::Stale => c.warning(),
            Freshness::Paused => c.text_dimmed(),
        }
    }
}

/// Format a clock time "as of HH:MM:SS" (local-ish, derived from epoch ms) for
/// the newest data point. Returns `None` when there is no data yet.
///
/// Pure modular arithmetic on the epoch — no timezone library; this is a
/// wall-clock-ish UTC stamp, which is what an operator wants for "as of".
pub fn as_of_clock(last_update_ms: Option<i64>) -> Option<String> {
    let ts = last_update_ms?;
    if ts <= 0 {
        return None;
    }
    let secs = ts / 1000;
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    Some(format!("{h:02}:{m:02}:{s:02}"))
}

/// Format a data age (now - ts) as a compact "Ns ago" / "Nm ago" / "Nh ago"
/// string. Negative/zero ages read "just now". Pure (no clock read).
pub fn age_string(age_ms: i64) -> String {
    if age_ms < 1000 {
        "just now".to_string()
    } else if age_ms < 60_000 {
        format!("{}s ago", age_ms / 1000)
    } else if age_ms < 3_600_000 {
        format!("{}m ago", age_ms / 60_000)
    } else if age_ms < 86_400_000 {
        format!("{}h ago", age_ms / 3_600_000)
    } else {
        format!("{}d ago", age_ms / 86_400_000)
    }
}

/// The global freshness indicator for the top bar: a colored dot, the verdict
/// label, and (when there is data) an "as of HH:MM:SS" stamp.
pub fn freshness_indicator<'a>(
    connected: bool,
    last_update_ms: Option<i64>,
    now_ms: i64,
) -> Element<'a, Message> {
    let verdict = Freshness::compute(connected, last_update_ms, now_ms);
    let dot = text("\u{25CF}") // ● filled circle — redundant with the label, never color-alone.
        .size(font::CAPTION)
        .style(move |theme: &Theme| text::Style {
            color: Some(verdict.color(theme)),
        });
    let label = text(verdict.label())
        .size(font::CAPTION)
        .style(move |theme: &Theme| text::Style {
            color: Some(verdict.color(theme)),
        });

    let mut content = row![dot, label]
        .spacing(space::XS)
        .align_y(Alignment::Center);

    if let Some(clock) = as_of_clock(last_update_ms) {
        content = content.push(text(format!("as of {clock}")).size(font::CAPTION).style(
            |theme: &Theme| text::Style {
                color: Some(theme::colors(theme).text_dimmed()),
            },
        ));
    }

    content.into()
}

/// A per-panel age label: "5s ago" muted once past `stale_after_ms`. Returns a
/// styled text element. `now_ms`/`last_update_ms` are passed in (pure).
pub fn age_label<'a>(
    last_update_ms: i64,
    now_ms: i64,
    stale_after_ms: i64,
) -> Element<'a, Message> {
    let age = now_ms.saturating_sub(last_update_ms);
    let is_stale = age >= stale_after_ms;
    text(age_string(age))
        .size(font::CAPTION)
        .style(move |theme: &Theme| {
            let c = theme::colors(theme);
            text::Style {
                color: Some(if is_stale {
                    c.text_dimmed()
                } else {
                    c.text_muted()
                }),
            }
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paused_when_disconnected() {
        assert_eq!(
            Freshness::compute(false, Some(1_000), 1_000),
            Freshness::Paused
        );
        // Disconnected dominates even with very fresh data.
        assert_eq!(
            Freshness::compute(false, Some(1_000), 1_500),
            Freshness::Paused
        );
    }

    #[test]
    fn live_within_window() {
        let now = 100_000;
        assert_eq!(
            Freshness::compute(true, Some(now - 5_000), now),
            Freshness::Live
        );
        // Exactly at the window edge is still Live.
        assert_eq!(
            Freshness::compute(true, Some(now - LIVE_WINDOW_MS), now),
            Freshness::Live
        );
    }

    #[test]
    fn stale_past_window_or_no_data() {
        let now = 100_000;
        assert_eq!(
            Freshness::compute(true, Some(now - LIVE_WINDOW_MS - 1), now),
            Freshness::Stale
        );
        // Connected but nothing received yet.
        assert_eq!(Freshness::compute(true, None, now), Freshness::Stale);
    }

    #[test]
    fn labels_distinct() {
        assert_eq!(Freshness::Live.label(), "Live");
        assert_eq!(Freshness::Stale.label(), "Stale");
        assert_eq!(Freshness::Paused.label(), "Paused");
    }

    #[test]
    fn as_of_clock_formats_and_guards() {
        assert_eq!(as_of_clock(None), None);
        assert_eq!(as_of_clock(Some(0)), None);
        // 01:02:03 = 3723 s.
        assert_eq!(as_of_clock(Some(3_723_000)).as_deref(), Some("01:02:03"));
        // Wraps past 24h.
        assert_eq!(as_of_clock(Some(90_000_000)).as_deref(), Some("01:00:00"));
    }

    #[test]
    fn age_string_buckets() {
        assert_eq!(age_string(0), "just now");
        assert_eq!(age_string(500), "just now");
        assert_eq!(age_string(5_000), "5s ago");
        assert_eq!(age_string(120_000), "2m ago");
        assert_eq!(age_string(7_200_000), "2h ago");
        assert_eq!(age_string(172_800_000), "2d ago");
    }
}
