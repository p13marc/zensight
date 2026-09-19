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
    ///
    /// `last_receive_ms` is **this process's** clock at decode, never a
    /// sensor's timestamp (#1117).
    ///
    /// `saturating_sub` is what made the distinction load-bearing: on a point
    /// stamped in the future the subtraction floors at 0, which reads as
    /// *fresher than possible* — so a single host an hour ahead pinned the
    /// indicator at **Live**, including after every sensor on the fleet had
    /// died. A VM resumed from a snapshot or a box without NTP is enough, and
    /// the probe sensor's `ntp_offset_ms` exists precisely because those are
    /// common.
    pub fn compute(connected: bool, last_receive_ms: Option<i64>, now_ms: i64) -> Self {
        if !connected {
            return Freshness::Paused;
        }
        match last_receive_ms {
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

/// The "as of" clock for the newest data point. `None` when there is no data
/// yet.
///
/// One formatter, shared (#1123). It used to hand-roll `(secs / 3600) % 24` —
/// **UTC with no suffix** — while the systemd detail rendered `chrono::Local`
/// and the chart range said "UTC" out loud. An operator in UTC+2 read
/// "as of 13:42" here and "15:42:10" on the unit that had just restarted, and
/// concluded the feed was two hours behind.
pub fn as_of_clock(last_update_ms: Option<i64>) -> Option<String> {
    let ts = last_update_ms?;
    if ts <= 0 {
        return None;
    }
    Some(crate::view::formatting::format_clock(ts))
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
/// How long after a reconnect the indicator says so (#1116).
///
/// Two minutes. Long enough that an operator who looked away during the blip
/// still sees it, short enough that it does not become furniture — a marker
/// that is always on says nothing.
const RECONNECT_NOTICE_MS: i64 = 2 * 60 * 1000;

#[allow(clippy::too_many_arguments)]
pub fn freshness_indicator<'a>(
    connected: bool,
    // The newest SENSOR timestamp, for the "as of" clock.
    last_update_ms: Option<i64>,
    // OUR clock at the last decode, for the verdict (#1117).
    last_receive_ms: Option<i64>,
    now_ms: i64,
    reconnected_at: Option<i64>,
    // How many devices' clocks disagree with ours (#1117).
    skewed_hosts: usize,
) -> Element<'a, Message> {
    let verdict = Freshness::compute(connected, last_receive_ms, now_ms);
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

    // The moment of reconnect (#1116). A reconnected GUI otherwise looks
    // exactly like one that has been watching all along, and the difference is
    // whether anything on screen spans a gap — alerts that resolved during the
    // blip, a host that went away, a sensor that was redeployed.
    if let Some(at) = reconnected_at
        && now_ms.saturating_sub(at) < RECONNECT_NOTICE_MS
        && let Some(clock) = as_of_clock(Some(at))
    {
        content = content.push(
            text(format!("· reconnected {clock}"))
                .size(font::CAPTION)
                .style(|theme: &Theme| text::Style {
                    color: Some(theme::colors(theme).text_dimmed()),
                }),
        );
    }

    // Clock skew is its own indicator, not a modifier of the verdict (#1117).
    // A skewed clock is not staleness — the data is arriving fine — and
    // reporting it as staleness would say the wrong thing about a fleet that
    // is working. What it does mean is that some host's "as of" cannot be
    // compared with any other's.
    if skewed_hosts > 0 {
        let label = if skewed_hosts == 1 {
            "· clock skew on 1 host".to_string()
        } else {
            format!("· clock skew on {skewed_hosts} hosts")
        };
        content =
            content.push(
                text(label)
                    .size(font::CAPTION)
                    .style(|theme: &Theme| text::Style {
                        color: Some(theme::colors(theme).warning()),
                    }),
            );
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

    /// **The acceptance criterion** (#1117): a point stamped an hour in the
    /// future, then silence, goes Stale after ten seconds.
    ///
    /// It did not, and the arithmetic is the whole reason: `now - ts` on a
    /// future timestamp is negative, `saturating_sub` floors it at 0, and 0 is
    /// inside every window — so the indicator read **Live**. One host resumed
    /// from a snapshot, or one box whose NTP never started, pinned it for the
    /// whole fleet, and it stayed pinned after every sensor had died.
    #[test]
    fn a_point_stamped_in_the_future_does_not_pin_the_verdict() {
        let now = 1_700_000_000_000;
        let an_hour_ahead = now + 3_600_000;

        // The premise, so the test does not merely restate the fix: the
        // sensor's own clock still saturates the way it always did.
        assert_eq!(
            Freshness::compute(true, Some(an_hour_ahead), now),
            Freshness::Live,
            "a future SENSOR timestamp still reads Live — which is why the \
             verdict must not be computed from one"
        );

        // What the verdict is computed from now: our clock at decode. The
        // point arrived, then nothing for eleven seconds.
        let received = now;
        assert_eq!(
            Freshness::compute(true, Some(received), now + 11_000),
            Freshness::Stale,
            "silence is silence, whatever the publisher's clock claims"
        );
        assert_eq!(
            Freshness::compute(true, Some(received), now + 5_000),
            Freshness::Live,
            "and inside the window it is still Live"
        );
    }

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

    /// The guards, and **the offset suffix** (#1123).
    ///
    /// Asserted as a shape rather than a literal, because the output is the
    /// viewer's local zone and a test that pinned a string would pass only on
    /// a UTC machine — which is exactly the assumption that produced three
    /// disagreeing formatters. What matters is that the clock *says which zone
    /// it is in*: a bare `13:42:10` beside the systemd detail's local
    /// `15:42:10` is what made an operator in UTC+2 conclude the feed was two
    /// hours behind.
    #[test]
    fn as_of_clock_formats_and_guards() {
        assert_eq!(as_of_clock(None), None);
        assert_eq!(as_of_clock(Some(0)), None);

        let clock = as_of_clock(Some(3_723_000)).expect("a positive instant formats");
        let (time, offset) = clock.split_once(' ').unwrap_or_else(|| {
            panic!("the clock must carry its offset, got {clock:?}");
        });
        assert_eq!(time.len(), 8, "HH:MM:SS, got {time:?}");
        assert_eq!(time.matches(':').count(), 2, "{time:?}");
        assert!(
            (offset.starts_with('+') || offset.starts_with('-')) && offset.contains(':'),
            "the offset is signed and separated, got {offset:?}"
        );

        // Two instants an hour apart are an hour apart on the clock, whatever
        // the zone.
        let a = as_of_clock(Some(3_723_000)).unwrap();
        let b = as_of_clock(Some(3_723_000 + 3_600_000)).unwrap();
        assert_ne!(a, b);
        assert_eq!(
            a.split_once(' ').unwrap().0[3..],
            b.split_once(' ').unwrap().0[3..],
            "the minutes and seconds are unchanged: {a} vs {b}"
        );
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
