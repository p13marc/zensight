//! Sysinfo fleet overview - aggregates CPU, memory, disk across all hosts.

use std::collections::HashMap;

use iced::widget::{Column, column, container, row, text};
use iced::{Alignment, Element, Length, Theme};

use zensight_common::TelemetryValue;

use crate::message::{DeviceId, Message};
use crate::view::components::{StatusLed, StatusLedState};
use crate::view::dashboard::DeviceState;

/// Host resource summary.
struct HostSummary {
    name: String,
    cpu_usage: Option<f64>,
    memory_used: Option<f64>,
    memory_total: Option<f64>,
    is_healthy: bool,
}

/// Render the sysinfo fleet overview.
pub fn sysinfo_overview<'a>(devices: &HashMap<&DeviceId, &DeviceState>) -> Element<'a, Message> {
    if devices.is_empty() {
        return text("No sysinfo hosts available")
            .size(12)
            .style(|_theme: &Theme| text::Style {
                color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
            })
            .into();
    }

    // Collect host summaries
    let summaries: Vec<HostSummary> = devices
        .iter()
        .map(|(id, state)| extract_host_summary(id, state))
        .collect();

    // Calculate fleet averages
    let (avg_cpu, _cpu_count) = calculate_average(&summaries, |s| s.cpu_usage);
    let (avg_mem, _mem_count) = calculate_memory_average(&summaries);

    // Count hosts above thresholds
    let high_cpu_count = summaries
        .iter()
        .filter(|s| s.cpu_usage.map(|v| v > 80.0).unwrap_or(false))
        .count();
    let high_mem_count = summaries
        .iter()
        .filter(|s| {
            if let (Some(used), Some(total)) = (s.memory_used, s.memory_total) {
                total > 0.0 && (used / total) > 0.8
            } else {
                false
            }
        })
        .count();

    // Fleet summary row
    let summary_row = row![
        render_stat("Hosts", devices.len().to_string()),
        render_stat("Avg CPU", format!("{:.1}%", avg_cpu)),
        render_stat("Avg Memory", format!("{:.1}%", avg_mem)),
        render_alert_stat("High CPU (>80%)", high_cpu_count),
        render_alert_stat("High Memory (>80%)", high_mem_count),
    ]
    .spacing(30)
    .align_y(Alignment::Center);

    // Host resource bars (mini heatmap)
    let host_bars = render_host_bars(&summaries);

    column![summary_row, host_bars]
        .spacing(15)
        .width(Length::Fill)
        .into()
}

/// Extract a summary from a device state.
fn extract_host_summary(id: &DeviceId, state: &DeviceState) -> HostSummary {
    let cpu_usage = get_metric_value(state, "cpu/usage");
    let memory_used = get_metric_value(state, "memory/used");
    let memory_total = get_metric_value(state, "memory/total");

    HostSummary {
        name: id.source.clone(),
        cpu_usage,
        memory_used,
        memory_total,
        is_healthy: state.is_healthy,
    }
}

/// Get a numeric metric value from device state.
fn get_metric_value(state: &DeviceState, metric: &str) -> Option<f64> {
    state
        .metrics
        .get(metric)
        .and_then(|point| match &point.value {
            TelemetryValue::Counter(v) => Some(*v as f64),
            TelemetryValue::Gauge(v) => Some(*v),
            _ => None,
        })
}

/// Calculate average of a metric across hosts.
fn calculate_average<F>(summaries: &[HostSummary], extractor: F) -> (f64, usize)
where
    F: Fn(&HostSummary) -> Option<f64>,
{
    let values: Vec<f64> = summaries.iter().filter_map(|s| extractor(s)).collect();
    let count = values.len();
    let avg = if count > 0 {
        values.iter().sum::<f64>() / count as f64
    } else {
        0.0
    };
    (avg, count)
}

/// Calculate average memory percentage.
fn calculate_memory_average(summaries: &[HostSummary]) -> (f64, usize) {
    let percentages: Vec<f64> = summaries
        .iter()
        .filter_map(|s| {
            if let (Some(used), Some(total)) = (s.memory_used, s.memory_total) {
                if total > 0.0 {
                    Some((used / total) * 100.0)
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect();

    let count = percentages.len();
    let avg = if count > 0 {
        percentages.iter().sum::<f64>() / count as f64
    } else {
        0.0
    };
    (avg, count)
}

/// Render a stat label and value.
fn render_stat<'a>(label: &'a str, value: String) -> Element<'a, Message> {
    column![
        text(label).size(10).style(|_theme: &Theme| text::Style {
            color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
        }),
        text(value).size(16)
    ]
    .spacing(2)
    .into()
}

/// Render an alert stat (red if > 0).
fn render_alert_stat<'a>(label: &'a str, count: usize) -> Element<'a, Message> {
    let color = if count > 0 {
        iced::Color::from_rgb(0.9, 0.3, 0.3)
    } else {
        iced::Color::from_rgb(0.4, 0.8, 0.4)
    };

    column![
        text(label).size(10).style(|_theme: &Theme| text::Style {
            color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
        }),
        text(count.to_string())
            .size(16)
            .style(move |_theme: &Theme| text::Style { color: Some(color) })
    ]
    .spacing(2)
    .into()
}

