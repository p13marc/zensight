//! Device detail view showing all metrics for a selected device.

use std::collections::{HashMap, HashSet, VecDeque};

use iced::widget::{
    Row, column, container, row, rule, scrollable, table, text, text_input, tooltip,
};
use iced::{Alignment, Element, Length, Theme};
use iced_anim::widget::button;

use zensight_common::{DeviceStatus, HostEntity, Protocol, TelemetryPoint, TelemetryValue};

use crate::app::DEVICE_SEARCH_ID;
use crate::message::{DeviceId, Message};
use crate::view::chart::{self, ChartState, DataPoint, TimeWindow, chart_view};
use crate::view::components::empty_state;
use crate::view::formatting::{format_timestamp, format_value};
use crate::view::icons::{self, IconSize};
use crate::view::specialized;
use crate::view::tokens::font;

/// Debounce delay for metric search input in milliseconds.
const SEARCH_DEBOUNCE_MS: i64 = 300;

/// Threshold for marking individual metrics as stale (60 seconds in ms).
const METRIC_STALE_THRESHOLD_MS: i64 = 60_000;

/// A row in the metrics table, containing pre-formatted data for display.
/// This struct is Clone so it can be used with the table widget.
#[derive(Debug, Clone)]
struct MetricTableRow {
    /// Metric name.
    name: String,
    /// Formatted value for display.
    value: String,
    /// Full value (if truncated).
    full_value: Option<String>,
    /// Type name (Counter, Gauge, Text, etc.).
    type_name: String,
    /// Formatted timestamp.
    timestamp: String,
    /// Whether this metric is chartable (numeric).
    is_chartable: bool,
    /// Whether this metric is currently in the chart.
    is_in_chart: bool,
    /// Whether the producer's slice declares this metric's subject (#1256).
    /// `false` renders the "not declared" marker beside the name.
    declared: bool,
    /// Whether this metric is favorited/pinned on this device (#27).
    is_favorite: bool,
    /// Trend indicator: "up", "down", "stable", or empty.
    trend: String,
    /// Whether this metric is stale (not updated recently).
    is_stale: bool,
    /// The device this metric belongs to (for the promote-to-alert action, #50).
    device_id: DeviceId,
    /// Current numeric value, if the metric is numeric (#50).
    numeric_value: Option<f64>,
}

/// Get the current timestamp in milliseconds.
fn current_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A pivot into a device view (#313): what the user came to see, carried
/// from the view that offered the pivot. A view that has no use for the
/// pivot ignores it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pivot {
    /// One process, from a unit's MainPID or a socket's owner. `start_time`
    /// is the `(pid, start_time)` identity pair — the stale-generation
    /// guard: a reused pid renders as "exited", never as the wrong process.
    Process { pid: i32, start_time: Option<u64> },
}

/// State for the device detail view.
#[derive(Debug)]
pub struct DeviceDetailState {
    /// The device being viewed.
    pub device_id: DeviceId,
    /// All metrics for this device (metric name -> telemetry point).
    pub metrics: HashMap<String, TelemetryPoint>,
    /// Metric history (for graphing).
    pub history: HashMap<String, VecDeque<TelemetryPoint>>,
    /// Pre-restart history seeded from the local tiered store (#22), keyed by
    /// metric name. Merged ahead of live `history` when a chart is opened so a
    /// device view opens pre-populated with trends that survived restart.
    pub seeded_history: HashMap<String, Vec<zensight_store::Sample>>,
    /// Maximum history size per metric.
    pub max_history: usize,
    /// Currently selected metric for the chart (if any).
    pub selected_metric: Option<String>,
    /// Chart state for the selected metric.
    pub chart: ChartState,
    /// Search filter for metrics (applied after debounce).
    pub metric_filter: String,
    /// Pending search filter (user input).
    pub pending_filter: String,
    /// The origin typed into the identity panel's "merge into" field (#1129):
    /// `h-<12hex>`, validated before the button enables.
    pub merge_target: String,
    /// Timestamp when pending filter was last updated.
    pub pending_filter_time: i64,
    /// Parallax stream catalogue + live preview tiles, fetched/opened on
    /// demand from the sensor's stream-control channels (#408).
    pub parallax_detail: crate::view::specialized::parallax_detail::ParallaxDetailState,
    /// The read procedures this view has called, by procedure path
    /// (#1261): the generic answer store every on-demand panel reads —
    /// bespoke ones decode a type from it, the default view renders it as
    /// it came.
    pub calls: crate::call::Calls,
    /// UI state (sort, filter, page) of the on-demand tables a view draws,
    /// by the name the view gives each (#1261).
    pub tables: std::collections::BTreeMap<String, crate::view::components::TableState>,
    /// A view's own filter controls (#1261) — a socket explorer's state chip,
    /// port substring and sort — keyed `<table>/<key>`, set by
    /// `Message::SetDetailFilter`; `""` is unset. Setting one resets the
    /// table's page, so a narrowed filter never hides matches behind "more".
    pub filters: std::collections::BTreeMap<String, String>,
    /// The write machine (#1261): one armed write procedure, one in flight,
    /// the last outcome per procedure — what a row's confirm/cancel and
    /// "busy" read, whichever producer the row belongs to.
    pub writes: crate::call::Writes,
    /// The firing alerts scoped to this device (#253, #1261): its source
    /// and its producer, projected by the app from the alert set so a view
    /// renders them without threading `AlertsState` through.
    pub alerts: Vec<zensight_common::Alert>,
    /// Counters a tab watches, last value seen (#283, #1261): when one moves
    /// the view's procedure is re-called. Seeded on first sight.
    pub counters_seen: std::collections::BTreeMap<String, f64>,
    /// How the user arrived, when it was a pivot (#313): the process
    /// explorer's pid filter with its stale-generation guard. `None` is the
    /// plain view.
    pub pivot: Option<Pivot>,
    /// SNMP device detail (#530): the joined `InterfaceTable` state doc
    /// (LWW off the bus) + interface-table UI state.
    pub snmp_detail: crate::view::specialized::snmp::SnmpDetailState,
    /// Whether the chart panel is expanded to a taller height (#36).
    pub chart_expanded: bool,
    /// Text-input buffer for the custom relative window in minutes (#36).
    pub chart_custom_input: String,
    /// Text-input buffers for the absolute `from`/`to` range picker (#36),
    /// `YYYY-MM-DD HH:MM`, local (#1123). Applied together via
    /// [`Self::apply_chart_range`].
    pub chart_from_input: String,
    pub chart_to_input: String,
    /// Favorited metric names for this device (#27). Projected from the app-level
    /// persisted favorites on selection; pinned metrics sort to the top of the
    /// table and show a filled star.
    pub favorites: HashSet<String>,
    /// Active tab of the tabbed specialized view (#243), remembered per device.
    /// Defaults to `Overview`.
    pub specialized_tab: crate::view::specialized::SpecializedTab,
    /// This device's host origin (`h-<12hex>`), once the source→origin map has
    /// learned it (#476). `None` in the ~5 s before the first health doc lands,
    /// which is why the Focus control is disabled rather than guessing: an
    /// origin-scoped selector built from a wrong origin subscribes to silence.
    pub origin: Option<String>,
    /// Whether the link is currently focused on *this* host (#476).
    pub focused: bool,
    /// Subjects the producer's slice does not declare — the honesty finding
    /// (#1256, gate 4), projected from the dashboard's device state.
    pub undeclared: crate::intake::Undeclared,
    /// `Some(false)` when the fleet holds no slice for this producer; `None`
    /// before a sweep has answered (#1256).
    pub slice_known: Option<bool>,
    /// State documents held for this device, by subject (#1256).
    pub documents: std::collections::BTreeMap<String, crate::intake::DocumentState>,
    /// Events-class records for this device, newest first (#1256).
    pub events: VecDeque<crate::intake::EventState>,
    /// The producer's family model (#1257), from the slice the fleet served
    /// or, failing that, the one this build compiled in. `None` when neither
    /// knows the producer: then there is nothing to derive, and the flat
    /// metric list below is all the view can honestly show.
    pub family: Option<crate::view::family::FamilyModel>,
    /// The producer's view definition (#1259): what it served at
    /// `@rpc/<producer>/views`, else the bundled one, else `None` — and then
    /// the default renderer over `family` is what the view shows.
    pub definition: Option<crate::view::definition::Definition>,
}

impl DeviceDetailState {
    /// Create a new device detail state.
    pub fn new(device_id: DeviceId) -> Self {
        Self::with_max_history(device_id, 500)
    }

    /// Create a new device detail state with configurable max history.
    pub fn with_max_history(device_id: DeviceId, max_history: usize) -> Self {
        Self {
            device_id: device_id.clone(),
            metrics: HashMap::new(),
            history: HashMap::new(),
            seeded_history: HashMap::new(),
            max_history,
            selected_metric: None,
            chart: ChartState::new(format!("{}", device_id)),
            metric_filter: String::new(),
            pending_filter: String::new(),
            merge_target: String::new(),
            pending_filter_time: 0,
            parallax_detail: Default::default(),
            calls: Default::default(),
            tables: Default::default(),
            filters: Default::default(),
            writes: Default::default(),
            alerts: Vec::new(),
            counters_seen: Default::default(),
            pivot: None,
            snmp_detail: Default::default(),
            chart_expanded: false,
            chart_custom_input: String::new(),
            chart_from_input: String::new(),
            chart_to_input: String::new(),
            favorites: HashSet::new(),
            specialized_tab: Default::default(),
            origin: None,
            focused: false,
            undeclared: Default::default(),
            slice_known: None,
            documents: std::collections::BTreeMap::new(),
            events: VecDeque::new(),
            family: None,
            definition: None,
        }
    }

    /// An on-demand table's UI state, a shared default when untouched (#1261).
    pub fn table(&self, name: &str) -> &crate::view::components::TableState {
        static DEFAULT: std::sync::OnceLock<crate::view::components::TableState> =
            std::sync::OnceLock::new();
        self.tables
            .get(name)
            .unwrap_or_else(|| DEFAULT.get_or_init(Default::default))
    }

    /// A view filter's value, `""` when unset (#1261).
    pub fn filter(&self, table: &str, key: &str) -> &str {
        self.filter_opt(table, key).unwrap_or("")
    }

    /// A view filter's value, `None` when never set — for a control whose
    /// untouched state means a default and whose cleared state (`""`) means
    /// everything (#1261).
    pub fn filter_opt(&self, table: &str, key: &str) -> Option<&str> {
        self.filters
            .get(&format!("{table}/{key}"))
            .map(String::as_str)
    }

    /// Replace the favorited-metric set for this device (#27). Called on selection
    /// with the projection of the app-level persisted favorites for this device.
    pub fn set_favorites(&mut self, favorites: HashSet<String>) {
        self.favorites = favorites;
    }

    /// Whether `metric` is favorited on this device (#27).
    pub fn is_favorite(&self, metric: &str) -> bool {
        self.favorites.contains(metric)
    }

    /// Toggle `metric`'s favorite state (#27); returns the new state.
    pub fn toggle_favorite(&mut self, metric: &str) -> bool {
        if self.favorites.remove(metric) {
            false
        } else {
            self.favorites.insert(metric.to_string());
            true
        }
    }

    /// Toggle the chart panel between default and expanded height (#36).
    pub fn toggle_chart_expand(&mut self) {
        self.chart_expanded = !self.chart_expanded;
    }

    /// Apply a custom relative window from the text input (#36). Empty input or
    /// an unparseable value clears the custom window.
    pub fn set_chart_custom_minutes(&mut self, input: String) {
        self.chart_custom_input = input;
        match self.chart_custom_input.trim().parse::<f64>() {
            Ok(minutes) => self.chart.set_custom_duration_minutes(minutes),
            Err(_) => self.chart.set_custom_duration_minutes(0.0),
        }
    }

    /// Apply the absolute `from`/`to` range inputs (#36). Parses both as
    /// `YYYY-MM-DD HH:MM` local (#1123); on success pins the chart's visible window and
    /// returns `Some((from_ms, to_ms))` so the caller can range-query the store.
    /// Returns `None` (and pins nothing) when either field is empty/unparseable
    /// or `from >= to`.
    pub fn apply_chart_range(&mut self) -> Option<(i64, i64)> {
        let from = crate::view::chart::parse_datetime_to_ms(&self.chart_from_input)?;
        let to = crate::view::chart::parse_datetime_to_ms(&self.chart_to_input)?;
        if from >= to {
            return None;
        }
        self.chart.set_absolute_range(from, to);
        Some((from, to))
    }

    /// Clear the absolute range and return to the preset / custom window (#36).
    pub fn clear_chart_range(&mut self) {
        self.chart_from_input.clear();
        self.chart_to_input.clear();
        self.chart.clear_absolute_range();
    }

    /// One chart interaction (#1306): the state change happens here, and
    /// what the app must do about it comes back as a [`chart::Effect`] —
    /// pure, so it is unit-tested without the app.
    pub fn apply_chart(&mut self, action: chart::Action) -> chart::Effect {
        use chart::{Action as A, Effect};
        match action {
            A::SelectMetric(name) => self.select_metric(name),
            A::ClearSelection => self.clear_chart_selection(),
            A::AddMetric(name) => self.add_metric_to_chart(name),
            A::RemoveMetric(name) => self.remove_metric_from_chart(&name),
            A::ToggleVisibility(name) => self.toggle_metric_visibility(&name),
            A::ToggleFavorite(metric) => {
                let now_fav = self.toggle_favorite(&metric);
                return Effect::Favorite { metric, now_fav };
            }
            A::SetTimeWindow(window) => self.set_time_window(window),
            A::SetCustomMinutes(input) => self.set_chart_custom_minutes(input),
            A::SetRangeFrom(input) => self.chart_from_input = input,
            A::SetRangeTo(input) => self.chart_to_input = input,
            A::ApplyRange => {
                return match self.apply_chart_range() {
                    Some((from, to)) => Effect::LoadRange { from, to },
                    None => Effect::InvalidRange,
                };
            }
            A::ClearRange => self.clear_chart_range(),
            A::ToggleExpand => self.toggle_chart_expand(),
            A::ZoomIn => self.zoom_in(),
            A::ZoomOut => self.zoom_out(),
            A::ZoomReset => self.reset_zoom(),
            A::PanLeft => self.pan_left(),
            A::PanRight => self.pan_right(),
            A::PanReset => self.reset_pan(),
            A::DragStart(x) => self.start_drag(x),
            A::DragUpdate(x, width) => self.update_drag(x, width),
            A::DragEnd => self.end_drag(),
            A::SetMetricFilter(filter) => self.set_metric_filter(filter),
        }
        Effect::None
    }

    /// Update the max history setting.
    pub fn set_max_history(&mut self, max_history: usize) {
        self.max_history = max_history;
        // Trim existing history if needed
        for history in self.history.values_mut() {
            while history.len() > max_history {
                history.pop_front();
            }
        }
    }

