//! Bandwidth live monitor (#319, epic #320) — a bmon/nethogs-style view of
//! network rate by **process** (netlink `@/query/bandwidth`, sock_diag goodput)
//! and by **service** (systemd streamed `unit/<name>/ip_*_bps`).
//!
//! No single OS source can own this, so every row shows a **source/semantics
//! badge**: two differently-measured numbers (app-goodput vs wire-L3 vs wire-L2)
//! must never be summed or compared without the badge in view. Per-process rows
//! are query-only (high cardinality, principle P2); per-service rows come off the
//! telemetry stream (low cardinality).

use iced::Element;
use iced::widget::{button, column, row, text};

use zensight_common::bandwidth::{BandwidthKey, BandwidthRecord, ByteSemantics};

use crate::message::Message;
use crate::view::components::{
    Column as TableColumn, DataTable, SortKey, Sparkline, TableState, badge, empty_state,
    section_header,
};
use crate::view::formatting::format_rate;
use crate::view::specialized::fetch::Fetch;
use crate::view::theme;
use crate::view::tokens::{font, space};

/// Which bandwidth breakdown the monitor is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BandwidthMode {
    /// Per-process rows fetched from `@/query/bandwidth` (sock_diag / eBPF).
    #[default]
    Processes,
    /// Per-service rows read from the streamed systemd `ip_*_bps` telemetry.
    Services,
}

/// One table row: a bandwidth record plus optional history for the sparkline.
/// Services carry a streamed series; query-only process rows have none.
#[derive(Debug, Clone)]
pub struct BwRow {
    pub record: BandwidthRecord,
    pub spark: Vec<f64>,
}

/// Bandwidth view state. Per-process rows are fetched on demand; per-service
/// rows are derived from the store each render (passed into [`bandwidth_view`]).
#[derive(Debug, Default)]
pub struct BandwidthState {
    pub mode: BandwidthMode,
    pub processes: Fetch<Vec<BandwidthRecord>>,
    /// Per-service rows derived from the store (recomputed by the app on open /
    /// mode-switch / incoming systemd `ip_*_bps` telemetry). Held here so the
    /// table can borrow them for the render's lifetime.
    pub services: Vec<BwRow>,
    pub table: TableState,
}

impl BandwidthState {
    /// Mark the per-process fetch in flight.
    pub fn loading(&mut self) {
        self.processes = Fetch::Loading;
    }

    /// Fold a completed per-process fetch into state.
    pub fn apply(&mut self, result: Result<Vec<BandwidthRecord>, String>) {
        self.processes = Fetch::from_result(result);
    }
}

/// Human display name for a record's key.
fn row_name(r: &BandwidthRecord) -> String {
    match &r.key {
        BandwidthKey::Process { pid, comm, .. } => {
            if *pid < 0 {
                comm.clone()
            } else {
                format!("{comm} ({pid})")
            }
        }
        BandwidthKey::Service { unit } => unit.clone(),
        BandwidthKey::Cgroup { path } => path.clone(),
    }
}

/// A source/semantics badge whose colour encodes the byte-semantics (the crux of
/// "never compare blindly"): app-goodput reads below wire, wire-L3 excludes L2.
fn semantics_color(sem: ByteSemantics) -> iced::Color {
    match sem {
        ByteSemantics::AppGoodput => theme::SEVERITY_INFO,
        ByteSemantics::WireL3 => theme::SEVERITY_WARNING,
        ByteSemantics::WireL2 => theme::ACCENT_ANOMALY,
    }
}

fn source_badge(r: &BandwidthRecord) -> Element<'static, Message> {
    badge(
        semantics_color(r.semantics),
        format!("{} · {}", r.source.as_str(), r.semantics.badge()),
    )
}

/// Render the bandwidth monitor: a mode toggle, an honest-tiering legend, and the
/// active mode's table. Per-service rows are read from `state.services` (kept in
/// sync by the app).
pub fn bandwidth_view(state: &BandwidthState) -> Element<'_, Message> {
    let mode_button = |label: &'static str, mode: BandwidthMode| {
        let active = state.mode == mode;
        let mut b = button(text(label).size(font::CAPTION)).padding([4, 12]);
        if !active {
            b = b.on_press(Message::SetBandwidthMode(mode));
        }
        b
    };
    let toggle = row![
        mode_button("Processes", BandwidthMode::Processes),
        mode_button("Services", BandwidthMode::Services),
    ]
    .spacing(space::XS);

    let legend = text(
        "Per-process = app-goodput (sock_diag, TCP-only, below wire); \
         per-service = wire-L3 (systemd IPAccounting, no L2). Each row's badge marks \
         its source/semantics — never compare rates across different badges.",
    )
    .size(font::CAPTION);

    let body = match state.mode {
        BandwidthMode::Processes => render_processes(state),
        BandwidthMode::Services => render_services(&state.services, &state.table),
    };

    column![section_header("Bandwidth", None), toggle, legend, body]
        .spacing(space::SM)
        .padding(space::MD)
        .into()
}

