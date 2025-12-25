//! Settings view for application configuration.

use iced::widget::{
    Column, button, column, container, pick_list, row, rule, scrollable, text, text_input,
};
use iced::{Alignment, Element, Length, Theme};

use crate::message::Message;

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
    ) -> Self {
        Self {
            zenoh_mode: ZenohMode::from_str(mode),
            zenoh_connect: connect.join(", "),
            zenoh_listen: listen.join(", "),
            stale_threshold_secs: (stale_threshold_ms / 1000).to_string(),
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

    /// Convert from string.
    pub fn from_str(s: &str) -> Self {
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
    let back_button = button(text("<- Back").size(14))
        .on_press(Message::CloseSettings)
        .style(iced::widget::button::secondary);

    let title = text("Settings").size(24);

    let modified_indicator = if state.modified {
        text("(unsaved changes)")
            .size(12)
            .style(|_theme: &Theme| text::Style {
                color: Some(iced::Color::from_rgb(1.0, 0.7, 0.0)),
            })
    } else {
        text("")
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
    .style(|_theme: &Theme| text::Style {
        color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
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
        .style(|_theme: &Theme| text::Style {
            color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
        });

    // Listen endpoints
    let listen_label = text("Listen endpoints:").size(14);
    let listen_input = text_input("tcp/0.0.0.0:7448", &state.zenoh_listen)
        .on_input(Message::SetZenohListen)
        .padding(8)
        .width(Length::Fixed(400.0));

    let listen_help = text("Comma-separated Zenoh locators to listen on (for router/peer mode)")
        .size(11)
        .style(|_theme: &Theme| text::Style {
            color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
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
        .style(|_theme: &Theme| text::Style {
            color: Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
        });

    let threshold_row = row![threshold_label, threshold_input]
        .spacing(10)
        .align_y(Alignment::Center);

    column![section_title, threshold_row, threshold_help,]
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
            .style(|_theme: &Theme| text::Style {
                color: Some(iced::Color::from_rgb(1.0, 0.3, 0.3)),
            });
        content = content.push(error_text);
    }

    // Success message
    if let Some(success) = &state.success {
        let success_text = text(success).size(14).style(|_theme: &Theme| text::Style {
            color: Some(iced::Color::from_rgb(0.3, 1.0, 0.3)),
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
        .style(|_theme: &Theme| text::Style {
            color: Some(iced::Color::from_rgb(0.6, 0.6, 0.6)),
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
        assert_eq!(ZenohMode::from_str("client"), ZenohMode::Client);
        assert_eq!(ZenohMode::from_str("peer"), ZenohMode::Peer);
        assert_eq!(ZenohMode::from_str("router"), ZenohMode::Router);
        assert_eq!(ZenohMode::from_str("unknown"), ZenohMode::Peer);
    }
}