    /// Update with a new telemetry point.
    pub fn update(&mut self, point: TelemetryPoint) {
        let metric_name = point.metric.clone();

        // Derive the chart data point up front (cheap: timestamp + value) so we
        // can move `point` into history below without re-deriving (#40).
        let data_point = DataPoint::from_telemetry(point.timestamp, &point.value);

        // Update current value (one clone — the snapshot map needs its own copy).
        self.metrics.insert(metric_name.clone(), point.clone());

        // Update the chart while we still hold `metric_name`.
        if let Some(dp) = data_point {
            // Single-series mode.
            if self.selected_metric.as_deref() == Some(metric_name.as_str()) {
                self.chart.push(dp.clone());
            }
            // Comparison mode (multi-series).
            if self.chart.has_series(&metric_name) {
                self.chart.push_to_series(&metric_name, dp);
            }
        }

        // Update history — move the original `point` in (its last use, no clone).
        let history = self.history.entry(metric_name).or_default();
        history.push_back(point);

        // Trim history if needed.
        if history.len() > self.max_history {
            history.pop_front();
        }
    }

    /// Select a metric for charting (single-metric mode).
    pub fn select_metric(&mut self, metric_name: String) {
        // If already in multi-series mode with this metric, just switch to single mode
        if self.chart.is_multi_series() {
            self.chart.clear_series();
        }

        self.selected_metric = Some(metric_name.clone());
        self.chart = ChartState::new(&metric_name);

        // Populate chart with stored history (pre-restart) + live history.
        let data_points = self.chart_points_for(&metric_name);
        if !data_points.is_empty() {
            self.chart.set_data(data_points);
        }
    }

    /// Build chart points for a metric, merging restart-survived store samples
    /// (older) ahead of the in-memory live history (newer), deduplicated by
    /// timestamp so the live point wins where they overlap. #22.
    fn chart_points_for(&self, metric_name: &str) -> Vec<DataPoint> {
        let mut points: Vec<DataPoint> = Vec::new();
        // Earliest live timestamp — store samples at/after it are superseded by live.
        let live_start = self
            .history
            .get(metric_name)
            .and_then(|h| h.front())
            .map(|p| p.timestamp);
        if let Some(seeded) = self.seeded_history.get(metric_name) {
            for s in seeded {
                if live_start.is_none_or(|start| s.ts < start) {
                    points.push(DataPoint::new(s.ts, s.value));
                }
            }
        }
        if let Some(history) = self.history.get(metric_name) {
            points.extend(
                history
                    .iter()
                    .filter_map(|p| DataPoint::from_telemetry(p.timestamp, &p.value)),
            );
        }
        points
    }

    /// The last `max` numeric history values for `metric` (seeded + live,
    /// oldest-first), for inline sparklines in specialized views (#44). Empty
    /// when the metric has no numeric history.
    pub fn history_values(&self, metric: &str, max: usize) -> Vec<f64> {
        let points = self.chart_points_for(metric);
        let start = points.len().saturating_sub(max);
        points[start..].iter().map(|p| p.value).collect()
    }

    /// Seed restart-survived history loaded from the store (#22). Stored per
    /// metric; merged into a chart when that metric is selected.
    pub fn seed_history(&mut self, series: Vec<(String, Vec<zensight_store::Sample>)>) {
        for (metric, samples) in series {
            if samples.is_empty() {
                continue;
            }
            self.seeded_history.insert(metric.clone(), samples);
            // If this metric's chart is already open, refresh it with the seed.
            if self.selected_metric.as_deref() == Some(metric.as_str()) {
                let points = self.chart_points_for(&metric);
                self.chart.set_data(points);
            }
        }
    }

    /// Clear the chart selection.
    pub fn clear_chart_selection(&mut self) {
        self.selected_metric = None;
        self.chart.clear_series();
    }

    /// Add a metric to the comparison chart (multi-series mode).
    pub fn add_metric_to_chart(&mut self, metric_name: String) {
        // Check if metric is chartable
        if !self.is_metric_chartable(&metric_name) {
            return;
        }

        // If this is the first metric being added in comparison mode,
        // set a generic title
        if !self.chart.is_multi_series() && self.selected_metric.is_none() {
            self.chart = ChartState::new("Metric Comparison");
        }

        // Clear single-series data when switching to multi-series
        if self.selected_metric.is_some() && !self.chart.is_multi_series() {
            // Convert current single metric to a series
            if let Some(ref current_metric) = self.selected_metric
                && let Some(history) = self.history.get(current_metric)
            {
                let data_points: Vec<DataPoint> = history
                    .iter()
                    .filter_map(|p| DataPoint::from_telemetry(p.timestamp, &p.value))
                    .collect();
                self.chart
                    .add_series_with_data(current_metric.clone(), data_points);
            }
            self.selected_metric = None;
            self.chart.set_data(Vec::new()); // Clear single-series data
        }

        // Add new series with historical data
        if let Some(history) = self.history.get(&metric_name) {
            let data_points: Vec<DataPoint> = history
                .iter()
                .filter_map(|p| DataPoint::from_telemetry(p.timestamp, &p.value))
                .collect();
            self.chart.add_series_with_data(&metric_name, data_points);
        } else {
            self.chart.add_series(&metric_name);
        }
    }

    /// Remove a metric from the comparison chart.
    pub fn remove_metric_from_chart(&mut self, metric_name: &str) {
        self.chart.remove_series(metric_name);

        // If only one series left, could switch back to single mode (optional)
        // For now, keep in multi-series mode even with one series
    }

    /// Toggle visibility of a metric in the comparison chart.
    pub fn toggle_metric_visibility(&mut self, metric_name: &str) {
        self.chart.toggle_series_visibility(metric_name);
    }

    /// Check if a metric is currently in the chart (single or multi-series).
    pub fn is_metric_in_chart(&self, metric_name: &str) -> bool {
        if self.chart.is_multi_series() {
            self.chart.has_series(metric_name)
        } else {
            self.selected_metric.as_ref() == Some(&metric_name.to_string())
        }
    }

    /// Check if in multi-series (comparison) mode.
    pub fn is_comparison_mode(&self) -> bool {
        self.chart.is_multi_series()
    }

    /// Get the number of metrics in comparison chart.
    pub fn comparison_count(&self) -> usize {
        self.chart.series_count()
    }

    /// Set the chart time window.
    pub fn set_time_window(&mut self, window: TimeWindow) {
        self.chart.set_time_window(window);
    }

    /// Zoom in on the chart.
    pub fn zoom_in(&mut self) {
        self.chart.zoom_in();
    }

    /// Zoom out on the chart.
    pub fn zoom_out(&mut self) {
        self.chart.zoom_out();
    }

    /// Reset chart zoom to 100%.
    pub fn reset_zoom(&mut self) {
        self.chart.reset_zoom();
    }

    /// Pan the chart left (back in time).
    pub fn pan_left(&mut self) {
        self.chart.pan_left();
    }

    /// Pan the chart right (forward in time).
    pub fn pan_right(&mut self) {
        self.chart.pan_right();
    }

    /// Reset chart pan to view current time.
    pub fn reset_pan(&mut self) {
        self.chart.reset_pan();
    }

    /// Start chart drag.
    pub fn start_drag(&mut self, x: f32) {
        self.chart.start_drag(x);
    }

    /// Update chart drag.
    pub fn update_drag(&mut self, x: f32, width: f32) {
        self.chart.update_drag(x, width);
    }

    /// End chart drag.
    pub fn end_drag(&mut self) {
        self.chart.end_drag();
    }

    /// Update the chart time and apply pending filter (call on tick).
    pub fn update_chart_time(&mut self) {
        self.chart.update_time();
        self.chart.update_zoom_feedback();
        self.chart.update_pan_feedback();
        self.apply_pending_filter();
    }

    /// Set the metric search filter (debounced).
    ///
    /// Updates the pending filter and timestamp. The actual filter
    /// is applied after the debounce delay via `apply_pending_filter`.
    pub fn set_metric_filter(&mut self, filter: String) {
        self.pending_filter = filter;
        self.pending_filter_time = current_timestamp();
    }

    /// Apply the pending filter if the debounce delay has elapsed.
    ///
    /// Returns `true` if the filter was applied (changed).
    pub fn apply_pending_filter(&mut self) -> bool {
        if self.pending_filter != self.metric_filter {
            let elapsed = current_timestamp() - self.pending_filter_time;
            if elapsed >= SEARCH_DEBOUNCE_MS {
                self.metric_filter = self.pending_filter.clone();
                return true;
            }
        }
        false
    }

    /// Get the current filter input (for display in the text input).
    pub fn filter_input(&self) -> &str {
        &self.pending_filter
    }

    /// Get metrics sorted by name, optionally filtered by the search string.
    pub fn sorted_metrics(&self) -> Vec<(&String, &TelemetryPoint)> {
        let filter_lower = self.metric_filter.to_lowercase();
        let mut metrics: Vec<_> = self
            .metrics
            .iter()
            .filter(|(name, _)| {
                if self.metric_filter.is_empty() {
                    true
                } else {
                    name.to_lowercase().contains(&filter_lower)
                }
            })
            .collect();
        // Favorites pinned to the top (#27), then alphabetical within each group.
        metrics.sort_by(|a, b| {
            let (fa, fb) = (self.is_favorite(a.0), self.is_favorite(b.0));
            fb.cmp(&fa).then_with(|| a.0.cmp(b.0))
        });
        metrics
    }

    /// Get the total metric count (unfiltered).
    pub fn total_metric_count(&self) -> usize {
        self.metrics.len()
    }

    /// Check if a metric is chartable. Counters/gauges chart directly; booleans
    /// chart as a 0/1 step series (#126) so flap-prone signals (`iface/*/up`,
    /// `carrier`) get a trend, not just a snapshot. Text/binary are not chartable.
    pub fn is_metric_chartable(&self, metric_name: &str) -> bool {
        if let Some(point) = self.metrics.get(metric_name) {
            matches!(
                point.value,
                TelemetryValue::Counter(_) | TelemetryValue::Gauge(_) | TelemetryValue::Boolean(_)
            )
        } else {
            false
        }
    }

    /// Export metrics to CSV format.
    pub fn export_to_csv(&self) -> String {
        let mut csv = String::new();

        // Header
        csv.push_str("timestamp,protocol,source,metric,value,type,labels\n");

        // Sort by metric name
        let mut metrics: Vec<_> = self.metrics.values().collect();
        metrics.sort_by(|a, b| a.metric.cmp(&b.metric));

        for point in metrics {
            let value_str = format_value_for_export(&point.value);
            let type_str = value_type_name(&point.value);
            let labels_str = point
                .labels
                .iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect::<Vec<_>>()
                .join(";");

            csv.push_str(&format!(
                "{},{},{},{},{},{},{}\n",
                point.timestamp,
                self.device_id.producer,
                escape_csv(&point.source),
                escape_csv(&point.metric),
                escape_csv(&value_str),
                type_str,
                escape_csv(&labels_str)
            ));
        }

        csv
    }

    /// Export metrics to JSON format.
    pub fn export_to_json(&self) -> String {
        let mut metrics: Vec<_> = self.metrics.values().collect();
        metrics.sort_by(|a, b| a.metric.cmp(&b.metric));

        serde_json::to_string_pretty(&metrics).unwrap_or_else(|_| "[]".to_string())
    }

    /// Export the full per-metric **time series** to CSV (#37) — every point the
    /// view holds, not just the latest snapshot. One row per (metric, sample),
    /// sorted by metric then timestamp, so the trend on screen is exportable.
    pub fn export_history_to_csv(&self) -> String {
        let mut csv = String::new();
        csv.push_str("timestamp,protocol,source,metric,value,type\n");

        let mut names: Vec<&String> = self.history.keys().collect();
        names.sort();
        for name in names {
            let Some(history) = self.history.get(name) else {
                continue;
            };
            for point in history.iter() {
                let value_str = format_value_for_export(&point.value);
                let type_str = value_type_name(&point.value);
                csv.push_str(&format!(
                    "{},{},{},{},{},{}\n",
                    point.timestamp,
                    self.device_id.producer,
                    escape_csv(&point.source),
                    escape_csv(&point.metric),
                    escape_csv(&value_str),
                    type_str
                ));
            }
        }
        csv
    }

    /// Export the full per-metric time series to JSON (#37): a map of metric name
    /// to its ordered list of telemetry points.
    pub fn export_history_to_json(&self) -> String {
        let mut ordered: std::collections::BTreeMap<&String, Vec<&TelemetryPoint>> =
            std::collections::BTreeMap::new();
        for (name, history) in &self.history {
            ordered.insert(name, history.iter().collect());
        }
        serde_json::to_string_pretty(&ordered).unwrap_or_else(|_| "{}".to_string())
    }

    /// Whether there is any time-series history to export (#37).
    pub fn has_history(&self) -> bool {
        self.history.values().any(|h| !h.is_empty())
    }
}

