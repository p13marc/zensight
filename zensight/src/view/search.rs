//! Global cross-device metric search (Plan v3-04 §F, #27).
//!
//! "Find all `*queue*`" across every device → a flat, ranked result list the
//! user can click to jump to the owning device. The matcher is a pure function
//! (case-insensitive substring over `protocol/source/metric`), unit-tested
//! independently of the UI; the view renders results into a panel.

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

/// Search all devices' metrics for `query` (case-insensitive substring over the
/// `protocol/source/metric` path). Returns up to [`MAX_RESULTS`] hits, sorted by
/// device then metric for stable display. Empty/whitespace query ⇒ no results.
/// Pure — the unit of testing for search.
pub fn search<'a>(devices: impl Iterator<Item = &'a DeviceState>, query: &str) -> Vec<SearchHit> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<SearchHit> = Vec::new();
    for device in devices {
        let proto = device.id.protocol.to_string().to_lowercase();
        let source = device.id.source.to_lowercase();
        for (metric, point) in &device.metrics {
            let path = format!("{proto}/{source}/{}", metric.to_lowercase());
            if path.contains(&q) {
                hits.push(SearchHit {
                    device: device.id.clone(),
                    metric: metric.clone(),
                    value: value_label(&point.value),
                });
                if hits.len() >= MAX_RESULTS {
                    break;
                }
            }
        }
        if hits.len() >= MAX_RESULTS {
            break;
        }
    }
    hits.sort_by(|a, b| {
        (a.device.protocol, &a.device.source, &a.metric).cmp(&(
            b.device.protocol,
            &b.device.source,
            &b.metric,
        ))
    });
    hits
}

/// Render the global search panel: an input + a results list. Clicking a result
/// selects its device. Built from a precomputed `hits` slice so the view stays
/// pure of the device map's lifetime.
pub fn global_search_panel<'a>(
    state: &'a GlobalSearchState,
    hits: Vec<SearchHit>,
) -> Element<'a, Message> {
    let input = text_input("Search metrics across all devices…", &state.query)
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
}
