//! Syslog overview - aggregates log severity distribution across all sources.

use std::collections::HashMap;

use iced::widget::{Column, Row, column, container, row, text};
use iced::{Alignment, Element, Length, Theme};

use zensight_common::TelemetryValue;

use crate::message::{DeviceId, Message};
use crate::view::components::empty_state;
use crate::view::dashboard::DeviceState;
use crate::view::theme;

/// Syslog severity — the one canonical model (#557), re-exported under the name
/// this overview used. `from_label`/`label`/`all` are its methods; the bar color
/// is [`theme::severity_color`].
use zensight_common::LogSeverity as Severity;

/// Log message summary.
struct LogMessage {
    source: String,
    severity: Severity,
    app_name: String,
    message: String,
    timestamp: i64,
}

/// Render the syslog overview.
pub fn syslog_overview<'a>(devices: &HashMap<&DeviceId, &DeviceState>) -> Element<'a, Message> {
    if devices.is_empty() {
        return empty_state("No syslog sources available", None);
    }

    // Collect all messages
    let messages = collect_messages(devices);
    let total_messages = messages.len();

    // Count by severity
    let mut severity_counts: HashMap<Severity, usize> = HashMap::new();
    for msg in &messages {
        *severity_counts.entry(msg.severity).or_insert(0) += 1;
    }

    // Summary row
    let summary_row = row![
        render_stat("Sources", devices.len().to_string()),
        render_stat("Total Messages", total_messages.to_string()),
    ]
    .spacing(30)
    .align_y(Alignment::Center);

    // Severity distribution
    let severity_dist = render_severity_distribution(&severity_counts, total_messages);

    // Recent critical messages
    let critical_messages = render_critical_messages(messages);

    column![summary_row, severity_dist, critical_messages]
        .spacing(15)
        .width(Length::Fill)
        .into()
}

/// Collect all log messages from all devices.
///
/// **Deliberately hand-parsed — do not convert this to the registry's typed parse
/// direction (#475).** It reads a `<facility>/<severity>` metric with the line as a
/// `Text` value, and that is *not a registered logs subject*: `logs.toml` carries
/// only the `logs/*` rollup families. Since #358 the sensor serves log lines from
/// `@rpc/logs/events` and publishes no per-line telemetry at all — the only things
/// still emitting this shape are `demo.rs` and `mock.rs`. A typed parse would
/// therefore return `None` for every line and blank the demo Logs overview.
///
/// The shape is unregistered because it is legacy, not because the registry missed
/// it. Leaving it as a string split is correct; converting it would be the bug.
fn collect_messages(devices: &HashMap<&DeviceId, &DeviceState>) -> Vec<LogMessage> {
    let mut messages = Vec::new();

    for (device_id, state) in devices {
        for (key, point) in &state.metrics {
            // Skip the derived rollup counters (those *are* registered) and any
            // non-text metric (#101 — the old `message/*` + numeric-severity
            // contract this read no longer exists).
            //
            // "Registered subject" IS the discriminator, and now it is spelled
            // that way. The old `starts_with("logs/")` test only worked because
            // the rollup names redundantly repeated the producer name (#470);
            // with that gone, the string test would have silently let every
            // rollup through as a log line.
            if zensight_common::registry::logs::Subject::parse_metric(key).is_some() {
                continue;
            }
            let TelemetryValue::Text(message) = &point.value else {
                continue;
            };

            // Severity from the metric path's 2nd segment, falling back to the
            // `severity` label; entries that resolve to neither are skipped.
            let parts: Vec<&str> = key.split('/').collect();
            let severity = parts
                .get(1)
                .and_then(|s| Severity::from_label(s))
                .or_else(|| {
                    point
                        .labels
                        .get("severity")
                        .and_then(|s| Severity::from_label(s))
                });
            let Some(severity) = severity else {
                continue;
            };

            let app_name = point
                .labels
                .get("app")
                .or_else(|| point.labels.get("app_name"))
                .or_else(|| point.labels.get("program"))
                .cloned()
                .unwrap_or_else(|| "-".to_string());

            messages.push(LogMessage {
                source: device_id.source.clone(),
                severity,
                app_name,
                message: message.clone(),
                timestamp: point.timestamp,
            });
        }
    }

    messages
}

/// Render a stat label and value.
fn render_stat<'a>(label: &'a str, value: String) -> Element<'a, Message> {
    column![
        text(label).size(10).style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).text_muted()),
        }),
        text(value).size(16)
    ]
    .spacing(2)
    .into()
}

/// Render severity distribution as bars.
fn render_severity_distribution<'a>(
    counts: &HashMap<Severity, usize>,
    total: usize,
) -> Element<'a, Message> {
    let title = text("Severity Distribution")
        .size(12)
        .style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).text_muted()),
        });

    let bars: Vec<Element<'a, Message>> = Severity::all()
        .iter()
        .filter_map(|&sev| {
            let count = counts.get(&sev).copied().unwrap_or(0);
            // Show if count > 0 or if it's a critical severity
            if count > 0 || (sev as u8) <= (Severity::Warning as u8) {
                Some(render_severity_bar(sev, count, total))
            } else {
                None
            }
        })
        .collect();

    column![title, Row::with_children(bars).spacing(15)]
        .spacing(8)
        .into()
}

