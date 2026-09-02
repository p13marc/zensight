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

/// Frame age above which a losing Transport hop is congestion rather than
/// in-flight loss (#801).
///
/// Measured, not guessed. #713 ran both failure modes and they are three orders
/// of magnitude apart in this one number
/// ([`loss-measurement.md`](../../../../docs/plans/adaptive-media/loss-measurement.md)):
///
/// | | frames missing | median frame age |
/// |---|---|---|
/// | `tcp/` at 300 kbit, 1.7 Mbps offered | 83 % | **3 502 ms** |
/// | `tcp/` at 100 kbit | 93 % | **9 085 ms** |
/// | `quic/…?mixed_rel=1`, 1 % packet loss | 20 % | **0.77 ms** |
/// | same, 5 % packet loss | 57 % | **0.78 ms** |
///
/// Anywhere between them separates the two. 500 ms is chosen to sit far above
/// what a healthy WAN path shows and far below what congestion showed, and the
/// test only runs once the hop is already losing [`DEGRADED_PCT`] — a fresh
/// stream with a leisurely age is not accused of anything.
const CONGESTED_AGE_MS: f32 = 500.0;

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
        /// For a losing Transport hop only: *why*, when frame age can say
        /// (#801). `None` when the stage is not Transport, or when the samples
        /// arrived unstamped and the question was never asked.
        cause: Option<TransportCause>,
    },
    /// Not enough is measured to name a stage, and why.
    NotMeasured(&'static str),
}

/// Why a losing Transport hop is losing (#801).
///
/// The two look identical in every counter this stack publishes — the sensor's
/// `stats/drops` reads **0** under congestion, because the frames die in
/// Zenoh's own transport queue under `CongestionControl::Drop`, upstream of
/// every counter the sensor has. What separates them is *frame age*, which the
/// tile already measures and already reports.
///
/// The distinction is the difference between two opposite fixes: a congested
/// sender wants a smaller tier (or a wider pipe), and a lossy link wants a
/// smaller *access unit* — which is also a smaller tier, but for a different
/// reason and with a different ceiling. #713 verdict 1 has the arithmetic.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TransportCause {
    /// Frames are arriving, but old: the sender's queue is deep and Zenoh is
    /// discarding what it cannot get onto the link.
    SenderCongested { age_ms: f32 },
    /// What arrives is fresh, and the rest never arrived at all.
    InFlightLoss { age_ms: f32 },
}

