//! The stream health panel (#719, epic #712) — *which stage* is at fault.
//!
//! # What this is for
//!
//! An operator looking at a soft picture should be able to answer "which stage
//! is losing it?" in one glance. That is the whole feature; the numbers are in
//! service of it. Five things can go wrong and they have five different fixes:
//! the camera is starved, the encoder is shedding, the link is lossy, the
//! decoder is behind, or the tile is dropping late frames on purpose. Until
//! #716/#717/#718 the tile measured none of it and all five looked identical.
//!
//! # Why it is a chain and not five gauges
//!
//! Five gauges printed side by side do not answer the question — a reader has
//! to hold four numbers in their head and do the subtraction. The verdict is
//! always a *comparison* between adjacent stages:
//!
//! | | |
//! |---|---|
//! | offered 30, encoded 12 | the **encoder** cannot keep up |
//! | encoded 30, received 12 | the **transport** is losing frames |
//! | received 30, decoded 12 | the **decoder** is behind |
//!
//! So the panel is a chain of rates with the loss *at each hop* between them,
//! and the worst hop is named in a sentence at the top. Whatever the layout,
//! those three cases must look different at a glance.
//!
//! # Absent is not zero, here too
//!
//! Every rate is an `Option`, and a missing one renders as [`NOT_ASKED`] — the
//! same vocabulary the fleet view uses (RFC 09 §5.1 O4). Three of them are
//! genuinely unmeasurable in some deployments and must not be faked:
//!
//! - **Capture fps is not on the wire at all.** The sensor publishes encoded
//!   fps (`stats/fps`) and nothing upstream of it. The first link in the chain
//!   is therefore the tier's *applied* fps — the producer's declared offer,
//!   labelled as configured rather than measured, because presenting a config
//!   value as a measurement is how a panel lies.
//! - **Frame age is absent when the producer does not timestamp** (RFC 07
//!   §1.3). Shown as not asked; the deadline reports itself inactive.
//! - **A tile with no decode queue has no queue depth** — a preview tile, which
//!   is latest-frame-wins with nothing pending.

use iced::widget::{Space, column, container, row, text};
use iced::{Element, Length, Theme};
use zensight_common::TelemetryValue;
use zensight_common::stream::MediaReceiverReport;

use crate::message::Message;
use crate::view::device::DeviceDetailState;
use crate::view::specialized::parallax_detail::TileState;
use crate::view::theme;
use crate::view::tokens::space;

/// How a number that was never measured is written. One spelling, so a reader
/// learns it once — and never has to wonder whether a `0` means "zero" or
/// "we didn't look".
pub const NOT_ASKED: &str = "not asked";

/// A hop must lose at least this fraction before the panel names it.
///
/// Rates on a live stream jitter by a few percent between three-second
/// windows — a tile that received 29 of 30 frames is not a transport fault. A
/// panel that shouts at 3 % teaches an operator to ignore it, which is worse
/// than one that says nothing.
const DEGRADED_PCT: f32 = 15.0;