/// Render a single severity bar.
fn render_severity_bar<'a>(severity: Severity, count: usize, total: usize) -> Element<'a, Message> {
    let pct = if total > 0 {
        (count as f64 / total as f64) * 100.0
    } else {
        0.0
    };

    let bar_width = (pct * 2.0).clamp(2.0, 100.0) as f32;
    let color = theme::severity_color(severity);

    let bar = container(text(""))
        .width(Length::Fixed(bar_width))
        .height(Length::Fixed(16.0))
        .style(move |_theme: &Theme| container::Style {
            background: Some(iced::Background::Color(color)),
            border: iced::Border {
                radius: 2.0.into(),
                ..Default::default()
            },
            ..Default::default()
        });

    column![
        text(severity.label())
            .size(9)
            .style(move |_theme: &Theme| text::Style { color: Some(color) }),
        bar,
        text(count.to_string()).size(10)
    ]
    .spacing(2)
    .align_x(Alignment::Center)
    .into()
}

/// Render recent critical/emergency messages.
fn render_critical_messages<'a>(messages: Vec<LogMessage>) -> Element<'a, Message> {
    // Filter critical and above
    let mut critical: Vec<LogMessage> = messages
        .into_iter()
        .filter(|m| (m.severity as u8) <= (Severity::Error as u8))
        .collect();

    if critical.is_empty() {
        return text("No critical messages")
            .size(11)
            .style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).success()),
            })
            .into();
    }

    // Sort by timestamp descending
    critical.sort_by_key(|b| std::cmp::Reverse(b.timestamp));

    let count = critical.len();
    let title = text(format!("Recent Critical Messages ({})", count))
        .size(12)
        .style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).warning()),
        });

    let rows: Vec<Element<'a, Message>> =
        critical.into_iter().take(5).map(render_log_row).collect();

    column![title, Column::with_children(rows).spacing(4)]
        .spacing(8)
        .into()
}

/// Render a single log message row.
fn render_log_row<'a>(msg: LogMessage) -> Element<'a, Message> {
    let color = theme::severity_color(msg.severity);

    let severity_label = text(msg.severity.label())
        .size(10)
        .style(move |_theme: &Theme| text::Style { color: Some(color) });

    let source = text(msg.source).size(10).style(|t: &Theme| text::Style {
        color: Some(theme::colors(t).text_muted()),
    });

    let app = text(msg.app_name).size(10).style(|t: &Theme| text::Style {
        color: Some(theme::colors(t).primary()),
    });

    let message_text = if msg.message.len() > 60 {
        format!("{}...", &msg.message[..57])
    } else {
        msg.message
    };

    row![severity_label, source, app, text(message_text).size(10)]
        .spacing(10)
        .align_y(Alignment::Center)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_severity_from_label() {
        // The live contract: abbreviated + full severity names (#101).
        assert_eq!(Severity::from_label("emerg"), Some(Severity::Emergency));
        assert_eq!(Severity::from_label("err"), Some(Severity::Error));
        assert_eq!(Severity::from_label("error"), Some(Severity::Error));
        assert_eq!(Severity::from_label("WARNING"), Some(Severity::Warning));
        assert_eq!(Severity::from_label("debug"), Some(Severity::Debug));
        // Numeric strings (the old, no-longer-emitted form) are not severities.
        assert_eq!(Severity::from_label("3"), None);
        assert_eq!(Severity::from_label("nonsense"), None);
    }

    #[test]
    fn collect_messages_reads_live_facility_severity_contract() {
        use std::collections::HashMap;

        use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

        use crate::view::dashboard::DeviceState;

        let id = DeviceId::fixture(Protocol::Logs, "host1");
        let mut state = DeviceState::new(id.clone());
        // A live log line: key `<facility>/<severity>`, message as Text value.
        state.metrics.insert(
            "auth/err".to_string(),
            TelemetryPoint::new(
                "host1",
                Protocol::Logs,
                "auth/err",
                TelemetryValue::Text("authentication failure".into()),
            )
            .with_label("app", "sshd"),
        );
        // A derived rollup counter — must be ignored.
        state.metrics.insert(
            "errors_total".to_string(),
            TelemetryPoint::new(
                "host1",
                Protocol::Logs,
                "errors_total",
                TelemetryValue::Counter(5),
            ),
        );

        let mut devices: HashMap<&DeviceId, &DeviceState> = HashMap::new();
        devices.insert(&id, &state);
        let messages = collect_messages(&devices);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].severity, Severity::Error);
        assert_eq!(messages[0].app_name, "sshd");
        assert_eq!(messages[0].message, "authentication failure");
    }
}