/// Escape a string for CSV (handle commas and quotes).
fn escape_csv(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Format a value for export.
pub(crate) fn format_value_for_export(value: &TelemetryValue) -> String {
    match value {
        TelemetryValue::Counter(v) => v.to_string(),
        TelemetryValue::Gauge(v) => v.to_string(),
        TelemetryValue::Text(s) => s.clone(),
        TelemetryValue::Boolean(b) => b.to_string(),
        TelemetryValue::Binary(data) => format!("<{} bytes>", data.len()),
    }
}

/// Render the device detail view.
///
/// This function first tries to render a protocol-specific specialized view.
/// If no specialized view is available for the protocol, it falls back to
/// the generic device view.
/// One sensor facet of a physical host, for the host-detail facet tabs (#133).
/// `DeviceId = (protocol, source)`, so a host running sysinfo+netlink+netring+logs
/// is four facets that share a `source`; the tabs let the protocol be a *facet of a
/// host* rather than a top-level navigation axis.
#[derive(Debug, Clone)]
pub struct FacetTab {
    /// The device handle, when this facet has actually published. `None` for a
    /// facet the correlator knows about but that has sent no telemetry: its
    /// origin is unknown, so it has no handle and cannot be opened (#474). It
    /// still shows, greyed, so the host's full sensor set stays visible.
    pub id: Option<DeviceId>,
    /// The member's `source` — the label shown when two facets share a producer.
    pub source: String,
    /// The producer name (a string since #1256 — a facet of a sensor the GUI
    /// was not compiled with is still a facet).
    pub producer: String,
    pub status: DeviceStatus,
    /// Whether this facet is the one currently open.
    pub active: bool,
}

impl FacetTab {
    /// A facet for a device we have actually heard from: protocol and source
    /// come off the handle, so they cannot drift from it.
    pub fn live(id: DeviceId, status: DeviceStatus, active: bool) -> Self {
        Self {
            producer: id.producer.clone(),
            source: id.source.clone(),
            id: Some(id),
            status,
            active,
        }
    }
}

/// Triage tint for a facet's status dot (green / amber / red / gray) — shared
/// status palette (D2).
fn facet_status_color(status: DeviceStatus) -> iced::Color {
    match status {
        DeviceStatus::Online => crate::view::theme::STATUS_ONLINE,
        DeviceStatus::Degraded => crate::view::theme::STATUS_DEGRADED,
        DeviceStatus::Offline => crate::view::theme::STATUS_OFFLINE,
        DeviceStatus::Unknown => crate::view::theme::STATUS_UNKNOWN,
    }
}

/// The host-detail facet tab strip (#133): one tab per sensor present on the host,
/// active tab highlighted, others click to switch facets. Returns `None` for a
/// single-sensor host (nothing to switch between).
fn facet_tab_strip(facets: &[FacetTab]) -> Option<Element<'static, Message>> {
    if facets.len() < 2 {
        return None;
    }
    let mut tabs = Row::new().spacing(8).align_y(Alignment::Center);
    tabs = tabs.push(text("Facets").size(font::BODY));
    // Same protocol on several facets (different sources correlated into one
    // host) → append the source so the tabs stay distinguishable.
    let dup_protocols =
        crate::view::host::duplicated_protocols(facets.iter().map(|f| f.producer.as_str()));
    for f in facets {
        // Copy out the data the widgets need so the strip owns it (Element<'static>)
        // and doesn't borrow `facets`.
        let status = f.status;
        let dot = container(text(""))
            .width(8)
            .height(8)
            .style(move |_: &Theme| container::Style {
                background: Some(iced::Background::Color(facet_status_color(status))),
                border: iced::Border::default().rounded(4.0),
                ..Default::default()
            });
        let mut label = row![
            dot,
            icons::for_producer::<Message>(&f.producer, IconSize::Small),
            text(crate::message::producer_display_name(&f.producer)).size(font::BODY),
        ]
        .spacing(5)
        .align_y(Alignment::Center);
        if dup_protocols.contains(f.producer.as_str()) {
            label = label.push(text(format!("· {}", f.source)).size(font::BODY).style(
                |t: &Theme| text::Style {
                    color: Some(crate::view::theme::colors(t).text_muted()),
                },
            ));
        }
        let tab = if f.active {
            button(label)
                .padding([4, 10])
                .style(iced::widget::button::primary)
        } else {
            // `on_press_maybe`: a facet with no handle (never published) is
            // rendered but inert — there is nothing to open.
            button(label)
                .padding([4, 10])
                .on_press_maybe(f.id.clone().map(Message::SelectDevice))
                .style(iced::widget::button::secondary)
        };
        tabs = tabs.push(tab);
    }
    Some(container(tabs).padding([8, 20]).into())
}

/// Everything the host-detail screen needs beyond [`DeviceDetailState`] (#350):
/// one bundle so later additions (e.g. artifact context, #351) append a field
/// instead of growing every signature on the path.
/// `'a` is the app-state lifetime the returned element may borrow from;
/// `'b` covers view-local slices (facets, filtered logs, entity lookup) that
/// are cloned into the element rather than borrowed.
pub struct DeviceViewCtx<'a, 'b> {
    pub state: &'a DeviceDetailState,
    pub syslog_filter: &'a specialized::SyslogFilterState,
    pub host_logs: &'b [specialized::SyslogMessage],
    pub facets: &'b [FacetTab],
    pub entity: Option<&'b HostEntity>,
    /// Whether the identity details (facts + resolution group) are expanded.
    /// Persisted app-side (`PersistentSettings::identity_expanded`).
    pub identity_expanded: bool,
    /// The app's shared artifact state, for contextual actions (#351) —
    /// e.g. the capture form on the netring Capture tab. `None` (tests, bare
    /// paths) renders those views without the in-context controls.
    pub artifact: Option<crate::view::artifact_fetch::ArtifactCtx<'a>>,
    /// Where the seeded history came from (#909). A `Local` source draws a
    /// caveat: the window on screen is one viewer's, and might be far shorter
    /// than the retention an operator has configured.
    pub history_source: crate::history::HistorySource,
}

/// Render the host-detail view (#133, single-bar since #350): ONE merged nav
/// bar (Back / prev / next / identity summary / exports) over the facet tab
/// strip over the active facet's nav-less content. The old layout stacked two
/// header layers (an always-expanded identity panel + the device nav header)
/// before any content — the identity facts + resolution group now live behind
/// a ▾/▸ toggle in the bar.
pub fn host_detail_view<'a>(ctx: DeviceViewCtx<'a, '_>) -> Element<'a, Message> {
    let identity = ctx.entity.map(|e| (e, ctx.identity_expanded));
    // The active facet's status gates the "Forget" affordance in the bar.
    let facet_status = ctx.facets.iter().find(|f| f.active).map(|f| f.status);
    let mut col =
        column![container(render_header(ctx.state, identity, facet_status)).padding([12, 20]),];
    if ctx.identity_expanded
        && let Some(entity) = ctx.entity
    {
        col = col.push(
            container(entity_identity_details(entity, &ctx.state.merge_target)).padding([0, 20]),
        );
    }
    if let Some(strip) = facet_tab_strip(ctx.facets) {
        col = col.push(strip);
    }
    // The caveat goes above the content, not inside a chart: it is true of
    // every series on the page, and a reader who scrolled past it would be
    // reading a five-minute window as five minutes of history.
    if let Some(caveat) = ctx.history_source.caveat() {
        col = col.push(
            container(
                text(caveat)
                    .size(font::CAPTION)
                    .style(|t: &Theme| text::Style {
                        color: Some(crate::view::theme::colors(t).status_warning()),
                    }),
            )
            .padding([4, 20]),
        );
    }
    col = col.push(rule::horizontal(1));
    col = col.push(device_content(
        ctx.state,
        ctx.syslog_filter,
        ctx.host_logs,
        ctx.artifact,
        ctx.entity,
    ));
    col.width(Length::Fill).height(Length::Fill).into()
}

/// The active facet's body WITHOUT navigation chrome — the host shell above
/// already renders the single merged bar (#350).
fn device_content<'a>(
    state: &'a DeviceDetailState,
    syslog_filter: &'a specialized::SyslogFilterState,
    host_logs: &[specialized::SyslogMessage],
    artifact: Option<crate::view::artifact_fetch::ArtifactCtx<'a>>,
    entity: Option<&HostEntity>,
) -> Element<'a, Message> {
    if state.device_id.is(Protocol::Logs) {
        return specialized::syslog_view(state, syslog_filter, host_logs);
    }
    if let Some(view) = specialized::specialized_view(state, artifact, entity) {
        return view;
    }
    generic_device_body(state)
}

/// The expanded identity details (#306/#350): identity facts (IPs / MACs /
/// vendor / platform / names) over the "Resolution group" drill-down that lists
/// each [`MemberClaim`] (`sensor/source · rule · confidence`) — the wrong-merge
/// diagnosis affordance — and, since #1129, the wrong-merge **repair**: one
/// "Split off" per non-canonical origin and a "Merge into" field, each a
/// `@catalog` `link`/`unlink` write. Rendered under the nav bar only when
/// expanded.
fn entity_identity_details(entity: &HostEntity, merge_target: &str) -> Element<'static, Message> {
    let mut col = column![].spacing(6);

    let mut facts: Vec<String> = Vec::new();
    if !entity.ips.is_empty() {
        facts.push(format!("IPs: {}", entity.ips.join(", ")));
    }
    if !entity.macs.is_empty() {
        facts.push(format!("MACs: {}", entity.macs.join(", ")));
    }
    if let Some(v) = &entity.vendor {
        facts.push(format!("vendor: {v}"));
    }
    if let Some(p) = &entity.platform {
        facts.push(format!("platform: {p}"));
    }
    if !entity.names.is_empty() {
        let names: Vec<String> = entity
            .names
            .iter()
            .map(|n| format!("{} ({})", n.name, n.provenance))
            .collect();
        facts.push(format!("names: {}", names.join(", ")));
    }
    for fact in facts {
        col = col.push(text(fact).size(font::CAPTION));
    }

    // Resolution-group drill-down: one row per member claim.
    col = col.push(text("Resolution group").size(font::BODY));
    for m in &entity.members {
        let row = text(format!(
            "{}/{} · {} · confidence {:.2}",
            m.sensor, m.source, m.rule, m.confidence
        ))
        .size(font::DENSE);
        col = col.push(row);
    }

    col = col.push(entity_assertion_controls(entity, merge_target));

    container(col).padding([4, 0]).into()
}

/// The origin an operator's assertion keeps — `host_id` when the catalog
/// resolved one, else the first origin the entity fused. `link`/`unlink`
/// name **origins** (`h-<12hex>`), never the evidence-derived entity id,
/// which changes shape the moment a merge does (RFC 06 §5.4).
pub fn canonical_origin(entity: &HostEntity) -> Option<&str> {
    entity
        .host_id
        .as_deref()
        .filter(|h| zenkey::grammar::is_valid_host_origin(h))
        .or_else(|| entity.origins.first().map(String::as_str))
}

/// The merge/split controls (#1129) — the write side of the workflow the
/// resolution group above only diagnoses. `@catalog`'s `link` and `unlink`
/// procedures shipped in 0.7.0 with no caller anywhere; the GUI consumed the
/// alias documents a merge produces and could never cause one.
///
/// - **Split off** — one per origin other than the canonical: `unlink` the
///   pair, so the catalog stops fusing that host into this entity. Rendered
///   only when the entity fused more than one origin; a single-origin entity
///   has nothing to split.
/// - **Merge into** — an origin typed by the operator; `link` this entity's
///   canonical origin *into* it. The button enables only for a well-formed
///   `h-<12hex>` that is not this entity's own — the catalog would refuse
///   both, and refusing locally is cheaper than a round trip to be told.
///
/// The refusal when the catalog's `allow_operator_assertions` is off comes
/// back named (`error/gated`, `refused_by`) and is rendered as such (#866).
fn entity_assertion_controls(entity: &HostEntity, merge_target: &str) -> Element<'static, Message> {
    let mut col = column![].spacing(crate::view::tokens::space::XS);
    let Some(canonical) = canonical_origin(entity) else {
        // No origin at all: nothing an assertion could name.
        return col.into();
    };
    let canonical = canonical.to_string();

    let others: Vec<&String> = entity.origins.iter().filter(|o| **o != canonical).collect();
    if !others.is_empty() {
        col = col.push(text("Origins").size(font::BODY));
        for origin in others {
            let old = origin.clone();
            let new = canonical.clone();
            col = col.push(
                row![
                    text(origin.clone()).size(font::DENSE),
                    button(text("Split off").size(font::DENSE))
                        .on_press(Message::UnlinkHosts { old, new })
                        .padding([2, 8]),
                ]
                .spacing(crate::view::tokens::space::SM)
                .align_y(Alignment::Center),
            );
        }
    }

    let target = merge_target.trim().to_string();
    let valid = zenkey::grammar::is_valid_host_origin(&target) && target != canonical;
    let merge = button(text("Merge into").size(font::DENSE)).padding([2, 8]);
    let merge = if valid {
        merge.on_press(Message::LinkHosts {
            old: canonical.clone(),
            new: target,
        })
    } else {
        merge
    };
    col = col.push(
        row![
            text_input("merge into origin h-…", merge_target)
                .on_input(Message::MergeTargetChanged)
                .size(font::DENSE)
                .width(Length::Fixed(220.0)),
            merge,
        ]
        .spacing(crate::view::tokens::space::SM)
        .align_y(Alignment::Center),
    );
    col.into()
}

/// The compact identity summary fragment for the nav bar (#350): entity-id
/// chip, live/stale freshness, "N sources · M IPs", and the ▾/▸ details toggle.
fn entity_identity_summary(entity: &HostEntity, expanded: bool) -> Element<'static, Message> {
    // Short entity-id chip.
    let id_chip = container(text(entity.entity_id.clone()).size(font::DENSE))
        .padding([2, 8])
        .style(container::rounded_box);

    // Staleness indicator vs the correlator's re-emit cadence.
    let now = current_timestamp();
    let stale = now - entity.last_updated > crate::entity::ENTITY_STALE_MS;
    let fresh_label = if stale { "stale" } else { "live" };
    let fresh_color = if stale {
        crate::view::theme::STATUS_UNKNOWN
    } else {
        crate::view::theme::STATUS_ONLINE
    };
    let freshness = text(fresh_label)
        .size(font::DENSE)
        .style(move |_: &Theme| text::Style {
            color: Some(fresh_color),
        });

    let summary = text(format!(
        "{} sources · {} IPs",
        entity.members.len(),
        entity.ips.len()
    ))
    .size(font::CAPTION);

    let toggle = button(
        row![
            text(if expanded { "▾" } else { "▸" }).size(font::DENSE),
            text("identity").size(font::DENSE),
        ]
        .spacing(4)
        .align_y(Alignment::Center),
    )
    .on_press(Message::ToggleIdentityDetails)
    .padding([2, 8])
    .style(iced::widget::button::text);

    row![id_chip, freshness, summary, toggle]
        .spacing(10)
        .align_y(Alignment::Center)
        .into()
}

pub fn device_view(state: &DeviceDetailState) -> Element<'_, Message> {
    // Try to use a specialized view for this protocol (bare path — no artifact
    // context, so contextual actions render their advert-less fallback).
    if let Some(specialized_view) = specialized::specialized_view(state, None, None) {
        // Wrap it with the shared nav header so every device screen has a Back
        // button + consistent chrome (specialized views don't render their own).
        return with_device_nav(state, specialized_view);
    }

    // Fall back to generic view
    generic_device_view(state)
}

/// Wrap a specialized device view with the shared navigation header (Back +
/// device identity + export buttons). Specialized views render only their domain
/// content, so this guarantees consistent navigation chrome across every device.
fn with_device_nav<'a>(
    state: &'a DeviceDetailState,
    content: Element<'a, Message>,
) -> Element<'a, Message> {
    column![
        container(render_header(state, None, None)).padding([12, 20]),
        rule::horizontal(1),
        content,
    ]
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

/// Render the device detail view with syslog filter state.
///
/// This is used when the device is a syslog source and we need to pass
/// the filter state for the specialized view.
pub fn device_view_with_syslog_filter<'a>(
    state: &'a DeviceDetailState,
    syslog_filter: &'a specialized::SyslogFilterState,
    host_logs: &[specialized::SyslogMessage],
) -> Element<'a, Message> {
    use zensight_common::Protocol;

    // For syslog devices, use the specialized view with filter state + the
    // host's recent log stream (so drilling in shows history, not just latest).
    if state.device_id.is(Protocol::Logs) {
        return with_device_nav(
            state,
            specialized::syslog_view(state, syslog_filter, host_logs),
        );
    }

    // For other protocols, use the standard device view
    device_view(state)
}