impl Verdict {
    /// The sentence at the top of the panel — the whole feature in one line.
    pub fn sentence(&self) -> String {
        match self {
            Self::Healthy => "Every stage is passing on what it was given.".to_string(),
            Self::Degraded {
                stage,
                lost_pct,
                cause,
            } => {
                let head = format!(
                    "{stage}: {lost_pct:.0}% of the frames offered to it are not coming out"
                );
                match cause {
                    Some(TransportCause::SenderCongested { age_ms }) => format!(
                        "{head} — the sender is congested. What does arrive is {} old, so the \
                         frames were discarded before the wire and no counter here saw it.",
                        human_age(*age_ms)
                    ),
                    Some(TransportCause::InFlightLoss { age_ms }) => format!(
                        "{head} — they were lost in flight. What does arrive is {} old, so \
                         nothing is queueing; the link is dropping.",
                        human_age(*age_ms)
                    ),
                    None => format!("{head}."),
                }
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
///
/// **Not** [`zensight_store::rate::counter_rate`], and deliberately so (#904).
/// That function derives `dt` from the gap between two sample timestamps; this
/// one is told the interval by the producer, in the report itself, because a
/// receiver report is a summary *of a window* rather than an observation at an
/// instant — the two timestamps here would measure when the reports arrived,
/// not the window they describe. The shared rule the two do agree on is the
/// only one that generalises: a counter that went backwards is a new
/// subscription's counters, so say nothing rather than something wrong.
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
    // things: a pipeline drop is a buffer the sink shed because nothing pulled
    // it in time, a rate-control drop is a frame the encoder never emitted at
    // all (#692). Folding them into one "encoder drops" would erase the
    // distinction between "this box is too slow" and "you asked for 400 kbps".
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
    let verdict = verdict(&chain, prev.is_some(), last.and_then(|l| l.frame_age_ms));
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

/// A frame age in the units a reader thinks in.
fn human_age(ms: f32) -> String {
    if ms >= 1000.0 {
        format!("{:.1} s", ms / 1000.0)
    } else if ms >= 1.0 {
        format!("{ms:.0} ms")
    } else {
        format!("{ms:.1} ms")
    }
}

/// Name the worst hop, if any hop is bad enough to be worth naming.
///
/// `have_rates` says whether the tile has reported *twice* — one cumulative
/// snapshot is not a rate — so that "still measuring" and "the producer is
/// telling us nothing" do not end up wearing the same sentence.
///
/// `age_ms` is the tile's median frame age, and it is what turns a losing
/// Transport hop into a *diagnosis* rather than a location (#801). `None` is
/// "not asked" — unstamped samples — and leaves the verdict at the location.
fn verdict(chain: &[Stage], have_rates: bool, age_ms: Option<f32>) -> Verdict {
    let worst = (1..chain.len())
        .filter_map(|i| hop_loss(chain, i).map(|pct| (i, pct)))
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    match worst {
        Some((i, pct)) if pct >= DEGRADED_PCT => Verdict::Degraded {
            stage: chain[i].name,
            lost_pct: pct,
            cause: (chain[i].name == "Transport")
                .then_some(age_ms)
                .flatten()
                .map(|age| {
                    if age > CONGESTED_AGE_MS {
                        TransportCause::SenderCongested { age_ms: age }
                    } else {
                        TransportCause::InFlightLoss { age_ms: age }
                    }
                }),
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
            Verdict::Degraded {
                stage, lost_pct, ..
            } => (stage, lost_pct.round() as i32),
            other => panic!("expected a degraded verdict, got {other:?}"),
        }
    }

    #[test]
    fn the_three_verdicts_name_three_different_stages() {
        assert_eq!(
            named(&verdict(&chain_of(30.0, 12.0, 12.0, 12.0), true, None)),
            ("Encoder", 60),
            "offered 30, encoded 12 — the encoder is not keeping up"
        );
        assert_eq!(
            named(&verdict(&chain_of(30.0, 30.0, 12.0, 12.0), true, None)),
            ("Transport", 60),
            "encoded 30, received 12 — the link is losing frames"
        );
        assert_eq!(
            named(&verdict(&chain_of(30.0, 30.0, 30.0, 12.0), true, None)),
            ("Decoder", 60),
            "received 30, decoded 12 — this box is behind"
        );
    }

    #[test]
    fn a_few_percent_of_jitter_is_not_a_verdict() {
        assert_eq!(
            verdict(&chain_of(30.0, 29.0, 29.0, 28.0), true, None),
            Verdict::Healthy,
            "rates jitter between three-second windows; a panel that shouts at \
             3% teaches an operator to ignore it"
        );
    }

    /// #801: a losing Transport hop is two opposite faults wearing one number,
    /// and the panel must not make an operator guess which.
    ///
    /// The inputs here are the ones #713 actually measured, not invented
    /// fixtures: a `tcp/` link at 300 kbit against ~1.7 Mbps offered lost 83 %
    /// of sequences at a median frame age of 3 502 ms, and a QUIC link at 1 %
    /// packet loss lost 20 % at 0.77 ms. Nothing else in this stack separates
    /// them — the sensor's own `stats/drops` reads 0 in *both* cases.
    #[test]
    fn a_losing_transport_hop_says_whether_the_sender_is_congested_or_the_link_is_dropping() {
        // The congested case: almost nothing arrives, and what does is seconds old.
        let congested = verdict(&chain_of(30.0, 30.0, 5.0, 5.0), true, Some(3502.0));
        let sentence = congested.sentence();
        assert!(
            sentence.contains("sender is congested"),
            "expected a congestion diagnosis, got: {sentence}"
        );
        assert!(
            sentence.contains("3.5 s"),
            "and the evidence for it, in units a reader thinks in: {sentence}"
        );

        // The lossy case: the same hop, the same shape of loss, a fresh stream.
        let lossy = verdict(&chain_of(30.0, 30.0, 24.0, 24.0), true, Some(0.77));
        let sentence = lossy.sentence();
        assert!(
            sentence.contains("lost in flight"),
            "expected an in-flight-loss diagnosis, got: {sentence}"
        );
        assert!(
            sentence.contains("0.8 ms"),
            "sub-millisecond ages must not round away to `0 ms`: {sentence}"
        );

        // Unstamped samples: the question was never asked, so it is not answered.
        let unasked = verdict(&chain_of(30.0, 30.0, 5.0, 5.0), true, None);
        assert!(
            matches!(
                unasked,
                Verdict::Degraded {
                    stage: "Transport",
                    cause: None,
                    ..
                }
            ),
            "an absent frame age must leave the verdict at the location, not \
             guess a cause: {unasked:?}"
        );
    }

    /// The cause belongs to Transport alone. A slow decoder is not congestion
    /// however old the frames are — the age it sees is *its own* backlog.
    #[test]
    fn only_the_transport_hop_gets_a_cause() {
        assert!(
            matches!(
                verdict(&chain_of(30.0, 30.0, 30.0, 5.0), true, Some(4000.0)),
                Verdict::Degraded {
                    stage: "Decoder",
                    cause: None,
                    ..
                }
            ),
            "a decoder verdict must not be dressed up as a transport diagnosis"
        );
    }

    /// Two stages can both be losing; the panel names the worse one, because
    /// that is the box to go and look at first.
    #[test]
    fn the_worst_hop_wins() {
        assert_eq!(
            named(&verdict(&chain_of(30.0, 24.0, 6.0, 6.0), true, None)),
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
            matches!(verdict(&chain, false, None), Verdict::NotMeasured(_)),
            "a chain with nothing to compare must not read as healthy"
        );
        assert!(
            verdict(&chain, false, None)
                .sentence()
                .contains("still measuring"),
            "and it must say WHY — a tile that has reported once has counters, \
             not rates, and that reads differently from a silent producer"
        );
        assert!(
            verdict(&chain, true, None)
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