/// Per-process table over the `@/query/bandwidth` fetch (no sparkline — the query
/// returns a point-in-time rate, not a series).
fn render_processes(state: &BandwidthState) -> Element<'_, Message> {
    if state.processes.is_loading() {
        return empty_state("Fetching per-process bandwidth…", None);
    }
    if let Some(err) = state.processes.error() {
        return empty_state(format!("Fetch failed: {err}"), None);
    }
    let Some(records) = state.processes.ready() else {
        return column![
            text("Per-process TCP bandwidth from the netlink sensor (sock_diag).")
                .size(font::CAPTION),
            button(text("Fetch").size(font::CAPTION))
                .padding([4, 12])
                .on_press(Message::RefreshBandwidth),
        ]
        .spacing(space::SM)
        .into();
    };
    if records.is_empty() {
        return empty_state(
            "No active TCP flows attributed to a process.",
            Some(
                button(text("Refresh").size(font::CAPTION))
                    .padding([4, 12])
                    .on_press(Message::RefreshBandwidth)
                    .into(),
            ),
        );
    }

    let columns = vec![
        TableColumn::fill("process", 4, |r: &BandwidthRecord| {
            text(row_name(r)).size(font::CAPTION).into()
        })
        .sortable(|r: &BandwidthRecord| SortKey::Text(row_name(r))),
        TableColumn::fixed("tx", 90.0, |r: &BandwidthRecord| {
            text(format_rate(r.tx_bps)).size(font::CAPTION).into()
        })
        .sortable(|r: &BandwidthRecord| SortKey::Num(r.tx_bps)),
        TableColumn::fixed("rx", 90.0, |r: &BandwidthRecord| {
            text(format_rate(r.rx_bps)).size(font::CAPTION).into()
        })
        .sortable(|r: &BandwidthRecord| SortKey::Num(r.rx_bps)),
        TableColumn::fixed("source", 150.0, |r: &BandwidthRecord| source_badge(r)),
        TableColumn::fixed("host", 120.0, |r: &BandwidthRecord| {
            text(r.host.clone().unwrap_or_else(|| "-".into()))
                .size(font::CAPTION)
                .into()
        })
        .sortable(|r: &BandwidthRecord| SortKey::Text(r.host.clone().unwrap_or_default())),
    ];
    column![
        button(text("Refresh").size(font::CAPTION))
            .padding([4, 12])
            .on_press(Message::RefreshBandwidth),
        DataTable::new(columns)
            .searchable(|r: &BandwidthRecord| row_name(r))
            .on_sort(Message::BandwidthTableSort)
            .on_filter(Message::BandwidthTableFilter)
            .noun("processes")
            .view(records, &state.table),
    ]
    .spacing(space::SM)
    .into()
}

/// Per-service table over the store-derived rows, with a streamed sparkline.
fn render_services<'a>(services: &'a [BwRow], table: &'a TableState) -> Element<'a, Message> {
    if services.is_empty() {
        return empty_state(
            "No systemd unit is publishing IPAccounting rates. Enable \
             ip_io_accounting on the systemd sensor and IPAccounting=yes on a unit.",
            None,
        );
    }
    let columns = vec![
        TableColumn::fill("service", 4, |r: &BwRow| {
            text(row_name(&r.record)).size(font::CAPTION).into()
        })
        .sortable(|r: &BwRow| SortKey::Text(row_name(&r.record))),
        TableColumn::fixed("tx", 90.0, |r: &BwRow| {
            text(format_rate(r.record.tx_bps))
                .size(font::CAPTION)
                .into()
        })
        .sortable(|r: &BwRow| SortKey::Num(r.record.tx_bps)),
        TableColumn::fixed("rx", 90.0, |r: &BwRow| {
            text(format_rate(r.record.rx_bps))
                .size(font::CAPTION)
                .into()
        })
        .sortable(|r: &BwRow| SortKey::Num(r.record.rx_bps)),
        TableColumn::fixed("trend", 90.0, |r: &BwRow| {
            if r.spark.len() >= 2 {
                Sparkline::new(r.spark.clone()).with_size(80.0, 20.0).view()
            } else {
                text("—").size(font::CAPTION).into()
            }
        }),
        TableColumn::fixed("source", 150.0, |r: &BwRow| source_badge(&r.record)),
        TableColumn::fixed("host", 120.0, |r: &BwRow| {
            text(r.record.host.clone().unwrap_or_else(|| "-".into()))
                .size(font::CAPTION)
                .into()
        })
        .sortable(|r: &BwRow| SortKey::Text(r.record.host.clone().unwrap_or_default())),
    ];
    DataTable::new(columns)
        .searchable(|r: &BwRow| row_name(&r.record))
        .on_sort(Message::BandwidthTableSort)
        .on_filter(Message::BandwidthTableFilter)
        .noun("services")
        .view(services, table)
}