/// Render the generic device detail view (fallback for protocols without specialized views).
pub fn generic_device_view(state: &DeviceDetailState) -> Element<'_, Message> {
    let header = render_header(state, None, None);

    let content = column![header, rule::horizontal(1), generic_device_body(state)]
        .spacing(10)
        .padding(20);

    container(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// The generic view's body (chart + metrics list) without navigation chrome —
/// used directly by the host shell, which owns the single merged bar (#350).
fn generic_device_body(state: &DeviceDetailState) -> Element<'_, Message> {
    // Show chart if a metric is selected (single) or in comparison mode (multi)
    let chart_section = if let Some(ref metric_name) = state.selected_metric {
        render_chart_section(state, Some(metric_name))
    } else if state.is_comparison_mode() {
        render_chart_section(state, None)
    } else {
        column![].into()
    };

    let metrics = render_metrics_list(state);

    let mut body = column![]
        .spacing(crate::view::tokens::space::SM)
        .padding(crate::view::tokens::space::LG);
    if let Some(finding) = intake_findings(state) {
        body = body.push(finding);
    }
    body = body.push(chart_section);
    // A definition (#1259) says how the family model is shown; without one
    // the default renderer (#1258) shows every family it has instances of.
    match &state.definition {
        Some(def) => {
            let rendered = crate::view::definition::render(state, def);
            if let Some(panels) = render_panels(rendered.panels) {
                body = body.push(panels);
            }
            // A broken view looks broken (#1259): the marker is its own
            // widget, the detail beside it.
            for failure in rendered.failures {
                let detail = failure
                    .strip_prefix(crate::view::definition::FAILURE_MARKER)
                    .unwrap_or(&failure)
                    .trim_start_matches(':')
                    .trim()
                    .to_string();
                body = body.push(
                    row![
                        text(crate::view::definition::FAILURE_MARKER)
                            .size(font::CAPTION)
                            .style(|t: &Theme| text::Style {
                                color: Some(crate::view::theme::colors(t).danger_text()),
                            }),
                        text(detail).size(font::CAPTION).style(muted_caption),
                    ]
                    .spacing(crate::view::tokens::space::SM),
                );
            }
        }
        None => {
            if let Some(families) = render_panels(family_panels(state)) {
                body = body.push(families);
            }
        }
    }
    body = body.push(metrics);
    if let Some(procedures) = render_procedures(state) {
        body = body.push(procedures);
    }
    if !state.documents.is_empty() {
        body = body.push(render_documents(state));
    }
    if !state.events.is_empty() {
        body = body.push(render_events(state));
    }
    body.into()
}

/// How many rows of a reply the generic renderer draws.
const REPLY_ROWS: usize = 200;

/// The fields that identify a row of a reply, in order of preference.
const ID_KEYS: [&str; 8] = ["id", "pid", "name", "unit", "target", "key", "path", "host"];

/// The read procedures a device's slice declares and the GUI can call with
/// no request (#1261, design §5.5): a card per procedure with its reply
/// type and description, a call button, and the answer rendered as the
/// reply's own shape — rows of objects as a table, an object as facts. What
/// a producer the GUI was not compiled with can still be asked. `None`
/// when the slice declares nothing callable, or is unknown.
fn render_procedures(state: &DeviceDetailState) -> Option<Element<'_, Message>> {
    use crate::view::components::card;
    use crate::view::specialized::fetch::Fetch;
    let family = state.family.as_ref()?;
    let procedures: Vec<&crate::view::family::Procedure> = family.callable().collect();
    if procedures.is_empty() {
        return None;
    }
    let mut col =
        column![text("Procedures").size(font::EMPHASIS)].spacing(crate::view::tokens::space::SM);
    for procedure in procedures {
        let fetch = state.calls.fetch(&procedure.path);
        let mut head = row![text(procedure.path.clone()).size(font::BODY)]
            .spacing(crate::view::tokens::space::SM)
            .align_y(Alignment::Center);
        if let Some(reply) = &procedure.reply {
            head = head.push(text(reply.clone()).size(font::CAPTION).style(muted_caption));
        }
        let label = match fetch {
            Fetch::Idle => "Call",
            Fetch::Loading => "Calling…",
            Fetch::Ready(_) | Fetch::Error(_) => "Call again",
        };
        let mut call = button(text(label).size(font::DENSE))
            .padding([
                crate::view::tokens::space::XS,
                crate::view::tokens::space::SM,
            ])
            .style(iced::widget::button::secondary);
        if !fetch.is_loading() {
            call = call.on_press(Message::Call(crate::call::Request::new(
                procedure.path.clone(),
                String::new(),
            )));
        }
        head = head.push(call);
        let mut body = column![head].spacing(crate::view::tokens::space::XS);
        if let Some(description) = &procedure.description {
            body = body.push(
                text(description.clone())
                    .size(font::CAPTION)
                    .style(muted_caption),
            );
        }
        match fetch {
            Fetch::Error(error) => {
                body = body.push(
                    text(format!("call failed: {error}"))
                        .size(font::CAPTION)
                        .style(|t: &Theme| text::Style {
                            color: Some(crate::view::theme::colors(t).danger_text()),
                        }),
                );
            }
            Fetch::Ready(reply) => {
                let total = reply.items().len();
                if let Some(panel) = render_panels(vec![reply_panel(&procedure.path, reply)]) {
                    body = body.push(panel);
                }
                if total > REPLY_ROWS {
                    body = body.push(
                        text(format!("showing {REPLY_ROWS} of {total} rows"))
                            .size(font::CAPTION)
                            .style(muted_caption),
                    );
                }
                if let Some(page) = &reply.page
                    && page.partial
                {
                    // The envelope's word, not a guess (RFC 05 §3.2): a short
                    // page is not the end when the producer says it stopped.
                    let more = match &page.next_cursor {
                        Some(_) => "partial answer — the producer stopped early; more follows",
                        None => "partial answer — the producer could not cover what was asked",
                    };
                    body = body.push(text(more).size(font::CAPTION).style(muted_caption));
                }
                body = body.push(
                    text(format!("as of {}", format_timestamp(reply.received_ms)))
                        .size(font::MICRO)
                        .style(muted_caption),
                );
            }
            Fetch::Idle | Fetch::Loading => {}
        }
        col = col.push(card(body));
    }
    Some(col.into())
}

/// A reply as a family panel (#1261): rows of objects become a table whose
/// columns are the objects' keys and whose instance is the row's
/// identifying field (`id`, `pid`, `name`, … — else the first key, which
/// is alphabetical, because JSON objects carry no order); one object is a
/// facts list; anything else is one `value` row. The
/// default renderer draws it like any family, so a reply from a producer
/// the GUI has never heard of looks like everything else on the screen.
pub fn reply_panel(procedure: &str, reply: &crate::call::Reply) -> FamilyPanel {
    fn scalar(value: &serde_json::Value) -> String {
        const CLIP: usize = 80;
        match value {
            serde_json::Value::Null => "—".to_string(),
            serde_json::Value::Bool(true) => "yes".to_string(),
            serde_json::Value::Bool(false) => "no".to_string(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::String(s) => s.clone(),
            other => {
                let mut compact = other.to_string();
                if compact.len() > CLIP {
                    let cut = compact
                        .char_indices()
                        .map(|(i, _)| i)
                        .take_while(|&i| i <= CLIP)
                        .last()
                        .unwrap_or(0);
                    compact.truncate(cut);
                    compact.push('…');
                }
                compact
            }
        }
    }
    fn row(instance: String, cells: Vec<FamilyCell>) -> FamilyRow {
        FamilyRow {
            instance,
            cells,
            verdict: None,
            note: None,
            limits: None,
        }
    }
    let items = reply.items();
    let is_table = reply.value.is_array() || reply.page.is_some();
    let rows: Vec<FamilyRow> = items
        .iter()
        .take(REPLY_ROWS)
        .enumerate()
        .map(|(i, item)| match item.as_object() {
            Some(object) => {
                let cells: Vec<FamilyCell> = object
                    .iter()
                    .map(|(k, v)| FamilyCell {
                        field: k.clone(),
                        text: scalar(v),
                    })
                    .collect();
                let instance = if is_table {
                    ID_KEYS
                        .iter()
                        .find_map(|k| cells.iter().find(|c| c.field == *k))
                        .or_else(|| cells.first())
                        .map_or_else(|| i.to_string(), |c| c.text.clone())
                } else {
                    procedure.to_string()
                };
                row(instance, cells)
            }
            None => row(
                if is_table {
                    i.to_string()
                } else {
                    procedure.to_string()
                },
                vec![FamilyCell {
                    field: "value".to_string(),
                    text: scalar(item),
                }],
            ),
        })
        .collect();
    FamilyPanel {
        title: procedure.to_string(),
        group: None,
        is_table,
        rows,
    }
}

/// One rendered cell of a family row: the field and its presented value.
#[derive(Debug, Clone, PartialEq)]
pub struct FamilyCell {
    pub field: String,
    /// The value as shown — `41.5 Cel`, `100000 By/s`, `yes`, or the honest
    /// placeholder when the field has no reading yet.
    pub text: String,
}

/// One row of a family panel (#1258): an instance with its cells and, when
/// the default grading rule applies, the verdict on its graded reading.
#[derive(Debug, Clone, PartialEq)]
pub struct FamilyRow {
    pub instance: String,
    pub cells: Vec<FamilyCell>,
    /// `None` is "no limit declared, no verdict" — not "ok".
    pub verdict: Option<crate::view::components::limit_table::LimitVerdict>,
    /// A definition's `note` for this row (#1259), or the provenance of a
    /// literal limit; the default renderer sets none.
    pub note: Option<String>,
    /// The limits a graded reading was judged against, as text (#1260):
    /// `warn 75.0°C · crit 89.0°C`. The default renderer sets none.
    pub limits: Option<String>,
}

/// One family panel: its title, whether it is a table (rows per instance)
/// or a facts list (one instance, a row per field), and its rows.
#[derive(Debug, Clone, PartialEq)]
pub struct FamilyPanel {
    pub title: String,
    /// The binding of the definition's `group_by` variable this panel is
    /// the card for (#1260); shown before the title. `None` without one.
    pub group: Option<String>,
    pub is_table: bool,
    pub rows: Vec<FamilyRow>,
}

/// The default renderer's rows (#1258, design §5.4), as data: one panel per
/// family the device has instances of, derived from the producer's slice.
///
/// - a **table** per family with variables — a row per instance, a cell per
///   field, kinds and units formatted: a gauge as `41.5 Cel`, a counter as a
///   rate (`100000 By/s`, from the last two points; before there are two it
///   says so), a bool as `yes`/`no`, text as itself;
/// - a **facts** panel for a var-less family — one row per field;
/// - grading only by [`crate::view::family::default_grading`]: a sibling the
///   slice names as a limit, or nothing. A reading with no limit is shown
///   plainly and carries no verdict, exactly as `limit_table` does.
///
/// Pure, so the ratchet can assert on it without a simulator; the widget is
/// [`render_families`] over it.
pub fn family_panels(state: &DeviceDetailState) -> Vec<FamilyPanel> {
    use crate::view::components::limit_table::LimitRow;
    use crate::view::family::default_grading;
    let Some(model) = state.family.as_ref() else {
        return Vec::new();
    };
    let mut panels = Vec::new();
    for fi in model.instances(state.metrics.iter()) {
        let family = &model.families[fi.family];
        let grading = default_grading(family);
        let title = if family.path.is_empty() {
            model.producer.clone()
        } else {
            family.path.clone()
        };
        let mut rows = Vec::new();
        for instance in &fi.instances {
            let mut cells = Vec::new();
            // A closed family's columns are its declared fields, in
            // declaration order; an open one's are whatever the tails were.
            let field_names: Vec<String> = if family.open {
                instance.values.keys().cloned().collect()
            } else {
                family.fields.iter().map(|f| f.name.clone()).collect()
            };
            for name in field_names {
                let Some(point) = instance.point(&name) else {
                    continue;
                };
                let text = match family.field(&name) {
                    Some(field) => cell_text(state, field, instance, &name),
                    None => format_value_for_export(&point.value),
                };
                cells.push(FamilyCell { field: name, text });
            }
            let verdict = grading.as_ref().and_then(|g| {
                let reading = instance.number(&family.fields[g.reading].name);
                let warning = g
                    .warning
                    .and_then(|i| instance.number(&family.fields[i].name));
                let critical = g
                    .critical
                    .and_then(|i| instance.number(&family.fields[i].name));
                LimitRow::new("", reading, "")
                    .with_limits(warning, critical)
                    .verdict()
            });
            rows.push(FamilyRow {
                instance: instance.id.clone(),
                cells,
                verdict,
                note: None,
                limits: None,
            });
        }
        panels.push(FamilyPanel {
            title,
            group: None,
            is_table: family.is_table(),
            rows,
        });
    }
    panels
}

/// The cell a field shows by default (#1258): its declared presentation
/// over the instance's point — a gauge with its unit, a counter as a rate
/// (or the honest "rate after the next sample"), a bool as yes/no, text as
/// itself, a kind-less number with its unit.
pub fn cell_text(
    state: &DeviceDetailState,
    field: &crate::view::family::Field,
    instance: &crate::view::family::Instance,
    name: &str,
) -> String {
    use crate::view::family::Presentation;
    let Some(point) = instance.point(name) else {
        return String::new();
    };
    let unit = field.display_unit();
    match field.presentation() {
        Presentation::Rate => match counter_rate(state, &point.metric) {
            Some(rate) => with_unit(fmt_num(rate), &unit),
            None => format!(
                "{} total · rate after the next sample",
                with_unit(fmt_num(instance.number(name).unwrap_or(0.0)), &field.unit)
            ),
        },
        Presentation::State => match instance.state(name) {
            Some(true) => "yes".to_string(),
            Some(false) => "no".to_string(),
            None => format_value_for_export(&point.value),
        },
        // A field with no declared kind that carries a number is a reading
        // too — bmc declares units and no kinds.
        Presentation::Absolute | Presentation::Unknown => match instance.number(name) {
            Some(v) => with_unit(fmt_num(v), &unit),
            None => format_value_for_export(&point.value),
        },
        Presentation::Label | Presentation::Distribution => format_value_for_export(&point.value),
    }
}

/// A counter's rate from the last two points the view holds: the live
/// history first, the store's seeded samples when the view has just opened.
pub(crate) fn counter_rate(state: &DeviceDetailState, metric: &str) -> Option<f64> {
    use crate::view::family::{rate_between, rate_between_samples};
    if let Some(h) = state.history.get(metric)
        && h.len() >= 2
    {
        let mut it = h.iter().rev();
        let cur = it.next()?;
        let prev = it.next()?;
        return rate_between(prev, cur);
    }
    let seeded = state.seeded_history.get(metric)?;
    if seeded.len() < 2 {
        return None;
    }
    let cur = &seeded[seeded.len() - 1];
    let prev = &seeded[seeded.len() - 2];
    rate_between_samples((prev.ts, prev.value), (cur.ts, cur.value))
}

/// A number as a reading: integers without decimals, the rest to two.
fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        let s = format!("{v:.2}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

fn with_unit(value: String, unit: &Option<String>) -> String {
    match unit {
        Some(u) => format!("{value} {u}"),
        None => value,
    }
}

/// The family panels as widgets (#1258): a header per family, a header row
/// of field names, a row per instance with its cells and, when graded, the
/// verdict word in the verdict's colour — never a colour without the word.
pub(crate) fn render_panels<'a>(panels: Vec<FamilyPanel>) -> Option<Element<'a, Message>> {
    use crate::view::components::limit_table::LimitVerdict;
    if panels.is_empty() {
        return None;
    }
    let muted = |t: &Theme| text::Style {
        color: Some(crate::view::theme::colors(t).text_muted()),
    };
    let mut col = column![].spacing(crate::view::tokens::space::MD);
    for panel in panels {
        let heading = match &panel.group {
            Some(g) => format!("{g} · {}", panel.title),
            None => panel.title.clone(),
        };
        let mut section =
            column![text(heading).size(font::EMPHASIS)].spacing(crate::view::tokens::space::XS);
        if panel.is_table {
            let header_fields: Vec<String> = panel
                .rows
                .first()
                .map(|r| r.cells.iter().map(|c| c.field.clone()).collect())
                .unwrap_or_default();
            let mut header = row![
                text("instance")
                    .size(font::DENSE)
                    .width(Length::FillPortion(2))
                    .style(muted)
            ]
            .spacing(crate::view::tokens::space::SM);
            for f in header_fields {
                header = header.push(
                    text(f)
                        .size(font::DENSE)
                        .width(Length::FillPortion(2))
                        .style(muted),
                );
            }
            section = section.push(header);
        }
        for r in panel.rows {
            let verdict = r.verdict;
            let mut line = row![].spacing(crate::view::tokens::space::SM);
            if panel.is_table {
                line = line.push(
                    text(r.instance.clone())
                        .size(font::CAPTION)
                        .width(Length::FillPortion(2)),
                );
                for c in &r.cells {
                    line = line.push(
                        text(c.text.clone())
                            .size(font::CAPTION)
                            .width(Length::FillPortion(2)),
                    );
                }
            } else {
                // A facts family: one row per field, then the row's note.
                for c in &r.cells {
                    section = section.push(
                        row![
                            text(c.field.clone())
                                .size(font::CAPTION)
                                .width(Length::FillPortion(2))
                                .style(muted),
                            text(c.text.clone())
                                .size(font::CAPTION)
                                .width(Length::FillPortion(4)),
                        ]
                        .spacing(crate::view::tokens::space::SM),
                    );
                }
                if let Some(note) = r.note {
                    section = section.push(text(note).size(font::DENSE).style(muted));
                }
                continue;
            }
            if let Some(v) = verdict {
                let word = match v {
                    LimitVerdict::Ok => "ok",
                    LimitVerdict::Warning => "warning",
                    LimitVerdict::Critical => "critical",
                };
                line = line.push(
                    text(word)
                        .size(font::CAPTION)
                        .width(Length::FillPortion(1))
                        .style(move |t: &Theme| text::Style {
                            color: Some(v.color(t)),
                        }),
                );
            }
            section = section.push(line);
            if let Some(limits) = r.limits {
                section = section.push(text(limits).size(font::DENSE).style(muted));
            }
            if let Some(note) = r.note {
                section = section.push(text(note).size(font::DENSE).style(muted));
            }
        }
        col = col.push(crate::view::components::card(section));
    }
    Some(col.into())
}

fn muted_caption(t: &Theme) -> text::Style {
    text::Style {
        color: Some(crate::view::theme::colors(t).text_muted()),
    }
}

/// The exact words the "not declared" marker uses — one widget, one string,
/// so a test can find it and a reader can grep it.
pub const UNDECLARED_MARKER: &str = "not declared";

fn undeclared_marker<'a>() -> Element<'a, Message> {
    text(UNDECLARED_MARKER)
        .size(font::MICRO)
        .style(|t: &Theme| text::Style {
            color: Some(crate::view::theme::colors(t).warning()),
        })
        .into()
}

/// The intake findings for a device (#1256, gate 4): the producer declares
/// no slice at all, or declares a slice that does not cover what it
/// publishes. Each names the party at fault. `None` when there is nothing
/// to say — which is the normal case, and must render nothing.
fn intake_findings(state: &DeviceDetailState) -> Option<Element<'_, Message>> {
    let mut lines: Vec<Element<'_, Message>> = Vec::new();
    if state.slice_known == Some(false) {
        lines.push(
            text(format!(
                "{} publishes {} subject{} and declares no slice — nothing on the bus \
                 answered introspect for it, so none of them can be judged",
                state.device_id.producer,
                state.metrics.len(),
                if state.metrics.len() == 1 { "" } else { "s" },
            ))
            .size(font::CAPTION)
            .into(),
        );
    }
    if !state.undeclared.is_empty() {
        let listed: Vec<&str> = state
            .undeclared
            .subjects
            .iter()
            .map(String::as_str)
            .take(8)
            .collect();
        let more = state.undeclared.len().saturating_sub(listed.len());
        let tail = if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        };
        // The marker is its own widget — a badge, dot plus the exact words —
        // so it reads the same here as beside a row, and a test can find it.
        lines.push(
            row![
                crate::view::components::badge(
                    crate::view::theme::STATUS_UNKNOWN,
                    UNDECLARED_MARKER
                ),
                text(format!(
                    "{} subject{} published by {} and not declared by its slice: {}{tail}",
                    state.undeclared.len(),
                    if state.undeclared.len() == 1 { "" } else { "s" },
                    state.device_id.producer,
                    listed.join(", "),
                ))
                .size(font::CAPTION),
            ]
            .spacing(crate::view::tokens::space::SM)
            .align_y(Alignment::Center)
            .into(),
        );
    }
    if lines.is_empty() {
        return None;
    }
    let column = lines
        .into_iter()
        .fold(column![].spacing(crate::view::tokens::space::XS), |c, l| {
            c.push(l)
        });
    Some(
        container(column)
            .padding(crate::view::tokens::space::SM)
            .width(Length::Fill)
            .style(|t: &Theme| container::Style {
                background: Some(iced::Background::Color(
                    crate::view::theme::colors(t).background_weak(),
                )),
                border: iced::Border {
                    color: crate::view::theme::colors(t).warning(),
                    width: 1.0,
                    radius: 4.0.into(),
                },
                ..Default::default()
            })
            .into(),
    )
}

