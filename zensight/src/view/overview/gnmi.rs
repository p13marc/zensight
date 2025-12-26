//! gNMI overview - aggregates path counts and subscriptions across all targets.

use std::collections::HashMap;

use iced::widget::{column, row, text};
use iced::{Alignment, Element, Length, Theme};

use crate::message::{DeviceId, Message};
use crate::view::components::{StatusLed, StatusLedState};
use crate::view::dashboard::DeviceState;

/// Render the gNMI overview.
pub fn gnmi_overview<'a>(devices: &HashMap<&DeviceId, &DeviceState>) -> Element<'a, Message> {
    if devices.is_empty() {
        return text("No gNMI targets available")
            .size(12)
            .style(|_theme: &Theme| text::Style {
                color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
            })
            .into();
    }

    // Count healthy/unhealthy
    let healthy = devices.values().filter(|d| d.is_healthy).count();
    let unhealthy = devices.len() - healthy;

    // Count total paths and infer subscriptions
    let mut total_paths = 0;
    let mut subscription_prefixes: HashMap<String, usize> = HashMap::new();

    for state in devices.values() {
        total_paths += state.metrics.len();

        // Infer subscriptions from path prefixes
        for key in state.metrics.keys() {
            let parts: Vec<&str> = key.split('/').collect();
            if parts.len() >= 2 {
                let prefix = format!("/{}/{}", parts[0], parts[1]);
                *subscription_prefixes.entry(prefix).or_insert(0) += 1;
            }
        }
    }

    let unique_subscriptions = subscription_prefixes.len();

    // Summary row
    let summary_row = row![
        render_stat("Targets", devices.len().to_string()),
        render_status_stat("Streaming", healthy, StatusLedState::Active),
        render_status_stat("Stale", unhealthy, StatusLedState::Warning),
        render_stat("Total Paths", total_paths.to_string()),
        render_stat("Subscriptions", unique_subscriptions.to_string()),
    ]
    .spacing(25)
    .align_y(Alignment::Center);

    // Top subscriptions
    let top_subs = render_top_subscriptions(&subscription_prefixes);

    column![summary_row, top_subs]
        .spacing(15)
        .width(Length::Fill)
        .into()
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

/// Render a status stat with LED.
fn render_status_stat<'a>(
    label: &'a str,
    count: usize,
    state: StatusLedState,
) -> Element<'a, Message> {
    let led = StatusLed::new(state).with_size(10.0);

    column![
        text(label).size(10).style(|_theme: &Theme| text::Style {
            color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
        }),
        row![led.view(), text(count.to_string()).size(16)]
            .spacing(6)
            .align_y(Alignment::Center)
    ]
    .spacing(2)
    .into()
}

/// Render top subscriptions by path count.
fn render_top_subscriptions<'a>(subscriptions: &HashMap<String, usize>) -> Element<'a, Message> {
    if subscriptions.is_empty() {
        return text("No subscription data")
            .size(11)
            .style(|_theme: &Theme| text::Style {
                color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
            })
            .into();
    }

    let title = text("Top Subscriptions")
        .size(12)
        .style(|_theme: &Theme| text::Style {
            color: Some(iced::Color::from_rgb(0.6, 0.6, 0.6)),
        });

    let mut sorted: Vec<_> = subscriptions.iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(a.1));

    let rows: Vec<Element<'a, Message>> = sorted
        .iter()
        .take(5)
        .map(|(prefix, count)| {
            row![
                text("•").size(11).style(|_theme: &Theme| text::Style {
                    color: Some(iced::Color::from_rgb(0.4, 0.8, 0.4)),
                }),
                text(prefix.to_string())
                    .size(11)
                    .style(|_theme: &Theme| text::Style {
                        color: Some(iced::Color::from_rgb(0.6, 0.8, 0.9)),
                    }),
                text(format!("({} paths)", count))
                    .size(10)
                    .style(|_theme: &Theme| text::Style {
                        color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
                    }),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .into()
        })
        .collect();

    column![title, column(rows).spacing(4)].spacing(8).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gnmi_overview_empty() {
        let devices: HashMap<&DeviceId, &DeviceState> = HashMap::new();
        let _view = gnmi_overview(&devices);
    }
}
