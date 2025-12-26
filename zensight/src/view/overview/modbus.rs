//! Modbus overview - aggregates register counts and status across all PLCs.

use std::collections::HashMap;

use iced::widget::{column, row, text};
use iced::{Alignment, Element, Length, Theme};

use zensight_common::TelemetryValue;

use crate::message::{DeviceId, Message};
use crate::view::components::{StatusLed, StatusLedState};
use crate::view::dashboard::DeviceState;
use crate::view::theme;

/// Register type counts.
struct RegisterCounts {
    coils: usize,
    discrete_inputs: usize,
    holding_registers: usize,
    input_registers: usize,
    coils_on: usize,
}

/// Render the Modbus overview.
pub fn modbus_overview<'a>(devices: &HashMap<&DeviceId, &DeviceState>) -> Element<'a, Message> {
    if devices.is_empty() {
        return text("No Modbus devices available")
            .size(12)
            .style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).text_muted()),
            })
            .into();
    }

    // Count healthy/unhealthy devices
    let healthy = devices.values().filter(|d| d.is_healthy).count();
    let unhealthy = devices.len() - healthy;

    // Aggregate register counts
    let counts = count_registers(devices);

    // Summary row
    let summary_row = row![
        render_stat("PLCs", devices.len().to_string()),
        render_status_stat("Online", healthy, StatusLedState::Active),
        render_status_stat("Offline", unhealthy, StatusLedState::Inactive),
        render_stat("Coils", counts.coils.to_string()),
        render_stat("Coils ON", counts.coils_on.to_string()),
        render_stat("Discrete Inputs", counts.discrete_inputs.to_string()),
        render_stat("Holding Regs", counts.holding_registers.to_string()),
        render_stat("Input Regs", counts.input_registers.to_string()),
    ]
    .spacing(25)
    .align_y(Alignment::Center);

    // Total registers
    let total_registers =
        counts.coils + counts.discrete_inputs + counts.holding_registers + counts.input_registers;

    let total_row = text(format!(
        "Total: {} registers across {} devices",
        total_registers,
        devices.len()
    ))
    .size(11)
    .style(|t: &Theme| text::Style {
        color: Some(theme::colors(t).text_muted()),
    });

    column![summary_row, total_row]
        .spacing(15)
        .width(Length::Fill)
        .into()
}

/// Count registers across all devices.
fn count_registers(devices: &HashMap<&DeviceId, &DeviceState>) -> RegisterCounts {
    let mut counts = RegisterCounts {
        coils: 0,
        discrete_inputs: 0,
        holding_registers: 0,
        input_registers: 0,
        coils_on: 0,
    };

    for state in devices.values() {
        for (key, point) in &state.metrics {
            if key.contains("coil") {
                counts.coils += 1;
                // Check if coil is ON
                let is_on = match &point.value {
                    TelemetryValue::Boolean(b) => *b,
                    TelemetryValue::Counter(c) => *c != 0,
                    TelemetryValue::Gauge(g) => *g != 0.0,
                    _ => false,
                };
                if is_on {
                    counts.coils_on += 1;
                }
            } else if key.contains("discrete") {
                counts.discrete_inputs += 1;
            } else if key.contains("holding") {
                counts.holding_registers += 1;
            } else if key.contains("input") {
                counts.input_registers += 1;
            }
        }
    }

    counts
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

/// Render a status stat with LED.
fn render_status_stat<'a>(
    label: &'a str,
    count: usize,
    state: StatusLedState,
) -> Element<'a, Message> {
    let led = StatusLed::new(state).with_size(10.0);

    column![
        text(label).size(10).style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).text_muted()),
        }),
        row![led.view(), text(count.to_string()).size(16)]
            .spacing(6)
            .align_y(Alignment::Center)
    ]
    .spacing(2)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_modbus_overview_empty() {
        let devices: HashMap<&DeviceId, &DeviceState> = HashMap::new();
        let _view = modbus_overview(&devices);
    }
}
