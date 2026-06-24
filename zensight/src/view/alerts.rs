//! Alerts view for threshold-based notifications.

use std::collections::{HashMap, HashSet, VecDeque};

use iced::widget::{
    Column, Row, column, container, pick_list, row, rule, scrollable, text, text_input, tooltip,
};
use iced::{Alignment, Element, Length, Theme};
use iced_anim::widget::button;

use zensight_common::{Alert as SensorAlert, AlertState as SensorAlertState, Protocol};

use crate::message::{DeviceId, Message};
use crate::view::components::{badge, section_header};
use crate::view::formatting::{format_timestamp, format_value};
use crate::view::icons::{self, IconSize};
use crate::view::tokens::{font, space};

/// Current wall-clock time in epoch milliseconds.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Alert rule definition.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AlertRule {
    /// Unique rule ID.
    pub id: u32,
    /// Rule name.
    pub name: String,
    /// Device ID pattern (None = all devices).
    pub device_pattern: Option<String>,
    /// Protocol filter (None = all protocols).
    pub protocol: Option<Protocol>,
    /// Metric name pattern.
    pub metric_pattern: String,
    /// Comparison operator.
    pub operator: ComparisonOp,
    /// Threshold value.
    pub threshold: f64,
    /// Severity level for triggered alerts.
    pub severity: Severity,
    /// Whether this rule is enabled.
    pub enabled: bool,
}

impl AlertRule {
    /// Create a new alert rule.
    pub fn new(id: u32, name: impl Into<String>, metric_pattern: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            device_pattern: None,
            protocol: None,
            metric_pattern: metric_pattern.into(),
            operator: ComparisonOp::GreaterThan,
            threshold: 0.0,
            severity: Severity::Warning,
            enabled: true,
        }
    }

    /// Set the severity for this rule (builder pattern).
    pub fn with_severity(mut self, severity: Severity) -> Self {
        self.severity = severity;
        self
    }

    /// Check if a metric matches this rule.
    pub fn matches(&self, device_id: &DeviceId, metric: &str) -> bool {
        // Check protocol filter
        if let Some(ref proto) = self.protocol
            && device_id.protocol != *proto
        {
            return false;
        }

        // Check device pattern
        if let Some(ref pattern) = self.device_pattern
            && !device_id.source.contains(pattern)
        {
            return false;
        }

        // Check metric pattern (simple contains match)
        metric.contains(&self.metric_pattern)
    }

    /// Evaluate if the value triggers this rule.
    pub fn evaluate(&self, value: f64) -> bool {
        if !self.enabled {
            return false;
        }

        match self.operator {
            ComparisonOp::GreaterThan => value > self.threshold,
            ComparisonOp::GreaterOrEqual => value >= self.threshold,
            ComparisonOp::LessThan => value < self.threshold,
            ComparisonOp::LessOrEqual => value <= self.threshold,
            ComparisonOp::Equal => (value - self.threshold).abs() < f64::EPSILON,
            ComparisonOp::NotEqual => (value - self.threshold).abs() >= f64::EPSILON,
        }
    }
}

/// Alert severity levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Severity {
    /// Informational alert.
    Info,
    /// Warning that may need attention.
    #[default]
    Warning,
    /// Critical issue requiring immediate attention.
    Critical,
}

impl Severity {
    /// All severity levels.
    pub const ALL: &'static [Severity] = &[Severity::Info, Severity::Warning, Severity::Critical];

    /// Get the display name for this severity.
    pub fn name(&self) -> &'static str {
        match self {
            Severity::Info => "Info",
            Severity::Warning => "Warning",
            Severity::Critical => "Critical",
        }
    }

    /// Get the color for this severity (RGB).
    pub fn color(&self) -> iced::Color {
        match self {
            Severity::Info => iced::Color::from_rgb(0.3, 0.6, 1.0), // Blue
            Severity::Warning => iced::Color::from_rgb(1.0, 0.7, 0.0), // Orange
            Severity::Critical => iced::Color::from_rgb(1.0, 0.2, 0.2), // Red
        }
    }
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

impl From<zensight_common::AlertSeverity> for Severity {
    fn from(s: zensight_common::AlertSeverity) -> Self {
        use zensight_common::AlertSeverity;
        match s {
            AlertSeverity::Info => Severity::Info,
            AlertSeverity::Warning => Severity::Warning,
            AlertSeverity::Critical => Severity::Critical,
        }
    }
}

/// Comparison operators for alert rules — shared with the sensors' headless
/// `metric-threshold` expectations (see `zensight_common::ComparisonOp`).
pub use zensight_common::ComparisonOp;

/// A triggered alert.
#[derive(Debug, Clone)]
pub struct Alert {
    /// Alert ID (unique).
    pub id: u64,
    /// Rule that triggered this alert.
    pub rule_id: u32,
    /// Rule name.
    pub rule_name: String,
    /// Device that triggered.
    pub device_id: DeviceId,
    /// Metric name.
    pub metric: String,
    /// Value that triggered.
    pub value: f64,
    /// Threshold that was crossed.
    pub threshold: f64,
    /// Operator.
    pub operator: ComparisonOp,
    /// Severity level.
    pub severity: Severity,
    /// When the alert was triggered (Unix epoch ms).
    pub timestamp: i64,
    /// Whether this alert has been acknowledged.
    pub acknowledged: bool,
}

impl Alert {
    /// Create a new alert.
    pub fn new(
        id: u64,
        rule: &AlertRule,
        device_id: DeviceId,
        metric: String,
        value: f64,
        timestamp: i64,
    ) -> Self {
        Self {
            id,
            rule_id: rule.id,
            rule_name: rule.name.clone(),
            device_id,
            metric,
            value,
            threshold: rule.threshold,
            operator: rule.operator,
            severity: rule.severity,
            timestamp,
            acknowledged: false,
        }
    }

    /// Format the alert message.
    pub fn message(&self) -> String {
        format!(
            "{}/{}: {} {} {} (threshold: {})",
            self.device_id.protocol,
            self.device_id.source,
            self.metric,
            self.operator.symbol(),
            format_value(self.value),
            format_value(self.threshold)
        )
    }
}

