//! Alerts view for threshold-based notifications.

use std::collections::HashMap;

use iced::widget::{
    Column, Row, button, column, container, pick_list, row, rule, scrollable, text, text_input,
};
use iced::{Alignment, Element, Length, Theme};

use zensight_common::Protocol;

use crate::message::{DeviceId, Message};
use crate::view::formatting::{format_timestamp, format_value};

/// Alert rule definition.
#[derive(Debug, Clone)]
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
            enabled: true,
        }
    }

    /// Check if a metric matches this rule.
    pub fn matches(&self, device_id: &DeviceId, metric: &str) -> bool {
        // Check protocol filter
        if let Some(ref proto) = self.protocol {
            if device_id.protocol != *proto {
                return false;
            }
        }

        // Check device pattern
        if let Some(ref pattern) = self.device_pattern {
            if !device_id.source.contains(pattern) {
                return false;
            }
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

/// Comparison operators for alert rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ComparisonOp {
    #[default]
    GreaterThan,
    GreaterOrEqual,
    LessThan,
    LessOrEqual,
    Equal,
    NotEqual,
}

impl ComparisonOp {
    /// All comparison operators.
    pub const ALL: &'static [ComparisonOp] = &[
        ComparisonOp::GreaterThan,
        ComparisonOp::GreaterOrEqual,
        ComparisonOp::LessThan,
        ComparisonOp::LessOrEqual,
        ComparisonOp::Equal,
        ComparisonOp::NotEqual,
    ];

    /// Get the symbol for this operator.
    pub fn symbol(&self) -> &'static str {
        match self {
            ComparisonOp::GreaterThan => ">",
            ComparisonOp::GreaterOrEqual => ">=",
            ComparisonOp::LessThan => "<",
            ComparisonOp::LessOrEqual => "<=",
            ComparisonOp::Equal => "==",
            ComparisonOp::NotEqual => "!=",
        }
    }
}

impl std::fmt::Display for ComparisonOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.symbol())
    }
}

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
    /// Number of unacknowledged alerts.
    pub unacknowledged_count: usize,
}

