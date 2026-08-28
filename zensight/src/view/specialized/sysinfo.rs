//! Sysinfo host monitoring specialized view.
//!
//! Displays system metrics with gauges for CPU, memory, and disk usage.

use std::collections::BTreeSet;

use iced::widget::{Column, Row, button, column, container, row, scrollable, text};
use iced::{Alignment, Element, Length, Theme};

use zensight_common::TelemetryValue;
/// The registry's parse direction for this producer (RFC 08 §1, issue #475).
/// `TelemetryPoint::metric` **is** the telemetry subject tail, verbatim, so a
/// metric name refines straight into a typed subject with its variables named.
use zensight_common::registry::sysinfo::Subject;

use crate::message::Message;
use crate::view::components::card;
use crate::view::components::{
    Gauge, ProgressBar, StatusLed, StatusLedState, empty_state, section_header,
};
use crate::view::device::DeviceDetailState;
use crate::view::formatting::format_timestamp;
use crate::view::icons::{self, IconSize};
use crate::view::specialized::sysinfo_detail::{PidVerdict, ProcessSort, pid_filter_verdict};
use crate::view::specialized::systemd_detail::unit_from_cgroup;
use crate::view::theme;
use crate::view::tokens::space;

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
        card(system_overview),
        card(cpu_section),
        card(memory_section),
        card(disk_section),
        card(network_section),
    ]
    .spacing(space::MD)
    .padding(space::LG);

    // Linux-specific sections (only show if data is present)
    if has_cpu_times(state) {
        content = content.push(card(render_cpu_times_section(state)));
    }

    if has_disk_io(state) {
        content = content.push(card(render_disk_io_section(state)));
    }

    if has_temperatures(state) {
        content = content.push(card(render_temperatures_section(state)));
    }
    if has_fans_or_power(state) {
        content = content.push(card(render_fans_power_section(state)));
    }

    if has_tcp_states(state) {
        content = content.push(card(render_tcp_states_section(state)));
    }

    if has_processes(state) {
        content = content.push(card(render_processes_section(state)));
    }

    // On-demand process explorer (#47) — the rich `@rpc/sysinfo/processes` table
    // (rss/vsz/threads/io/state/uid), always available for a sysinfo host since
    // it's pulled lazily rather than streamed.
    content = content.push(card(render_process_explorer(state)));

    // Saturation/pressure depth (#47) — only when the sensor publishes it.
    if has_prefix(state, "pressure/") {
        content = content.push(card(render_psi_section(state)));
    }
    // The same question as PSI, one level deeper: PSI says *how much* time was
    // lost to contention, the histograms say *how long an individual wait was*.
    // Pulled on demand (#99, `@rpc/sysinfo/latency`) — never streamed.
    content = content.push(card(render_latency_section(state)));
    if has_prefix(state, "cgroup/") {
        content = content.push(card(render_cgroup_section(state)));
    }
    if has_system_health(state) {
        content = content.push(card(render_system_health_section(state)));
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

    let mut header = row![back_button, protocol_icon, host_name, os_text, metric_count]
        .spacing(15)
        .align_y(Alignment::Center);
    // Host health score + state (#130) — the one number derived by the sensor
    // (#97 system/saturation_score + system/health_state) for at-a-glance triage.
    if let Some(score) = get_metric_value(state, "system/saturation_score") {
        let st = get_metric_text(state, "system/health_state").unwrap_or_default();
        let color = match st.as_str() {
            "crit" => theme::SEVERITY_CRITICAL,
            "warn" => theme::SEVERITY_WARNING,
            _ => theme::STATUS_ONLINE,
        };
        let label = if st.is_empty() {
            format!("health {score:.0}")
        } else {
            format!("health {score:.0} · {st}")
        };
        header = header.push(crate::view::components::badge(color, label));
    }
    header.into()
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

    // Load average - sensor publishes with "period" label (1m, 5m, 15m)
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
        info_items.push(empty_state("Waiting for system metrics...", None));
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

    // Overall CPU usage, with a trend sparkline from history (#44).
    if let Some(cpu_usage) = get_metric_value(state, "cpu/usage") {
        let gauge = Gauge::percentage(cpu_usage, "Usage").with_width(200.0);
        cpu_content = cpu_content.push(
            row![
                gauge.view(),
                super::metric_trend_and_alert(state, "cpu/usage")
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        );
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

    // Per-core usage (sensor publishes as cpu/{N}/usage)
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
        mem_content = mem_content.push(
            row![
                progress.view(),
                super::metric_trend_and_alert(state, "memory/used")
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        );
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

    // The mounts this host reports, from the registry (RFC 08 §1's parse
    // direction, #475): `disk/{mount}/used` yields its `mount` variable typed
    // and named, instead of a strip_prefix/strip_suffix pair that silently
    // stops matching if the subject ever moves.
    let mut mounts: Vec<String> = state
        .metrics
        .keys()
        .filter_map(|k| match Subject::parse_metric(k) {
            Some(Subject::DiskUsed { mount }) => Some(mount.to_string()),
            _ => None,
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
        disk_content = disk_content.push(empty_state("No disk metrics available", None));
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
    // `network/{iface}/rx_bytes`. Note the old `contains("/rx_bytes")` also
    // matched `network/tcp/...`-shaped keys by accident; the registry knows
    // `network/tcp/*` is a different subject family and will not hand it back
    // here (literal beats var, RFC 08 §1).
    let mut interfaces: Vec<String> = state
        .metrics
        .keys()
        .filter_map(|k| match Subject::parse_metric(k) {
            Some(Subject::NetworkRxBytes { iface }) => Some(iface.to_string()),
            _ => None,
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
        net_content = net_content.push(empty_state("No network metrics available", None));
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
        content = content.push(empty_state("Waiting for CPU time metrics...", None));
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
        .filter_map(|k| match Subject::parse_metric(k) {
            // `disk/{device}/io/read_rate` — the device was `parts[1]`.
            Some(Subject::DiskIoReadRate { device }) => Some(device.to_string()),
            _ => None,
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
///
/// Gated on a real `SensorsTemp` parse, not a `starts_with("sensors/")` prefix:
/// fans publish `sensors/{chip}/{label}/rpm` under the same prefix, so the
/// prefix form rendered an empty "No temperature sensors found" card on any host
/// running `collect.power` without `collect.temperatures` (#515).
fn has_temperatures(state: &DeviceDetailState) -> bool {
    state
        .metrics
        .keys()
        .any(|k| matches!(Subject::parse_metric(k), Some(Subject::SensorsTemp { .. })))
}

/// Whether the host publishes any of the `collect.power` families this panel
/// renders: hwmon fans, battery, or RAPL zones (#515).
///
/// `system/entropy_avail` is a deliberate part of this gate, and the coupling is
/// load-bearing rather than accidental. Fans, batteries and RAPL are each
/// hardware- or permission-dependent and legitimately empty on a normal server;
/// entropy is the only subject `map_power` publishes unconditionally whenever
/// `collect.power` is on, so it is the sole on-wire evidence that the power
/// collector *ran*. Without it, a fanless, batteryless host whose `energy_uj` is
/// root-only renders no panel at all — indistinguishable from
/// `collect.power: false`, which is precisely the confusion this panel exists to
/// remove. Entropy keeps rendering under System health; here it only opens the
/// card.
fn has_fans_or_power(state: &DeviceDetailState) -> bool {
    state.metrics.contains_key("system/entropy_avail")
        || state.metrics.keys().any(|k| {
            matches!(
                Subject::parse_metric(k),
                Some(
                    Subject::SensorsRpm { .. }
                        | Subject::BatteryCapacity { .. }
                        | Subject::BatteryStatus { .. }
                        | Subject::PowerRaplWatts { .. }
                )
            )
        })
}

/// Render temperature sensors section (Linux-specific).
fn render_temperatures_section(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = row![text("Temperatures").size(16)]
        .spacing(8)
        .align_y(Alignment::Center);

    let mut content = Column::new().spacing(8);

    // Find all temperature sensors: sensors/{chip}/{label}/temp
    let mut sensors: Vec<(String, String, f64, Option<f64>)> = Vec::new();

    for key in state.metrics.keys() {
        // `sensors/{chip}/{label}/temp` — two variables, both named by the
        // registry instead of read off `parts[1]`/`parts[2]`.
        if let Some(Subject::SensorsTemp { chip, label }) = Subject::parse_metric(key)
            && let Some(temp) = get_metric_value(state, key)
        {
            let critical = get_metric_value(state, &format!("sensors/{chip}/{label}/critical"));
            sensors.push((chip.to_string(), label.to_string(), temp, critical));
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
        content = content.push(empty_state("No temperature sensors found", None));
    }

    column![title, content].spacing(10).into()
}

/// The copy for "no RAPL watts", kept as a constant because it is the
/// load-bearing distinction this panel makes and a UI test pins it.
///
/// It names all three causes rather than picking one: nothing on the wire tells
/// them apart. `collect_rapl()` returns an empty `Vec` for a missing
/// `/sys/class/powercap` *and* for `EACCES` on `energy_uj`, silently and with no
/// log. The third is transient and would make a two-cause message an outright
/// lie: watts are a rate derived from an energy delta, so the collector emits
/// nothing until it holds two readings — a correctly-configured root sensor on
/// real RAPL hardware shows this note for its first poll interval (5s by
/// default).
const RAPL_UNAVAILABLE: &str = "No RAPL zones are reporting. Either this CPU exposes none, or \
                                the sensor cannot read them (/sys/class/powercap/*/energy_uj is \
                                root-only since CVE-2020-8694), or it has just started and needs \
                                two readings to derive a rate. This is not a reading of zero \
                                watts.";

/// Fans, battery and RAPL power — the `collect.power` families (#515).
///
/// Three renderings here are deliberate and easy to get wrong:
///
/// * **0 RPM is a reading, not a gap.** Laptops stop their fans at idle, and the
///   collector publishes the zero on purpose (it used to drop it, which made
///   "idle" look like "dead"). A fan pinned at 0 *under load* is the finding, so
///   the zero must render as plainly as any other speed.
/// * **"not available" is not "0 W".** RAPL watts are usually absent — see
///   [`RAPL_UNAVAILABLE`] — and a panel that showed 0 W there would be inventing
///   a measurement. Same doctrine as the eBPF latency panel's `available: false`.
/// * **A missing battery is not a fault.** Servers have none; the block simply
///   does not appear.
fn render_fans_power_section(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = row![text("Fans & power").size(16)].spacing(8);
    let mut col = Column::new().spacing(4).push(title);

    // ── Fans ───────────────────────────────────────────────────────────────
    let mut fans: Vec<(String, String, f64)> = Vec::new();
    for key in state.metrics.keys() {
        if let Some(Subject::SensorsRpm { chip, label }) = Subject::parse_metric(key)
            && let Some(rpm) = get_metric_value(state, key)
        {
            fans.push((chip.to_string(), label.to_string(), rpm));
        }
    }
    fans.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

    if fans.is_empty() {
        // Distinct from a fan reading 0: this host reports no fan at all.
        col = col.push(sub_label("Fans"));
        col = col.push(empty_state("No fans reported by hwmon", None));
    } else {
        col = col.push(sub_label("Fans"));
        for (chip, label, rpm) in &fans {
            col = col.push(
                row![
                    text(format!("{chip}/{label}"))
                        .size(12)
                        .width(Length::Fixed(220.0))
                        .style(|t: &Theme| text::Style {
                            color: Some(theme::colors(t).text_muted()),
                        }),
                    // No `{:.0}` styling games: 0 renders as "0 RPM".
                    text(format!("{rpm:.0} RPM")).size(12),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            );
        }
    }

    // ── Battery ────────────────────────────────────────────────────────────
    // Capacity and status are independently optional — `map_power` emits each
    // only when sysfs offered it — so a name may have one, the other, or both.
    // A `BTreeSet` dedups the name across the two key families and sorts in one
    // pass. No battery at all => no block: servers have none, and that is not a
    // fault worth a row.
    let mut batteries: BTreeSet<String> = BTreeSet::new();
    for key in state.metrics.keys() {
        if let Some(Subject::BatteryCapacity { name } | Subject::BatteryStatus { name }) =
            Subject::parse_metric(key)
        {
            batteries.insert(name.to_string());
        }
    }

    if !batteries.is_empty() {
        col = col.push(sub_label("Battery"));
        for name in &batteries {
            let capacity = get_metric_value(state, &format!("battery/{name}/capacity"));
            let status = get_metric_text(state, &format!("battery/{name}/status"));
            // Capacity and status are separate text widgets, not one joined
            // string: the row reads the same, and each stays independently
            // assertable.
            let mut battery_row = row![
                text(name.clone())
                    .size(12)
                    .width(Length::Fixed(220.0))
                    .style(|t: &Theme| text::Style {
                        color: Some(theme::colors(t).text_muted()),
                    }),
            ]
            .spacing(8)
            .align_y(Alignment::Center);
            if let Some(c) = capacity {
                battery_row = battery_row.push(text(format!("{c:.0}%")).size(12));
            }
            if let Some(s) = status {
                battery_row = battery_row.push(text(s).size(12));
            }
            col = col.push(battery_row);
        }
    }

    // ── Power (RAPL) ───────────────────────────────────────────────────────
    let mut zones: Vec<(String, String, f64)> = Vec::new();
    for key in state.metrics.keys() {
        if let Some(Subject::PowerRaplWatts { zone }) = Subject::parse_metric(key)
            && let Some(watts) = get_metric_value(state, key)
        {
            // The friendly name ("package-0") rides as a label; the key carries
            // the raw zone ("intel-rapl:0" — `sanitize_key` leaves colons
            // alone). Prefer the name, fall back to the zone, so the row is
            // meaningful even on the degraded path where labels are missing.
            let display = get_metric_label(state, key, "name").unwrap_or_else(|| zone.to_string());
            zones.push((zone.to_string(), display, watts));
        }
    }
    // Sort on the zone from the key: stable, and present regardless of labels.
    zones.sort_by(|a, b| a.0.cmp(&b.0));

    col = col.push(sub_label("Power (RAPL)"));
    if zones.is_empty() {
        col = col.push(empty_state(RAPL_UNAVAILABLE, None));
    } else {
        for (_, display, watts) in &zones {
            col = col.push(
                row![
                    text(display.clone())
                        .size(12)
                        .width(Length::Fixed(220.0))
                        .style(|t: &Theme| text::Style {
                            color: Some(theme::colors(t).text_muted()),
                        }),
                    text(format!("{watts:.1} W")).size(12),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            );
        }
    }

    col.into()
}

/// A sub-heading inside a card, for panels that group several families.
fn sub_label<'a>(label: &'a str) -> Element<'a, Message> {
    text(label).size(14).into()
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
        if let Some(count) = get_metric_value(state, &format!("tcp/{}", key))
            && count > 0.0
        {
            state_items.push(text(format!("{}: {:.0}", label, count)).size(11).into());
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

/// Whether any metric key starts with `prefix` (#47).
fn has_prefix(state: &DeviceDetailState, prefix: &str) -> bool {
    state.metrics.keys().any(|k| k.starts_with(prefix))
}

/// Whether the host publishes any of the system-health saturation counters (#47).
fn has_system_health(state: &DeviceDetailState) -> bool {
    const KEYS: &[&str] = &[
        "system/procs_running",
        "system/file_descriptors_used_percent",
        "system/context_switches_total",
        "system/processes_zombie",
        "system/entropy_avail",
    ];
    KEYS.iter().any(|k| state.metrics.contains_key(*k))
}

/// A `label: value` row (#47), formatted to `prec` decimals with an optional
/// unit suffix; renders "-" when the metric is absent.
fn kv_row<'a>(
    state: &DeviceDetailState,
    label: &str,
    metric: &str,
    unit: &str,
    prec: usize,
) -> Element<'a, Message> {
    let value = match get_metric_value(state, metric) {
        Some(v) => format!("{v:.prec$}{unit}"),
        None => "-".to_string(),
    };
    row![
        text(label.to_string())
            .size(12)
            .width(Length::Fixed(220.0))
            .style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).text_muted()),
            }),
        text(value).size(12),
    ]
    .spacing(8)
    .into()
}

/// PSI / pressure-stall card (#47): the canonical saturation signal.
fn render_psi_section(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = row![text("Pressure (PSI)").size(16)].spacing(8);
    let mut col = Column::new().spacing(4).push(title);
    for (res, label) in [("cpu", "CPU"), ("io", "I/O"), ("memory", "Memory")] {
        // some/avg10 is the headline; pair it with a trend sparkline.
        let some10 = format!("pressure/{res}/some_avg10");
        if get_metric_value(state, &some10).is_some() {
            col = col.push(
                row![
                    text(format!("{label} some avg10"))
                        .size(12)
                        .width(Length::Fixed(220.0))
                        .style(|t: &Theme| text::Style {
                            color: Some(theme::colors(t).text_muted()),
                        }),
                    text(format!(
                        "{:.1}%",
                        get_metric_value(state, &some10).unwrap_or(0.0)
                    ))
                    .size(12),
                    super::metric_sparkline(state, &some10),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            );
        }
        col = col
            .push(kv_row(
                state,
                &format!("{label} some avg300"),
                &format!("pressure/{res}/some_avg300"),
                "%",
                1,
            ))
            .push(kv_row(
                state,
                &format!("{label} full avg10"),
                &format!("pressure/{res}/full_avg10"),
                "%",
                1,
            ));
    }
    col.into()
}

/// cgroup-v2 throttling / OOM / memory card (#47).
fn render_cgroup_section(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = row![text("cgroup").size(16)].spacing(8);
    column![
        title,
        kv_row(
            state,
            "CPU throttled (count)",
            "cgroup/cpu/nr_throttled",
            "",
            0
        ),
        kv_row(
            state,
            "CPU throttled (usec)",
            "cgroup/cpu/throttled_usec",
            "",
            0
        ),
        kv_row(state, "memory current", "cgroup/memory/current", " B", 0),
        kv_row(state, "memory max", "cgroup/memory/max", " B", 0),
        kv_row(state, "memory used %", "cgroup/memory/used_percent", "%", 1),
        kv_row(
            state,
            "OOM kills (total)",
            "cgroup/memory/oom_kills_total",
            "",
            0
        ),
    ]
    .spacing(4)
    .into()
}

/// System-health / saturation-ceiling card (#47): FD exhaustion, runqueue, churn.
fn render_system_health_section(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = row![text("System health").size(16)].spacing(8);
    column![
        title,
        kv_row(
            state,
            "FD used %",
            "system/file_descriptors_used_percent",
            "%",
            1
        ),
        kv_row(state, "FDs used", "system/file_descriptors_used", "", 0),
        kv_row(state, "procs running", "system/procs_running", "", 0),
        kv_row(state, "procs blocked", "system/procs_blocked", "", 0),
        kv_row(state, "zombies", "system/processes_zombie", "", 0),
        kv_row(
            state,
            "ctx switches (total)",
            "system/context_switches_total",
            "",
            0
        ),
        kv_row(state, "forks (total)", "system/forks_total", "", 0),
        kv_row(state, "entropy avail", "system/entropy_avail", "", 0),
    ]
    .spacing(4)
    .into()
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

/// On-demand process explorer (#47): a sort toggle (CPU / Memory / I/O) that
/// fetches the rich `@rpc/sysinfo/processes` table and renders it. Distinct from the
/// streamed top-10 above — this carries rss/vsz/threads/io/state/uid and is
/// pulled lazily (the per-pid firehose is never streamed, principle P2).
fn render_process_explorer(state: &DeviceDetailState) -> Element<'_, Message> {
    let detail = &state.sysinfo_detail;
    let loading = detail.processes.is_loading();

    // One button per sort; the active sort is highlighted, and all are disabled
    // while a fetch is in flight.
    let sort_button = |sort: ProcessSort| {
        let active = detail.sort == sort;
        let label = format!("By {}", sort.label());
        let mut b = button(text(label).size(11)).padding([4, 10]);
        if !loading {
            b = b.on_press(Message::FetchSysinfoProcesses(sort));
        }
        if active {
            b = b.style(iced::widget::button::primary);
        } else {
            b = b.style(iced::widget::button::secondary);
        }
        b
    };
    let controls = row![
        sort_button(ProcessSort::Cpu),
        sort_button(ProcessSort::Mem),
        sort_button(ProcessSort::Io),
        text(if loading { "Fetching…" } else { "" })
            .size(11)
            .style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).text_muted()),
            }),
    ]
    .spacing(space::SM)
    .align_y(Alignment::Center);

    let mut col = column![section_header("Process Explorer", None), controls].spacing(space::SM);

    // Pid pivot banner (#313): the explorer was opened from a unit MainPID or a
    // socket owner — show the filter, its stale-generation verdict, and a way out.
    if let Some(f) = &detail.pid_filter {
        let clear = button(text("Clear").size(11))
            .padding([3, 9])
            .style(iced::widget::button::secondary)
            .on_press(Message::ClearSysinfoPidFilter);
        let verdict: Element<'_, Message> = match detail.processes.ready() {
            Some(procs) => match pid_filter_verdict(procs, f) {
                PidVerdict::Live => text("").size(11).into(),
                PidVerdict::Reused => text("pid reused by another process — the original exited")
                    .size(11)
                    .style(|t: &Theme| text::Style {
                        color: Some(theme::colors(t).warning()),
                    })
                    .into(),
                PidVerdict::Gone => text("not in the fetched table — exited (or below top-N)")
                    .size(11)
                    .style(|t: &Theme| text::Style {
                        color: Some(theme::colors(t).text_muted()),
                    })
                    .into(),
            },
            None => text("").size(11).into(),
        };
        col = col.push(
            row![
                text(format!("Filtered to pid {}", f.pid)).size(12),
                verdict,
                clear,
            ]
            .spacing(space::SM)
            .align_y(Alignment::Center),
        );
    }

    if let Some(err) = detail.processes.error() {
        col = col.push(empty_state(format!("Fetch failed: {err}"), None));
    } else if let Some(procs) = detail.processes.ready() {
        // Apply the pid pivot; a reused pid is NOT shown as a match (#313).
        let visible: Vec<&zensight_common::ProcessRecord> = match &detail.pid_filter {
            Some(f) => match pid_filter_verdict(procs, f) {
                PidVerdict::Live => procs.iter().filter(|p| p.pid == f.pid).collect(),
                PidVerdict::Reused | PidVerdict::Gone => Vec::new(),
            },
            None => procs.iter().collect(),
        };
        if visible.is_empty() {
            col = col.push(empty_state(
                if detail.pid_filter.is_some() {
                    "No matching live process."
                } else {
                    "No processes returned"
                },
                None,
            ));
        } else {
            let mut list = Column::new().spacing(3).push(
                row![
                    text("pid").size(10).width(70),
                    text("name").size(10).width(160),
                    text("user").size(10).width(90),
                    text("cpu%").size(10).width(60),
                    text("rss").size(10).width(90),
                    text("vsz").size(10).width(90),
                    text("thr").size(10).width(50),
                    text("state").size(10).width(70),
                    text("io r/w").size(10).width(140),
                    text("unit").size(10).width(150),
                    text("command").size(10).width(iced::Length::Fill),
                ]
                .spacing(8),
            );
            for p in visible.iter().take(200) {
                // cmdline arrives scrubbed + capped from the sensor (#302);
                // fall back to the bare name for kernel threads / stripped args.
                let command = if p.cmdline.is_empty() {
                    p.name.clone()
                } else {
                    p.cmdline.clone()
                };
                // Identity chip (#313): the process's cgroup resolves to a
                // systemd unit → pivot to that unit's drill-down. Non-unit
                // cgroups render as plain text (never a dead button).
                let unit_cell: Element<'_, Message> =
                    match p.cgroup.as_deref().and_then(unit_from_cgroup) {
                        Some(unit) => iced::widget::container(
                            button(text(unit.clone()).size(11))
                                .padding([2, 6])
                                .style(iced::widget::button::text)
                                .on_press(Message::PivotToUnit {
                                    host: state.device_id.source.clone(),
                                    unit,
                                }),
                        )
                        .width(150)
                        .into(),
                        None => text("—").size(11).width(150).into(),
                    };
                list = list.push(
                    row![
                        text(p.pid.to_string()).size(11).width(70),
                        text(p.name.clone()).size(11).width(160),
                        text(p.user.clone().unwrap_or_default()).size(11).width(90),
                        text(format!("{:.1}", p.cpu)).size(11).width(60),
                        text(format_bytes(p.rss as f64)).size(11).width(90),
                        text(format_bytes(p.vsz as f64)).size(11).width(90),
                        text(p.threads.map(|t| t.to_string()).unwrap_or_default())
                            .size(11)
                            .width(50),
                        text(p.state.clone()).size(11).width(70),
                        text(format!(
                            "{} / {}",
                            format_bytes(p.io_read as f64),
                            format_bytes(p.io_write as f64)
                        ))
                        .size(11)
                        .width(140),
                        unit_cell,
                        text(command).size(11).width(iced::Length::Fill),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                );
            }
            col = col
                .push(text(format!("{} processes", visible.len())).size(12))
                .push(list);
        }
    } else {
        col = col.push(empty_state(
            "Pick a sort to fetch the live process table.",
            None,
        ));
    }

    col.into()
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

/// Read a label off a metric's point. Labels ride with the point into
/// `state.metrics`, so this is a lookup, not a scan (contrast
/// [`get_metric_with_label`], which searches *by* a label's value).
fn get_metric_label(state: &DeviceDetailState, metric: &str, label: &str) -> Option<String> {
    state
        .metrics
        .get(metric)
        .and_then(|point| point.labels.get(label).cloned())
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

/// eBPF saturation histograms (#99): scheduler run-queue delay and block-I/O
/// latency, pulled from `@rpc/sysinfo/latency`.
///
/// Rendered as percentiles rather than a mean, because the tail *is* the
/// finding: a p99 run-queue delay of 40 ms behind a p50 of 20 µs is a stall that
/// an average erases completely.
///
/// The sensor declares this queryable even when it is not built with `ebpf`,
/// replying `available: false`. That distinction is worth keeping — "this kernel
/// or build cannot measure it" and "nothing answered" are different problems —
/// so they get different empty states.
fn render_latency_section(state: &DeviceDetailState) -> Element<'_, Message> {
    let title = row![text("Saturation latency (eBPF)").size(16)].spacing(8);
    let mut col = Column::new().spacing(4).push(title);

    let detail = &state.sysinfo_detail;
    if detail.latency.is_loading() {
        return col
            .push(empty_state("Fetching latency histograms…", None))
            .into();
    }
    if let Some(err) = detail.latency.error() {
        return col
            .push(empty_state(format!("Fetch failed: {err}"), None))
            .into();
    }
    let Some(report) = detail.latency.ready() else {
        return col
            .push(
                button(text("Fetch latency histograms").size(12))
                    .padding([4, 10])
                    .on_press(Message::FetchSysinfoLatency),
            )
            .into();
    };

    if !report.available {
        return col
            .push(empty_state(
                "The sensor is not collecting these — they need `collect.ebpf`, a binary \
                 built with the `ebpf` feature, a supported kernel, and CAP_BPF + \
                 CAP_PERFMON (a rootless container cannot load BPF at all).",
                None,
            ))
            .into();
    }

    col = col.push(
        text(format!("over the last {}s", report.window_secs))
            .size(12)
            .style(|t: &Theme| text::Style {
                color: Some(theme::colors(t).text_muted()),
            }),
    );

    // A histogram with no samples renders nothing, which is right — but if
    // BOTH are empty the panel used to be a title and a Refresh button with a
    // blank gap between them, indistinguishable from a broken view. That state
    // is reachable on any idle host, so it needs to say so (#685).
    let mut rendered_any = false;

    for (label, hist) in [
        ("Run-queue delay (runqlat)", &report.runqlat),
        ("Block I/O (biolatency)", &report.biolatency),
    ] {
        if hist.total == 0 {
            continue;
        }
        rendered_any = true;
        col = col.push(text(label).size(14)).push(
            row![
                latency_stat("p50", hist.p50_us),
                latency_stat("p95", hist.p95_us),
                latency_stat("p99", hist.p99_us),
                latency_stat("max", hist.max_us),
                text(format!("{} samples", hist.total))
                    .size(12)
                    .style(|t: &Theme| text::Style {
                        color: Some(theme::colors(t).text_muted()),
                    }),
            ]
            .spacing(space::MD)
            .align_y(Alignment::Center),
        );
    }

    if !rendered_any {
        col = col.push(empty_state(
            "No samples in this window. The collector is attached and measuring — \
             the host was simply idle enough that nothing was scheduled off-CPU or \
             waiting on disk long enough to record.",
            None,
        ));
    }

    col.push(
        button(text("Refresh").size(12))
            .padding([4, 10])
            .on_press(Message::FetchSysinfoLatency),
    )
    .into()
}

/// One percentile, rendered in the unit a human reads it in: µs below a
/// millisecond, ms above.
fn latency_stat<'a>(label: &'a str, us: u64) -> Element<'a, Message> {
    let value = if us >= 1000 {
        format!("{:.1} ms", us as f64 / 1000.0)
    } else {
        format!("{us} µs")
    };
    column![
        text(label).size(10).style(|t: &Theme| text::Style {
            color: Some(theme::colors(t).text_muted()),
        }),
        text(value).size(14),
    ]
    .spacing(2)
    .into()
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
        let device_id = DeviceId::fixture(Protocol::Sysinfo, "server01");
        let state = DeviceDetailState::new(device_id);
        // Just verify it doesn't panic
        let _view = sysinfo_host_view(&state);
    }

    // ── Identity pivots (#313) ────────────────────────────────────────────────

    use crate::view::specialized::fetch::Fetch;
    use crate::view::specialized::sysinfo_detail::PidFilter;
    use iced_test::simulator;
    use zensight_common::ProcessRecord;

    fn proc(pid: i32, start_time: u64, cgroup: Option<&str>) -> ProcessRecord {
        ProcessRecord {
            pid,
            name: "redis-server".into(),
            cpu: 1.0,
            rss: 1024,
            vsz: 4096,
            threads: None,
            state: "S".into(),
            io_read: 0,
            io_write: 0,
            uid: Some(1000),
            cmdline: "redis-server *:6379".into(),
            exe: None,
            ppid: None,
            cgroup: cgroup.map(String::from),
            start_time,
            user: Some("redis".into()),
        }
    }

    #[test]
    fn process_row_unit_chip_pivots_to_unit() {
        let mut state = DeviceDetailState::new(DeviceId::fixture(Protocol::Sysinfo, "server01"));
        state.sysinfo_detail.processes = Fetch::Ready(vec![
            proc(42, 100, Some("/system.slice/redis.service")),
            proc(43, 100, Some("/sys/fs/cgroup")), // non-unit cgroup → plain "—"
        ]);
        let mut ui = simulator(render_process_explorer(&state));
        assert!(ui.find("—").is_ok(), "non-unit cgroup renders inert text");
        let _ = ui.click("redis.service");
        let msgs: Vec<Message> = ui.into_messages().collect();
        assert!(msgs.iter().any(|m| matches!(
            m,
            Message::PivotToUnit { host, unit }
                if host == "server01" && unit == "redis.service"
        )));
    }

    #[test]
    fn pid_filter_banner_guards_stale_generations() {
        let mut state = DeviceDetailState::new(DeviceId::fixture(Protocol::Sysinfo, "server01"));
        state.sysinfo_detail.processes = Fetch::Ready(vec![proc(42, 999, None)]);
        // The pivot expected start_time 100 but pid 42 now has 999 → reused.
        state.sysinfo_detail.pid_filter = Some(PidFilter {
            pid: 42,
            start_time: Some(100),
        });
        let mut ui = simulator(render_process_explorer(&state));
        assert!(ui.find("Filtered to pid 42").is_ok());
        assert!(
            ui.find("pid reused by another process — the original exited")
                .is_ok()
        );
        // The impostor row is NOT shown as a match.
        assert!(ui.find("No matching live process.").is_ok());
        // Clear emits the un-filter message.
        let _ = ui.click("Clear");
        let msgs: Vec<Message> = ui.into_messages().collect();
        assert!(
            msgs.iter()
                .any(|m| matches!(m, Message::ClearSysinfoPidFilter))
        );
    }

    #[test]
    fn pid_filter_live_match_shows_only_that_process() {
        let mut state = DeviceDetailState::new(DeviceId::fixture(Protocol::Sysinfo, "server01"));
        state.sysinfo_detail.processes = Fetch::Ready(vec![proc(42, 100, None), proc(7, 5, None)]);
        state.sysinfo_detail.pid_filter = Some(PidFilter {
            pid: 42,
            start_time: Some(100),
        });
        let mut ui = simulator(render_process_explorer(&state));
        assert!(ui.find("42").is_ok());
        assert!(ui.find("1 processes").is_ok());
    }
}
