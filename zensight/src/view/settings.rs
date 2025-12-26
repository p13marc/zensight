//! Settings view for application configuration.

use std::path::PathBuf;

use iced::widget::{Column, column, container, pick_list, row, rule, scrollable, text, text_input};
use iced::{Alignment, Element, Length, Theme};
use iced_anim::widget::button;
use serde::{Deserialize, Serialize};

use crate::app::CurrentView;
use crate::message::Message;
use crate::view::alerts::AlertRule;
use crate::view::groups::GroupsState;
use crate::view::icons::{self, IconSize};
use zensight_common::Protocol;

/// Persistent settings that are saved to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistentSettings {
    /// Zenoh connection mode.
    pub zenoh_mode: String,
    /// Zenoh router/peer endpoints to connect to.
    pub zenoh_connect: Vec<String>,
    /// Zenoh endpoints to listen on.
    pub zenoh_listen: Vec<String>,
    /// Stale threshold in seconds.
    pub stale_threshold_secs: u64,
    /// Use dark theme (true) or light theme (false).
    #[serde(default = "default_dark_theme")]
    pub dark_theme: bool,
    /// Maximum number of metric history entries per device.
    #[serde(default = "default_max_history")]
    pub max_history: usize,
    /// Maximum number of alerts to keep.
    #[serde(default = "default_max_alerts")]
    pub max_alerts: usize,
    /// Device groups configuration.
    #[serde(default)]
    pub groups: GroupsState,
    /// Alert rules.
    #[serde(default)]
    pub alert_rules: Vec<AlertRule>,
    /// Selected overview protocol tab.
    #[serde(default)]
    pub overview_selected_protocol: Option<Protocol>,
    /// Whether the overview section is expanded.
    #[serde(default = "default_overview_expanded")]
    pub overview_expanded: bool,
    /// Last active view (Dashboard, Alerts, or Topology).
    #[serde(default)]
    pub current_view: CurrentView,
}

fn default_overview_expanded() -> bool {
    true
}

fn default_dark_theme() -> bool {
    true
}

fn default_max_history() -> usize {
    500
}

fn default_max_alerts() -> usize {
    100
}

impl Default for PersistentSettings {
    fn default() -> Self {
        Self {
            zenoh_mode: "peer".to_string(),
            zenoh_connect: vec![],
            zenoh_listen: vec![],
            stale_threshold_secs: 120,
            dark_theme: true,
            max_history: default_max_history(),
            max_alerts: default_max_alerts(),
            groups: GroupsState::default(),
            alert_rules: Vec::new(),
            overview_selected_protocol: None,
            overview_expanded: default_overview_expanded(),
            current_view: CurrentView::default(),
        }
    }
}

impl PersistentSettings {
    /// Get the settings file path.
    pub fn config_path() -> Option<PathBuf> {
        dirs::config_dir().map(|p| p.join("zensight").join("settings.json5"))
    }

    /// Load settings from disk, or create defaults if not found.
    pub fn load() -> Self {
        let Some(path) = Self::config_path() else {
            tracing::warn!("Could not determine config directory");
            return Self::default();
        };

        if !path.exists() {
            tracing::info!("No settings file found at {:?}, creating defaults", path);
            let defaults = Self::default();
            // Create the config file with defaults
            if let Err(e) = defaults.save() {
                tracing::warn!("Failed to create default settings file: {}", e);
            }
            return defaults;
        }

        match std::fs::read_to_string(&path) {
            Ok(contents) => match json5::from_str(&contents) {
                Ok(settings) => {
                    tracing::info!("Loaded settings from {:?}", path);
                    settings
                }
                Err(e) => {
                    tracing::error!("Failed to parse settings file: {}", e);
                    Self::default()
                }
            },
            Err(e) => {
                tracing::error!("Failed to read settings file: {}", e);
                Self::default()
            }
        }
    }