/// One stage on the path from camera to screen.
#[derive(Debug, Clone, PartialEq)]
pub struct Stage {
    /// What the reader calls it.
    pub name: &'static str,
    /// The rate that survived this stage, in frames per second. `None` is
    /// *not asked* — never zero.
    pub fps: Option<f32>,
    /// Whether [`Self::fps`] is a measurement or the producer's declared
    /// offer. The first link in the chain is an offer, and saying so is the
    /// difference between a panel and a lie.
    pub measured: bool,
    /// This stage's own supporting numbers, already formatted.
    pub detail: Vec<(&'static str, String)>,
}

/// Which stage is losing the picture.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Every stage passes on what it was given.
    Healthy,
    /// A named hop is losing frames.
    Degraded {
        /// The stage that *received* less than it was offered.
        stage: &'static str,
        lost_pct: f32,
    },
    /// Not enough is measured to name a stage, and why.
    NotMeasured(&'static str),
}

impl Verdict {
    /// The sentence at the top of the panel — the whole feature in one line.
    pub fn sentence(&self) -> String {
        match self {
            Self::Healthy => "Every stage is passing on what it was given.".to_string(),
            Self::Degraded { stage, lost_pct } => {
                format!("{stage}: {lost_pct:.0}% of the frames offered to it are not coming out.")
            }
            Self::NotMeasured(why) => format!("Nothing to compare yet — {why}."),
        }
    }
}

/// The whole panel's data: the chain, the two stages that are not rates, and
/// the verdict.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamHealth {
    /// The rate chain, in order. Adjacent pairs are the hops.
    pub chain: Vec<Stage>,
    /// Frame age and deadline sheds.
    pub presentation: Vec<(&'static str, String)>,
    /// Last IDR and what was asked for.
    pub recovery: Vec<(&'static str, String)>,
    pub verdict: Verdict,
}

/// The latest numeric value of one of the sensor's own `{stream}/stats/*`
/// series, whichever shape it was published in.
fn latest_stat(state: &DeviceDetailState, stream: &str, metric: &str) -> Option<f64> {
    state
        .history
        .get(&format!("{stream}/stats/{metric}"))?
        .iter()
        .rev()
        .find_map(|point| match point.value {
            TelemetryValue::Gauge(v) => Some(v),
            TelemetryValue::Counter(n) => Some(n as f64),
            _ => None,
        })
}

/// Growth of a cumulative counter between the two most recent samples.
///
/// A counter's absolute value only says something happened at *some* point in
/// this stream's life; the health panel is about now.
fn stat_growth(state: &DeviceDetailState, stream: &str, metric: &str) -> Option<u64> {
    let history = state.history.get(&format!("{stream}/stats/{metric}"))?;
    let mut counters = history.iter().rev().filter_map(|p| match p.value {
        TelemetryValue::Counter(n) => Some(n),
        TelemetryValue::Gauge(v) if v >= 0.0 => Some(v as u64),
        _ => None,
    });
    match (counters.next(), counters.next()) {
        (Some(latest), Some(previous)) => Some(latest.saturating_sub(previous)),
        _ => None,
    }
}

/// A per-second rate from two cumulative snapshots of one counter.
///
/// The wire counters are cumulative — that is what makes a resend idempotent —
/// so one report cannot answer "how many arrived in the last three seconds".
/// Two can, over the newer one's `interval_ms`.
fn rate(
    last: &MediaReceiverReport,
    prev: Option<&MediaReceiverReport>,
    of: fn(&MediaReceiverReport) -> u64,
) -> Option<f32> {
    let prev = prev?;
    if last.interval_ms == 0 {
        return None;
    }
    // A counter that went backwards is a new subscription's counters, not a
    // negative rate: say nothing rather than something wrong.
    let delta = of(last).checked_sub(of(prev))?;
    Some(delta as f32 * 1000.0 / last.interval_ms as f32)
}

/// Format an optional millisecond figure, negatives and all.
fn ms(v: Option<f32>) -> String {
    match v {
        Some(v) => format!("{v:.0} ms"),
        None => NOT_ASKED.to_string(),
    }
}

/// Read one stream's health out of everything already on hand: the sensor's
/// own stats (subscribed like any other telemetry), the per-tier status doc,
/// and the tile's last two receiver reports.
pub fn stream_health(state: &DeviceDetailState, stream: &str, tile: &TileState) -> StreamHealth {
    let detail = &state.parallax_detail;
    let applied = tile
        .selected_tier
        .as_deref()
        .and_then(|tier| detail.applied_tier(stream, tier));

    // ── Source: the producer's declared offer, labelled as such ──
    let mut source_detail = Vec::new();
    if let Some(applied) = applied {
        source_detail.push((
            "encoding",
            format!(
                "{}×{} · {} kbps cap",
                applied.applied.width, applied.applied.height, applied.applied.bitrate_kbps
            ),
        ));
        source_detail.push(("viewers", applied.viewers.to_string()));
    }
    let source = Stage {
        name: "Source",
        fps: applied.map(|a| a.applied.fps as f32),
        measured: false,
        detail: source_detail,
    };

    // ── Encoder: what the sensor says it actually egressed ──
    let mut encoder_detail = vec![(
        "bitrate",
        match latest_stat(state, stream, "kbps") {
            Some(v) => format!("{v:.0} kbps"),
            None => NOT_ASKED.to_string(),
        },
    )];
    // `drops` and `rc_drops` are disjoint by construction and mean different
    // things: a pipeline drop leaves a sequence gap, a rate-control drop does
    // not. Folding them into one "encoder drops" would erase the distinction
    // between "this box is too slow" and "you asked for 400 kbps".
    if let Some(n) = stat_growth(state, stream, "drops") {
        encoder_detail.push(("pipeline drops", n.to_string()));
    }
    if let Some(n) = stat_growth(state, stream, "rc_drops") {
        encoder_detail.push(("bitrate cap drops", n.to_string()));
    }
    if let Some(v) = latest_stat(state, stream, "encode_p95_ms") {
        encoder_detail.push(("encode p95", format!("{v:.1} ms")));
    }
    let encoder = Stage {
        name: "Encoder",
        fps: latest_stat(state, stream, "fps").map(|v| v as f32),
        measured: true,
        detail: encoder_detail,
    };

    // ── Transport and Decoder: the tile's own two most recent reports ──
    let last = tile.last_report.as_ref();
    let prev = tile.prev_report.as_ref();

    let transport = Stage {
        name: "Transport",
        fps: last.and_then(|l| rate(l, prev, |r| r.received_frames)),
        measured: true,
        detail: match last {
            Some(l) => {
                let denominator = l.received_frames + l.lost_frames;
                let loss = if denominator == 0 {
                    0.0
                } else {
                    l.lost_frames as f64 * 100.0 / denominator as f64
                };
                vec![
                    ("loss", format!("{loss:.1}%")),
                    ("jitter", ms(l.interarrival_jitter_ms)),
                    ("gaps", l.lost_frames.to_string()),
                ]
            }
            None => vec![("loss", NOT_ASKED.to_string())],
        },
    };

    let decoder = Stage {
        name: "Decoder",
        fps: last.and_then(|l| rate(l, prev, |r| r.decoded_frames)),
        measured: true,
        detail: match last {
            Some(l) => vec![
                (
                    "queue",
                    match l.decoder_queue_depth {
                        Some(d) => d.to_string(),
                        // A preview tile decodes latest-frame-wins with
                        // nothing pending: `0` would read "empty" where the
                        // truth is "no queue".
                        None => "no queue".to_string(),
                    },
                ),
                ("shed", l.dropped_frames.to_string()),
            ],
            None => vec![("queue", NOT_ASKED.to_string())],
        },
    };

    let presentation = match last {
        Some(l) => vec![
            ("frame age", ms(l.frame_age_ms)),
            ("worst", ms(l.frame_age_max_ms)),
        ],
        None => vec![("frame age", NOT_ASKED.to_string())],
    };

    let recovery = match last {
        Some(l) => vec![
            (
                "last keyframe",
                match l.last_keyframe_sequence {
                    Some(seq) => format!("#{seq}"),
                    None => NOT_ASKED.to_string(),
                },
            ),
            (
                "age",
                match l.since_last_keyframe_ms {
                    Some(v) => format!("{:.1} s", v as f32 / 1000.0),
                    None => NOT_ASKED.to_string(),
                },
            ),
        ],
        None => vec![("last keyframe", NOT_ASKED.to_string())],
    };

    let chain = vec![source, encoder, transport, decoder];
    let verdict = verdict(&chain, prev.is_some());
    StreamHealth {
        chain,
        presentation,
        recovery,
        verdict,
    }
}

/// The loss at the hop *into* `chain[i]`, as a percentage of what the previous
/// stage passed on. `None` when either end is not asked.
fn hop_loss(chain: &[Stage], i: usize) -> Option<f32> {
    let (before, after) = (chain.get(i.checked_sub(1)?)?, chain.get(i)?);
    let (before, after) = (before.fps?, after.fps?);
    if before <= 0.0 {
        return None;
    }
    Some(((before - after) / before * 100.0).max(0.0))
}

/// Name the worst hop, if any hop is bad enough to be worth naming.
///
/// `have_rates` says whether the tile has reported *twice* — one cumulative
/// snapshot is not a rate — so that "still measuring" and "the producer is
/// telling us nothing" do not end up wearing the same sentence.
fn verdict(chain: &[Stage], have_rates: bool) -> Verdict {
    let worst = (1..chain.len())
        .filter_map(|i| hop_loss(chain, i).map(|pct| (i, pct)))
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    match worst {
        Some((i, pct)) if pct >= DEGRADED_PCT => Verdict::Degraded {
            stage: chain[i].name,
            lost_pct: pct,
        },
        Some(_) => Verdict::Healthy,
        None if !have_rates => Verdict::NotMeasured("this tile is still measuring"),
        None => Verdict::NotMeasured("nothing on this path is publishing a rate"),
    }
}

/// The colour a hop's loss reads as. Design-system accessors only — the CI
/// colour guard is a merge gate.
fn hop_style(loss: Option<f32>) -> impl Fn(&Theme) -> text::Style + Copy {
    move |t: &Theme| {
        let colors = theme::colors(t);
        let color = match loss {
            None => colors.text_dimmed(),
            Some(pct) if pct >= DEGRADED_PCT => colors.danger_text(),
            Some(pct) if pct >= DEGRADED_PCT / 3.0 => colors.warning(),
            Some(_) => colors.success(),
        };
        text::Style { color: Some(color) }
    }
}

fn dimmed(t: &Theme) -> text::Style {
    text::Style {
        color: Some(theme::colors(t).text_dimmed()),
    }
}

fn muted(t: &Theme) -> text::Style {
    text::Style {
        color: Some(theme::colors(t).text_muted()),
    }
}

/// A stage's card: name, rate, then its own numbers.
fn stage_card<'a>(stage: &Stage) -> Element<'a, Message> {
    let rate = match stage.fps {
        Some(fps) => format!("{fps:.0} fps"),
        None => NOT_ASKED.to_string(),
    };
    let mut card = column![
        text(stage.name.to_string()).size(11).style(muted),
        text(rate).size(16),
        // The first link is the producer's declared offer, not a measurement,
        // and the panel says so rather than letting a reader assume otherwise.
        text(if stage.measured {
            "measured"
        } else {
            "offered"
        })
        .size(10)
        .style(dimmed),
    ]
    .spacing(space::XS);
    for (label, value) in &stage.detail {
        card = card.push(text(format!("{label} {value}")).size(11).style(dimmed));
    }
    container(card)
        .padding(space::SM)
        .width(Length::Fixed(150.0))
        .into()
}

/// The connector between two stages: how much of what went in came out.
fn hop<'a>(loss: Option<f32>) -> Element<'a, Message> {
    let label = match loss {
        Some(pct) if pct >= 1.0 => format!("→ −{pct:.0}%"),
        Some(_) => "→".to_string(),
        None => "→ ?".to_string(),
    };
    container(text(label).size(12).style(hop_style(loss)))
        .padding(space::XS)
        .center_y(Length::Fill)
        .into()
}

/// A stage that is not a rate: a column of label/value pairs. Takes the facts
/// by value — the panel's data is computed per render and the widgets outlive
/// the struct it came out of.
fn facts<'a>(name: &'static str, facts: Vec<(&'static str, String)>) -> Element<'a, Message> {
    let mut card = column![text(name).size(11).style(muted)].spacing(space::XS);
    for (label, value) in facts {
        card = card.push(text(format!("{label} {value}")).size(11).style(dimmed));
    }
    container(card)
        .padding(space::SM)
        .width(Length::Fixed(150.0))
        .into()
}

/// The panel: a verdict sentence over the chain, then the two stages that are
/// not rates.
pub fn health_panel<'a>(
    state: &'a DeviceDetailState,
    stream: &'a str,
    tile: &'a TileState,
) -> Element<'a, Message> {
    let health = stream_health(state, stream, tile);
    let verdict_style = match health.verdict {
        Verdict::Degraded { lost_pct, .. } => hop_style(Some(lost_pct)),
        Verdict::Healthy => hop_style(Some(0.0)),
        Verdict::NotMeasured(_) => hop_style(None),
    };

    let mut chain = row![].align_y(iced::Alignment::Center);
    for (i, stage) in health.chain.iter().enumerate() {
        if i > 0 {
            chain = chain.push(hop(hop_loss(&health.chain, i)));
        }
        chain = chain.push(stage_card(stage));
    }
    chain = chain
        .push(Space::new().width(space::MD))
        .push(facts("Presentation", health.presentation))
        .push(facts("Recovery", health.recovery));

    column![
        text(health.verdict.sentence())
            .size(13)
            .style(verdict_style),
        iced::widget::scrollable(chain).direction(iced::widget::scrollable::Direction::Horizontal(
            iced::widget::scrollable::Scrollbar::default()
        )),
    ]
    .spacing(space::SM)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(received: u64, decoded: u64, lost: u64) -> MediaReceiverReport {
        MediaReceiverReport {
            stream: "cam0".into(),
            codec: Some("h264".into()),
            tier: Some("high".into()),
            consumer_id: "zs-1-1".into(),
            interval_ms: 1_000,
            received_frames: received,
            lost_frames: lost,
            decoded_frames: decoded,
            last_sequence: received + lost,
            ..Default::default()
        }
    }

    /// The three verdicts the issue names must come out different. This is the
    /// whole feature: a reader should not have to subtract two numbers to find
    /// out which box to go and look at.
    fn chain_of(source: f32, encoder: f32, transport: f32, decoder: f32) -> Vec<Stage> {
        [
            ("Source", source, false),
            ("Encoder", encoder, true),
            ("Transport", transport, true),
            ("Decoder", decoder, true),
        ]
        .into_iter()
        .map(|(name, fps, measured)| Stage {
            name,
            fps: Some(fps),
            measured,
            detail: Vec::new(),
        })
        .collect()
    }

    /// The stage a verdict names, and roughly how bad it says it is. The
    /// percentage is compared loosely on purpose — `30.0 - 12.0) / 30.0` is
    /// 60.000004 in f32, and pinning that would be a test about floating point
    /// rather than about which box an operator should go and look at.
    fn named(v: &Verdict) -> (&'static str, i32) {
        match v {
            Verdict::Degraded { stage, lost_pct } => (stage, lost_pct.round() as i32),
            other => panic!("expected a degraded verdict, got {other:?}"),
        }
    }

    #[test]
    fn the_three_verdicts_name_three_different_stages() {
        assert_eq!(
            named(&verdict(&chain_of(30.0, 12.0, 12.0, 12.0), true)),
            ("Encoder", 60),
            "offered 30, encoded 12 — the encoder is not keeping up"
        );
        assert_eq!(
            named(&verdict(&chain_of(30.0, 30.0, 12.0, 12.0), true)),
            ("Transport", 60),
            "encoded 30, received 12 — the link is losing frames"
        );
        assert_eq!(
            named(&verdict(&chain_of(30.0, 30.0, 30.0, 12.0), true)),
            ("Decoder", 60),
            "received 30, decoded 12 — this box is behind"
        );
    }

    #[test]
    fn a_few_percent_of_jitter_is_not_a_verdict() {
        assert_eq!(
            verdict(&chain_of(30.0, 29.0, 29.0, 28.0), true),
            Verdict::Healthy,
            "rates jitter between three-second windows; a panel that shouts at \
             3% teaches an operator to ignore it"
        );
    }

    /// Two stages can both be losing; the panel names the worse one, because
    /// that is the box to go and look at first.
    #[test]
    fn the_worst_hop_wins() {
        assert_eq!(
            named(&verdict(&chain_of(30.0, 24.0, 6.0, 6.0), true)),
            ("Transport", 75)
        );
    }

    #[test]
    fn an_unmeasured_chain_says_so_rather_than_reading_healthy() {
        let mut chain = chain_of(30.0, 30.0, 30.0, 30.0);
        for stage in chain.iter_mut().skip(1) {
            stage.fps = None;
        }
        assert!(
            matches!(verdict(&chain, false), Verdict::NotMeasured(_)),
            "a chain with nothing to compare must not read as healthy"
        );
        assert!(
            verdict(&chain, false)
                .sentence()
                .contains("still measuring"),
            "and it must say WHY — a tile that has reported once has counters, \
             not rates, and that reads differently from a silent producer"
        );
        assert!(
            verdict(&chain, true)
                .sentence()
                .contains("publishing a rate"),
            "a tile with two reports and still nothing to compare is a \
             different diagnosis"
        );
    }

    /// Cumulative counters plus the window they cover is a rate; one report on
    /// its own is not.
    #[test]
    fn a_rate_needs_two_reports() {
        let first = report(100, 100, 0);
        let second = report(130, 128, 2);
        assert_eq!(
            rate(&second, None, |r| r.received_frames),
            None,
            "one cumulative snapshot cannot be a rate"
        );
        assert_eq!(
            rate(&second, Some(&first), |r| r.received_frames),
            Some(30.0),
            "30 frames in the 1000 ms the newer report covers"
        );
        assert_eq!(
            rate(&second, Some(&first), |r| r.decoded_frames),
            Some(28.0)
        );
    }

    /// A tile that reopened starts its counters again. That is a new
    /// subscription, not a negative rate.
    #[test]
    fn counters_that_went_backwards_produce_no_rate_at_all() {
        let before = report(9_000, 9_000, 0);
        let after = report(3, 3, 0);
        assert_eq!(rate(&after, Some(&before), |r| r.received_frames), None);
    }

    #[test]
    fn a_hop_into_an_unmeasured_stage_has_no_loss() {
        let mut chain = chain_of(30.0, 30.0, 30.0, 30.0);
        chain[2].fps = None;
        assert_eq!(hop_loss(&chain, 2), None, "not asked, not zero");
        assert_eq!(hop_loss(&chain, 3), None);
        assert_eq!(hop_loss(&chain, 1), Some(0.0));
    }

    /// A stage that gained frames (a rate hiccup between windows) is not
    /// "negative loss" — it is no loss.
    #[test]
    fn a_stage_that_gained_frames_reads_as_no_loss() {
        assert_eq!(hop_loss(&chain_of(30.0, 31.0, 31.0, 31.0), 1), Some(0.0));
    }
}
