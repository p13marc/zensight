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
use crate::view::theme;

/// Render the sysinfo host specialized view.
pub fn sysinfo_host_view(state: &DeviceDetailState) -> Element<'_, Message> {
    let header = render_header(state);
    let system_overview = render_system_overview(state);
    let cpu_section = render_cpu_section(state);
    let memory_section = render_memory_section(state);
    let disk_section = render_disk_section(state);
    let network_section = render_network_section(state);

    let mut content = column![
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

    // Linux-specific sections (only show if data is present)
    if has_cpu_times(state) {
        content = content.push(rule::horizontal(1));
        content = content.push(render_cpu_times_section(state));
    }

    if has_disk_io(state) {
        content = content.push(rule::horizontal(1));
        content = content.push(render_disk_io_section(state));
    }

    if has_temperatures(state) {
        content = content.push(rule::horizontal(1));
        content = content.push(render_temperatures_section(state));
    }

    if has_tcp_states(state) {
        content = content.push(rule::horizontal(1));
        content = content.push(render_tcp_states_section(state));
    }

    if has_processes(state) {
        content = content.push(rule::horizontal(1));
        content = content.push(render_processes_section(state));
    }

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

    let os_text = text(os_info).size(14).style(|t: &Theme| text::Style {
        color: Some(theme::colors(t).text_muted()),
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
                text(uptime_str).size(12).style(|t: &Theme| text::Style {
                    color: Some(theme::colors(t).success()),
                })
            ]
            .spacing(8)
            .into(),
        );
    }

    // Load average - bridge publishes with "period" label (1m, 5m, 15m)
    // Look for metrics with period labels
    let load1 = get_metric_with_label(state, "system/load", "period", "1m");
    let load5 = get_metric_with_label(state, "system/load", "period", "5m");
    let load15 = get_metric_with_label(state, "system/load", "period", "15m");

    if load1.is_some() || load5.is_some() || load15.is_some() {
        let load_str = format!(
            "{:.2} {:.2} {:.2}",
            load1.unwrap_or(0.0),
            load5.unwrap_or(0.0),
            load15.unwrap_or(0.0)
        );

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
                .style(|t: &Theme| text::Style {
                    color: Some(theme::colors(t).text_muted()),
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

    // Count cores from available metrics
    let core_count = (0..128)
        .filter(|i| get_metric_value(state, &format!("cpu/{}/usage", i)).is_some())
        .count();

    if core_count > 0 {
        cpu_content = cpu_content.push(text(format!("{} cores", core_count)).size(12).style(
            |t: &Theme| text::Style {
                color: Some(theme::colors(t).text_muted()),
            },
        ));
    }

    // Per-core usage (bridge publishes as cpu/{N}/usage)
    let mut core_items: Vec<Element<'_, Message>> = Vec::new();
    for i in 0..128 {
        let usage_metric = format!("cpu/{}/usage", i);
        let freq_metric = format!("cpu/{}/frequency", i);

        if let Some(core_usage) = get_metric_value(state, &usage_metric) {
            let freq = get_metric_value(state, &freq_metric);
            let label = if let Some(mhz) = freq {
                format!("Core {} ({:.0} MHz)", i, mhz)
            } else {
                format!("Core {}", i)
            };

            let mini_gauge = Gauge::percentage(core_usage, label).with_width(140.0);
            core_items.push(mini_gauge.view());
        }
    }

    if !core_items.is_empty() {
        cpu_content = cpu_content.push(text("Per-Core Usage").size(12));
        // Arrange in rows of 4
        let mut core_rows = Column::new().spacing(5);
        let mut current_row = Row::new().spacing(15);
        let mut count = 0;
        for gauge in core_items {
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

    if let (Some(used), Some(total)) = (swap_used, swap_total)
        && total > 0.0
    {
        let used_gb = used / 1_073_741_824.0;
        let total_gb = total / 1_073_741_824.0;

        let progress = ProgressBar::new(used_gb, total_gb, "Swap", "GB");
        mem_content = mem_content.push(progress.view());
    }

    // Memory available
    if let Some(available) = get_metric_value(state, "memory/available") {
        let available_gb = available / 1_073_741_824.0;
        mem_content = mem_content.push(
            text(format!("Available: {:.1} GB", available_gb))
                .size(11)
                .style(|t: &Theme| text::Style {
                    color: Some(theme::colors(t).text_muted()),
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
            |t: &Theme| text::Style {
                color: Some(theme::colors(t).text_muted()),
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
        let rx_rate_key = format!("network/{}/rx_rate", iface);
        let tx_rate_key = format!("network/{}/tx_rate", iface);

        let rx = get_metric_value(state, &rx_key).unwrap_or(0.0);
        let tx = get_metric_value(state, &tx_key).unwrap_or(0.0);
        let rx_rate = get_metric_value(state, &rx_rate_key);
        let tx_rate = get_metric_value(state, &tx_rate_key);

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

        let mut iface_row = row![
            status_led.view(),
            text(format!("rx: {}", rx_str)).size(11),
            text(format!("tx: {}", tx_str)).size(11),
        ]
        .spacing(20)
        .align_y(Alignment::Center);

        // Add rates if available
        if let (Some(rx_r), Some(tx_r)) = (rx_rate, tx_rate) {
            iface_row = iface_row.push(
                text(format!(
                    "({}/s / {}/s)",
                    format_bytes(rx_r),
                    format_bytes(tx_r)
                ))
                .size(10)
                .style(|t: &Theme| text::Style {
                    color: Some(theme::colors(t).text_muted()),
                }),
            );
        }

        net_content = net_content.push(iface_row);
    }

    if iface_count == 0 {
        net_content = net_content.push(text("No network metrics available").size(12).style(
            |t: &Theme| text::Style {
                color: Some(theme::colors(t).text_muted()),
            },
        ));
    }

    column![title, net_content].spacing(10).into()
}

// =====================
// Linux-specific sections
// =====================

/// Check if CPU times data is available.
fn has_cpu_times(state: &DeviceDetailState) -> bool {
    state
        .metrics
        .keys()
        .any(|k| k.starts_with("cpu/times/") || k.contains("/times/"))
}

/// Render CPU times breakdown section (Linux-specific).
fn render_cpu_times_section(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = row![icons::cpu(IconSize::Medium), text("CPU Times").size(16)]
        .spacing(8)
        .align_y(Alignment::Center);

    let mut content = Column::new().spacing(10);

    // Look for aggregate CPU times
    let time_types = [
        ("user", "User"),
        ("nice", "Nice"),
        ("system", "System"),
        ("idle", "Idle"),
        ("iowait", "IO Wait"),
        ("irq", "IRQ"),
        ("softirq", "Soft IRQ"),
        ("steal", "Steal"),
    ];

    let mut bars: Vec<Element<'_, Message>> = Vec::new();

    for (key, label) in time_types {
        if let Some(value) = get_metric_value(state, &format!("cpu/times/{}", key)) {
            let bar = ProgressBar::new(value, 100.0, label, "%");
            bars.push(bar.view());
        }
    }

    if bars.is_empty() {
        content = content.push(text("Waiting for CPU time metrics...").size(12).style(
            |t: &Theme| text::Style {
                color: Some(theme::colors(t).text_muted()),
            },
        ));
    } else {
        for bar in bars {
            content = content.push(bar);
        }
    }

    column![title, content].spacing(10).into()
}

/// Check if disk I/O data is available.
fn has_disk_io(state: &DeviceDetailState) -> bool {
    state.metrics.keys().any(|k| k.contains("/io/read_"))
}

/// Render disk I/O section (Linux-specific).
fn render_disk_io_section(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = row![icons::disk(IconSize::Medium), text("Disk I/O").size(16)]
        .spacing(8)
        .align_y(Alignment::Center);

    let mut content = Column::new().spacing(8);

    // Find all devices with I/O stats
    let mut devices: Vec<String> = state
        .metrics
        .keys()
        .filter_map(|k| {
            if k.contains("/io/read_rate") {
                // Extract device name: disk/{device}/io/read_rate
                let parts: Vec<&str> = k.split('/').collect();
                if parts.len() >= 4 && parts[0] == "disk" {
                    return Some(parts[1].to_string());
                }
            }
            None
        })
        .collect();

    devices.sort();
    devices.dedup();

    for device in devices {
        let read_rate = get_metric_value(state, &format!("disk/{}/io/read_rate", device));
        let write_rate = get_metric_value(state, &format!("disk/{}/io/write_rate", device));
        let read_iops = get_metric_value(state, &format!("disk/{}/io/read_iops", device));
        let write_iops = get_metric_value(state, &format!("disk/{}/io/write_iops", device));

        let mut row_items: Vec<Element<'_, Message>> = vec![text(device).size(12).into()];

        if let (Some(rr), Some(wr)) = (read_rate, write_rate) {
            row_items.push(text(format!("R: {}/s", format_bytes(rr))).size(11).into());
            row_items.push(text(format!("W: {}/s", format_bytes(wr))).size(11).into());
        }

        if let (Some(ri), Some(wi)) = (read_iops, write_iops) {
            row_items.push(
                text(format!("{:.0}/{:.0} IOPS", ri, wi))
                    .size(11)
                    .style(|t: &Theme| text::Style {
                        color: Some(theme::colors(t).text_muted()),
                    })
                    .into(),
            );
        }

        content = content.push(
            Row::with_children(row_items)
                .spacing(20)
                .align_y(Alignment::Center),
        );
    }

    column![title, content].spacing(10).into()
}

/// Check if temperature data is available.
fn has_temperatures(state: &DeviceDetailState) -> bool {
    state.metrics.keys().any(|k| k.starts_with("sensors/"))
}

/// Render temperature sensors section (Linux-specific).
fn render_temperatures_section(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = row![text("Temperatures").size(16)]
        .spacing(8)
        .align_y(Alignment::Center);

    let mut content = Column::new().spacing(8);

    // Find all temperature sensors: sensors/{chip}/{label}/temp
    let mut sensors: Vec<(String, String, f64, Option<f64>)> = Vec::new();

    for (key, _point) in &state.metrics {
        if key.starts_with("sensors/") && key.ends_with("/temp") {
            let parts: Vec<&str> = key.split('/').collect();
            if parts.len() >= 4 {
                let chip = parts[1];
                let label = parts[2];
                if let Some(temp) = get_metric_value(state, key) {
                    let critical =
                        get_metric_value(state, &format!("sensors/{}/{}/critical", chip, label));
                    sensors.push((chip.to_string(), label.to_string(), temp, critical));
                }
            }
        }
    }

    sensors.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

    for (chip, label, temp, critical) in &sensors {
        let temp_text = text(format!("{:.1}°C", temp)).size(12);
        let styled_temp = if let Some(crit) = critical {
            if *temp >= *crit * 0.9 {
                temp_text.style(|t: &Theme| text::Style {
                    color: Some(theme::colors(t).danger()),
                })
            } else if *temp >= *crit * 0.75 {
                temp_text.style(|t: &Theme| text::Style {
                    color: Some(theme::colors(t).warning()),
                })
            } else {
                temp_text.style(|t: &Theme| text::Style {
                    color: Some(theme::colors(t).success()),
                })
            }
        } else {
            temp_text
        };

        let sensor_row = row![text(format!("{}/{}", chip, label)).size(11), styled_temp,]
            .spacing(15)
            .align_y(Alignment::Center);

        content = content.push(sensor_row);
    }

    if sensors.is_empty() {
        content = content.push(
            text("No temperature sensors found")
                .size(12)
                .style(|t: &Theme| text::Style {
                    color: Some(theme::colors(t).text_muted()),
                }),
        );
    }

    column![title, content].spacing(10).into()
}

/// Check if TCP states data is available.
fn has_tcp_states(state: &DeviceDetailState) -> bool {
    state.metrics.keys().any(|k| k.starts_with("tcp/"))
}

/// Render TCP connection states section (Linux-specific).
fn render_tcp_states_section(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = row![text("TCP Connections").size(16)]
        .spacing(8)
        .align_y(Alignment::Center);

    let mut content = Column::new().spacing(8);

    // Total connections
    if let Some(total) = get_metric_value(state, "tcp/total") {
        content = content.push(text(format!("Total: {:.0}", total)).size(12));
    }

    // State breakdown
    let states = [
        ("established", "Established"),
        ("listen", "Listen"),
        ("time_wait", "Time Wait"),
        ("close_wait", "Close Wait"),
        ("syn_sent", "SYN Sent"),
        ("syn_recv", "SYN Recv"),
        ("fin_wait1", "FIN Wait 1"),
        ("fin_wait2", "FIN Wait 2"),
        ("closing", "Closing"),
        ("last_ack", "Last ACK"),
        ("close", "Close"),
    ];

    let mut state_items: Vec<Element<'_, Message>> = Vec::new();

    for (key, label) in states {
        if let Some(count) = get_metric_value(state, &format!("tcp/{}", key)) {
            if count > 0.0 {
                state_items.push(text(format!("{}: {:.0}", label, count)).size(11).into());
            }
        }
    }

    if !state_items.is_empty() {
        // Arrange in rows of 4
        let mut rows = Column::new().spacing(5);
        let mut current_row = Row::new().spacing(20);
        let mut count = 0;
        for item in state_items {
            current_row = current_row.push(item);
            count += 1;
            if count % 4 == 0 {
                rows = rows.push(current_row);
                current_row = Row::new().spacing(20);
            }
        }
        if count % 4 != 0 {
            rows = rows.push(current_row);
        }
        content = content.push(rows);
    }

    column![title, content].spacing(10).into()
}

/// Check if process data is available.
fn has_processes(state: &DeviceDetailState) -> bool {
    state.metrics.keys().any(|k| k.starts_with("process/"))
}

/// Render top processes section.
fn render_processes_section(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = row![text("Top Processes").size(16)]
        .spacing(8)
        .align_y(Alignment::Center);

    let mut content = Column::new().spacing(5);

    // Header row
    content = content.push(
        row![
            text("Rank").size(10).width(40),
            text("Name").size(10).width(150),
            text("CPU %").size(10).width(60),
            text("Memory").size(10).width(80),
        ]
        .spacing(10),
    );

    // Find processes (process/{rank}/cpu)
    for rank in 1..=10 {
        let cpu_key = format!("process/{}/cpu", rank);
        let mem_key = format!("process/{}/memory", rank);

        if let Some(cpu) = get_metric_value(state, &cpu_key) {
            let memory = get_metric_value(state, &mem_key).unwrap_or(0.0);

            // Get process name from labels
            let name = state
                .metrics
                .get(&cpu_key)
                .and_then(|p| p.labels.get("name"))
                .map(|s| s.as_str())
                .unwrap_or("unknown");

            let proc_row = row![
                text(format!("{}", rank)).size(11).width(40),
                text(name).size(11).width(150),
                text(format!("{:.1}%", cpu)).size(11).width(60),
                text(format_bytes(memory)).size(11).width(80),
            ]
            .spacing(10)
            .align_y(Alignment::Center);

            content = content.push(proc_row);
        }
    }

    column![title, content].spacing(10).into()
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

/// Get a metric value with a specific label match.
fn get_metric_with_label(
    state: &DeviceDetailState,
    metric_prefix: &str,
    label_key: &str,
    label_value: &str,
) -> Option<f64> {
    state
        .metrics
        .iter()
        .find(|(k, point)| {
            k.as_str() == metric_prefix
                && point.labels.get(label_key).map(|v| v.as_str()) == Some(label_value)
        })
        .and_then(|(_, point)| match &point.value {
            TelemetryValue::Counter(v) => Some(*v as f64),
            TelemetryValue::Gauge(v) => Some(*v),
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

fn section_style(t: &Theme) -> container::Style {
    container::Style {
        background: Some(iced::Background::Color(theme::colors(t).card_background())),
        border: iced::Border {
            color: theme::colors(t).border(),
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