/// State for the alerts system.
#[derive(Debug, Default)]
pub struct AlertsState {
    /// Alert rules.
    pub rules: Vec<AlertRule>,
    /// Triggered alerts (most recent first).
    pub alerts: Vec<Alert>,
    /// Next rule ID.
    next_rule_id: u32,
    /// Next alert ID.
    next_alert_id: u64,
    /// Maximum alerts to keep.
    pub max_alerts: usize,
    /// Recently alerted (device+metric -> last alert time) to prevent spam.
    recent_alerts: HashMap<String, i64>,
    /// Cooldown between alerts for same metric (ms).
    pub alert_cooldown_ms: i64,
    /// Form state for adding new rule.
    pub new_rule_name: String,
    /// Form state for metric pattern.
    pub new_rule_metric: String,
    /// Form state for threshold.
    pub new_rule_threshold: String,
    /// Form state for operator.
    pub new_rule_operator: ComparisonOp,
    /// Form state for severity.
    pub new_rule_severity: Severity,
    /// Number of unacknowledged alerts.
    pub unacknowledged_count: usize,
    /// Test result message (None if not tested, Some(result) if tested).
    pub test_result: Option<String>,
    /// Sensor-pushed alerts (anomalies + expectation violations), keyed by the
    /// alert's stable `alert_key`. Lifecycle-managed: firing inserts/updates,
    /// resolved removes. Rendered alongside rule-triggered alerts (Plan 07).
    pub external: HashMap<String, SensorAlert>,
    /// `alert_key`s of external alerts the user has acknowledged. Acknowledged
    /// alerts stay visible (dimmed) but drop out of the active count / badge.
    acknowledged_external: HashSet<String>,
    /// Silenced sources (#26, Alertmanager model): `source` -> expiry epoch ms.
    /// While silenced, that source's incidents are hidden and excluded from the
    /// active count, with a muted-count chip surfaced instead.
    silenced_sources: HashMap<String, i64>,
    /// Per-`alert_key` incident timeline: firing→resolved transitions (#26).
    /// Bounded to the most recent transitions so it never grows unbounded.
    timelines: HashMap<String, VecDeque<TransitionEvent>>,
}

/// One firing/resolved transition in an incident's timeline (#26).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransitionEvent {
    /// The state entered at this transition.
    pub state: SensorAlertState,
    /// When it happened (epoch ms).
    pub at: i64,
}

/// Max transitions kept per incident timeline (bounded buffer).
const MAX_TIMELINE_EVENTS: usize = 32;

/// A group of firing external alerts from one source (an "incident"), for the
/// grouped/acknowledge-able anomalies feed.
pub struct ExternalIncident<'a> {
    pub source: &'a str,
    /// Alerts in this group, severity-then-recency order.
    pub alerts: Vec<&'a SensorAlert>,
    /// How many of them are not yet acknowledged.
    pub unacked: usize,
    /// Highest severity in the group.
    pub top_severity: Option<zensight_common::AlertSeverity>,
}

/// Outcome of ingesting a sensor-pushed alert, so the app can decide whether to
/// raise a toast (new), stay quiet (update), or toast a recovery (resolved).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalAlertOutcome {
    New,
    Updated,
    Resolved,
    /// A resolve for an alert we weren't tracking — ignored.
    Unknown,
}

impl AlertsState {
    /// Create a new alerts state.
    pub fn new() -> Self {
        Self::with_max_alerts(100)
    }

    /// Create a new alerts state with configurable max alerts.
    pub fn with_max_alerts(max_alerts: usize) -> Self {
        Self {
            rules: Vec::new(),
            alerts: Vec::new(),
            next_rule_id: 1,
            next_alert_id: 1,
            max_alerts,
            recent_alerts: HashMap::new(),
            alert_cooldown_ms: 60_000, // 1 minute
            new_rule_name: String::new(),
            new_rule_metric: String::new(),
            new_rule_threshold: String::new(),
            new_rule_operator: ComparisonOp::GreaterThan,
            new_rule_severity: Severity::Warning,
            unacknowledged_count: 0,
            test_result: None,
            external: HashMap::new(),
            acknowledged_external: HashSet::new(),
            silenced_sources: HashMap::new(),
            timelines: HashMap::new(),
        }
    }

    /// Ingest a sensor-pushed alert. Firing alerts are inserted/updated by
    /// `alert_key`; resolved alerts are removed. Returns what happened so the
    /// caller can toast appropriately.
    pub fn ingest_external(&mut self, alert: SensorAlert) -> ExternalAlertOutcome {
        let key = alert.alert_key();
        match alert.state {
            SensorAlertState::Resolved => {
                if self.external.remove(&key).is_some() {
                    self.record_transition(&key, SensorAlertState::Resolved, alert.timestamp);
                    ExternalAlertOutcome::Resolved
                } else {
                    ExternalAlertOutcome::Unknown
                }
            }
            SensorAlertState::Firing => {
                let outcome = if self.external.contains_key(&key) {
                    ExternalAlertOutcome::Updated
                } else {
                    // Only a fresh firing (not an update of an existing one) is a
                    // new timeline transition.
                    self.record_transition(&key, SensorAlertState::Firing, alert.timestamp);
                    ExternalAlertOutcome::New
                };
                self.external.insert(key, alert);
                outcome
            }
        }
    }

    /// Append a transition to an incident's bounded timeline (#26).
    fn record_transition(&mut self, key: &str, state: SensorAlertState, at: i64) {
        let tl = self.timelines.entry(key.to_string()).or_default();
        tl.push_back(TransitionEvent { state, at });
        while tl.len() > MAX_TIMELINE_EVENTS {
            tl.pop_front();
        }
    }