    /// Save settings to disk.
    pub fn save(&self) -> Result<(), String> {
        let Some(path) = Self::config_path() else {
            return Err("Could not determine config directory".to_string());
        };

        // Ensure parent directory exists
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return Err(format!("Failed to create config directory: {}", e));
        }

        // Serialize to JSON5 (pretty-printed JSON is valid JSON5)
        let contents = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize settings: {}", e))?;

        std::fs::write(&path, contents)
            .map_err(|e| format!("Failed to write settings file: {}", e))?;

        tracing::info!("Saved settings to {:?}", path);
        Ok(())
    }

    /// Convert to SettingsState for UI.
    pub fn to_state(&self) -> SettingsState {
        SettingsState::from_config(
            &self.zenoh_mode,
            &self.zenoh_connect,
            &self.zenoh_listen,
            (self.stale_threshold_secs * 1000) as i64,
            self.dark_theme,
            self.max_history,
            self.max_alerts,
        )
    }

    /// Create from SettingsState.
    /// Note: groups, alert_rules, and overview state should be set separately.
    pub fn from_state(state: &SettingsState) -> Self {
        Self {
            zenoh_mode: state.zenoh_mode.as_str().to_string(),
            zenoh_connect: state.connect_endpoints(),
            zenoh_listen: state.listen_endpoints(),
            stale_threshold_secs: state.stale_threshold_secs.parse().unwrap_or(120),
            dark_theme: state.dark_theme,
            max_history: state.max_history.parse().unwrap_or(default_max_history()),
            max_alerts: state.max_alerts.parse().unwrap_or(default_max_alerts()),
            groups: GroupsState::default(),
            alert_rules: Vec::new(),
            overview_selected_protocol: None,
            overview_expanded: default_overview_expanded(),
            current_view: CurrentView::default(),
        }
    }
}

/// Application settings state.
#[derive(Debug, Clone)]
pub struct SettingsState {
    /// Zenoh connection mode.
    pub zenoh_mode: ZenohMode,
    /// Zenoh router/peer endpoints to connect to.
    pub zenoh_connect: String,
    /// Zenoh endpoints to listen on.
    pub zenoh_listen: String,
    /// Stale threshold in seconds (devices not updated are marked unhealthy).
    pub stale_threshold_secs: String,
    /// Use dark theme.
    pub dark_theme: bool,
    /// Maximum metric history entries per device.
    pub max_history: String,
    /// Maximum alerts to keep.
    pub max_alerts: String,
    /// Whether settings have been modified.
    pub modified: bool,
    /// Last error message (if any).
    pub error: Option<String>,
    /// Success message (if any).
    pub success: Option<String>,
}

impl Default for SettingsState {
    fn default() -> Self {
        Self {
            zenoh_mode: ZenohMode::Peer,
            zenoh_connect: String::new(),
            zenoh_listen: String::new(),
            stale_threshold_secs: "120".to_string(),
            dark_theme: true,
            max_history: "500".to_string(),
            max_alerts: "100".to_string(),
            modified: false,
            error: None,
            success: None,
        }
    }
}

impl SettingsState {
    /// Create settings from current app configuration.
    pub fn from_config(
        mode: &str,
        connect: &[String],
        listen: &[String],
        stale_threshold_ms: i64,
        dark_theme: bool,
        max_history: usize,
        max_alerts: usize,
    ) -> Self {
        Self {
            zenoh_mode: ZenohMode::parse(mode),
            zenoh_connect: connect.join(", "),
            zenoh_listen: listen.join(", "),
            stale_threshold_secs: (stale_threshold_ms / 1000).to_string(),
            dark_theme,
            max_history: max_history.to_string(),
            max_alerts: max_alerts.to_string(),
            modified: false,
            error: None,
            success: None,
        }
    }

    /// Update Zenoh mode.
    pub fn set_mode(&mut self, mode: ZenohMode) {
        self.zenoh_mode = mode;
        self.modified = true;
        self.clear_messages();
    }

