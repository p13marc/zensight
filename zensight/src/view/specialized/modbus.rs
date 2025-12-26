//! Modbus PLC specialized view.
//!
//! Displays register tables with live values, gauges for analog values,
//! and boolean indicators for discrete values.

use iced::widget::{Column, Row, column, container, row, rule, scrollable, text};
use iced::{Alignment, Element, Length, Theme};
use iced_anim::widget::button;

use zensight_common::TelemetryValue;

use crate::message::Message;
use crate::view::components::{StatusLed, StatusLedState};
use crate::view::device::DeviceDetailState;
use crate::view::icons::{self, IconSize};

/// Register type in Modbus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegisterType {
    Coil,
    DiscreteInput,
    HoldingRegister,
    InputRegister,
}

impl RegisterType {
    fn from_metric_path(path: &str) -> Option<Self> {
        if path.contains("coil") {
            Some(RegisterType::Coil)
        } else if path.contains("discrete") {
            Some(RegisterType::DiscreteInput)
        } else if path.contains("holding") {
            Some(RegisterType::HoldingRegister)
        } else if path.contains("input") {
            Some(RegisterType::InputRegister)
        } else {
            None
        }
    }
}

/// Parsed Modbus register.
#[derive(Debug, Clone)]
struct ModbusRegister {
    address: u16,
    register_type: RegisterType,
    name: String,
    value: RegisterValue,
    unit: Option<String>,
}

#[derive(Debug, Clone)]
enum RegisterValue {
    Boolean(bool),
    Integer(i64),
    Float(f64),
    Unknown,
}

/// Render the Modbus PLC specialized view.
pub fn modbus_plc_view(state: &DeviceDetailState) -> Element<'_, Message> {
    let header = render_header(state);
    let connection_info = render_connection_info(state);
    let register_sections = render_register_sections(state);

    let content = column![
        header,
        rule::horizontal(1),
        connection_info,
        rule::horizontal(1),
        register_sections,
    ]
    .spacing(15)
    .padding(20);

    container(scrollable(content))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Render the header with back button and device info.
fn render_header(state: &DeviceDetailState) -> Element<'_, Message> {
    let back_button = button(
        row![icons::arrow_left(IconSize::Medium), text("Back").size(14)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .on_press(Message::ClearSelection)
    .style(iced::widget::button::secondary);

    let protocol_icon = icons::protocol_icon(state.device_id.protocol, IconSize::Large);
    let device_name = text(&state.device_id.source).size(24);

    let metric_count = text(format!("{} registers", state.metrics.len())).size(14);

    row![back_button, protocol_icon, device_name, metric_count]
        .spacing(15)
        .align_y(Alignment::Center)
        .into()
}

/// Render connection information.
fn render_connection_info(state: &DeviceDetailState) -> Element<'_, Message> {
    let mut info_items: Vec<Element<'_, Message>> = Vec::new();

    // Try to get connection info from labels
    if let Some(point) = state.metrics.values().next() {
        if let Some(addr) = point.labels.get("address") {
            info_items.push(
                row![text("Address:").size(12), text(addr).size(12)]
                    .spacing(8)
                    .into(),
            );
        }

        if let Some(unit_id) = point.labels.get("unit_id") {
            info_items.push(
                row![text("Unit ID:").size(12), text(unit_id).size(12)]
                    .spacing(8)
                    .into(),
            );
        }

        if let Some(proto) = point.labels.get("modbus_type") {
            info_items.push(
                row![
                    text("Protocol:").size(12),
                    text(format!("Modbus {}", proto.to_uppercase())).size(12)
                ]
                .spacing(8)
                .into(),
            );
        }
    }

    if info_items.is_empty() {
        info_items.push(
            text("Modbus Device")
                .size(12)
                .style(|_theme: &Theme| text::Style {
                    color: Some(iced::Color::from_rgb(0.6, 0.6, 0.6)),
                })
                .into(),
        );
    }

    container(Row::with_children(info_items).spacing(30))
        .padding(10)
        .style(section_style)
        .width(Length::Fill)
        .into()
}