    /// The recorded transition timeline for an incident, oldest-first (#26).
    pub fn timeline(&self, alert_key: &str) -> Vec<TransitionEvent> {
        self.timelines
            .get(alert_key)
            .map(|d| d.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Silence (mute) a source until `now + duration_ms` (#26). While silenced,
    /// its incidents are hidden and excluded from the active count.
    pub fn silence_source(&mut self, source: &str, now_ms: i64, duration_ms: i64) {
        self.silenced_sources
            .insert(source.to_string(), now_ms + duration_ms);
    }

    /// Lift a silence on a source immediately.
    pub fn unsilence_source(&mut self, source: &str) {
        self.silenced_sources.remove(source);
    }

    /// Whether `source` is currently silenced at `now_ms` (expired silences are
    /// treated as lifted; callers may also prune via [`Self::prune_silences`]).
    pub fn is_silenced(&self, source: &str, now_ms: i64) -> bool {
        self.silenced_sources
            .get(source)
            .is_some_and(|&expiry| expiry > now_ms)
    }

    /// Drop expired silences. Call on tick. Returns how many were lifted.
    pub fn prune_silences(&mut self, now_ms: i64) -> usize {
        let before = self.silenced_sources.len();
        self.silenced_sources
            .retain(|_, &mut expiry| expiry > now_ms);
        before - self.silenced_sources.len()
    }

    /// Count of currently-silenced sources at `now_ms` (for the muted chip).
    pub fn silenced_count(&self, now_ms: i64) -> usize {
        self.silenced_sources
            .values()
            .filter(|&&e| e > now_ms)
            .count()
    }

    /// Clear an external alert by its key (resolve tombstone / Delete). Returns
    /// the removed alert, if any.
    pub fn clear_external(&mut self, alert_key: &str) -> Option<SensorAlert> {
        self.acknowledged_external.remove(alert_key);
        self.external.remove(alert_key)
    }

    /// Iterate currently-firing sensor-pushed alerts, severity-then-recency order.
    pub fn active_external(&self) -> Vec<&SensorAlert> {
        let mut v: Vec<&SensorAlert> = self.external.values().collect();
        v.sort_by(|a, b| {
            b.severity
                .cmp(&a.severity)
                .then(b.timestamp.cmp(&a.timestamp))
        });
        v
    }

    /// Has this external alert been acknowledged?
    pub fn is_external_acked(&self, alert_key: &str) -> bool {
        self.acknowledged_external.contains(alert_key)
    }

    /// Acknowledge every currently-firing external alert from `source`.
    pub fn acknowledge_external_source(&mut self, source: &str) {
        for (key, alert) in &self.external {
            if alert.source == source {
                self.acknowledged_external.insert(key.clone());
            }
        }
    }

    /// Acknowledge all currently-firing external alerts.
    pub fn acknowledge_all_external(&mut self) {
        self.acknowledged_external
            .extend(self.external.keys().cloned());
    }

    /// Firing external alerts grouped by source (current time). Silenced sources
    /// are hidden. See [`Self::external_by_source_at`] for a clock-injected form.
    pub fn external_by_source(&self) -> Vec<ExternalIncident<'_>> {
        self.external_by_source_at(now_ms())
    }

    /// Firing external alerts grouped by source, each group sorted by severity
    /// then recency, with its un-acknowledged count and highest severity. Groups
    /// are ordered by (has-unacked, highest-severity, source). Sources silenced
    /// at `now_ms` are excluded (#26). Pure given the clock.
    pub fn external_by_source_at(&self, now_ms: i64) -> Vec<ExternalIncident<'_>> {
        let mut by_source: HashMap<&str, Vec<&SensorAlert>> = HashMap::new();
        for alert in self.external.values() {
            if self.is_silenced(&alert.source, now_ms) {
                continue;
            }
            by_source.entry(&alert.source).or_default().push(alert);
        }
        let mut groups: Vec<ExternalIncident<'_>> = by_source
            .into_iter()
            .map(|(source, mut alerts)| {
                alerts.sort_by(|a, b| {
                    b.severity
                        .cmp(&a.severity)
                        .then(b.timestamp.cmp(&a.timestamp))
                });
                let unacked = alerts
                    .iter()
                    .filter(|a| !self.acknowledged_external.contains(&a.alert_key()))
                    .count();
                let top_severity = alerts.iter().map(|a| a.severity).max();
                ExternalIncident {
                    source,
                    alerts,
                    unacked,
                    top_severity,
                }
            })
            .collect();
        groups.sort_by(|a, b| {
            (b.unacked > 0)
                .cmp(&(a.unacked > 0))
                .then(b.top_severity.cmp(&a.top_severity))
                .then(a.source.cmp(b.source))
        });
        groups
    }

    /// Count of *un-acknowledged*, *non-silenced* firing external alerts (badge).
    pub fn external_count(&self) -> usize {
        let now = now_ms();
        self.external
            .values()
            .filter(|a| {
                !self.acknowledged_external.contains(&a.alert_key())
                    && !self.is_silenced(&a.source, now)
            })
            .count()
    }

    /// Update the max alerts setting.
    pub fn set_max_alerts(&mut self, max_alerts: usize) {
        self.max_alerts = max_alerts;
        // Trim existing alerts if needed
        while self.alerts.len() > max_alerts {
            if let Some(removed) = self.alerts.pop()
                && !removed.acknowledged
            {
                self.unacknowledged_count = self.unacknowledged_count.saturating_sub(1);
            }
        }
    }

    /// Add a new rule.
    pub fn add_rule(&mut self) -> Result<(), String> {
        if self.new_rule_name.trim().is_empty() {
            return Err("Rule name is required".to_string());
        }

        if self.new_rule_metric.trim().is_empty() {
            return Err("Metric pattern is required".to_string());
        }

        let threshold: f64 = self
            .new_rule_threshold
            .parse()
            .map_err(|_| "Threshold must be a number".to_string())?;

        let rule = AlertRule {
            id: self.next_rule_id,
            name: self.new_rule_name.trim().to_string(),
            device_pattern: None,
            protocol: None,
            metric_pattern: self.new_rule_metric.trim().to_string(),
            operator: self.new_rule_operator,
            threshold,
            severity: self.new_rule_severity,
            enabled: true,
        };

        self.rules.push(rule);
        self.next_rule_id += 1;

        // Clear form
        self.new_rule_name.clear();
        self.new_rule_metric.clear();
        self.new_rule_threshold.clear();
        self.new_rule_operator = ComparisonOp::GreaterThan;
        self.new_rule_severity = Severity::Warning;

        Ok(())
    }

    /// Test the current form rule against provided metrics.
    /// Returns the number of metrics that would match.
    pub fn test_rule(&mut self, metrics: &[(String, String, f64)]) -> Result<(), String> {
        // Validate inputs first
        if self.new_rule_metric.trim().is_empty() {
            self.test_result = Some("Error: Metric pattern is required".to_string());
            return Err("Metric pattern is required".to_string());
        }

        let threshold: f64 = self.new_rule_threshold.parse().map_err(|e| {
            self.test_result = Some(format!("Error: Invalid threshold - {}", e));
            format!("Threshold must be a number: {}", e)
        })?;

        let pattern = self.new_rule_metric.trim().to_lowercase();
        let operator = self.new_rule_operator;

        // Count matches
        let mut matches = Vec::new();
        for (device, metric, value) in metrics {
            let metric_lower = metric.to_lowercase();
            if metric_lower.contains(&pattern) {
                let would_trigger = operator.evaluate(*value, threshold);
                if would_trigger {
                    matches.push(format!(
                        "{}/{}: {} {} {}",
                        device,
                        metric,
                        value,
                        operator.symbol(),
                        threshold
                    ));
                }
            }
        }

        if matches.is_empty() {
            self.test_result = Some(format!(
                "No matches. Pattern '{}' with {} {} would not trigger on any current metrics.",
                pattern,
                operator.symbol(),
                threshold
            ));
        } else {
            let preview: Vec<_> = matches.iter().take(5).cloned().collect();
            let more = if matches.len() > 5 {
                format!(" ... and {} more", matches.len() - 5)
            } else {
                String::new()
            };
            self.test_result = Some(format!(
                "Would match {} metric(s):\n{}{}",
                matches.len(),
                preview.join("\n"),
                more
            ));
        }

        Ok(())
    }

    /// Clear the test result.
    pub fn clear_test_result(&mut self) {
        self.test_result = None;
    }

    /// Remove a rule by ID.
    pub fn remove_rule(&mut self, rule_id: u32) {
        self.rules.retain(|r| r.id != rule_id);
    }

    /// Toggle a rule's enabled state.
    pub fn toggle_rule(&mut self, rule_id: u32) {
        if let Some(rule) = self.rules.iter_mut().find(|r| r.id == rule_id) {
            rule.enabled = !rule.enabled;
        }
    }

    /// Check a metric value against all rules.
    pub fn check_metric(
        &mut self,
        device_id: &DeviceId,
        metric: &str,
        value: f64,
        timestamp: i64,
    ) -> Option<Alert> {
        // Check cooldown
        let key = format!("{}/{}/{}", device_id.protocol, device_id.source, metric);
        if let Some(&last_alert) = self.recent_alerts.get(&key)
            && timestamp - last_alert < self.alert_cooldown_ms
        {
            return None;
        }

        // Find matching rule that triggers
        for rule in &self.rules {
            if rule.matches(device_id, metric) && rule.evaluate(value) {
                let alert = Alert::new(
                    self.next_alert_id,
                    rule,
                    device_id.clone(),
                    metric.to_string(),
                    value,
                    timestamp,
                );

                self.next_alert_id += 1;
                self.alerts.insert(0, alert.clone());
                self.unacknowledged_count += 1;

                // Update cooldown
                self.recent_alerts.insert(key, timestamp);

                // Trim old alerts
                while self.alerts.len() > self.max_alerts {
                    if let Some(removed) = self.alerts.pop()
                        && !removed.acknowledged
                    {
                        self.unacknowledged_count = self.unacknowledged_count.saturating_sub(1);
                    }
                }

                return Some(alert);
            }
        }

        None
    }

    /// Acknowledge an alert.
    pub fn acknowledge(&mut self, alert_id: u64) {
        if let Some(alert) = self.alerts.iter_mut().find(|a| a.id == alert_id)
            && !alert.acknowledged
        {
            alert.acknowledged = true;
            self.unacknowledged_count = self.unacknowledged_count.saturating_sub(1);
        }
    }

    /// Acknowledge all alerts.
    pub fn acknowledge_all(&mut self) {
        for alert in &mut self.alerts {
            alert.acknowledged = true;
        }
        self.unacknowledged_count = 0;
    }

    /// Clear all alerts.
    pub fn clear_alerts(&mut self) {
        self.alerts.clear();
        self.unacknowledged_count = 0;
    }

    /// Update form state.
    pub fn set_new_rule_name(&mut self, name: String) {
        self.new_rule_name = name;
    }

    pub fn set_new_rule_metric(&mut self, metric: String) {
        self.new_rule_metric = metric;
    }

    pub fn set_new_rule_threshold(&mut self, threshold: String) {
        self.new_rule_threshold = threshold;
    }

    pub fn set_new_rule_operator(&mut self, operator: ComparisonOp) {
        self.new_rule_operator = operator;
    }

    pub fn set_new_rule_severity(&mut self, severity: Severity) {
        self.new_rule_severity = severity;
    }
}

/// Render the alerts view.
pub fn alerts_view(state: &AlertsState) -> Element<'_, Message> {
    let header = render_header(state);
    let external_section = render_external_alerts_section(state);
    let new_rule_form = render_new_rule_form(state);
    let rules_section = render_rules_section(state);
    let alerts_section = render_alerts_section(state);