    /// Update connect endpoints.
    pub fn set_connect(&mut self, connect: String) {
        self.zenoh_connect = connect;
        self.modified = true;
        self.clear_messages();
    }

    /// Update listen endpoints.
    pub fn set_listen(&mut self, listen: String) {
        self.zenoh_listen = listen;
        self.modified = true;
        self.clear_messages();
    }

    /// Update stale threshold.
    pub fn set_stale_threshold(&mut self, threshold: String) {
        self.stale_threshold_secs = threshold;
        self.modified = true;
        self.clear_messages();
    }

    /// Update max history.
    pub fn set_max_history(&mut self, max_history: String) {
        self.max_history = max_history;
        self.modified = true;
        self.clear_messages();
    }

    /// Update max alerts.
    pub fn set_max_alerts(&mut self, max_alerts: String) {
        self.max_alerts = max_alerts;
        self.modified = true;
        self.clear_messages();
    }

    /// Validate the settings.
    pub fn validate(&self) -> Result<(), String> {
        // Validate stale threshold
        let threshold: i64 = self
            .stale_threshold_secs
            .parse()
            .map_err(|_| "Stale threshold must be a number".to_string())?;

        if threshold < 1 {
            return Err("Stale threshold must be at least 1 second".to_string());
        }

        if threshold > 86400 {
            return Err("Stale threshold cannot exceed 24 hours".to_string());
        }

        // Validate endpoints format (basic check)
        for endpoint in self.parse_endpoints(&self.zenoh_connect) {
            if !endpoint.is_empty() && !endpoint.contains('/') {
                return Err(format!("Invalid connect endpoint format: {}", endpoint));
            }
        }

        for endpoint in self.parse_endpoints(&self.zenoh_listen) {
            if !endpoint.is_empty() && !endpoint.contains('/') {
                return Err(format!("Invalid listen endpoint format: {}", endpoint));
            }
        }

        // Validate max history
        let max_history: usize = self
            .max_history
            .parse()
            .map_err(|_| "Max history must be a number".to_string())?;

        if max_history < 10 {
            return Err("Max history must be at least 10".to_string());
        }

        if max_history > 10000 {
            return Err("Max history cannot exceed 10000".to_string());
        }

        // Validate max alerts
        let max_alerts: usize = self
            .max_alerts
            .parse()
            .map_err(|_| "Max alerts must be a number".to_string())?;

        if max_alerts < 10 {
            return Err("Max alerts must be at least 10".to_string());
        }

        if max_alerts > 1000 {
            return Err("Max alerts cannot exceed 1000".to_string());
        }

        Ok(())
    }

    /// Parse comma-separated endpoints.
    fn parse_endpoints(&self, input: &str) -> Vec<String> {
        input
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Get connect endpoints as a vector.
    pub fn connect_endpoints(&self) -> Vec<String> {
        self.parse_endpoints(&self.zenoh_connect)
    }

    /// Get listen endpoints as a vector.
    pub fn listen_endpoints(&self) -> Vec<String> {
        self.parse_endpoints(&self.zenoh_listen)
    }

    /// Get stale threshold in milliseconds.
    pub fn stale_threshold_ms(&self) -> i64 {
        self.stale_threshold_secs.parse::<i64>().unwrap_or(120) * 1000
    }

    /// Get max history value.
    pub fn max_history_value(&self) -> usize {
        self.max_history.parse().unwrap_or(500)
    }

    /// Get max alerts value.
    pub fn max_alerts_value(&self) -> usize {
        self.max_alerts.parse().unwrap_or(100)
    }

    /// Mark settings as saved.
    pub fn mark_saved(&mut self) {
        self.modified = false;
        self.error = None;
        self.success = Some("Settings saved successfully".to_string());
    }

    /// Set error message.
    pub fn set_error(&mut self, error: String) {
        self.error = Some(error);
        self.success = None;
    }

    /// Clear messages.
    fn clear_messages(&mut self) {
        self.error = None;
        self.success = None;
    }
}

/// Zenoh connection mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ZenohMode {
    /// Client mode - connects to routers only.
    Client,
    /// Peer mode - can connect to peers and routers.
    #[default]
    Peer,
    /// Router mode - accepts connections from clients and peers.
    Router,
}