/// The state documents held for a device (#1256): one card per subject with
/// what the intake knows — the declared type or "untyped", the schema
/// verdict as the shared three-state badge, and whether the subject was
/// declared at all — above the value, pretty-printed and clipped.
fn render_documents(state: &DeviceDetailState) -> Element<'_, Message> {
    use crate::intake::Declared;
    use crate::view::components::badge;
    const CLIP: usize = 2_000;

    let mut col =
        column![text("Documents").size(font::EMPHASIS)].spacing(crate::view::tokens::space::SM);
    for doc in state.documents.values() {
        let type_label: Element<'_, Message> = match &doc.type_name {
            Some(ty) => text(ty.clone()).size(font::CAPTION).into(),
            None => text("untyped")
                .size(font::CAPTION)
                .style(|t: &Theme| text::Style {
                    color: Some(crate::view::theme::colors(t).text_muted()),
                })
                .into(),
        };
        let mut head = row![
            text(doc.subject.clone()).size(font::BODY),
            type_label,
            crate::view::components::verdict::verdict_badge(&doc.verdict),
        ]
        .spacing(crate::view::tokens::space::SM)
        .align_y(Alignment::Center);
        match doc.declared {
            Declared::Yes => {}
            Declared::No => {
                head = head.push(badge(crate::view::theme::STATUS_UNKNOWN, UNDECLARED_MARKER));
            }
            Declared::NoSlice => {
                head = head.push(badge(crate::view::theme::STATUS_UNKNOWN, "no slice"));
            }
        }
        let mut pretty = serde_json::to_string_pretty(&doc.value).unwrap_or_default();
        if pretty.len() > CLIP {
            let cut = pretty
                .char_indices()
                .map(|(i, _)| i)
                .take_while(|&i| i <= CLIP)
                .last()
                .unwrap_or(0);
            pretty.truncate(cut);
            pretty.push_str("\n…");
        }
        let card = column![
            head,
            text(pretty).size(font::CAPTION).font(iced::Font::MONOSPACE),
            text(format!("as of {}", format_timestamp(doc.received_ms)))
                .size(font::MICRO)
                .style(|t: &Theme| text::Style {
                    color: Some(crate::view::theme::colors(t).text_muted()),
                }),
        ]
        .spacing(crate::view::tokens::space::XS);
        col = col.push(crate::view::components::card(card));
    }
    col.into()
}

/// The events-class records held for a device (#1256): caption rows,
/// newest first, the value on one line.
fn render_events(state: &DeviceDetailState) -> Element<'_, Message> {
    const CLIP: usize = 200;
    let mut col =
        column![text("Events").size(font::EMPHASIS)].spacing(crate::view::tokens::space::XS);
    for ev in &state.events {
        let mut line = serde_json::to_string(&ev.value).unwrap_or_default();
        if line.len() > CLIP {
            let cut = line
                .char_indices()
                .map(|(i, _)| i)
                .take_while(|&i| i <= CLIP)
                .last()
                .unwrap_or(0);
            line.truncate(cut);
            line.push('…');
        }
        col = col.push(
            text(format!(
                "{} · {} · {}",
                format_timestamp(ev.received_ms),
                ev.subject,
                line
            ))
            .size(font::CAPTION),
        );
    }
    col.into()
}

/// Render the shared nav header: Back / prev / next / protocol icon / name /
/// metric count / exports — plus, on the host shell, the compact identity
/// summary with its ▾/▸ details toggle (#350).
fn render_header<'a>(
    state: &'a DeviceDetailState,
    identity: Option<(&HostEntity, bool)>,
    facet_status: Option<DeviceStatus>,
) -> Element<'a, Message> {
    let back_button = button(
        row![
            icons::arrow_left(IconSize::Medium),
            text("Back").size(font::BODY)
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .on_press(Message::ClearSelection)
    .style(iced::widget::button::secondary);

    // #35: step through the current filtered device set without returning to the
    // dashboard between hops.
    let prev_button = button(text("‹").size(font::EMPHASIS))
        .on_press(Message::SelectAdjacentDevice { forward: false })
        .padding([4, 10])
        .style(iced::widget::button::secondary);
    let next_button = button(text("›").size(font::EMPHASIS))
        .on_press(Message::SelectAdjacentDevice { forward: true })
        .padding([4, 10])
        .style(iced::widget::button::secondary);

    let protocol_icon = icons::for_producer(&state.device_id.producer, IconSize::Large);
    // On the host shell, prefer the entity's resolved name over the raw
    // per-sensor source id (#350).
    let display_name: &str = identity
        .and_then(|(e, _)| e.hostname.as_deref().or(e.fqdn.as_deref()))
        .unwrap_or(&state.device_id.source);
    let device_name = text(display_name.to_string()).size(font::TITLE);
    let identity_summary: Option<Element<'static, Message>> =
        identity.map(|(entity, expanded)| entity_identity_summary(entity, expanded));
    let metric_count = text(format!("{} metrics", state.metrics.len())).size(font::BODY);

    let csv_button = button(
        row![
            icons::export(IconSize::Small),
            text("CSV").size(font::CAPTION)
        ]
        .spacing(4)
        .align_y(Alignment::Center),
    )
    .on_press(Message::ExportToCsv)
    .style(iced::widget::button::secondary);

    let json_button = button(
        row![
            icons::export(IconSize::Small),
            text("JSON").size(font::CAPTION)
        ]
        .spacing(4)
        .align_y(Alignment::Center),
    )
    .on_press(Message::ExportToJson)
    .style(iced::widget::button::secondary);

    // An Offline facet is likely stale (e.g. a one-off sensor run that will
    // never come back) — offer to drop it from the in-memory device map. It
    // reappears automatically if telemetry resumes.
    let forget_button: Option<Element<'a, Message>> = (facet_status == Some(DeviceStatus::Offline))
        .then(|| {
            let btn = button(text("Forget").size(font::CAPTION))
                .on_press(Message::ForgetDevice(state.device_id.clone()))
                .padding([2, 8])
                .style(iced::widget::button::text);
            tooltip(
                btn,
                container(
                    text("Remove this stale facet; it returns if telemetry resumes.")
                        .size(font::DENSE),
                )
                .padding(6)
                .style(container::rounded_box),
                tooltip::Position::Bottom,
            )
            .into()
        });

    // "Focus this host" (#476): drop the fleet subscriptions and declare
    // `zensight/v1/<origin>/**` instead. On a constrained link a technician
    // debugging one host otherwise pays for every host's telemetry to reach
    // their laptop; the v1 grammar put the origin at a fixed position, which is
    // what makes "this host and nothing else" expressible at all.
    let focus_button: Element<'a, Message> = match (&state.origin, state.focused) {
        (_, true) => tooltip(
            button(text("Exit focus").size(font::CAPTION))
                .on_press(Message::SetFocusHost(None))
                .padding([2, 8])
                .style(iced::widget::button::primary),
            container(text("Resubscribe to the whole fleet.").size(font::DENSE))
                .padding(6)
                .style(container::rounded_box),
            tooltip::Position::Bottom,
        )
        .into(),
        (Some(origin), false) => tooltip(
            button(text("Focus this host").size(font::CAPTION))
                .on_press(Message::SetFocusHost(Some(origin.clone())))
                .padding([2, 8])
                .style(iced::widget::button::secondary),
            container(
                text(
                    "Subscribe to this host only. Fleet telemetry stops crossing \
                     the link until you exit — the point of focus on a constrained \
                     link.",
                )
                .size(font::DENSE),
            )
            .padding(6)
            .style(container::rounded_box),
            tooltip::Position::Bottom,
        )
        .into(),
        // Origin not learned yet: disabled (no `on_press`) rather than guessing.
        (None, false) => tooltip(
            button(text("Focus this host").size(font::CAPTION))
                .padding([2, 8])
                .style(iced::widget::button::secondary),
            container(text("Waiting for this host's identity (health doc).").size(font::DENSE))
                .padding(6)
                .style(container::rounded_box),
            tooltip::Position::Bottom,
        )
        .into(),
    };

    let mut bar = row![
        back_button,
        prev_button,
        next_button,
        protocol_icon,
        device_name,
    ]
    .spacing(15)
    .align_y(Alignment::Center);
    if let Some(summary) = identity_summary {
        bar = bar.push(summary);
    }
    bar = bar
        .push(metric_count)
        .push(focus_button)
        .push(csv_button)
        .push(json_button);
    if let Some(forget) = forget_button {
        bar = bar.push(forget);
    }
    bar.into()
}

