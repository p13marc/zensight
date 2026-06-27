//! Global cross-device metric search (Plan v3-04 §F, #27).
//!
//! "Find all `*queue*`" across every device → a flat, ranked result list the
//! user can click to jump to the owning device. The matcher is a pure function
//! over the `protocol/source/metric` path, unit-tested independently of the UI;
//! the view renders results into a panel.
//!
//! Matching is two-tier and case-insensitive: a **substring** hit always
//! outranks a **fuzzy** (order-preserving subsequence) hit, so typing a literal
//! fragment behaves exactly as before while typos / abbreviations
//! (`cpuse` → `cpu/usage`) still surface lower down. Results are ranked by score
//! (word-boundary and contiguity bonuses), then `protocol/source/metric` for a
//! stable display.

use std::sync::LazyLock;

use iced::widget::{Column, Id, button, column, container, row, scrollable, text, text_input};
use iced::{Element, Length};

use crate::message::{DeviceId, Message};
use crate::view::dashboard::DeviceState;
use crate::view::tokens::{font, space};

/// Text input id for the global search box (#27).
pub static GLOBAL_SEARCH_ID: LazyLock<Id> = LazyLock::new(|| Id::new("global-metric-search"));

/// Compact display string for a telemetry value (local to keep search pure of
/// the dashboard's private formatter).
fn value_label(value: &zensight_common::TelemetryValue) -> String {
    use crate::view::formatting::format_value;
    use zensight_common::TelemetryValue;
    match value {
        TelemetryValue::Counter(v) => format_value(*v as f64),
        TelemetryValue::Gauge(v) => format_value(*v),
        TelemetryValue::Boolean(b) => b.to_string(),
        TelemetryValue::Text(s) => s.clone(),
        TelemetryValue::Binary(b) => format!("{} bytes", b.len()),
    }
}

/// Max results shown (bounded — a broad query must not build a giant list).
pub const MAX_RESULTS: usize = 200;

/// State for the global metric search overlay (#27).
#[derive(Debug, Default)]
pub struct GlobalSearchState {
    /// Whether the search panel is open.
    pub open: bool,
    /// Current query text.
    pub query: String,
}

impl GlobalSearchState {
    /// Open the panel (optionally seeding the query).
    pub fn open(&mut self) {
        self.open = true;
    }

    /// Close the panel and clear the query.
    pub fn close(&mut self) {
        self.open = false;
        self.query.clear();
    }
}

/// One global-search hit: which device, which metric, and its current value.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    /// Owning device.
    pub device: DeviceId,
    /// Metric name.
    pub metric: String,
    /// Pre-formatted current value.
    pub value: String,
}

/// Characters that delimit a "word" in a metric path; a match right after one
/// (or at the very start) earns a word-boundary bonus.
const SEPARATORS: &[char] = &['/', '_', '-', '.', ' ', ':'];

/// Floor for any substring score, so every substring hit outranks every fuzzy
/// (subsequence) hit regardless of their per-character bonuses.
const SUBSTRING_BASE: i32 = 1000;

/// Score `needle` against `haystack` (both already lowercased). Returns `None`
/// when there is no match. A substring match scores in the [`SUBSTRING_BASE`]+
/// tier (earlier position + word boundary preferred); otherwise an
/// order-preserving subsequence ("fuzzy") match scores below that tier. Internal
/// whitespace in `needle` is ignored so multi-word queries match across
/// separators. Shared with the command palette (#28) so both rank identically.
pub(crate) fn match_score(haystack: &str, needle: &str) -> Option<i32> {
    let compact: String = needle.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.is_empty() {
        return None;
    }

    // Tier 1: substring — preserves the original behavior and always wins.
    if let Some(pos) = haystack.find(&compact) {
        let mut score = SUBSTRING_BASE - (pos.min(500) as i32);
        let at_boundary = pos == 0
            || haystack[..pos]
                .chars()
                .next_back()
                .is_some_and(is_separator);
        if at_boundary {
            score += 50;
        }
        return Some(score);
    }

    // Tier 2: fuzzy subsequence.
    fuzzy_score(haystack, &compact)
}

fn is_separator(c: char) -> bool {
    SEPARATORS.contains(&c)
}