/// Render register sections grouped by type.
fn render_register_sections(state: &DeviceDetailState) -> Element<'_, Message> {
    let registers = parse_registers(state);

    // Group by register type (use owned values)
    let mut coils: Vec<ModbusRegister> = Vec::new();
    let mut discretes: Vec<ModbusRegister> = Vec::new();
    let mut holdings: Vec<ModbusRegister> = Vec::new();
    let mut inputs: Vec<ModbusRegister> = Vec::new();

    for reg in registers {
        match reg.register_type {
            RegisterType::Coil => coils.push(reg),
            RegisterType::DiscreteInput => discretes.push(reg),
            RegisterType::HoldingRegister => holdings.push(reg),
            RegisterType::InputRegister => inputs.push(reg),
        }
    }

    let is_empty =
        coils.is_empty() && discretes.is_empty() && holdings.is_empty() && inputs.is_empty();
    let mut sections = Column::new().spacing(15);

    if !coils.is_empty() {
        sections = sections.push(render_boolean_section("Coils", coils));
    }

    if !discretes.is_empty() {
        sections = sections.push(render_boolean_section("Discrete Inputs", discretes));
    }

    if !holdings.is_empty() {
        sections = sections.push(render_register_table("Holding Registers", holdings));
    }

    if !inputs.is_empty() {
        sections = sections.push(render_register_table("Input Registers", inputs));
    }

    if is_empty {
        sections = sections.push(text("No register data available").size(12).style(
            |_theme: &Theme| text::Style {
                color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
            },
        ));
    }

    sections.into()
}

/// Render a section for boolean registers (coils, discrete inputs).
fn render_boolean_section<'a>(
    title: &'static str,
    registers: Vec<ModbusRegister>,
) -> Element<'a, Message> {
    let title_row = row![icons::toggle(IconSize::Medium), text(title).size(16)]
        .spacing(8)
        .align_y(Alignment::Center);

    let mut led_grid: Vec<Element<'a, Message>> = Vec::new();

    for reg in registers.into_iter() {
        let is_on = match &reg.value {
            RegisterValue::Boolean(b) => *b,
            RegisterValue::Integer(i) => *i != 0,
            RegisterValue::Float(f) => *f != 0.0,
            RegisterValue::Unknown => false,
        };

        let label = if reg.name.is_empty() {
            format!("{}", reg.address)
        } else {
            reg.name.clone()
        };

        let led = StatusLed::new(if is_on {
            StatusLedState::Active
        } else {
            StatusLedState::Inactive
        })
        .with_label(label)
        .with_size(14.0);

        led_grid.push(container(led.view()).width(Length::Fixed(120.0)).into());
    }

    // Arrange in rows of 4
    let mut rows = Column::new().spacing(8);
    let mut current_row = Row::new().spacing(15);
    let mut count = 0;
    for led_element in led_grid {
        current_row = current_row.push(led_element);
        count += 1;
        if count % 4 == 0 {
            rows = rows.push(current_row);
            current_row = Row::new().spacing(15);
        }
    }
    if count % 4 != 0 {
        rows = rows.push(current_row);
    }

    column![title_row, rows].spacing(10).into()
}

