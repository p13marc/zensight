//! Sysinfo host monitoring specialized view.
//!
//! Displays system metrics with gauges for CPU, memory, and disk usage.

use iced::widget::{Column, Row, column, container, row, rule, scrollable, text};
use iced::{Alignment, Element, Length, Theme};

use zensight_common::TelemetryValue;

use crate::message::Message;
use crate::view::components::{Gauge, ProgressBar, StatusLed, StatusLedState};
use crate::view::device::DeviceDetailState;
use crate::view::formatting::format_timestamp;
use crate::view::icons::{self, IconSize};

/// Render the sysinfo host specialized view.
pub fn sysinfo_host_view(state: &DeviceDetailState) -> Element<'_, Message> {
    let header = render_header(state);
    let system_overview = render_system_overview(state);
    let cpu_section = render_cpu_section(state);
    let memory_section = render_memory_section(state);
    let disk_section = render_disk_section(state);
    let network_section = render_network_section(state);

    let content = column![
        header,
        rule::horizontal(1),
        system_overview,
        rule::horizontal(1),
        cpu_section,
        rule::horizontal(1),
        memory_section,
        rule::horizontal(1),
        disk_section,
        rule::horizontal(1),
        network_section,
    ]
    .spacing(15)
    .padding(20);

    container(scrollable(content))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Render the header with back button and host info.