/// Render the chart section.
fn render_chart_section<'a>(
    state: &'a DeviceDetailState,
    metric_name: Option<&'a str>,
) -> Element<'a, Message> {
    // Chart header with close button and time window buttons
    let close_button = button(icons::close(IconSize::Small))
        .on_press(Message::Chart(chart::Action::ClearSelection))
        .style(iced::widget::button::secondary);

    // Title depends on mode
    let title_text = if state.is_comparison_mode() {
        format!("Comparing {} metrics", state.comparison_count())
    } else if let Some(name) = metric_name {
        name.to_string()
    } else {
        "Chart".to_string()
    };

    let chart_title = row![
        icons::chart(IconSize::Medium),
        text(title_text).size(font::BODY)
    ]
    .spacing(6)
    .align_y(Alignment::Center);

    // Time window buttons
    let time_buttons: Element<'_, Message> = Row::with_children(
        TimeWindow::all()
            .iter()
            .map(|&window| {
                let is_selected = state.chart.time_window() == window;
                let btn = button(text(window.label()).size(font::DENSE))
                    .on_press(Message::Chart(chart::Action::SetTimeWindow(window)))
                    .style(if is_selected {
                        iced::widget::button::primary
                    } else {
                        iced::widget::button::secondary
                    });
                btn.into()
            })
            .collect::<Vec<_>>(),
    )
    .spacing(5)
    .into();

    // Custom relative window input (#36): "last N minutes", overrides presets.
    let custom_input = text_input("min", &state.chart_custom_input)
        .on_input(|v| Message::Chart(chart::Action::SetCustomMinutes(v)))
        .width(Length::Fixed(64.0))
        .size(font::DENSE);
    let custom_window = row![text("Custom:").size(font::DENSE), custom_input]
        .spacing(4)
        .align_y(Alignment::Center);

    // Expand/collapse the chart height (#36): no more fixed 200px sliver.
    let expand_button = button(
        text(if state.chart_expanded {
            "Collapse"
        } else {
            "Expand"
        })
        .size(font::DENSE),
    )
    .on_press(Message::Chart(chart::Action::ToggleExpand))
    .style(iced::widget::button::secondary);

    let header = row![
        chart_title,
        time_buttons,
        custom_window,
        expand_button,
        close_button
    ]
    .spacing(15)
    .align_y(Alignment::Center);

    // Absolute from/to range picker (#36): load an exact past window from the
    // store, e.g. "2026-06-26 14:05" → "2026-06-26 14:12", in the viewer's
    // LOCAL zone since #1123 — everything an operator reads a timestamp from
    // is local, and typing a UTC instant into one field on a page of local
    // ones is a conversion nobody should do in their head.
    let range_active = state.chart.absolute_range().is_some();
    let from_input = text_input("YYYY-MM-DD HH:MM", &state.chart_from_input)
        .on_input(|v| Message::Chart(chart::Action::SetRangeFrom(v)))
        .on_submit(Message::Chart(chart::Action::ApplyRange))
        .width(Length::Fixed(150.0))
        .size(font::DENSE);
    let to_input = text_input("YYYY-MM-DD HH:MM", &state.chart_to_input)
        .on_input(|v| Message::Chart(chart::Action::SetRangeTo(v)))
        .on_submit(Message::Chart(chart::Action::ApplyRange))
        .width(Length::Fixed(150.0))
        .size(font::DENSE);
    let apply_btn = button(text("Apply").size(font::DENSE))
        .on_press(Message::Chart(chart::Action::ApplyRange))
        .style(if range_active {
            iced::widget::button::primary
        } else {
            iced::widget::button::secondary
        });
    let mut range_row = row![
        text("Range (local):").size(font::DENSE),
        from_input,
        text("→").size(font::DENSE),
        to_input,
        apply_btn,
    ]
    .spacing(6)
    .align_y(Alignment::Center);
    if range_active {
        range_row = range_row.push(
            button(text("Clear").size(font::DENSE))
                .on_press(Message::Chart(chart::Action::ClearRange))
                .style(iced::widget::button::text),
        );
    }

    // Inline per-series legend with toggle/remove (#36) — manage a comparison
    // without switching back to the metrics table.
    let legend: Element<'_, Message> = if state.is_comparison_mode() {
        let mut legend_row = Row::new().spacing(12).align_y(Alignment::Center);
        for series in state.chart.series() {
            let swatch_color = crate::view::components::kit::rgb(series.color);
            let swatch = container(text(""))
                .width(10)
                .height(10)
                .style(move |_t: &Theme| container::Style {
                    background: Some(iced::Background::Color(swatch_color)),
                    border: iced::Border::default().rounded(2.0),
                    ..Default::default()
                });
            let name = series.name.clone();
            let toggle =
                button(text(if series.visible { "shown" } else { "hidden" }).size(font::MICRO))
                    .on_press(Message::Chart(chart::Action::ToggleVisibility(
                        name.clone(),
                    )))
                    .style(iced::widget::button::text);
            let remove = button(text("×").size(font::CAPTION))
                .on_press(Message::Chart(chart::Action::RemoveMetric(name.clone())))
                .style(iced::widget::button::text);
            legend_row = legend_row.push(
                row![swatch, text(name).size(font::DENSE), toggle, remove]
                    .spacing(4)
                    .align_y(Alignment::Center),
            );
        }
        legend_row.into()
    } else {
        column![].into()
    };

    // Default to a usable height; expand for detailed inspection. The custom
    // window doesn't change height — only the visible time range.
    let chart_height = if state.chart_expanded { 520.0 } else { 320.0 };
    let chart: Element<'_, Message> = chart_view(&state.chart, chart_height);

    // Stats row
    let stats = state.chart.stats();
    let stats_row = row![
        text(format!(
            "Current: {}",
            stats.current.map_or("-".to_string(), format_value)
        ))
        .size(font::CAPTION),
        text(format!("Min: {}", format_value(stats.min))).size(font::CAPTION),
        text(format!("Max: {}", format_value(stats.max))).size(font::CAPTION),
        text(format!("Avg: {}", format_value(stats.avg))).size(font::CAPTION),
        text(format!("Points: {}", stats.count)).size(font::CAPTION),
    ]
    .spacing(20);

    let chart_container = container(
        column![header, range_row, legend, chart, stats_row]
            .spacing(10)
            .padding(10),
    )
    .style(|theme: &Theme| {
        let colors = crate::view::theme::colors(theme);
        container::Style {
            background: Some(iced::Background::Color(colors.card_background())),
            border: iced::Border {
                color: colors.border(),
                width: 1.0,
                radius: 6.0.into(),
            },
            ..Default::default()
        }
    })
    .width(Length::Fill);

    column![chart_container, rule::horizontal(1)]
        .spacing(10)
        .into()
}

/// Convert metrics to table rows.
fn build_metric_table_rows(state: &DeviceDetailState) -> Vec<MetricTableRow> {
    state
        .sorted_metrics()
        .into_iter()
        .map(|(name, point)| {
            let (value, full_value) = format_value_display_with_full(&point.value);
            let trend = if let Some(history) = state.history.get(name) {
                if history.len() > 1 {
                    compute_trend(history)
                } else {
                    String::new()
                }
            } else {
                String::new()
            };

            let is_stale = (current_timestamp() - point.timestamp) > METRIC_STALE_THRESHOLD_MS;

            MetricTableRow {
                name: name.to_string(),
                value,
                full_value,
                type_name: value_type_name(&point.value).to_string(),
                timestamp: format_timestamp(point.timestamp),
                is_chartable: state.is_metric_chartable(name),
                is_in_chart: state.is_metric_in_chart(name),
                declared: !state.undeclared.metrics.contains(name),
                is_favorite: state.is_favorite(name),
                trend,
                is_stale,
                device_id: state.device_id.clone(),
                numeric_value: match &point.value {
                    TelemetryValue::Counter(v) => Some(*v as f64),
                    TelemetryValue::Gauge(v) => Some(*v),
                    _ => None,
                },
            }
        })
        .collect()
}

/// Compute trend direction from history. Panic-proof: reads the last two points
/// via a reverse iterator, so any history length (0, 1, …) is handled by the
/// `else` branch rather than by indexing.
fn compute_trend(history: &VecDeque<TelemetryPoint>) -> String {
    let mut recent = history.iter().rev();
    let (Some(last), Some(prev)) = (recent.next(), recent.next()) else {
        return String::new();
    };

    match (&last.value, &prev.value) {
        (TelemetryValue::Gauge(a), TelemetryValue::Gauge(b)) => {
            if a > b {
                "↑".to_string()
            } else if a < b {
                "↓".to_string()
            } else {
                "→".to_string()
            }
        }
        (TelemetryValue::Counter(a), TelemetryValue::Counter(b)) => {
            if a > b {
                "↑".to_string()
            } else if a < b {
                "↓".to_string()
            } else {
                "→".to_string()
            }
        }
        _ => String::new(),
    }
}

/// Render the list of all metrics using a table widget.
fn render_metrics_list(state: &DeviceDetailState) -> Element<'_, Message> {
    let total_count = state.total_metric_count();
    let table_rows = build_metric_table_rows(state);
    let filtered_count = table_rows.len();

    // Search filter input (with ID for keyboard focus)
    let search_input = text_input("Search metrics... (Ctrl+F)", state.filter_input())
        .id(DEVICE_SEARCH_ID.clone())
        .on_input(|v| Message::Chart(chart::Action::SetMetricFilter(v)))
        .size(font::BODY)
        .padding(8)
        .width(Length::Fixed(300.0));

    // Count indicator
    let count_text = if state.metric_filter.is_empty() {
        text(format!("{} metrics", total_count)).size(font::CAPTION)
    } else {
        text(format!("{} of {} metrics", filtered_count, total_count)).size(font::CAPTION)
    };

    let search_row = row![search_input, count_text]
        .spacing(15)
        .align_y(Alignment::Center);

    if total_count == 0 {
        return column![search_row, empty_state("No metrics received yet…", None)]
            .spacing(10)
            .into();
    }

    if table_rows.is_empty() {
        return column![search_row, empty_state("No metrics match the filter", None)]
            .spacing(10)
            .into();
    }

    // Build table columns
    // Note: closures consume MetricTableRow, so we clone strings for owned values

    // Favorite/pin column (#27): a star toggles whether the metric is pinned to
    // the top of the table; persisted per device across restarts.
    let favorite_column = table::column(
        text("").size(font::CAPTION),
        |row: MetricTableRow| -> Element<'_, Message> {
            let is_fav = row.is_favorite;
            let glyph = if is_fav { "★" } else { "☆" };
            button(
                text(glyph)
                    .size(font::BODY)
                    .style(move |theme: &Theme| text::Style {
                        color: Some(if is_fav {
                            crate::view::theme::ACCENT_GOLD
                        } else {
                            crate::view::theme::colors(theme).text_dimmed()
                        }),
                    }),
            )
            .on_press(Message::Chart(chart::Action::ToggleFavorite(row.name)))
            .style(iced::widget::button::text)
            .padding(0)
            .into()
        },
    )
    .width(34);

    let name_column = table::column(
        text("Metric").size(font::CAPTION),
        |row: MetricTableRow| -> Element<'_, Message> {
            let name = row.name.clone();
            let name_display = row.name;
            // Make the name clickable to select for chart
            let label: Element<'_, Message> = if row.is_chartable {
                button(text(name_display).size(font::CAPTION))
                    .on_press(Message::Chart(chart::Action::SelectMetric(name)))
                    .style(if row.is_in_chart {
                        iced::widget::button::primary
                    } else {
                        iced::widget::button::text
                    })
                    .padding(0)
                    .into()
            } else {
                text(name_display).size(font::CAPTION).into()
            };
            if row.declared {
                return label;
            }
            // The honesty marker (#1256): a subject the producer's own slice
            // does not declare is shown, and says so — never silently
            // absent, never silently the same as a declared one.
            row![label, undeclared_marker()]
                .spacing(crate::view::tokens::space::SM)
                .align_y(Alignment::Center)
                .into()
        },
    )
    .width(Length::FillPortion(3));

    let value_column = table::column(
        text("Value").size(font::CAPTION),
        |row: MetricTableRow| -> Element<'_, Message> {
            let value = row.value;
            let is_stale = row.is_stale;
            let value_widget = text(value).size(font::CAPTION).style(move |theme: &Theme| {
                if is_stale {
                    text::Style {
                        color: Some(crate::view::theme::colors(theme).text_dimmed()),
                    }
                } else {
                    text::Style::default()
                }
            });
            if let Some(full) = row.full_value {
                tooltip(
                    value_widget,
                    container(text(full).size(font::DENSE))
                        .padding(6)
                        .max_width(400.0)
                        .style(container::rounded_box),
                    tooltip::Position::Bottom,
                )
                .into()
            } else {
                value_widget.into()
            }
        },
    )
    .width(Length::FillPortion(2));

    let type_column = table::column(
        text("Type").size(font::CAPTION),
        |row: MetricTableRow| -> Element<'_, Message> {
            let type_name = row.type_name;
            text(type_name)
                .size(font::DENSE)
                .style(|theme: &Theme| text::Style {
                    color: Some(crate::view::theme::colors(theme).text_dimmed()),
                })
                .into()
        },
    )
    .width(80);

    let trend_column = table::column(
        text("Trend").size(font::CAPTION),
        |row: MetricTableRow| -> Element<'_, Message> {
            let trend = row.trend;
            let color = match trend.as_str() {
                "↑" => crate::view::theme::STATUS_ONLINE,
                "↓" => crate::view::theme::STATUS_OFFLINE,
                _ => crate::view::theme::STATUS_UNKNOWN,
            };
            text(trend)
                .size(font::BODY)
                .style(move |_: &Theme| text::Style { color: Some(color) })
                .into()
        },
    )
    .width(50);

    let time_column = table::column(
        text("Updated").size(font::CAPTION),
        |row: MetricTableRow| -> Element<'_, Message> {
            let timestamp = row.timestamp;
            let is_stale = row.is_stale;
            if is_stale {
                row![
                    text(timestamp)
                        .size(font::DENSE)
                        .style(|theme: &Theme| text::Style {
                            color: Some(crate::view::theme::colors(theme).text_dimmed()),
                        }),
                    text("stale")
                        .size(font::MICRO)
                        .style(|_theme: &Theme| text::Style {
                            color: Some(crate::view::theme::ACCENT_STALE),
                        })
                ]
                .spacing(4)
                .align_y(Alignment::Center)
                .into()
            } else {
                text(timestamp)
                    .size(font::DENSE)
                    .style(|theme: &Theme| text::Style {
                        color: Some(crate::view::theme::colors(theme).text_dimmed()),
                    })
                    .into()
            }
        },
    )
    .width(120);

    let actions_column = table::column(
        text("").size(font::CAPTION),
        |row: MetricTableRow| -> Element<'_, Message> {
            if row.is_chartable {
                let metric_name = row.name.clone();
                let chart_btn =
                    button(text(if row.is_in_chart { "−" } else { "+" }).size(font::DENSE))
                        .on_press(if row.is_in_chart {
                            Message::Chart(chart::Action::RemoveMetric(metric_name))
                        } else {
                            Message::Chart(chart::Action::AddMetric(metric_name))
                        })
                        .style(if row.is_in_chart {
                            iced::widget::button::danger
                        } else {
                            iced::widget::button::secondary
                        })
                        .padding([2, 8]);

                // Promote this metric to an alert rule (#50): seeds the rule form
                // with the metric path + current value and opens the authoring view.
                let alert_btn = button(text("alert").size(font::MICRO))
                    .on_press(Message::PromoteMetricToAlert {
                        device: row.device_id.clone(),
                        metric: row.name.clone(),
                        value: row.numeric_value.unwrap_or(0.0),
                    })
                    .style(iced::widget::button::secondary)
                    .padding([2, 8]);

                row![chart_btn, alert_btn].spacing(4).into()
            } else {
                text("").into()
            }
        },
    )
    .width(110);

    let metrics_table = table(
        [
            favorite_column,
            name_column,
            value_column,
            type_column,
            trend_column,
            time_column,
            actions_column,
        ],
        table_rows,
    )
    .padding(6)
    .padding_y(4);

    column![
        search_row,
        scrollable(metrics_table)
            .width(Length::Fill)
            .height(Length::Fill)
    ]
    .spacing(10)
    .into()
}