    let content = column![
        header,
        rule::horizontal(1),
        external_section,
        rule::horizontal(1),
        new_rule_form,
        rule::horizontal(1),
        rules_section,
        rule::horizontal(1),
        alerts_section,
    ]
    .spacing(15)
    .padding(20);

    container(scrollable(content))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Render header with back button.
fn render_header(state: &AlertsState) -> Element<'_, Message> {
    let back_button = button(
        row![icons::arrow_left(IconSize::Medium), text("Back").size(14)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .on_press(Message::CloseAlerts)
    .style(iced::widget::button::secondary);

    let title = row![
        icons::alert(IconSize::XLarge),
        text("Alerts & Notifications").size(24)
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    let unack_badge: Element<'_, Message> = if state.unacknowledged_count > 0 {
        row![
            icons::status_warning(IconSize::Small),
            text(format!("{} unacknowledged", state.unacknowledged_count))
                .size(14)
                .style(|theme: &Theme| text::Style {
                    color: Some(crate::view::theme::colors(theme).warning()),
                })
        ]
        .spacing(5)
        .align_y(Alignment::Center)
        .into()
    } else {
        row![].into()
    };

    let expectations_button = button(text("Expectations").size(13))
        .on_press(Message::OpenExpectations)
        .style(iced::widget::button::secondary);

    let security_button = button(text("Security").size(13))
        .on_press(Message::OpenSecurity)
        .style(iced::widget::button::secondary);

    let header_row = row![
        back_button,
        title,
        unack_badge,
        expectations_button,
        security_button
    ]
    .spacing(15)
    .align_y(Alignment::Center);

    // Scope subtitle so Alerts vs Security is legible (#39): this view owns
    // operational, threshold-based alerts; Security owns network anomalies.
    let subtitle = text("Operational threshold alerts — rule-based and sensor-pushed")
        .size(font::CAPTION)
        .style(|theme: &Theme| text::Style {
            color: Some(crate::view::theme::colors(theme).text_dimmed()),
        });

    column![header_row, subtitle].spacing(4).into()
}

/// Render the new rule form.
fn render_new_rule_form(state: &AlertsState) -> Element<'_, Message> {
    let section_title = text("Add Alert Rule").size(18);

    let name_input = text_input("Rule name", &state.new_rule_name)
        .on_input(Message::SetAlertRuleName)
        .padding(8)
        .width(Length::Fixed(180.0));

    let metric_input = text_input("Metric pattern (e.g., ifInErrors)", &state.new_rule_metric)
        .on_input(Message::SetAlertRuleMetric)
        .padding(8)
        .width(Length::Fixed(220.0));

    let operator_picker = pick_list(
        ComparisonOp::ALL,
        Some(state.new_rule_operator),
        Message::SetAlertRuleOperator,
    );

    let threshold_input = text_input("Threshold", &state.new_rule_threshold)
        .on_input(Message::SetAlertRuleThreshold)
        .padding(8)
        .width(Length::Fixed(90.0));

    let severity_picker = pick_list(
        Severity::ALL,
        Some(state.new_rule_severity),
        Message::SetAlertRuleSeverity,
    );

    let test_button = button(text("Test").size(14))
        .on_press(Message::TestAlertRule)
        .style(iced::widget::button::secondary);

    let add_button = button(text("Add Rule").size(14))
        .on_press(Message::AddAlertRule)
        .style(iced::widget::button::primary);

    let form_row = row![
        name_input,
        metric_input,
        operator_picker,
        threshold_input,
        severity_picker,
        test_button,
        add_button
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    // Show test result if available
    let mut form_content = Column::new().spacing(10).push(section_title).push(form_row);

    if let Some(ref result) = state.test_result {
        let is_error = result.starts_with("Error:");
        let is_no_match = result.starts_with("No matches");

        let result_text = text(result.clone()).size(12).style(move |theme: &Theme| {
            let colors = crate::view::theme::colors(theme);
            let color = if is_error {
                colors.danger()
            } else if is_no_match {
                colors.text_muted()
            } else {
                colors.success()
            };
            text::Style { color: Some(color) }
        });

        let result_container = container(result_text).padding(8).style(|theme: &Theme| {
            let colors = crate::view::theme::colors(theme);
            container::Style {
                background: Some(iced::Background::Color(colors.card_background())),
                border: iced::Border {
                    color: colors.border(),
                    width: 1.0,
                    radius: 4.0.into(),
                },
                ..Default::default()
            }
        });

        form_content = form_content.push(result_container);
    }

    form_content.into()
}

/// Render the rules section.
fn render_rules_section(state: &AlertsState) -> Element<'_, Message> {
    let section_title = text(format!("Rules ({})", state.rules.len())).size(18);

    if state.rules.is_empty() {
        return column![section_title, text("No alert rules defined").size(14)]
            .spacing(10)
            .into();
    }

    let mut rules_list = Column::new().spacing(5);

    for rule in &state.rules {
        rules_list = rules_list.push(render_rule_row(rule));
    }

    column![section_title, rules_list].spacing(10).into()
}

/// Render a single rule row.
fn render_rule_row(rule: &AlertRule) -> Element<'_, Message> {
    let status: Element<'_, Message> = if rule.enabled {
        icons::status_healthy(IconSize::Small)
    } else {
        icons::status_warning(IconSize::Small)
    };

    let name = text(rule.name.clone()).size(14);

    // Severity badge with color
    let severity_color = rule.severity.color();
    let severity_badge = text(rule.severity.name())
        .size(11)
        .style(move |_theme: &Theme| text::Style {
            color: Some(severity_color),
        });

    let condition = text(format!(
        "{} {} {}",
        rule.metric_pattern,
        rule.operator.symbol(),
        format_value(rule.threshold)
    ))
    .size(12)
    .style(|theme: &Theme| text::Style {
        color: Some(crate::view::theme::colors(theme).text_muted()),
    });

    let toggle_label = if rule.enabled { "Disable" } else { "Enable" };
    let toggle_button = button(text(toggle_label).size(11))
        .on_press(Message::ToggleAlertRule(rule.id))
        .style(iced::widget::button::secondary);

    let remove_button = button(
        row![icons::trash(IconSize::Small), text("Remove").size(11)]
            .spacing(4)
            .align_y(Alignment::Center),
    )
    .on_press(Message::RemoveAlertRule(rule.id))
    .style(iced::widget::button::danger);

    row![
        status,
        name,
        severity_badge,
        condition,
        toggle_button,
        remove_button
    ]
    .spacing(10)
    .align_y(Alignment::Center)
    .into()
}