fn render_header(state: &DeviceDetailState) -> Element<'_, Message> {
    use iced::widget::button;

    let back_button = button(
        row![icons::arrow_left(IconSize::Medium), text("Back").size(14)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .on_press(Message::ClearSelection)
    .style(iced::widget::button::secondary);

    let protocol_icon = icons::protocol_icon(state.device_id.protocol, IconSize::Large);
    let host_name = text(&state.device_id.source).size(24);

    // Try to get OS info
    let os_info = get_metric_text(state, "system/os_name")
        .or_else(|| get_metric_text(state, "system/kernel_version"))
        .unwrap_or_else(|| "Unknown OS".to_string());

    let os_text = text(os_info).size(14).style(|_theme: &Theme| text::Style {
        color: Some(iced::Color::from_rgb(0.6, 0.6, 0.6)),
    });

    let metric_count = text(format!("{} metrics", state.metrics.len())).size(14);

    row![back_button, protocol_icon, host_name, os_text, metric_count]
        .spacing(15)
        .align_y(Alignment::Center)
        .into()
}

/// Render system overview with uptime and load.
fn render_system_overview(state: &DeviceDetailState) -> Element<'_, Message> {
    let mut info_items: Vec<Element<'_, Message>> = Vec::new();

    // Uptime
    if let Some(uptime) = get_metric_value(state, "system/uptime") {
        let uptime_secs = uptime as u64;
        let days = uptime_secs / 86400;
        let hours = (uptime_secs % 86400) / 3600;
        let mins = (uptime_secs % 3600) / 60;
        let uptime_str = format!("{}d {}h {}m", days, hours, mins);

        info_items.push(
            row![
                text("Uptime:").size(12),
                text(uptime_str)
                    .size(12)
                    .style(|_theme: &Theme| text::Style {
                        color: Some(iced::Color::from_rgb(0.4, 0.8, 0.4)),
                    })
            ]
            .spacing(8)
            .into(),
        );
    }

    // Load average
    if let Some(load1) = get_metric_value(state, "system/load_avg_1") {
        let load5 = get_metric_value(state, "system/load_avg_5").unwrap_or(0.0);
        let load15 = get_metric_value(state, "system/load_avg_15").unwrap_or(0.0);
        let load_str = format!("{:.2} {:.2} {:.2}", load1, load5, load15);

        info_items.push(
            row![text("Load:").size(12), text(load_str).size(12)]
                .spacing(8)
                .into(),
        );
    }

    // Boot time
    if let Some(boot_time) = get_metric_value(state, "system/boot_time") {
        let boot_str = format_timestamp(boot_time as i64);
        info_items.push(
            row![text("Boot:").size(12), text(boot_str).size(12)]
                .spacing(8)
                .into(),
        );
    }

    if info_items.is_empty() {
        info_items.push(
            text("Waiting for system metrics...")
                .size(12)
                .style(|_theme: &Theme| text::Style {
                    color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
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

/// Render CPU section with usage gauge and per-core breakdown.
fn render_cpu_section(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = row![icons::cpu(IconSize::Medium), text("CPU").size(16)]
        .spacing(8)
        .align_y(Alignment::Center);

    let mut cpu_content = Column::new().spacing(10);

    // Overall CPU usage
    if let Some(cpu_usage) = get_metric_value(state, "cpu/usage") {
        let gauge = Gauge::percentage(cpu_usage, "Usage").with_width(200.0);
        cpu_content = cpu_content.push(gauge.view());
    }

    // CPU count
    if let Some(cpu_count) = get_metric_value(state, "cpu/count") {
        cpu_content = cpu_content.push(text(format!("{} cores", cpu_count as u32)).size(12).style(
            |_theme: &Theme| text::Style {
                color: Some(iced::Color::from_rgb(0.6, 0.6, 0.6)),
            },
        ));
    }

    // Per-core usage (look for cpu/core/N/usage patterns)
    let mut core_gauges: Vec<Element<'_, Message>> = Vec::new();
    for i in 0..32 {
        // Check up to 32 cores
        let metric_name = format!("cpu/core/{}/usage", i);
        if let Some(core_usage) = get_metric_value(state, &metric_name) {
            let mini_gauge = Gauge::percentage(core_usage, format!("Core {}", i)).with_width(100.0);
            core_gauges.push(mini_gauge.view());
        }
    }

    if !core_gauges.is_empty() {
        cpu_content = cpu_content.push(text("Per-Core Usage").size(12));
        // Arrange in rows of 4
        let mut core_rows = Column::new().spacing(5);
        let mut current_row = Row::new().spacing(15);
        let mut count = 0;
        for gauge in core_gauges {
            current_row = current_row.push(gauge);
            count += 1;
            if count % 4 == 0 {
                core_rows = core_rows.push(current_row);
                current_row = Row::new().spacing(15);
            }
        }
        if count % 4 != 0 {
            core_rows = core_rows.push(current_row);
        }
        cpu_content = cpu_content.push(core_rows);
    }

    column![title, cpu_content].spacing(10).into()
}

/// Render memory section with usage bar.
fn render_memory_section(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = row![icons::memory(IconSize::Medium), text("Memory").size(16)]
        .spacing(8)
        .align_y(Alignment::Center);

    let mut mem_content = Column::new().spacing(10);

    // Memory usage
    let mem_used = get_metric_value(state, "memory/used");
    let mem_total = get_metric_value(state, "memory/total");

    if let (Some(used), Some(total)) = (mem_used, mem_total) {
        // Convert bytes to GB
        let used_gb = used / 1_073_741_824.0;
        let total_gb = total / 1_073_741_824.0;

        let progress = ProgressBar::new(used_gb, total_gb, "RAM", "GB");
        mem_content = mem_content.push(progress.view());
    }

    // Swap usage
    let swap_used = get_metric_value(state, "memory/swap_used");
    let swap_total = get_metric_value(state, "memory/swap_total");

    if let (Some(used), Some(total)) = (swap_used, swap_total) {
        if total > 0.0 {
            let used_gb = used / 1_073_741_824.0;
            let total_gb = total / 1_073_741_824.0;

            let progress = ProgressBar::new(used_gb, total_gb, "Swap", "GB");
            mem_content = mem_content.push(progress.view());
        }
    }

    // Memory available
    if let Some(available) = get_metric_value(state, "memory/available") {
        let available_gb = available / 1_073_741_824.0;
        mem_content = mem_content.push(
            text(format!("Available: {:.1} GB", available_gb))
                .size(11)
                .style(|_theme: &Theme| text::Style {
                    color: Some(iced::Color::from_rgb(0.6, 0.6, 0.6)),
                }),
        );
    }

    column![title, mem_content].spacing(10).into()
}

/// Render disk section with usage bars for each mount.
fn render_disk_section(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = row![icons::disk(IconSize::Medium), text("Disk").size(16)]
        .spacing(8)
        .align_y(Alignment::Center);

    let mut disk_content = Column::new().spacing(10);

    // Find all disk metrics (disk/<mount>/used, disk/<mount>/total)
    let mut mounts: Vec<String> = state
        .metrics
        .keys()
        .filter_map(|k| {
            if k.starts_with("disk/") && k.ends_with("/used") {
                let mount = k.strip_prefix("disk/")?.strip_suffix("/used")?;
                Some(mount.to_string())
            } else {
                None
            }
        })
        .collect();

    mounts.sort();
    mounts.dedup();

    let mut disk_count = 0;
    for mount in mounts {
        let used_key = format!("disk/{}/used", mount);
        let total_key = format!("disk/{}/total", mount);

        if let (Some(used), Some(total)) = (
            get_metric_value(state, &used_key),
            get_metric_value(state, &total_key),
        ) {
            // Convert bytes to GB
            let used_gb = used / 1_073_741_824.0;
            let total_gb = total / 1_073_741_824.0;

            let label = if mount.is_empty() { "/" } else { &mount };
            let progress = ProgressBar::new(used_gb, total_gb, label, "GB");
            disk_content = disk_content.push(progress.view());
            disk_count += 1;
        }
    }

    if disk_count == 0 {
        disk_content = disk_content.push(text("No disk metrics available").size(12).style(
            |_theme: &Theme| text::Style {
                color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
            },
        ));
    }

    column![title, disk_content].spacing(10).into()
}

/// Render network section with interface stats.
fn render_network_section(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = row![icons::network(IconSize::Medium), text("Network").size(16)]
        .spacing(8)
        .align_y(Alignment::Center);

    let mut net_content = Column::new().spacing(8);

    // Find all network interfaces (network/<iface>/rx_bytes, etc.)
    let mut interfaces: Vec<String> = state
        .metrics
        .keys()
        .filter_map(|k| {
            if k.starts_with("network/") && k.contains("/rx_bytes") {
                let iface = k.strip_prefix("network/")?.split('/').next()?;
                Some(iface.to_string())
            } else {
                None
            }
        })
        .collect();

    interfaces.sort();
    interfaces.dedup();

    let iface_count = interfaces.len();
    for iface in interfaces {
        let rx_key = format!("network/{}/rx_bytes", iface);
        let tx_key = format!("network/{}/tx_bytes", iface);

        let rx = get_metric_value(state, &rx_key).unwrap_or(0.0);
        let tx = get_metric_value(state, &tx_key).unwrap_or(0.0);

        // Check if interface is up
        let status_key = format!("network/{}/is_up", iface);
        let is_up = get_metric_bool(state, &status_key).unwrap_or(true);

        let status_led = StatusLed::new(if is_up {
            StatusLedState::Active
        } else {
            StatusLedState::Inactive
        })
        .with_label(&iface)
        .with_state_text();

        // Format bytes as human-readable
        let rx_str = format_bytes(rx);
        let tx_str = format_bytes(tx);

        let iface_row = row![
            status_led.view(),
            text(format!("rx: {}", rx_str)).size(11),
            text(format!("tx: {}", tx_str)).size(11),
        ]
        .spacing(20)
        .align_y(Alignment::Center);

        net_content = net_content.push(iface_row);
    }

    if iface_count == 0 {
        net_content = net_content.push(text("No network metrics available").size(12).style(
            |_theme: &Theme| text::Style {
                color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
            },
        ));
    }

    column![title, net_content].spacing(10).into()
}

// Helper functions

fn get_metric_value(state: &DeviceDetailState, metric: &str) -> Option<f64> {
    state
        .metrics
        .get(metric)
        .and_then(|point| match &point.value {
            TelemetryValue::Counter(v) => Some(*v as f64),
            TelemetryValue::Gauge(v) => Some(*v),
            _ => None,
        })
}

fn get_metric_text(state: &DeviceDetailState, metric: &str) -> Option<String> {
    state
        .metrics
        .get(metric)
        .and_then(|point| match &point.value {
            TelemetryValue::Text(s) => Some(s.clone()),
            _ => None,
        })
}

fn get_metric_bool(state: &DeviceDetailState, metric: &str) -> Option<bool> {
    state
        .metrics
        .get(metric)
        .and_then(|point| match &point.value {
            TelemetryValue::Boolean(b) => Some(*b),
            TelemetryValue::Gauge(v) => Some(*v != 0.0),
            TelemetryValue::Counter(v) => Some(*v != 0),
            _ => None,
        })
}

fn format_bytes(bytes: f64) -> String {
    if bytes >= 1_073_741_824.0 {
        format!("{:.1} GB", bytes / 1_073_741_824.0)
    } else if bytes >= 1_048_576.0 {
        format!("{:.1} MB", bytes / 1_048_576.0)
    } else if bytes >= 1024.0 {
        format!("{:.1} KB", bytes / 1024.0)
    } else {
        format!("{:.0} B", bytes)
    }
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
    fn test_format_bytes() {
        assert_eq!(format_bytes(500.0), "500 B");
        assert_eq!(format_bytes(1536.0), "1.5 KB");
        assert_eq!(format_bytes(1_572_864.0), "1.5 MB");
        assert_eq!(format_bytes(1_610_612_736.0), "1.5 GB");
    }

    #[test]
    fn test_sysinfo_view_renders() {
        let device_id = DeviceId::new(Protocol::Sysinfo, "server01");
        let state = DeviceDetailState::new(device_id);
        // Just verify it doesn't panic
        let _view = sysinfo_host_view(&state);
    }
}