/// Format a telemetry value for display.
/// Returns (display_text, Option<full_text>) - full_text is Some if truncated.
fn format_value_display_with_full(value: &TelemetryValue) -> (String, Option<String>) {
    match value {
        TelemetryValue::Counter(v) => (format!("{}", v), None),
        TelemetryValue::Gauge(v) => {
            let display = if v.fract() == 0.0 {
                format!("{:.0}", v)
            } else {
                format!("{:.2}", v)
            };
            (display, None)
        }
        TelemetryValue::Text(s) => {
            if s.len() > 50 {
                (format!("{}...", &s[..47]), Some(s.clone()))
            } else {
                (s.clone(), None)
            }
        }
        TelemetryValue::Boolean(b) => (if *b { "true" } else { "false" }.to_string(), None),
        TelemetryValue::Binary(data) => (format!("<{} bytes>", data.len()), None),
    }
}

/// Get the type name for a telemetry value.
fn value_type_name(value: &TelemetryValue) -> &'static str {
    match value {
        TelemetryValue::Counter(_) => "counter",
        TelemetryValue::Gauge(_) => "gauge",
        TelemetryValue::Text(_) => "text",
        TelemetryValue::Boolean(_) => "bool",
        TelemetryValue::Binary(_) => "binary",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The honesty finding (#1256, gate 4): a device with an undeclared
    /// subject renders the marker; one without renders no such word.
    /// A reply renders as its own shape (#1261): a list of objects is a
    /// table keyed by the first field, one object is a facts list, an
    /// envelope's rows are its `items`, and a long answer is capped.
    #[test]
    fn a_reply_is_a_table_of_objects_or_a_facts_list() {
        use crate::call::Reply;
        use serde_json::json;
        let list = Reply::new(
            json!([{ "pid": 42, "name": "redis", "cpu": 1.5, "user": null, "live": true }]),
            0,
        );
        let panel = reply_panel("processes", &list);
        assert!(panel.is_table);
        assert_eq!(panel.title, "processes");
        assert_eq!(panel.rows[0].instance, "42");
        let cells: Vec<(&str, &str)> = panel.rows[0]
            .cells
            .iter()
            .map(|c| (c.field.as_str(), c.text.as_str()))
            .collect();
        assert!(cells.contains(&("name", "redis")));
        assert!(cells.contains(&("cpu", "1.5")));
        assert!(
            cells.contains(&("user", "—")),
            "null is shown as absent, not as `null`"
        );
        assert!(cells.contains(&("live", "yes")));

        let one = Reply::new(json!({ "available": false, "window_secs": 10 }), 0);
        let panel = reply_panel("latency", &one);
        assert!(!panel.is_table);
        assert_eq!(panel.rows.len(), 1);
        assert_eq!(panel.rows[0].instance, "latency");
        assert_eq!(panel.rows[0].cells[0].text, "no");

        let page = Reply::new(
            json!({ "items": [{ "id": "a" }, { "id": "b" }], "partial": true }),
            0,
        );
        let panel = reply_panel("events", &page);
        assert!(panel.is_table);
        assert_eq!(panel.rows.len(), 2);

        let long: Vec<serde_json::Value> = (0..300).map(|i| json!({ "i": i })).collect();
        let panel = reply_panel("many", &Reply::new(json!(long), 0));
        assert_eq!(panel.rows.len(), REPLY_ROWS);

        let scalar = Reply::new(json!(7), 0);
        let panel = reply_panel("count", &scalar);
        assert_eq!(panel.rows[0].cells[0].field, "value");
        assert_eq!(panel.rows[0].cells[0].text, "7");
    }

    /// The generic view offers every callable read procedure of the slice,
    /// and draws the answer, the failure, or the fact that it is on its way
    /// (#1261). `sysinfo`'s slice declares `processes`, `latency` and
    /// `thresholds` without a request type; `thresholds/set` needs one and
    /// `introspect` is the GUI's own, so neither is offered.
    #[test]
    fn the_generic_view_offers_the_slice_s_read_procedures() {
        use iced_test::simulator;
        let mut state = DeviceDetailState::new(DeviceId::fixture("sysinfo", "server01"));
        state.family = crate::view::family::FamilyModel::for_producer("sysinfo");
        let offered: Vec<&str> = state
            .family
            .as_ref()
            .unwrap()
            .callable()
            .map(|p| p.path.as_str())
            .collect();
        assert_eq!(offered, ["processes", "latency", "thresholds"]);

        let mut ui = simulator(render_procedures(&state).expect("callable procedures"));
        assert!(ui.find("Procedures").is_ok());
        assert!(ui.find("LatencyReport").is_ok(), "the reply type is named");
        let _ = ui.click("Call");
        let msgs: Vec<Message> = ui.into_messages().collect();
        assert!(
            msgs.iter().any(|m| matches!(
                m,
                Message::Call(r) if r.procedure == "processes" && r.params.is_empty()
            )),
            "the first card's button calls its procedure with no params"
        );

        state.calls.loading("latency", "");
        let mut ui = simulator(render_procedures(&state).unwrap());
        assert!(ui.find("Calling…").is_ok());
        drop(ui);

        state
            .calls
            .set_ready("latency", "", serde_json::json!({ "available": false }));
        let mut ui = simulator(render_procedures(&state).unwrap());
        assert!(ui.find("available").is_ok());
        assert!(ui.find("no").is_ok());
        assert!(ui.find("Call again").is_ok());
        drop(ui);

        state
            .calls
            .set_failed("thresholds", "No sysinfo sensor responded");
        let mut ui = simulator(render_procedures(&state).unwrap());
        assert!(ui.find("call failed: No sysinfo sensor responded").is_ok());

        // A device whose producer the GUI does not know offers nothing —
        // there is no slice to read procedures from.
        let unknown = DeviceDetailState::new(DeviceId::fixture("fake-sensor", "rack7"));
        assert!(render_procedures(&unknown).is_none());
    }

    #[test]
    fn undeclared_marker_renders_only_when_there_is_something_undeclared() {
        use iced_test::simulator;
        let mut state = DeviceDetailState::new(DeviceId::fixture("fake-sensor", "rack7"));
        state.update(make_test_point("rack7/temp/inlet/celsius"));
        state.update(make_test_point("rack7/humidity/pct"));
        state.slice_known = Some(true);
        {
            let mut ui = simulator(generic_device_view(&state));
            assert!(
                ui.find(UNDECLARED_MARKER).is_err(),
                "nothing undeclared, no marker"
            );
        }

        state
            .undeclared
            .insert("rack7/humidity/pct", "rack7/humidity/pct");
        let mut ui = simulator(generic_device_view(&state));
        assert!(ui.find(UNDECLARED_MARKER).is_ok(), "the finding renders");
    }

    /// No slice at all is a finding about the fleet, worded as such — and
    /// only once a sweep has answered (`Some(false)`), never before.
    #[test]
    fn no_slice_banner_names_the_fleet_not_the_producer() {
        use iced_test::simulator;
        let mut state = DeviceDetailState::new(DeviceId::fixture("fake-sensor", "rack7"));
        state.update(make_test_point("rack7/temp/inlet/celsius"));
        {
            let mut ui = simulator(generic_device_view(&state));
            assert!(
                ui.find("fake-sensor publishes 1 subject and declares no slice — nothing on the bus answered introspect for it, so none of them can be judged").is_err(),
                "no sweep yet, no finding"
            );
        }
        state.slice_known = Some(false);
        let mut ui = simulator(generic_device_view(&state));
        assert!(
            ui.find("fake-sensor publishes 1 subject and declares no slice — nothing on the bus answered introspect for it, so none of them can be judged").is_ok()
        );
    }

    /// A held document renders its subject, its type (or "untyped"), and
    /// the shared three-state verdict badge — never a boolean.
    #[test]
    fn document_card_shows_type_and_verdict() {
        use crate::intake::{Declared, DocumentState};
        use iced_test::simulator;
        use zensight_common::schema::{NotValidated, Verdict};
        let mut state = DeviceDetailState::new(DeviceId::fixture("fake-sensor", "rack7"));
        state.documents.insert(
            "rack7/status".into(),
            DocumentState {
                subject: "rack7/status".into(),
                type_name: Some("FakeUnitStatus".into()),
                value: serde_json::json!({"mode": "run"}),
                verdict: Verdict::NotValidated(NotValidated::NoSchema),
                declared: Declared::Yes,
                received_ms: 0,
                typed: Default::default(),
            },
        );
        state.documents.insert(
            "rack7/mystery".into(),
            DocumentState {
                subject: "rack7/mystery".into(),
                type_name: None,
                value: serde_json::json!({}),
                verdict: Verdict::NotValidated(NotValidated::NoSchema),
                declared: Declared::No,
                received_ms: 0,
                typed: Default::default(),
            },
        );
        let mut ui = simulator(generic_device_view(&state));
        assert!(ui.find("Documents").is_ok());
        assert!(ui.find("rack7/status").is_ok());
        assert!(ui.find("FakeUnitStatus").is_ok());
        assert!(ui.find("rack7/mystery").is_ok());
        assert!(ui.find("untyped").is_ok());
        assert!(
            ui.find(UNDECLARED_MARKER).is_ok(),
            "the undeclared document says so"
        );
        assert!(
            ui.find(crate::view::components::verdict::verdict_label(
                &Verdict::NotValidated(NotValidated::NoSchema)
            ))
            .is_ok(),
            "the verdict badge carries the shared label"
        );
    }

    /// The default renderer (#1258): a family with variables is a table
    /// whose rows carry the reading with its declared unit and, where the
    /// slice names a limit, the verdict word — and only there.
    #[test]
    fn a_family_table_renders_readings_units_and_verdicts() {
        use crate::view::components::limit_table::LimitVerdict;
        use iced_test::simulator;
        let mut state = DeviceDetailState::new(DeviceId::fixture("bmc", "bmc01"));
        for (metric, v) in [
            ("bmc01/thermal/inlet/celsius", 41.0),
            ("bmc01/thermal/inlet/upper_warning_c", 35.0),
            ("bmc01/thermal/inlet/upper_critical_c", 89.0),
            ("bmc01/thermal/exhaust/celsius", 70.0),
            ("bmc01/psu/1/input_watts", 210.0),
            ("bmc01/psu/1/present", 1.0),
        ] {
            state.metrics.insert(
                metric.to_string(),
                TelemetryPoint::new("bmc01", metric.to_string(), TelemetryValue::Gauge(v)),
            );
        }
        state.family = crate::view::family::FamilyModel::for_producer("bmc");
        let panels = family_panels(&state);
        let thermal = panels
            .iter()
            .find(|p| p.title == "{chassis}/thermal/{sensor}")
            .expect("thermal panel");
        assert!(thermal.is_table);
        let inlet = thermal
            .rows
            .iter()
            .find(|r| r.instance == "bmc01/inlet")
            .unwrap();
        assert!(inlet.cells.iter().any(|c| c.text == "41 Cel"));
        assert_eq!(
            inlet.verdict,
            Some(LimitVerdict::Warning),
            "41 against warn 35 / crit 89"
        );
        let exhaust = thermal
            .rows
            .iter()
            .find(|r| r.instance == "bmc01/exhaust")
            .unwrap();
        assert_eq!(exhaust.verdict, None, "no limit published — no verdict");
        let psu = panels
            .iter()
            .find(|p| p.title == "{chassis}/psu/{psu}")
            .unwrap();
        assert_eq!(
            psu.rows[0].verdict, None,
            "capacity is a limit only when a definition says so"
        );
        // bmc declares no `kind` on `present`, so the model cannot call it a
        // bool and the cell shows the number as read — `1`, not `yes`. A
        // slice that says `kind = "bool"` gets the word; this one says nothing.
        assert!(
            psu.rows[0]
                .cells
                .iter()
                .any(|c| c.field == "present" && c.text == "1")
        );

        let mut ui = simulator(generic_device_view(&state));
        assert!(ui.find("41 Cel").is_ok());
        assert!(ui.find("warning").is_ok());
        assert!(ui.find("210 W").is_ok());
    }

    /// A counter is a rate once there are two points, and says so until then.
    #[test]
    fn a_counter_row_is_a_rate_or_says_it_is_not_yet() {
        use iced_test::simulator;
        let mut state = DeviceDetailState::new(DeviceId::fixture("fake-sensor", "rack7"));
        state.family = Some(crate::view::family::FamilyModel::from_slice(
            &zenkey::slice::parse_slice(crate::mock::fake_sensor::SLICE).unwrap(),
        ));
        let mut p1 = TelemetryPoint::new(
            "rack7",
            "rack7/uplink/rx_bytes",
            TelemetryValue::Counter(1_000_000),
        );
        p1.timestamp = 1_700_000_000_000;
        state.update(p1.clone());
        let one = family_panels(&state);
        let cell = &one[0].rows[0].cells[0];
        assert_eq!(cell.field, "uplink/rx_bytes");
        assert!(
            cell.text.contains("rate after the next sample"),
            "one point is not a rate: {}",
            cell.text
        );

        let mut p2 = p1.clone();
        p2.timestamp += 10_000;
        p2.value = TelemetryValue::Counter(2_000_000);
        state.update(p2);
        let two = family_panels(&state);
        assert_eq!(two[0].rows[0].cells[0].text, "100000 By/s");
        let mut ui = simulator(generic_device_view(&state));
        assert!(ui.find("100000 By/s").is_ok());
    }

    /// A var-less family is a facts panel, one row per field; a producer
    /// with no model renders no panel and the flat list stands alone.
    #[test]
    fn facts_render_as_a_list_and_no_model_renders_no_panel() {
        use iced_test::simulator;
        let mut state = DeviceDetailState::new(DeviceId::fixture("pve", "pve01"));
        state.metrics.insert(
            "cluster/quorate".into(),
            TelemetryPoint::new("pve01", "cluster/quorate", TelemetryValue::Gauge(1.0)),
        );
        assert!(family_panels(&state).is_empty(), "no model, no panels");
        state.family = crate::view::family::FamilyModel::for_producer("pve");
        let panels = family_panels(&state);
        assert_eq!(panels.len(), 1);
        assert_eq!(panels[0].title, "cluster");
        assert!(!panels[0].is_table);
        assert_eq!(panels[0].rows[0].cells[0].field, "quorate");
        let mut ui = simulator(generic_device_view(&state));
        assert!(ui.find("quorate").is_ok());
    }

    #[test]
    fn favorites_toggle_and_pin_to_top_of_sorted_metrics() {
        let mut state = DeviceDetailState::new(DeviceId::fixture("snmp", "test".to_string()));
        for m in ["zzz", "aaa", "mmm"] {
            state.update(make_test_point(m));
        }
        // Default order is alphabetical.
        let order: Vec<&str> = state
            .sorted_metrics()
            .iter()
            .map(|(n, _)| n.as_str())
            .collect();
        assert_eq!(order, vec!["aaa", "mmm", "zzz"]);

        // Favoriting "zzz" pins it to the top; the rest stay alphabetical.
        assert!(state.toggle_favorite("zzz"));
        assert!(state.is_favorite("zzz"));
        let order: Vec<&str> = state
            .sorted_metrics()
            .iter()
            .map(|(n, _)| n.as_str())
            .collect();
        assert_eq!(order, vec!["zzz", "aaa", "mmm"]);

        // Toggling again unpins it (back to alphabetical).
        assert!(!state.toggle_favorite("zzz"));
        assert!(!state.is_favorite("zzz"));
        let order: Vec<&str> = state
            .sorted_metrics()
            .iter()
            .map(|(n, _)| n.as_str())
            .collect();
        assert_eq!(order, vec!["aaa", "mmm", "zzz"]);

        // A projected favorites set is honoured.
        state.set_favorites(["aaa".to_string()].into_iter().collect());
        let order: Vec<&str> = state
            .sorted_metrics()
            .iter()
            .map(|(n, _)| n.as_str())
            .collect();
        assert_eq!(order, vec!["aaa", "mmm", "zzz"]);
    }

    #[test]
    fn apply_chart_range_pins_window_and_returns_bounds() {
        let mut state = DeviceDetailState::new(DeviceId::fixture("snmp", "test".to_string()));
        // Valid from < to → pins the chart window and returns the bounds.
        state.chart_from_input = "2026-06-26 14:05".to_string();
        state.chart_to_input = "2026-06-26 14:12".to_string();
        let range = state.apply_chart_range();
        assert_eq!(range, Some((1_782_482_700_000, 1_782_483_120_000)));
        assert_eq!(state.chart.absolute_range(), range);

        // Inverted range → no-op (nothing returned, window unchanged).
        state.chart_from_input = "2026-06-26 15:00".to_string();
        state.chart_to_input = "2026-06-26 14:00".to_string();
        assert_eq!(state.apply_chart_range(), None);
        assert_eq!(state.chart.absolute_range(), range);

        // Unparseable → no-op.
        state.chart_from_input = "nope".to_string();
        assert_eq!(state.apply_chart_range(), None);

        // Clearing resets inputs + the pinned window.
        state.clear_chart_range();
        assert!(state.chart.absolute_range().is_none());
        assert!(state.chart_from_input.is_empty());
    }

    fn make_test_point(metric: &str) -> TelemetryPoint {
        TelemetryPoint {
            timestamp: 1000,
            source: "test".to_string(),
            metric: metric.to_string(),
            value: TelemetryValue::Gauge(42.0),
            labels: std::collections::HashMap::new(),
            unit: None,
        }
    }

    /// #126: counters, gauges and booleans are chartable (booleans as a 0/1 step
    /// series); text/binary and unknown metrics are not.
    #[test]
    fn booleans_and_numbers_are_chartable() {
        let device_id = DeviceId::fixture("netlink", "h".to_string());
        let mut state = DeviceDetailState::new(device_id);
        let mk = |metric: &str, value: TelemetryValue| {
            let mut p = make_test_point(metric);
            p.value = value;
            p
        };
        state.update(mk("iface/eth0/up", TelemetryValue::Boolean(true)));
        state.update(mk("cpu/usage", TelemetryValue::Gauge(1.0)));
        state.update(mk("rx/bytes", TelemetryValue::Counter(10)));
        state.update(mk("daemon/info", TelemetryValue::Text("hi".into())));

        assert!(state.is_metric_chartable("iface/eth0/up"));
        assert!(state.is_metric_chartable("cpu/usage"));
        assert!(state.is_metric_chartable("rx/bytes"));
        assert!(!state.is_metric_chartable("daemon/info"));
        assert!(!state.is_metric_chartable("unknown"));
    }

    #[test]
    fn test_history_values_returns_trailing_numeric_series() {
        let device_id = DeviceId::fixture("sysinfo", "h".to_string());
        let mut state = DeviceDetailState::new(device_id);
        for (ts, v) in [(1, 10.0), (2, 20.0), (3, 30.0), (4, 40.0)] {
            let mut p = make_test_point("cpu/usage");
            p.timestamp = ts;
            p.value = TelemetryValue::Gauge(v);
            state.update(p);
        }
        // Last 2 of 4 samples, oldest-first.
        assert_eq!(state.history_values("cpu/usage", 2), vec![30.0, 40.0]);
        // Unknown metric → empty.
        assert!(state.history_values("nope", 10).is_empty());
    }

    #[test]
    fn test_history_export_is_time_series_not_snapshot() {
        let device_id = DeviceId::fixture("snmp", "test".to_string());
        let mut state = DeviceDetailState::new(device_id);

        // Three samples of the same metric over time.
        for (ts, v) in [(1000, 1.0), (2000, 2.0), (3000, 3.0)] {
            let mut p = make_test_point("cpu/usage");
            p.timestamp = ts;
            p.value = TelemetryValue::Gauge(v);
            state.update(p);
        }

        assert!(state.has_history());
        let csv = state.export_history_to_csv();
        // header + 3 data rows (the trend), not a single snapshot row.
        let rows = csv.lines().count();
        assert_eq!(rows, 4, "expected header + 3 samples, got:\n{csv}");
        assert!(csv.contains("1000,"));
        assert!(csv.contains("3000,"));

        // The latest-snapshot export keeps only one row per metric.
        let snapshot = state.export_to_csv();
        assert_eq!(snapshot.lines().count(), 2);
    }

    #[test]
    fn test_metric_filter_empty_returns_all() {
        let device_id = DeviceId::fixture("snmp", "test".to_string());
        let mut state = DeviceDetailState::new(device_id);

        state.update(make_test_point("cpu/usage"));
        state.update(make_test_point("memory/used"));
        state.update(make_test_point("disk/io"));

        // Empty filter returns all metrics
        assert_eq!(state.sorted_metrics().len(), 3);
        assert_eq!(state.total_metric_count(), 3);
    }

    #[test]
    fn test_metric_filter_substring_match() {
        let device_id = DeviceId::fixture("snmp", "test".to_string());
        let mut state = DeviceDetailState::new(device_id);

        state.update(make_test_point("cpu/usage"));
        state.update(make_test_point("cpu/temperature"));
        state.update(make_test_point("memory/used"));
        state.update(make_test_point("disk/io"));

        // Filter for "cpu" should return 2 metrics (after applying)
        state.set_metric_filter("cpu".to_string());
        // Directly set the applied filter for testing
        state.metric_filter = state.pending_filter.clone();

        let filtered = state.sorted_metrics();
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|(name, _)| name.contains("cpu")));

        // Total count should still be 4
        assert_eq!(state.total_metric_count(), 4);
    }

    #[test]
    fn test_metric_filter_case_insensitive() {
        let device_id = DeviceId::fixture("snmp", "test".to_string());
        let mut state = DeviceDetailState::new(device_id);

        state.update(make_test_point("CPU/Usage"));
        state.update(make_test_point("memory/used"));

        // Filter should be case-insensitive (apply immediately for testing)
        state.set_metric_filter("cpu".to_string());
        state.metric_filter = state.pending_filter.clone();
        assert_eq!(state.sorted_metrics().len(), 1);

        state.set_metric_filter("CPU".to_string());
        state.metric_filter = state.pending_filter.clone();
        assert_eq!(state.sorted_metrics().len(), 1);

        state.set_metric_filter("CpU".to_string());
        state.metric_filter = state.pending_filter.clone();
        assert_eq!(state.sorted_metrics().len(), 1);
    }

    #[test]
    fn test_metric_filter_debounce() {
        let device_id = DeviceId::fixture("snmp", "test".to_string());
        let mut state = DeviceDetailState::new(device_id);

        state.update(make_test_point("cpu/usage"));
        state.update(make_test_point("memory/used"));

        // Set filter - should not apply immediately
        state.set_metric_filter("cpu".to_string());
        assert_eq!(state.filter_input(), "cpu");
        assert_eq!(state.metric_filter, ""); // Not applied yet

        // Simulate time passing by setting an old timestamp
        state.pending_filter_time = current_timestamp() - SEARCH_DEBOUNCE_MS - 1;

        // Now apply should work
        assert!(state.apply_pending_filter());
        assert_eq!(state.metric_filter, "cpu");
        assert_eq!(state.sorted_metrics().len(), 1);
    }

    fn point_at(metric: &str, value: f64, ts: i64) -> TelemetryPoint {
        TelemetryPoint {
            timestamp: ts,
            source: "test".to_string(),
            metric: metric.to_string(),
            value: TelemetryValue::Gauge(value),
            labels: std::collections::HashMap::new(),
            unit: None,
        }
    }

    fn device() -> DeviceDetailState {
        DeviceDetailState::new(DeviceId::fixture("snmp", "test".to_string()))
    }

    #[test]
    fn seeded_history_prepended_to_chart() {
        let mut state = device();
        // Live history starts at ts=5000.
        state.update(point_at("cpu", 50.0, 5_000));
        state.update(point_at("cpu", 55.0, 6_000));
        // Pre-restart samples from the store, older than live.
        state.seed_history(vec![(
            "cpu".to_string(),
            vec![
                zensight_store::Sample {
                    ts: 1_000,
                    value: 10.0,
                },
                zensight_store::Sample {
                    ts: 2_000,
                    value: 20.0,
                },
            ],
        )]);
        state.select_metric("cpu".to_string());
        let pts = state.chart.data();
        // 2 seeded + 2 live, oldest first.
        assert_eq!(pts.len(), 4);
        assert_eq!(pts[0].timestamp, 1_000);
        assert_eq!(pts[0].value, 10.0);
        assert_eq!(pts[3].timestamp, 6_000);
    }

    #[test]
    fn seeded_history_overlap_excluded_by_live() {
        let mut state = device();
        state.update(point_at("cpu", 50.0, 2_000));
        // Seed includes a sample at the same ts as live (2000) and one after — both
        // are at/after the live start so the live point wins (no duplicate at 2000).
        state.seed_history(vec![(
            "cpu".to_string(),
            vec![
                zensight_store::Sample {
                    ts: 1_000,
                    value: 10.0,
                },
                zensight_store::Sample {
                    ts: 2_000,
                    value: 99.0,
                },
                zensight_store::Sample {
                    ts: 3_000,
                    value: 99.0,
                },
            ],
        )]);
        state.select_metric("cpu".to_string());
        let pts = state.chart.data();
        // Only the seeded ts=1000 (before live start) + the single live point.
        assert_eq!(pts.len(), 2);
        assert_eq!(pts[0].timestamp, 1_000);
        assert_eq!(pts[1].timestamp, 2_000);
        assert_eq!(pts[1].value, 50.0);
    }

    #[test]
    fn seed_history_refreshes_open_chart() {
        let mut state = device();
        state.update(point_at("cpu", 50.0, 5_000));
        state.select_metric("cpu".to_string());
        assert_eq!(state.chart.data().len(), 1);
        // Seeding after the chart is open refreshes it in place.
        state.seed_history(vec![(
            "cpu".to_string(),
            vec![zensight_store::Sample {
                ts: 1_000,
                value: 10.0,
            }],
        )]);
        assert_eq!(state.chart.data().len(), 2);
    }
}