/// Greedy order-preserving subsequence score. `None` if `needle` is not a
/// subsequence of `haystack`. Higher is better: word-boundary starts and
/// contiguous runs are rewarded, gaps are penalized. Both inputs lowercased.
fn fuzzy_score(haystack: &str, needle: &str) -> Option<i32> {
    let h: Vec<char> = haystack.chars().collect();
    let mut score = 0i32;
    let mut h_idx = 0usize;
    let mut prev: Option<usize> = None;
    for nc in needle.chars() {
        // Advance to the next occurrence of `nc`.
        while h_idx < h.len() && h[h_idx] != nc {
            h_idx += 1;
        }
        if h_idx >= h.len() {
            return None;
        }
        let idx = h_idx;
        let mut bonus = 0i32;
        if idx == 0 || is_separator(h[idx - 1]) {
            bonus += 10; // start of a path segment / word
        }
        match prev {
            Some(p) if idx == p + 1 => bonus += 8, // contiguous run
            Some(p) => bonus -= ((idx - p - 1) as i32).min(10), // gap penalty
            None => bonus -= (idx as i32).min(10), // leading gap
        }
        score += 10 + bonus;
        prev = Some(idx);
        h_idx += 1;
    }
    Some(score)
}

/// Search all devices' metrics for `query` over the `protocol/source/metric`
/// path (see [`match_score`] for the two-tier substring/fuzzy ranking). Returns
/// up to [`MAX_RESULTS`] hits, ranked by score then `protocol/source/metric` for
/// a stable display. Empty/whitespace query ⇒ no results. Pure — the unit of
/// testing for search.
pub fn search<'a>(devices: impl Iterator<Item = &'a DeviceState>, query: &str) -> Vec<SearchHit> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(i32, SearchHit)> = Vec::new();
    for device in devices {
        let proto = device.id.protocol.to_string().to_lowercase();
        let source = device.id.source.to_lowercase();
        for (metric, point) in &device.metrics {
            let path = format!("{proto}/{source}/{}", metric.to_lowercase());
            if let Some(score) = match_score(&path, &q) {
                scored.push((
                    score,
                    SearchHit {
                        device: device.id.clone(),
                        metric: metric.clone(),
                        value: value_label(&point.value),
                    },
                ));
            }
        }
    }
    // Rank by score (desc), then by path for a stable, deterministic order.
    scored.sort_by(|(sa, a), (sb, b)| {
        sb.cmp(sa).then_with(|| {
            (a.device.protocol, &a.device.source, &a.metric).cmp(&(
                b.device.protocol,
                &b.device.source,
                &b.metric,
            ))
        })
    });
    scored.truncate(MAX_RESULTS);
    scored.into_iter().map(|(_, hit)| hit).collect()
}

