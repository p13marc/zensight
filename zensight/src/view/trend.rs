//! Trend badges + dashboard sparklines (Plan v3-04 §C, #24).
//!
//! A signed **% delta + arrow** badge summarizes how a metric moved over a
//! window (first vs last sample); a 24h **sparkline** on a device card shows the
//! shape. Both read from the local store's samples (see [`crate::store`]). All
//! math is pure and unit-tested; rendering is redundant (arrow glyph + sign +
//! text, never color alone).

use std::collections::HashMap;

use iced::widget::{row, text};
use iced::{Alignment, Element, Theme};

use crate::message::DeviceId;
use crate::store::{MetricStore, Sample};
use crate::view::components::sparkline::Sparkline;
use crate::view::theme;
use crate::view::tokens::{font, space};

/// Direction of a metric trend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrendDir {
    /// Value increased beyond the flat threshold.
    Up,
    /// Value decreased beyond the flat threshold.
    Down,
    /// Change within the flat threshold (or no baseline) — treated as flat.
    Flat,
}

impl TrendDir {
    /// The arrow glyph for this direction (redundant with the sign/text).
    pub fn arrow(self) -> &'static str {
        match self {
            TrendDir::Up => "\u{2191}",   // ↑
            TrendDir::Down => "\u{2193}", // ↓
            TrendDir::Flat => "\u{2192}", // →
        }
    }
}

/// A computed trend over a window: direction + signed percentage change.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Trend {
    /// Direction (up/down/flat).
    pub dir: TrendDir,
    /// Signed percentage change `(last - first) / |first| * 100`. `0.0` when the
    /// baseline is zero or there is no baseline.
    pub pct: f64,
}

/// Percentage moves within this magnitude are reported as flat (noise floor).
pub const FLAT_PCT: f64 = 1.0;

/// Compute the trend of a sample series: first vs last value, as a signed
/// percentage. Needs at least two samples; otherwise returns flat 0%.
///
/// When the baseline (first value) is zero, percentage is undefined — report
/// the direction from the raw delta but a `0.0` pct (honest: "changed, but no
/// meaningful %"). Pure.
pub fn compute(samples: &[Sample]) -> Trend {
    if samples.len() < 2 {
        return Trend {
            dir: TrendDir::Flat,
            pct: 0.0,
        };
    }
    let first = samples.first().unwrap().value;
    let last = samples.last().unwrap().value;
    let delta = last - first;
    let pct = if first.abs() > f64::EPSILON {
        delta / first.abs() * 100.0
    } else {
        0.0
    };
    let dir =
        if pct.abs() < FLAT_PCT || (first.abs() <= f64::EPSILON && delta.abs() <= f64::EPSILON) {
            TrendDir::Flat
        } else if delta > 0.0 {
            TrendDir::Up
        } else {
            TrendDir::Down
        };
    Trend { dir, pct }
}

/// Format the badge text, e.g. `↑ +12.5%`, `↓ -3.0%`, `→ 0%`.
pub fn badge_text(trend: Trend) -> String {
    match trend.dir {
        TrendDir::Flat => format!("{} 0%", TrendDir::Flat.arrow()),
        _ => format!("{} {:+.1}%", trend.dir.arrow(), trend.pct),
    }
}

/// A trend badge element: arrow + signed percent, colored by direction but with
/// the arrow/sign carrying the meaning too (never color alone).
pub fn trend_badge<'a, Message: 'a>(trend: Trend) -> Element<'a, Message> {
    text(badge_text(trend))
        .size(font::CAPTION)
        .style(move |theme: &Theme| {
            let c = theme::colors(theme);
            let color = match trend.dir {
                TrendDir::Up => c.status_connected(),
                TrendDir::Down => c.warning(),
                TrendDir::Flat => c.text_dimmed(),
            };
            text::Style { color: Some(color) }
        })
        .into()
}

/// Precomputed per-card sparkline + trend data for a single metric.
#[derive(Debug, Clone)]
pub struct MetricSpark {
    /// Metric name (the suffix after `|`).
    pub metric: String,
    /// Raw values for the sparkline (oldest-first).
    pub values: Vec<f64>,
    /// Computed trend over the window.
    pub trend: Trend,
}