/// Render a table for analog registers (holding, input).
fn render_register_table<'a>(
    title: &'static str,
    registers: Vec<ModbusRegister>,
) -> Element<'a, Message> {
    let title_row = row![icons::table(IconSize::Medium), text(title).size(16)]
        .spacing(8)
        .align_y(Alignment::Center);

    // Table header
    let header = container(
        row![
            text("Address").size(11).width(Length::Fixed(70.0)),
            text("Name").size(11).width(Length::Fill),
            text("Value").size(11).width(Length::Fixed(100.0)),
            text("Unit").size(11).width(Length::Fixed(60.0)),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    )
    .padding(8)
    .style(|_theme: &Theme| container::Style {
        background: Some(iced::Background::Color(iced::Color::from_rgb(
            0.18, 0.18, 0.2,
        ))),
        ..Default::default()
    });

    let mut table_rows = Column::new().spacing(2);

    for reg in registers.into_iter() {
        let addr_text = text(format!("{}", reg.address))
            .size(11)
            .width(Length::Fixed(70.0));

        let name_text = text(reg.name).size(11).width(Length::Fill);

        let value_str = match &reg.value {
            RegisterValue::Boolean(b) => if *b { "1" } else { "0" }.to_string(),
            RegisterValue::Integer(i) => format!("{}", i),
            RegisterValue::Float(f) => format!("{:.2}", f),
            RegisterValue::Unknown => "-".to_string(),
        };

        let value_text = text(value_str).size(11).width(Length::Fixed(100.0));

        let unit_str = reg.unit.unwrap_or_else(|| "-".to_string());
        let unit_text =
            text(unit_str)
                .size(11)
                .width(Length::Fixed(60.0))
                .style(|_theme: &Theme| text::Style {
                    color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
                });

        let row_content = row![addr_text, name_text, value_text, unit_text]
            .spacing(10)
            .align_y(Alignment::Center);

        let row_container =
            container(row_content)
                .padding(8)
                .width(Length::Fill)
                .style(|_theme: &Theme| container::Style {
                    background: Some(iced::Background::Color(iced::Color::from_rgb(
                        0.13, 0.13, 0.15,
                    ))),
                    border: iced::Border {
                        color: iced::Color::from_rgb(0.2, 0.2, 0.22),
                        width: 1.0,
                        radius: 2.0.into(),
                    },
                    ..Default::default()
                });

        table_rows = table_rows.push(row_container);
    }

    column![title_row, header, table_rows].spacing(10).into()
}

/// Parse Modbus registers from metrics.
fn parse_registers(state: &DeviceDetailState) -> Vec<ModbusRegister> {
    let mut registers = Vec::new();

    for (key, point) in &state.metrics {
        // Expect format: <type>/<address> or <type>/<address>/<name>
        let reg_type = match RegisterType::from_metric_path(key) {
            Some(t) => t,
            None => continue,
        };

        // Try to extract address from the metric path
        let address: u16 = key
            .split('/')
            .filter_map(|p| p.parse().ok())
            .next()
            .unwrap_or(0);

        // Get name from labels or last path component
        let name = point
            .labels
            .get("name")
            .cloned()
            .or_else(|| {
                let parts: Vec<_> = key.split('/').collect();
                if parts.len() > 2 {
                    Some(parts[2..].join("/"))
                } else {
                    None
                }
            })
            .unwrap_or_default();

        let value = match &point.value {
            TelemetryValue::Boolean(b) => RegisterValue::Boolean(*b),
            TelemetryValue::Counter(c) => RegisterValue::Integer(*c as i64),
            TelemetryValue::Gauge(g) => RegisterValue::Float(*g),
            TelemetryValue::Text(s) => s
                .parse::<f64>()
                .map(RegisterValue::Float)
                .or_else(|_| s.parse::<i64>().map(RegisterValue::Integer))
                .unwrap_or(RegisterValue::Unknown),
            _ => RegisterValue::Unknown,
        };

        let unit = point.labels.get("unit").cloned();

        registers.push(ModbusRegister {
            address,
            register_type: reg_type,
            name,
            value,
            unit,
        });
    }

    registers.sort_by_key(|r| (r.register_type as u8, r.address));
    registers
}

fn section_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(iced::Background::Color(iced::Color::from_rgb(
            0.12, 0.12, 0.14,
        ))),
        border: iced::Border {
            color: iced::Color::from_rgb(0.25, 0.25, 0.3),
            width: 1.0,
            radius: 6.0.into(),
        },
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::DeviceId;
    use zensight_common::Protocol;

    #[test]
    fn test_register_type_detection() {
        assert_eq!(
            RegisterType::from_metric_path("holding/40001"),
            Some(RegisterType::HoldingRegister)
        );
        assert_eq!(
            RegisterType::from_metric_path("coil/1"),
            Some(RegisterType::Coil)
        );
    }

    #[test]
    fn test_modbus_view_renders() {
        let device_id = DeviceId::new(Protocol::Modbus, "plc01");
        let state = DeviceDetailState::new(device_id);
        let _view = modbus_plc_view(&state);
    }
}