#[cfg(test)]
mod chart_actions {
    use super::*;
    use crate::view::chart::{Action, Effect};

    fn device() -> DeviceDetailState {
        DeviceDetailState::new(DeviceId::new("sysinfo", "h-000000000001", "host01"))
    }

    /// The favorite flip reports its new state so the app can keep the
    /// persisted set — the one chart action with a side effect elsewhere.
    #[test]
    fn toggle_favorite_reports_new_state() {
        let mut d = device();
        assert_eq!(
            d.apply_chart(Action::ToggleFavorite("cpu/usage".into())),
            Effect::Favorite {
                metric: "cpu/usage".into(),
                now_fav: true
            }
        );
        assert!(d.is_favorite("cpu/usage"));
        assert_eq!(
            d.apply_chart(Action::ToggleFavorite("cpu/usage".into())),
            Effect::Favorite {
                metric: "cpu/usage".into(),
                now_fav: false
            }
        );
    }

    /// A range that does not parse is a warning, not a load; one that does
    /// pins the window and asks for that slice.
    #[test]
    fn apply_range_invalid_warns_valid_loads() {
        let mut d = device();
        assert_eq!(
            d.apply_chart(Action::SetRangeFrom("nope".into())),
            Effect::None
        );
        assert_eq!(d.apply_chart(Action::ApplyRange), Effect::InvalidRange);
        assert_eq!(
            d.apply_chart(Action::SetRangeFrom("2026-09-22 10:00".into())),
            Effect::None
        );
        assert_eq!(
            d.apply_chart(Action::SetRangeTo("2026-09-22 11:00".into())),
            Effect::None
        );
        match d.apply_chart(Action::ApplyRange) {
            Effect::LoadRange { from, to } => assert!(from < to),
            other => panic!("expected a load, got {other:?}"),
        }
        assert_eq!(d.apply_chart(Action::ClearRange), Effect::None);
        assert!(d.chart_from_input.is_empty());
    }

    /// The pure interactions change state and ask nothing of the app.
    #[test]
    fn navigation_actions_have_no_effect() {
        let mut d = device();
        for a in [
            Action::ZoomIn,
            Action::ZoomOut,
            Action::ZoomReset,
            Action::PanLeft,
            Action::PanRight,
            Action::PanReset,
            Action::DragStart(1.0),
            Action::DragUpdate(5.0, 100.0),
            Action::DragEnd,
            Action::ToggleExpand,
            Action::SetMetricFilter("cpu".into()),
        ] {
            assert_eq!(d.apply_chart(a), Effect::None);
        }
        assert!(d.chart_expanded);
    }
}