/// Render the alerts section.
/// Render the sensor-pushed alerts section (anomalies + expectation violations).
fn render_external_alerts_section(state: &AlertsState) -> Element<'_, Message> {
    let groups = state.external_by_source();
    let total: usize = groups.iter().map(|g| g.alerts.len()).sum();

    let actions: Option<Element<'_, Message>> = if state.external_count() > 0 {
        Some(
            button(text("Ack all").size(font::CAPTION))
                .on_press(Message::AcknowledgeAllExternal)
                .padding([space::XS, space::SM])
                .style(iced::widget::button::secondary)
                .into(),
        )
    } else {
        None
    };
    let muted = state.silenced_count(now_ms());
    let title = if muted > 0 {
        format!("Anomalies & Expectations ({total}) · {muted} muted")
    } else {
        format!("Anomalies & Expectations ({total})")
    };
    let section_title = section_header(title, actions);

    if groups.is_empty() {
        return column![
            section_title,
            text("No active sensor alerts")
                .size(font::BODY)
                .style(|theme: &Theme| {
                    text::Style {
                        color: Some(crate::view::theme::colors(theme).text_dimmed()),
                    }
                })
        ]
        .spacing(space::SM)
        .into();
    }

    let mut list = Column::new().spacing(space::SM);
    for group in &groups {
        list = list.push(render_incident(state, group));
    }

    column![section_title, list].spacing(space::SM).into()
}