impl AlertsState {
    /// Create a new alerts state.
    pub fn new() -> Self {
        Self {
            rules: Vec::new(),
            alerts: Vec::new(),
            next_rule_id: 1,
            next_alert_id: 1,
            max_alerts: 100,
            recent_alerts: HashMap::new(),
            alert_cooldown_ms: 60_000, // 1 minute
            new_rule_name: String::new(),
            new_rule_metric: String::new(),
            new_rule_threshold: String::new(),
            new_rule_operator: ComparisonOp::GreaterThan,
            unacknowledged_count: 0,
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
            enabled: true,
        };

        self.rules.push(rule);
        self.next_rule_id += 1;

        // Clear form
        self.new_rule_name.clear();
        self.new_rule_metric.clear();
        self.new_rule_threshold.clear();
        self.new_rule_operator = ComparisonOp::GreaterThan;

        Ok(())
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
        if let Some(&last_alert) = self.recent_alerts.get(&key) {
            if timestamp - last_alert < self.alert_cooldown_ms {
                return None;
            }
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
                    if let Some(removed) = self.alerts.pop() {
                        if !removed.acknowledged {
                            self.unacknowledged_count = self.unacknowledged_count.saturating_sub(1);
                        }
                    }
                }

                return Some(alert);
            }
        }

        None
    }

    /// Acknowledge an alert.
    pub fn acknowledge(&mut self, alert_id: u64) {
        if let Some(alert) = self.alerts.iter_mut().find(|a| a.id == alert_id) {
            if !alert.acknowledged {
                alert.acknowledged = true;
                self.unacknowledged_count = self.unacknowledged_count.saturating_sub(1);
            }
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
}

/// Render the alerts view.
pub fn alerts_view(state: &AlertsState) -> Element<'_, Message> {
    let header = render_header(state);
    let new_rule_form = render_new_rule_form(state);
    let rules_section = render_rules_section(state);
    let alerts_section = render_alerts_section(state);

    let content = column![
        header,
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
    let back_button = button(text("<- Back").size(14))
        .on_press(Message::CloseAlerts)
        .style(iced::widget::button::secondary);

    let title = text("Alerts & Notifications").size(24);

    let unack_badge = if state.unacknowledged_count > 0 {
        text(format!("{} unacknowledged", state.unacknowledged_count))
            .size(14)
            .style(|_theme: &Theme| text::Style {
                color: Some(iced::Color::from_rgb(1.0, 0.5, 0.0)),
            })
    } else {
        text("").size(14)
    };

    row![back_button, title, unack_badge]
        .spacing(15)
        .align_y(Alignment::Center)
        .into()
}

/// Render the new rule form.
fn render_new_rule_form(state: &AlertsState) -> Element<'_, Message> {
    let section_title = text("Add Alert Rule").size(18);

    let name_input = text_input("Rule name", &state.new_rule_name)
        .on_input(Message::SetAlertRuleName)
        .padding(8)
        .width(Length::Fixed(200.0));

    let metric_input = text_input("Metric pattern (e.g., ifInErrors)", &state.new_rule_metric)
        .on_input(Message::SetAlertRuleMetric)
        .padding(8)
        .width(Length::Fixed(250.0));

    let operator_picker = pick_list(
        ComparisonOp::ALL,
        Some(state.new_rule_operator),
        Message::SetAlertRuleOperator,
    );

    let threshold_input = text_input("Threshold", &state.new_rule_threshold)
        .on_input(Message::SetAlertRuleThreshold)
        .padding(8)
        .width(Length::Fixed(100.0));

    let add_button = button(text("Add Rule").size(14))
        .on_press(Message::AddAlertRule)
        .style(iced::widget::button::primary);

    let form_row = row![
        name_input,
        metric_input,
        operator_picker,
        threshold_input,
        add_button
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    column![section_title, form_row].spacing(10).into()
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
    let status = if rule.enabled {
        text("\u{25CF}").style(|_theme: &Theme| text::Style {
            color: Some(iced::Color::from_rgb(0.2, 0.8, 0.2)),
        })
    } else {
        text("\u{25CF}").style(|_theme: &Theme| text::Style {
            color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
        })
    };

    let name = text(rule.name.clone()).size(14);
    let condition = text(format!(
        "{} {} {}",
        rule.metric_pattern,
        rule.operator.symbol(),
        format_value(rule.threshold)
    ))
    .size(12)
    .style(|_theme: &Theme| text::Style {
        color: Some(iced::Color::from_rgb(0.6, 0.6, 0.6)),
    });

    let toggle_label = if rule.enabled { "Disable" } else { "Enable" };
    let toggle_button = button(text(toggle_label).size(11))
        .on_press(Message::ToggleAlertRule(rule.id))
        .style(iced::widget::button::secondary);

    let remove_button = button(text("Remove").size(11))
        .on_press(Message::RemoveAlertRule(rule.id))
        .style(iced::widget::button::danger);

    row![status, name, condition, toggle_button, remove_button]
        .spacing(10)
        .align_y(Alignment::Center)
        .into()
}

/// Render the alerts section.
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

/// Render a single alert row.
fn render_alert_row(alert: &Alert) -> Element<'_, Message> {
    let status = if alert.acknowledged {
        text("\u{2713}").style(|_theme: &Theme| text::Style {
            color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
        })
    } else {
        text("\u{26A0}").style(|_theme: &Theme| text::Style {
            color: Some(iced::Color::from_rgb(1.0, 0.5, 0.0)),
        })
    };

    let message = text(alert.message()).size(13);

    let time = text(format_timestamp(alert.timestamp))
        .size(11)
        .style(|_theme: &Theme| text::Style {
            color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
        });

    let mut row_content: Row<'_, Message> =
        Row::new().push(status).push(message).push(time).spacing(10);

    if !alert.acknowledged {
        let ack_button = button(text("Ack").size(10))
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
}