/// Render the global search panel: an input + a results list. Clicking a result
/// selects its device. Built from a precomputed `hits` slice so the view stays
/// pure of the device map's lifetime.
pub fn global_search_panel<'a>(
    state: &'a GlobalSearchState,
    hits: Vec<SearchHit>,
) -> Element<'a, Message> {
    let input = text_input("Search metrics across all devices (fuzzy)…", &state.query)
        .id(GLOBAL_SEARCH_ID.clone())
        .on_input(Message::SetGlobalSearch)
        .on_submit(Message::SetGlobalSearch(state.query.clone()))
        .padding(space::SM)
        .size(font::BODY);

    let header = row![
        text("Global Metric Search").size(font::SECTION),
        container(text("")).width(Length::Fill),
        button(text("Close").size(font::CAPTION))
            .on_press(Message::CloseGlobalSearch)
            .padding([space::XS, space::SM])
            .style(iced::widget::button::secondary),
    ]
    .align_y(iced::Alignment::Center)
    .spacing(space::SM);

    let count = text(if state.query.trim().is_empty() {
        "Type to search".to_string()
    } else {
        format!("{} result(s)", hits.len())
    })
    .size(font::CAPTION);

    let mut list = Column::new().spacing(2);
    for hit in hits {
        let label = format!(
            "{}/{} · {} = {}",
            hit.device.protocol, hit.device.source, hit.metric, hit.value
        );
        list = list.push(
            button(text(label).size(font::CAPTION))
                .on_press(Message::SelectDevice(hit.device))
                .width(Length::Fill)
                .padding([space::XS, space::SM])
                .style(iced::widget::button::text),
        );
    }

    container(
        column![
            header,
            input,
            count,
            scrollable(list).height(Length::Fixed(360.0))
        ]
        .spacing(space::SM)
        .padding(space::MD),
    )
    .width(Length::Fixed(520.0))
    .style(iced::widget::container::rounded_box)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

    fn dev(source: &str, proto: Protocol, metrics: &[(&str, f64)]) -> DeviceState {
        let id = DeviceId {
            protocol: proto,
            source: source.to_string(),
        };
        let mut d = DeviceState::new(id.clone());
        for (m, v) in metrics {
            d.metrics.insert(
                m.to_string(),
                TelemetryPoint {
                    timestamp: 0,
                    source: source.to_string(),
                    protocol: proto,
                    metric: m.to_string(),
                    value: TelemetryValue::Gauge(*v),
                    labels: HashMap::new(),
                },
            );
        }
        d
    }

    #[test]
    fn empty_query_no_results() {
        let d = dev("r1", Protocol::Snmp, &[("cpu", 1.0)]);
        assert!(search([&d].into_iter(), "").is_empty());
        assert!(search([&d].into_iter(), "   ").is_empty());
    }

    #[test]
    fn matches_metric_substring_case_insensitive() {
        let a = dev("r1", Protocol::Snmp, &[("queue/depth", 5.0), ("cpu", 1.0)]);
        let b = dev("r2", Protocol::Modbus, &[("input/queue_len", 9.0)]);
        let hits = search([&a, &b].into_iter(), "QUEUE");
        assert_eq!(hits.len(), 2);
        // Sorted by (protocol, source, metric): snmp(r1) before modbus(r2)?
        // Protocol ordering is by the enum's Ord; just assert both present.
        let metrics: Vec<&str> = hits.iter().map(|h| h.metric.as_str()).collect();
        assert!(metrics.contains(&"queue/depth"));
        assert!(metrics.contains(&"input/queue_len"));
    }

    #[test]
    fn matches_on_source_and_protocol() {
        let a = dev("router01", Protocol::Snmp, &[("cpu", 1.0)]);
        // Query matches the source name.
        assert_eq!(search([&a].into_iter(), "router").len(), 1);
        // Query matches the protocol.
        assert_eq!(search([&a].into_iter(), "snmp").len(), 1);
        // Non-match.
        assert!(search([&a].into_iter(), "zzz").is_empty());
    }

    #[test]
    fn results_are_bounded() {
        // One device with > MAX_RESULTS matching metrics.
        let mut metrics: Vec<(String, f64)> = Vec::new();
        for i in 0..(MAX_RESULTS + 50) {
            metrics.push((format!("queue/{i}"), i as f64));
        }
        let id = DeviceId {
            protocol: Protocol::Snmp,
            source: "r1".to_string(),
        };
        let mut d = DeviceState::new(id);
        for (m, v) in &metrics {
            d.metrics.insert(
                m.clone(),
                TelemetryPoint {
                    timestamp: 0,
                    source: "r1".to_string(),
                    protocol: Protocol::Snmp,
                    metric: m.clone(),
                    value: TelemetryValue::Gauge(*v),
                    labels: HashMap::new(),
                },
            );
        }
        assert_eq!(search([&d].into_iter(), "queue").len(), MAX_RESULTS);
    }

    #[test]
    fn fuzzy_subsequence_matches_non_substring() {
        let d = dev("server01", Protocol::Sysinfo, &[("cpu/usage", 42.0)]);
        // "cpuse" is not a substring of "sysinfo/server01/cpu/usage" but is an
        // order-preserving subsequence.
        let hits = search([&d].into_iter(), "cpuse");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].metric, "cpu/usage");
    }

    #[test]
    fn non_subsequence_does_not_match() {
        let d = dev("server01", Protocol::Sysinfo, &[("cpu/usage", 42.0)]);
        // Wrong order — not a subsequence.
        assert!(search([&d].into_iter(), "egasu").is_empty());
    }

    #[test]
    fn substring_outranks_fuzzy() {
        // "mem" is a substring of mem/used; only a fuzzy subsequence of
        // "m...e...m" inside modem/eth. The substring hit must come first.
        let d = dev(
            "host",
            Protocol::Sysinfo,
            &[("modem/realm", 1.0), ("mem/used", 2.0)],
        );
        let hits = search([&d].into_iter(), "mem");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].metric, "mem/used");
    }

    #[test]
    fn whitespace_in_query_is_ignored() {
        let d = dev("host", Protocol::Sysinfo, &[("cpu/usage", 1.0)]);
        // Internal spaces are stripped to "cpuusage", which matches the path
        // "…/cpu/usage" as a fuzzy subsequence (the '/' blocks a substring).
        assert_eq!(search([&d].into_iter(), "cpu usage").len(), 1);
    }

    #[test]
    fn word_boundary_match_ranks_above_mid_word() {
        // Query "load" — boundary hit (load/avg) should outrank a mid-segment
        // fuzzy/substring hit (payload).
        let d = dev(
            "host",
            Protocol::Sysinfo,
            &[("payload/bytes", 1.0), ("load/avg", 2.0)],
        );
        let hits = search([&d].into_iter(), "load");
        assert_eq!(hits[0].metric, "load/avg");
    }
}