/// Render mini resource bars for each host.
fn render_host_bars<'a>(summaries: &[HostSummary]) -> Element<'a, Message> {
    if summaries.is_empty() {
        return text("").into();
    }

    let title = text("Host Resources")
        .size(12)
        .style(|_theme: &Theme| text::Style {
            color: Some(iced::Color::from_rgb(0.6, 0.6, 0.6)),
        });

    // Sort by name
    let mut sorted: Vec<&HostSummary> = summaries.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));

    // Limit to first 10 hosts for display
    let display_hosts: Vec<_> = sorted.into_iter().take(10).collect();
    let remaining = summaries.len().saturating_sub(10);

    let host_rows: Vec<Element<'a, Message>> = display_hosts
        .into_iter()
        .map(|host| render_host_row(host))
        .collect();

    let mut content = Column::with_children(host_rows).spacing(4);

    if remaining > 0 {
        content = content.push(
            text(format!("... and {} more hosts", remaining))
                .size(10)
                .style(|_theme: &Theme| text::Style {
                    color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
                }),
        );
    }

    column![title, content].spacing(8).into()
}

/// Render a single host row with mini gauges.
fn render_host_row<'a>(host: &HostSummary) -> Element<'a, Message> {
    let status = StatusLed::new(if host.is_healthy {
        StatusLedState::Active
    } else {
        StatusLedState::Warning
    })
    .with_size(8.0);

    let name = text(truncate_name(&host.name, 15)).size(11);

    let cpu_bar = render_mini_bar(host.cpu_usage, "CPU");
    let mem_pct = host
        .memory_used
        .zip(host.memory_total)
        .map(|(used, total)| {
            if total > 0.0 {
                (used / total) * 100.0
            } else {
                0.0
            }
        });
    let mem_bar = render_mini_bar(mem_pct, "Mem");

    row![
        status.view(),
        container(name).width(Length::Fixed(100.0)),
        cpu_bar,
        mem_bar,
    ]
    .spacing(10)
    .align_y(Alignment::Center)
    .into()
}

/// Render a mini bar for a percentage value.
fn render_mini_bar<'a>(value: Option<f64>, _label: &str) -> Element<'a, Message> {
    let pct = value.unwrap_or(0.0).clamp(0.0, 100.0);
    let width: f32 = 60.0;
    let filled = width * (pct as f32) / 100.0;

    let color = if pct > 90.0 {
        iced::Color::from_rgb(0.9, 0.2, 0.2)
    } else if pct > 80.0 {
        iced::Color::from_rgb(0.9, 0.7, 0.2)
    } else {
        iced::Color::from_rgb(0.3, 0.7, 0.4)
    };

    let filled_bar = container(text(""))
        .width(Length::Fixed(filled))
        .height(Length::Fixed(8.0))
        .style(move |_theme: &Theme| container::Style {
            background: Some(iced::Background::Color(color)),
            ..Default::default()
        });

    let empty_bar = container(text(""))
        .width(Length::Fixed(width - filled))
        .height(Length::Fixed(8.0))
        .style(|_theme: &Theme| container::Style {
            background: Some(iced::Background::Color(iced::Color::from_rgb(
                0.2, 0.2, 0.2,
            ))),
            ..Default::default()
        });

    let bar = container(row![filled_bar, empty_bar]).style(|_theme: &Theme| container::Style {
        border: iced::Border {
            color: iced::Color::from_rgb(0.3, 0.3, 0.3),
            width: 1.0,
            radius: 2.0.into(),
        },
        ..Default::default()
    });

    let value_text = text(format!("{:.0}%", pct)).size(9);

    row![bar, value_text]
        .spacing(4)
        .align_y(Alignment::Center)
        .into()
}

/// Truncate a name to a maximum length.
fn truncate_name(name: &str, max_len: usize) -> String {
    if name.len() > max_len {
        format!("{}...", &name[..max_len - 3])
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_name() {
        assert_eq!(truncate_name("short", 10), "short");
        assert_eq!(truncate_name("verylongname", 10), "verylon...");
    }

    #[test]
    fn test_calculate_average_empty() {
        let summaries: Vec<HostSummary> = vec![];
        let (avg, count) = calculate_average(&summaries, |s| s.cpu_usage);
        assert_eq!(avg, 0.0);
        assert_eq!(count, 0);
    }
}