/// Render one source-grouped incident: a header (source · count · severity ·
/// Ack) followed by its alert rows.
fn render_incident<'a>(
    state: &'a AlertsState,
    incident: &ExternalIncident<'a>,
) -> Element<'a, Message> {
    let sev_color = incident
        .top_severity
        .map(|s| Severity::from(s).color())
        .unwrap_or_else(|| Severity::Info.color());

    let count_label = if incident.unacked > 0 {
        format!("{} ({} new)", incident.alerts.len(), incident.unacked)
    } else {
        format!("{} acknowledged", incident.alerts.len())
    };

    let mut header = row![
        badge(sev_color, incident.source.to_string()),
        text(count_label)
            .size(font::CAPTION)
            .style(|theme: &Theme| text::Style {
                color: Some(crate::view::theme::colors(theme).text_dimmed()),
            }),
    ]
    .spacing(space::SM)
    .align_y(Alignment::Center);

    // Right-aligned action cluster: View + Ack (if unacked) + Mute 1h/4h/24h.
    let spacer = container(text("")).width(Length::Fill);
    header = header.push(spacer);
    // #35: jump to the source device that raised this incident.
    if let Some(first) = incident.alerts.first() {
        let device = DeviceId::new(first.protocol, incident.source.to_string());
        header = header.push(
            button(text("View").size(font::CAPTION))
                .on_press(Message::InvestigateAlert {
                    device,
                    metric: None,
                })
                .padding([space::XS, space::SM])
                .style(iced::widget::button::secondary),
        );
    }
    if incident.unacked > 0 {
        let ack = button(text("Ack").size(font::CAPTION))
            .on_press(Message::AcknowledgeExternalSource(
                incident.source.to_string(),
            ))
            .padding([space::XS, space::SM])
            .style(iced::widget::button::secondary);
        header = header.push(ack);
    }
    for (label, dur) in [
        ("Mute 1h", 3_600_000i64),
        ("4h", 14_400_000),
        ("24h", 86_400_000),
    ] {
        header = header.push(
            button(text(label).size(font::CAPTION))
                .on_press(Message::SilenceSource(incident.source.to_string(), dur))
                .padding([space::XS, space::SM])
                .style(iced::widget::button::text),
        );
    }

    let mut col = Column::new().spacing(2).push(header);
    for alert in &incident.alerts {
        let acked = state.is_external_acked(&alert.alert_key());
        col = col.push(render_external_alert_row(alert, acked));
        // Incident timeline strip: firing→resolved transitions (#26).
        let tl = state.timeline(&alert.alert_key());
        if tl.len() > 1 {
            col = col.push(render_timeline(&tl));
        }
    }
    col.into()
}

/// Render an incident timeline strip: "Firing 10:42 → Resolved 10:45 → ..." (#26).
fn render_timeline<'a>(events: &[TransitionEvent]) -> Element<'a, Message> {
    let parts: Vec<String> = events
        .iter()
        .map(|e| {
            let state = match e.state {
                SensorAlertState::Firing => "Firing",
                SensorAlertState::Resolved => "Resolved",
            };
            format!("{state} {}", format_timestamp(e.at))
        })
        .collect();
    text(format!("  {}", parts.join(" → ")))
        .size(font::CAPTION)
        .style(|theme: &Theme| text::Style {
            color: Some(crate::view::theme::colors(theme).text_dimmed()),
        })
        .into()
}

/// Render a single sensor-pushed alert row (dimmed when acknowledged).
fn render_external_alert_row<'a>(alert: &'a SensorAlert, acked: bool) -> Element<'a, Message> {
    let severity: Severity = alert.severity.into();
    let icon: Element<'a, Message> = match severity {
        Severity::Critical => icons::status_error(IconSize::Small),
        Severity::Warning => icons::status_warning(IconSize::Small),
        Severity::Info => icons::info(IconSize::Small),
    };

    let severity_color = severity.color();
    let severity_badge = text(severity.name())
        .size(10)
        .style(move |_theme: &Theme| text::Style {
            color: Some(severity_color),
        });

    let kind = text(if acked { "ack'd" } else { alert.kind.as_str() })
        .size(10)
        .style(|theme: &Theme| text::Style {
            color: Some(crate::view::theme::colors(theme).text_dimmed()),
        });

    let summary: Element<'a, Message> = if alert.summary.len() > MAX_ALERT_MESSAGE_LEN {
        let truncated = format!("{}...", &alert.summary[..MAX_ALERT_MESSAGE_LEN]);
        tooltip(
            text(truncated).size(13),
            container(text(alert.summary.clone()).size(12))
                .padding(8)
                .max_width(400.0)
                .style(container::rounded_box),
            tooltip::Position::Bottom,
        )
        .into()
    } else {
        text(alert.summary.clone()).size(13).into()
    };

    let source = text(format!("{}/{}", alert.protocol, alert.source))
        .size(11)
        .style(|theme: &Theme| text::Style {
            color: Some(crate::view::theme::colors(theme).text_dimmed()),
        });

    let time = text(format_timestamp(alert.timestamp))
        .size(11)
        .style(|theme: &Theme| text::Style {
            color: Some(crate::view::theme::colors(theme).text_dimmed()),
        });

    Row::new()
        .push(icon)
        .push(severity_badge)
        .push(kind)
        .push(summary)
        .push(source)
        .push(time)
        .spacing(10)
        .align_y(Alignment::Center)
        .into()
}

