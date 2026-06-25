//! Syslog overview - aggregates log severity distribution across all sources.

use std::collections::HashMap;

use iced::widget::{Column, Row, column, container, row, text};
use iced::{Alignment, Element, Length, Theme};

use zensight_common::TelemetryValue;

use crate::message::{DeviceId, Message};
use crate::view::dashboard::DeviceState;
use crate::view::theme;

/// Syslog severity levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Severity {
    Emergency = 0,
    Alert = 1,
    Critical = 2,
    Error = 3,
    Warning = 4,
    Notice = 5,
    Informational = 6,
    Debug = 7,
}

impl Severity {
    /// Parse the abbreviated/full severity name the logs sensor emits as the
    /// second metric-path segment and the `severity` label (e.g. `crit`, `err`,
    /// `warning`). This is the live contract — the old numeric-`severity` path
    /// the overview used to read no longer exists (#101).
    fn from_label(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "emerg" | "emergency" => Severity::Emergency,
            "alert" => Severity::Alert,
            "crit" | "critical" => Severity::Critical,
            "err" | "error" => Severity::Error,
            "warning" | "warn" => Severity::Warning,
            "notice" => Severity::Notice,
            "info" | "informational" => Severity::Informational,
            "debug" => Severity::Debug,
            _ => return None,
        })
    }

    fn label(&self) -> &'static str {
        match self {
            Severity::Emergency => "EMERG",
            Severity::Alert => "ALERT",
            Severity::Critical => "CRIT",
            Severity::Error => "ERR",
            Severity::Warning => "WARN",
            Severity::Notice => "NOTICE",
            Severity::Informational => "INFO",
            Severity::Debug => "DEBUG",
        }
    }

    fn color(&self) -> iced::Color {
        match self {
            Severity::Emergency | Severity::Alert => iced::Color::from_rgb(0.95, 0.2, 0.2),
            Severity::Critical | Severity::Error => iced::Color::from_rgb(0.9, 0.4, 0.3),
            Severity::Warning => iced::Color::from_rgb(0.9, 0.7, 0.2),
            Severity::Notice => iced::Color::from_rgb(0.4, 0.7, 0.9),
            Severity::Informational => iced::Color::from_rgb(0.5, 0.8, 0.5),
            Severity::Debug => iced::Color::from_rgb(0.6, 0.6, 0.6),
        }
    }

    fn all() -> &'static [Severity] {
        &[
            Severity::Emergency,
            Severity::Alert,
            Severity::Critical,
            Severity::Error,
            Severity::Warning,
            Severity::Notice,
            Severity::Informational,
            Severity::Debug,
        ]
    }
}

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
        return text("No syslog sources available")
            .size(12)
            .style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).text_muted()),
            })
            .into();
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
fn collect_messages(devices: &HashMap<&DeviceId, &DeviceState>) -> Vec<LogMessage> {
    let mut messages = Vec::new();

    for (device_id, state) in devices {
        for (key, point) in &state.metrics {
            // The logs sensor publishes each line under `<facility>/<severity>`
            // with the message as a Text value. Skip the derived `logs/*` rollup
            // counters and any non-text metric (#101 — the old `message/*` +
            // numeric-severity contract this read no longer exists).
            if key.starts_with("logs/") {
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
    let color = severity.color();

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
    critical.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

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
    let color = msg.severity.color();

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

        let id = DeviceId::new(Protocol::Syslog, "host1");
        let mut state = DeviceState::new(id.clone());
        // A live log line: key `<facility>/<severity>`, message as Text value.
        state.metrics.insert(
            "auth/err".to_string(),
            TelemetryPoint::new(
                "host1",
                Protocol::Syslog,
                "auth/err",
                TelemetryValue::Text("authentication failure".into()),
            )
            .with_label("app", "sshd"),
        );
        // A derived rollup counter — must be ignored.
        state.metrics.insert(
            "logs/errors_total".to_string(),
            TelemetryPoint::new(
                "host1",
                Protocol::Syslog,
                "logs/errors_total",
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