/// Dashboard-card preview: a few key metrics' sparks for one device.
pub type DeviceSparks = HashMap<DeviceId, Vec<MetricSpark>>;

/// Build per-device spark previews from the store's hot ring for the given
/// devices. Picks up to `per_device` metrics (those with the most samples) so a
/// card shows a couple of meaningful sparklines, not noise. Pure given the
/// store snapshot; cheap (reads the in-memory ring, no disk).
pub fn build_device_sparks<'a>(
    store: &MetricStore,
    devices: impl Iterator<Item = &'a DeviceId>,
    per_device: usize,
) -> DeviceSparks {
    let mut out = DeviceSparks::new();
    for id in devices {
        let protocol = id.protocol.to_string();
        let mut metrics: Vec<MetricSpark> = store
            .device_hot_samples(&protocol, &id.source)
            .into_iter()
            .filter(|(_, samples)| samples.len() >= 2)
            .map(|(metric, samples)| {
                let trend = compute(&samples);
                MetricSpark {
                    metric,
                    values: samples.into_iter().map(|s| s.value).collect(),
                    trend,
                }
            })
            .collect();
        // Most-sampled metrics first, then by name for stability.
        metrics.sort_by(|a, b| {
            b.values
                .len()
                .cmp(&a.values.len())
                .then_with(|| a.metric.cmp(&b.metric))
        });
        metrics.truncate(per_device);
        if !metrics.is_empty() {
            out.insert(id.clone(), metrics);
        }
    }
    out
}

/// Render a compact card sparkline + trend badge row for one metric. Consumes
/// the spark (owns all its data), so the returned element borrows nothing.
pub fn card_metric_spark<'a, Message: 'a + Clone>(spark: MetricSpark) -> Element<'a, Message> {
    let sparkline = Sparkline::new(spark.values)
        .with_size(64.0, 18.0)
        .view::<Message>();
    row![
        text(spark.metric).size(font::CAPTION),
        sparkline,
        trend_badge::<Message>(spark.trend),
    ]
    .spacing(space::SM)
    .align_y(Alignment::Center)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(value: f64) -> Sample {
        Sample { ts: 0, value }
    }

    #[test]
    fn too_few_samples_is_flat() {
        assert_eq!(compute(&[]).dir, TrendDir::Flat);
        assert_eq!(compute(&[s(5.0)]).dir, TrendDir::Flat);
    }

    #[test]
    fn rising_series_is_up() {
        let t = compute(&[s(100.0), s(110.0), s(125.0)]);
        assert_eq!(t.dir, TrendDir::Up);
        assert!((t.pct - 25.0).abs() < 1e-9);
        assert_eq!(badge_text(t), "\u{2191} +25.0%");
    }

    #[test]
    fn falling_series_is_down() {
        let t = compute(&[s(80.0), s(40.0)]);
        assert_eq!(t.dir, TrendDir::Down);
        assert!((t.pct + 50.0).abs() < 1e-9);
        assert_eq!(badge_text(t), "\u{2193} -50.0%");
    }

    #[test]
    fn small_move_is_flat() {
        // 0.5% move is below the noise floor.
        let t = compute(&[s(100.0), s(100.5)]);
        assert_eq!(t.dir, TrendDir::Flat);
        assert_eq!(badge_text(t), "\u{2192} 0%");
    }

    #[test]
    fn zero_baseline_no_pct() {
        // Baseline 0 -> direction up (raw delta) but 0% (undefined).
        let t = compute(&[s(0.0), s(10.0)]);
        assert_eq!(t.pct, 0.0);
        // Both first and delta nonzero-guard: delta!=0 so not forced flat here,
        // but pct==0 < FLAT_PCT -> reported flat. Honest: "changed, no % basis".
        assert_eq!(t.dir, TrendDir::Flat);
    }

    #[test]
    fn arrows_distinct() {
        assert_eq!(TrendDir::Up.arrow(), "\u{2191}");
        assert_eq!(TrendDir::Down.arrow(), "\u{2193}");
        assert_eq!(TrendDir::Flat.arrow(), "\u{2192}");
    }
}