impl ZenohMode {
    /// All available modes.
    pub const ALL: &'static [ZenohMode] = &[ZenohMode::Client, ZenohMode::Peer, ZenohMode::Router];

    /// Parse from string (defaults to Peer for unknown values).
    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "client" => ZenohMode::Client,
            "router" => ZenohMode::Router,
            _ => ZenohMode::Peer,
        }
    }

    /// Convert to string.
    pub fn as_str(&self) -> &'static str {
        match self {
            ZenohMode::Client => "client",
            ZenohMode::Peer => "peer",
            ZenohMode::Router => "router",
        }
    }
}

impl std::fmt::Display for ZenohMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Render the settings view.
pub fn settings_view(state: &SettingsState) -> Element<'_, Message> {
    let header = render_header(state);
    let zenoh_section = render_zenoh_section(state);
    let display_section = render_display_section(state);
    let actions = render_actions(state);

    let content = column![
        header,
        rule::horizontal(1),
        zenoh_section,
        rule::horizontal(1),
        display_section,
        rule::horizontal(1),
        actions,
    ]
    .spacing(20)
    .padding(20);

    container(scrollable(content))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Render header with back button.
fn render_header(state: &SettingsState) -> Element<'_, Message> {
    let back_button = button(
        row![icons::arrow_left(IconSize::Medium), text("Back").size(14)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .on_press(Message::CloseSettings)
    .style(iced::widget::button::secondary);

    let title = row![icons::settings(IconSize::XLarge), text("Settings").size(24)]
        .spacing(10)
        .align_y(Alignment::Center);

    let modified_indicator: Element<'_, Message> = if state.modified {
        row![
            icons::status_warning(IconSize::Small),
            text("(unsaved changes)")
                .size(12)
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

    row![back_button, title, modified_indicator]
        .spacing(15)
        .align_y(Alignment::Center)
        .into()
}

/// Render Zenoh connection section.
fn render_zenoh_section(state: &SettingsState) -> Element<'_, Message> {
    let section_title = text("Zenoh Connection").size(18);

    // Mode picker
    let mode_label = text("Mode:").size(14);
    let mode_picker = pick_list(
        ZenohMode::ALL,
        Some(state.zenoh_mode),
        Message::SetZenohMode,
    )
    .placeholder("Select mode");

    let mode_help = text(match state.zenoh_mode {
        ZenohMode::Client => "Connects to routers only, minimal resource usage",
        ZenohMode::Peer => "Connects to peers and routers, enables discovery",
        ZenohMode::Router => "Accepts connections, routes traffic between nodes",
    })
    .size(11)
    .style(|theme: &Theme| text::Style {
        color: Some(crate::view::theme::colors(theme).text_dimmed()),
    });

    let mode_row = row![mode_label, mode_picker]
        .spacing(10)
        .align_y(Alignment::Center);

    // Connect endpoints
    let connect_label = text("Connect endpoints:").size(14);
    let connect_input = text_input(
        "tcp/localhost:7447, tcp/192.168.1.1:7447",
        &state.zenoh_connect,
    )
    .on_input(Message::SetZenohConnect)
    .padding(8)
    .width(Length::Fixed(400.0));

    let connect_help = text("Comma-separated Zenoh locators to connect to")
        .size(11)
        .style(|theme: &Theme| text::Style {
            color: Some(crate::view::theme::colors(theme).text_dimmed()),
        });

    // Listen endpoints
    let listen_label = text("Listen endpoints:").size(14);
    let listen_input = text_input("tcp/0.0.0.0:7448", &state.zenoh_listen)
        .on_input(Message::SetZenohListen)
        .padding(8)
        .width(Length::Fixed(400.0));

    let listen_help = text("Comma-separated Zenoh locators to listen on (for router/peer mode)")
        .size(11)
        .style(|theme: &Theme| text::Style {
            color: Some(crate::view::theme::colors(theme).text_dimmed()),
        });

    column![
        section_title,
        mode_row,
        mode_help,
        connect_label,
        connect_input,
        connect_help,
        listen_label,
        listen_input,
        listen_help,
    ]
    .spacing(8)
    .into()
}

/// Render display settings section.
fn render_display_section(state: &SettingsState) -> Element<'_, Message> {
    let section_title = text("Display Settings").size(18);

    // Stale threshold
    let threshold_label = text("Stale threshold (seconds):").size(14);
    let threshold_input = text_input("120", &state.stale_threshold_secs)
        .on_input(Message::SetStaleThreshold)
        .padding(8)
        .width(Length::Fixed(100.0));

    let threshold_help = text("Devices not updated within this time are marked as unhealthy")
        .size(11)
        .style(|theme: &Theme| text::Style {
            color: Some(crate::view::theme::colors(theme).text_dimmed()),
        });

    let threshold_row = row![threshold_label, threshold_input]
        .spacing(10)
        .align_y(Alignment::Center);

    // Max history
    let history_label = text("Max metric history per device:").size(14);
    let history_input = text_input("500", &state.max_history)
        .on_input(Message::SetMaxHistory)
        .padding(8)
        .width(Length::Fixed(100.0));

    let history_help = text("Maximum data points to keep per metric (10-10000)")
        .size(11)
        .style(|theme: &Theme| text::Style {
            color: Some(crate::view::theme::colors(theme).text_dimmed()),
        });

    let history_row = row![history_label, history_input]
        .spacing(10)
        .align_y(Alignment::Center);

    // Max alerts
    let alerts_label = text("Max alerts to keep:").size(14);
    let alerts_input = text_input("100", &state.max_alerts)
        .on_input(Message::SetMaxAlerts)
        .padding(8)
        .width(Length::Fixed(100.0));

    let alerts_help = text("Maximum alerts to keep in history (10-1000)")
        .size(11)
        .style(|theme: &Theme| text::Style {
            color: Some(crate::view::theme::colors(theme).text_dimmed()),
        });

    let alerts_row = row![alerts_label, alerts_input]
        .spacing(10)
        .align_y(Alignment::Center);

    column![
        section_title,
        threshold_row,
        threshold_help,
        history_row,
        history_help,
        alerts_row,
        alerts_help,
    ]
    .spacing(8)
    .into()
}