fn render_alerts_section(state: &AlertsState) -> Element<'_, Message> {
    let section_title = text(format!("Alert History ({})", state.alerts.len())).size(18);

    let actions = row![
        button(text("Acknowledge All").size(12))
            .on_press(Message::AcknowledgeAllAlerts)
            .style(iced::widget::button::secondary),
        button(text("Clear All").size(12))
            .on_press(Message::ClearAlerts)
            .style(iced::widget::button::secondary),
    ]
    .spacing(10);

    let header = row![section_title, actions]
        .spacing(20)
        .align_y(Alignment::Center);

    if state.alerts.is_empty() {
        return column![header, text("No alerts triggered").size(14)]
            .spacing(10)
            .into();
    }

    let mut alerts_list = Column::new().spacing(5);

    for alert in state.alerts.iter().take(50) {
        alerts_list = alerts_list.push(render_alert_row(alert));
    }

    column![header, alerts_list].spacing(10).into()
}

/// Maximum length for alert message before truncation.
const MAX_ALERT_MESSAGE_LEN: usize = 60;

/// Render a single alert row.
fn render_alert_row(alert: &Alert) -> Element<'_, Message> {
    let status: Element<'_, Message> = if alert.acknowledged {
        icons::check(IconSize::Small)
    } else {
        // Use severity-appropriate icon for unacknowledged alerts
        match alert.severity {
            Severity::Critical => icons::status_error(IconSize::Small),
            Severity::Warning => icons::status_warning(IconSize::Small),
            Severity::Info => icons::info(IconSize::Small),
        }
    };

    // Severity badge with color
    let severity_color = alert.severity.color();
    let severity_badge = text(alert.severity.name())
        .size(10)
        .style(move |_theme: &Theme| text::Style {
            color: Some(severity_color),
        });

    let full_message = alert.message();
    let message: Element<'_, Message> = if full_message.len() > MAX_ALERT_MESSAGE_LEN {
        let truncated = format!("{}...", &full_message[..MAX_ALERT_MESSAGE_LEN]);
        tooltip(
            text(truncated).size(13),
            container(text(full_message.clone()).size(12))
                .padding(8)
                .max_width(400.0)
                .style(container::rounded_box),
            tooltip::Position::Bottom,
        )
        .into()
    } else {
        text(full_message).size(13).into()
    };

    let time = text(format_timestamp(alert.timestamp))
        .size(11)
        .style(|theme: &Theme| text::Style {
            color: Some(crate::view::theme::colors(theme).text_dimmed()),
        });

    // #35: jump straight to the offending device + metric chart.
    let investigate = button(text("View").size(10))
        .on_press(Message::InvestigateAlert {
            device: alert.device_id.clone(),
            metric: Some(alert.metric.clone()),
        })
        .padding([space::XS, space::SM])
        .style(iced::widget::button::secondary);

    let mut row_content: Row<'_, Message> = Row::new()
        .push(status)
        .push(severity_badge)
        .push(message)
        .push(time)
        .push(investigate)
        .spacing(10);

    if !alert.acknowledged {
        let ack_button = button(
            row![icons::check(IconSize::Small), text("Ack").size(10)]
                .spacing(3)
                .align_y(Alignment::Center),
        )
        .on_press(Message::AcknowledgeAlert(alert.id))
        .style(iced::widget::button::secondary);
        row_content = row_content.push(ack_button);
    }

    row_content.align_y(Alignment::Center).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alert_rule_matches() {
        let rule = AlertRule::new(1, "Test", "ifInErrors");

        let device = DeviceId {
            protocol: Protocol::Snmp,
            source: "router01".to_string(),
        };

        assert!(rule.matches(&device, "if/1/ifInErrors"));
        assert!(rule.matches(&device, "ifInErrors"));
        assert!(!rule.matches(&device, "ifOutErrors"));
    }

    #[test]
    fn test_alert_rule_evaluate() {
        let mut rule = AlertRule::new(1, "Test", "errors");
        rule.threshold = 100.0;

        rule.operator = ComparisonOp::GreaterThan;
        assert!(rule.evaluate(150.0));
        assert!(!rule.evaluate(100.0));
        assert!(!rule.evaluate(50.0));

        rule.operator = ComparisonOp::LessThan;
        assert!(!rule.evaluate(150.0));
        assert!(!rule.evaluate(100.0));
        assert!(rule.evaluate(50.0));
    }

    #[test]
    fn test_alerts_state_check_metric() {
        let mut state = AlertsState::new();

        let mut rule = AlertRule::new(1, "High Errors", "errors");
        rule.threshold = 100.0;
        rule.operator = ComparisonOp::GreaterThan;
        state.rules.push(rule);

        let device = DeviceId {
            protocol: Protocol::Snmp,
            source: "router01".to_string(),
        };

        // Should trigger
        let alert = state.check_metric(&device, "if/1/errors", 150.0, 1000);
        assert!(alert.is_some());
        assert_eq!(state.alerts.len(), 1);
        assert_eq!(state.unacknowledged_count, 1);

        // Should not trigger (below threshold)
        let alert = state.check_metric(&device, "if/1/errors", 50.0, 2000);
        assert!(alert.is_none());

        // Should not trigger (cooldown)
        let alert = state.check_metric(&device, "if/1/errors", 200.0, 3000);
        assert!(alert.is_none());

        // Should trigger after cooldown
        let alert = state.check_metric(&device, "if/1/errors", 200.0, 100000);
        assert!(alert.is_some());
        assert_eq!(state.alerts.len(), 2);
    }

    #[test]
    fn test_acknowledge_alert() {
        let mut state = AlertsState::new();

        state.rules.push(AlertRule::new(1, "Test", "errors"));
        state.rules[0].threshold = 0.0;

        let device = DeviceId {
            protocol: Protocol::Snmp,
            source: "test".to_string(),
        };

        state.check_metric(&device, "errors", 100.0, 1000);
        assert_eq!(state.unacknowledged_count, 1);

        state.acknowledge(1);
        assert_eq!(state.unacknowledged_count, 0);
        assert!(state.alerts[0].acknowledged);
    }

    #[test]
    fn test_comparison_operators() {
        assert_eq!(ComparisonOp::GreaterThan.symbol(), ">");
        assert_eq!(ComparisonOp::LessOrEqual.symbol(), "<=");
        assert_eq!(ComparisonOp::NotEqual.symbol(), "!=");
    }

    fn ext_alert(rule: &str, sev: zensight_common::AlertSeverity) -> SensorAlert {
        SensorAlert::new(
            "host1",
            Protocol::Netlink,
            zensight_common::AlertKind::Expectation,
            rule,
            sev,
            "summary",
        )
    }

    #[test]
    fn ingest_external_lifecycle() {
        use zensight_common::AlertSeverity;
        let mut state = AlertsState::new();
        let a = ext_alert("ssh-listening", AlertSeverity::Critical);
        let key = a.alert_key();

        assert_eq!(state.ingest_external(a.clone()), ExternalAlertOutcome::New);
        assert_eq!(state.external_count(), 1);
        // Same key again → Updated, no duplicate.
        assert_eq!(
            state.ingest_external(a.clone()),
            ExternalAlertOutcome::Updated
        );
        assert_eq!(state.external_count(), 1);
        // Resolve removes it.
        assert_eq!(
            state.ingest_external(a.resolved()),
            ExternalAlertOutcome::Resolved
        );
        assert_eq!(state.external_count(), 0);
        // Resolve again → Unknown.
        let b = ext_alert("ssh-listening", AlertSeverity::Critical).resolved();
        assert_eq!(state.ingest_external(b), ExternalAlertOutcome::Unknown);
        // clear_external by key is a no-op now.
        assert!(state.clear_external(&key).is_none());
    }

    #[test]
    fn silence_hides_source_and_expires() {
        use zensight_common::AlertSeverity;
        let mut state = AlertsState::new();
        state.ingest_external(ext_alert("ssh", AlertSeverity::Critical));
        assert_eq!(state.external_by_source_at(0).len(), 1);

        // Silence host1 for 1h at t=0.
        state.silence_source("host1", 0, 3_600_000);
        assert!(state.is_silenced("host1", 1_000));
        assert_eq!(state.silenced_count(1_000), 1);
        // Hidden while silenced.
        assert!(state.external_by_source_at(1_000).is_empty());
        // Expired after the window: visible again, count 0.
        assert!(!state.is_silenced("host1", 3_600_001));
        assert_eq!(state.external_by_source_at(3_600_001).len(), 1);
        // Prune drops the expired silence.
        assert_eq!(state.prune_silences(3_600_001), 1);
        assert_eq!(state.silenced_count(3_600_001), 0);
    }

    #[test]
    fn unsilence_lifts_immediately() {
        let mut state = AlertsState::new();
        state.silence_source("h", 0, 3_600_000);
        assert!(state.is_silenced("h", 10));
        state.unsilence_source("h");
        assert!(!state.is_silenced("h", 10));
    }

    #[test]
    fn timeline_records_firing_resolved_transitions() {
        use zensight_common::AlertSeverity;
        let mut state = AlertsState::new();
        let mut a = ext_alert("ssh", AlertSeverity::Warning);
        a.timestamp = 1_000;
        let key = a.alert_key();
        state.ingest_external(a.clone());
        // A repeat firing (update) does NOT add a transition.
        let mut a2 = a.clone();
        a2.timestamp = 1_500;
        state.ingest_external(a2);
        // Resolve adds a Resolved transition.
        let mut r = a.resolved();
        r.timestamp = 2_000;
        state.ingest_external(r);
        // Fires again.
        let mut a3 = ext_alert("ssh", AlertSeverity::Warning);
        a3.timestamp = 3_000;
        state.ingest_external(a3);

        let tl = state.timeline(&key);
        assert_eq!(tl.len(), 3);
        assert_eq!(tl[0].state, SensorAlertState::Firing);
        assert_eq!(tl[0].at, 1_000);
        assert_eq!(tl[1].state, SensorAlertState::Resolved);
        assert_eq!(tl[1].at, 2_000);
        assert_eq!(tl[2].state, SensorAlertState::Firing);
        assert_eq!(tl[2].at, 3_000);
    }

    #[test]
    fn active_external_sorted_by_severity() {
        use zensight_common::AlertSeverity;
        let mut state = AlertsState::new();
        state.ingest_external(ext_alert("a", AlertSeverity::Info));
        state.ingest_external(ext_alert("b", AlertSeverity::Critical));
        state.ingest_external(ext_alert("c", AlertSeverity::Warning));
        let active = state.active_external();
        assert_eq!(active[0].severity, AlertSeverity::Critical);
        assert_eq!(active[2].severity, AlertSeverity::Info);
    }

    #[test]
    fn external_grouping_and_acknowledge() {
        use zensight_common::{AlertKind, AlertSeverity};
        let mk = |source: &str, rule: &str, sev| {
            SensorAlert::new(
                source,
                Protocol::Netlink,
                AlertKind::Anomaly,
                rule,
                sev,
                "s",
            )
        };
        let mut state = AlertsState::new();
        state.ingest_external(mk("hostA", "r1", AlertSeverity::Warning));
        state.ingest_external(mk("hostA", "r2", AlertSeverity::Critical));
        state.ingest_external(mk("hostB", "r3", AlertSeverity::Info));

        // Two source groups; hostA (Critical) sorts first with 2 alerts.
        let groups = state.external_by_source();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].source, "hostA");
        assert_eq!(groups[0].alerts.len(), 2);
        assert_eq!(groups[0].unacked, 2);
        assert_eq!(state.external_count(), 3);

        // Acknowledging hostA drops it from the count and below the un-acked hostB.
        state.acknowledge_external_source("hostA");
        assert_eq!(state.external_count(), 1);
        let groups = state.external_by_source();
        assert_eq!(groups[0].source, "hostB");
        let host_a = groups.iter().find(|g| g.source == "hostA").unwrap();
        assert_eq!(host_a.unacked, 0);

        state.acknowledge_all_external();
        assert_eq!(state.external_count(), 0);
    }
}