#[cfg(test)]
mod tests {
    use iced_test::simulator;
    use zensight_common::bandwidth::{
        BandwidthKey, BandwidthRecord, BandwidthSource, ByteSemantics, ProtoScope,
    };

    use super::*;

    fn service_row(unit: &str, tx: f64, rx: f64) -> BwRow {
        BwRow {
            record: BandwidthRecord {
                key: BandwidthKey::Service { unit: unit.into() },
                tx_bps: tx,
                rx_bps: rx,
                source: BandwidthSource::Systemd,
                semantics: ByteSemantics::WireL3,
                proto: ProtoScope::All,
                host: Some("server01".into()),
            },
            spark: vec![1.0, 2.0, 3.0],
        }
    }

    #[test]
    fn row_name_labels_process_service_and_unattributed() {
        let proc = BandwidthRecord {
            key: BandwidthKey::Process {
                pid: 42,
                start_time: 1,
                comm: "curl".into(),
            },
            tx_bps: 0.0,
            rx_bps: 0.0,
            source: BandwidthSource::SockDiag,
            semantics: ByteSemantics::AppGoodput,
            proto: ProtoScope::Tcp,
            host: None,
        };
        assert_eq!(row_name(&proc), "curl (42)");
        let unattributed = BandwidthRecord {
            key: BandwidthKey::Process {
                pid: -1,
                start_time: 0,
                comm: "unattributed".into(),
            },
            ..proc.clone()
        };
        assert_eq!(row_name(&unattributed), "unattributed");
        assert_eq!(
            row_name(&service_row("nginx.service", 0.0, 0.0).record),
            "nginx.service"
        );
    }

    #[test]
    fn semantics_colors_are_distinct() {
        let g = semantics_color(ByteSemantics::AppGoodput);
        let l3 = semantics_color(ByteSemantics::WireL3);
        let l2 = semantics_color(ByteSemantics::WireL2);
        assert_ne!(g, l3);
        assert_ne!(l3, l2);
        assert_ne!(g, l2);
    }

    #[test]
    fn processes_mode_renders_rows_badges_and_legend() {
        let mut state = BandwidthState {
            mode: BandwidthMode::Processes,
            ..Default::default()
        };
        state.apply(Ok(crate::mock::bandwidth::processes()));
        let mut ui = simulator(bandwidth_view(&state));
        // A process row, its goodput/source badge, and the refresh action.
        assert!(ui.find("nginx (1421)").is_ok());
        assert!(ui.find("sock_diag · goodput").is_ok());
        assert!(ui.find("Refresh").is_ok());
    }

    #[test]
    fn services_mode_renders_units_and_wire_l3_badge() {
        let state = BandwidthState {
            mode: BandwidthMode::Services,
            services: vec![
                service_row("nginx.service", 1000.0, 2000.0),
                service_row("postgresql.service", 500.0, 900.0),
            ],
            ..Default::default()
        };
        let mut ui = simulator(bandwidth_view(&state));
        assert!(ui.find("nginx.service").is_ok());
        assert!(ui.find("systemd · wire-L3").is_ok());
    }

    #[test]
    fn empty_processes_shows_empty_state() {
        let mut state = BandwidthState {
            mode: BandwidthMode::Processes,
            ..Default::default()
        };
        state.apply(Ok(Vec::new()));
        let mut ui = simulator(bandwidth_view(&state));
        assert!(
            ui.find("No active TCP flows attributed to a process.")
                .is_ok()
        );
    }

    #[test]
    fn mode_toggle_emits_set_mode() {
        let state = BandwidthState {
            mode: BandwidthMode::Processes,
            ..Default::default()
        };
        let mut ui = simulator(bandwidth_view(&state));
        let _ = ui.click("Services");
        let msgs: Vec<Message> = ui.into_messages().collect();
        assert!(
            msgs.iter()
                .any(|m| matches!(m, Message::SetBandwidthMode(BandwidthMode::Services)))
        );
    }
}