/// Render action buttons and messages.
fn render_actions(state: &SettingsState) -> Element<'_, Message> {
    let mut content = Column::new().spacing(10);

    // Error message
    if let Some(error) = &state.error {
        let error_text = text(format!("Error: {}", error))
            .size(14)
            .style(|theme: &Theme| text::Style {
                color: Some(crate::view::theme::colors(theme).danger()),
            });
        content = content.push(error_text);
    }

    // Success message
    if let Some(success) = &state.success {
        let success_text = text(success).size(14).style(|theme: &Theme| text::Style {
            color: Some(crate::view::theme::colors(theme).success()),
        });
        content = content.push(success_text);
    }

    // Buttons
    let save_button = button(text("Save Settings").size(14))
        .on_press(Message::SaveSettings)
        .style(iced::widget::button::primary);

    let reset_button = button(text("Reset to Defaults").size(14))
        .on_press(Message::ResetSettings)
        .style(iced::widget::button::secondary);

    let buttons = row![save_button, reset_button].spacing(10);

    content = content.push(buttons);

    // Note about restart
    let note = text("Note: Zenoh connection changes require application restart to take effect")
        .size(11)
        .style(|theme: &Theme| text::Style {
            color: Some(crate::view::theme::colors(theme).text_muted()),
        });

    content = content.push(note);

    content.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_settings_validation() {
        let mut settings = SettingsState::default();

        // Valid settings
        assert!(settings.validate().is_ok());

        // Invalid threshold
        settings.stale_threshold_secs = "abc".to_string();
        assert!(settings.validate().is_err());

        settings.stale_threshold_secs = "0".to_string();
        assert!(settings.validate().is_err());

        settings.stale_threshold_secs = "100000".to_string();
        assert!(settings.validate().is_err());

        settings.stale_threshold_secs = "60".to_string();
        assert!(settings.validate().is_ok());
    }

    #[test]
    fn test_parse_endpoints() {
        let settings = SettingsState::default();

        let endpoints = settings.parse_endpoints("tcp/localhost:7447, tcp/192.168.1.1:7447");
        assert_eq!(endpoints.len(), 2);
        assert_eq!(endpoints[0], "tcp/localhost:7447");
        assert_eq!(endpoints[1], "tcp/192.168.1.1:7447");

        let empty = settings.parse_endpoints("");
        assert!(empty.is_empty());
    }

    #[test]
    fn test_zenoh_mode() {
        assert_eq!(ZenohMode::parse("client"), ZenohMode::Client);
        assert_eq!(ZenohMode::parse("peer"), ZenohMode::Peer);
        assert_eq!(ZenohMode::parse("router"), ZenohMode::Router);
        assert_eq!(ZenohMode::parse("unknown"), ZenohMode::Peer);
    }

    #[test]
    fn test_persistent_settings_serialization_roundtrip() {
        let settings = PersistentSettings {
            zenoh_mode: "router".to_string(),
            zenoh_connect: vec!["tcp/localhost:7447".to_string()],
            zenoh_listen: vec!["tcp/0.0.0.0:7448".to_string()],
            stale_threshold_secs: 60,
            dark_theme: true,
            max_history: 1000,
            max_alerts: 200,
            groups: GroupsState::default(),
            alert_rules: Vec::new(),
            overview_selected_protocol: None,
            overview_expanded: true,
            current_view: CurrentView::default(),
        };

        // Serialize to JSON
        let json = serde_json::to_string(&settings).expect("serialize");

        // Deserialize back
        let restored: PersistentSettings = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(restored.zenoh_mode, "router");
        assert_eq!(restored.zenoh_connect, vec!["tcp/localhost:7447"]);
        assert_eq!(restored.zenoh_listen, vec!["tcp/0.0.0.0:7448"]);
        assert_eq!(restored.stale_threshold_secs, 60);
        assert_eq!(restored.max_history, 1000);
        assert_eq!(restored.max_alerts, 200);
    }

    #[test]
    fn test_persistent_settings_state_conversion() {
        let persistent = PersistentSettings {
            zenoh_mode: "client".to_string(),
            zenoh_connect: vec!["tcp/router:7447".to_string()],
            zenoh_listen: vec![],
            stale_threshold_secs: 90,
            dark_theme: false,
            max_history: 750,
            max_alerts: 150,
            groups: GroupsState::default(),
            alert_rules: Vec::new(),
            overview_selected_protocol: None,
            overview_expanded: true,
            current_view: CurrentView::default(),
        };

        // Convert to UI state
        let state = persistent.to_state();
        assert_eq!(state.zenoh_mode, ZenohMode::Client);
        assert_eq!(state.zenoh_connect, "tcp/router:7447");
        assert!(state.zenoh_listen.is_empty());
        assert_eq!(state.stale_threshold_secs, "90");
        assert_eq!(state.max_history, "750");
        assert_eq!(state.max_alerts, "150");

        // Convert back to persistent
        let restored = PersistentSettings::from_state(&state);
        assert_eq!(restored.zenoh_mode, "client");
        assert_eq!(restored.zenoh_connect, vec!["tcp/router:7447"]);
        assert!(restored.zenoh_listen.is_empty());
        assert_eq!(restored.stale_threshold_secs, 90);
        assert_eq!(restored.max_history, 750);
        assert_eq!(restored.max_alerts, 150);
    }
}
