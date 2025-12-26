//! Theme-aware color palette for ZenSight.
//!
//! This module provides semantic colors that automatically adapt to the current theme.
//! Use these instead of hardcoded Color::from_rgb() values.

use iced::{Color, Theme};

/// Get colors from the theme's extended palette.
/// This provides theme-aware colors for consistent light/dark mode support.
pub struct ThemeColors<'a> {
    theme: &'a Theme,
}

impl<'a> ThemeColors<'a> {
    /// Create a new ThemeColors from a theme reference.
    pub fn new(theme: &'a Theme) -> Self {
        Self { theme }
    }

    /// Get the extended palette from the theme.
    fn palette(&self) -> &iced::theme::palette::Extended {
        self.theme.extended_palette()
    }

    // ========================================================================
    // Background Colors
    // ========================================================================

    /// Primary background color (main content area).
    pub fn background(&self) -> Color {
        self.palette().background.base.color
    }

    /// Weaker background (slightly elevated surfaces).
    pub fn background_weak(&self) -> Color {
        self.palette().background.weak.color
    }

    /// Stronger background (cards, panels).
    pub fn background_strong(&self) -> Color {
        self.palette().background.strong.color
    }

    /// Strongest background (elevated cards, modals).
    pub fn background_strongest(&self) -> Color {
        self.palette().background.strongest.color
    }

    // ========================================================================
    // Text Colors
    // ========================================================================

    /// Primary text color.
    pub fn text(&self) -> Color {
        self.palette().background.base.text
    }

    /// Muted/secondary text color.
    pub fn text_muted(&self) -> Color {
        self.palette().background.weak.text
    }

    /// Dimmed text (less important, disabled).
    pub fn text_dimmed(&self) -> Color {
        // Use a color between muted and background
        let text = self.text();
        let bg = self.background();
        Color::from_rgb(
            text.r * 0.5 + bg.r * 0.5,
            text.g * 0.5 + bg.g * 0.5,
            text.b * 0.5 + bg.b * 0.5,
        )
    }

    // ========================================================================
    // Semantic Colors (these stay consistent across themes)
    // ========================================================================

    /// Success/healthy color (green).
    pub fn success(&self) -> Color {
        self.palette().success.base.color
    }

    /// Success text color.
    pub fn success_text(&self) -> Color {
        self.palette().success.base.text
    }

    /// Danger/error color (red).
    pub fn danger(&self) -> Color {
        self.palette().danger.base.color
    }

    /// Danger text color.
    pub fn danger_text(&self) -> Color {
        self.palette().danger.base.text
    }

    /// Warning color (amber/orange).
    pub fn warning(&self) -> Color {
        // Iced doesn't have a built-in warning, use a custom amber
        if self.is_dark() {
            Color::from_rgb(0.9, 0.7, 0.2)
        } else {
            Color::from_rgb(0.8, 0.6, 0.0)
        }
    }

    /// Primary accent color.
    pub fn primary(&self) -> Color {
        self.palette().primary.base.color
    }

    /// Primary text on primary background.
    pub fn primary_text(&self) -> Color {
        self.palette().primary.base.text
    }

    /// Secondary accent color.
    pub fn secondary(&self) -> Color {
        self.palette().secondary.base.color
    }

    // ========================================================================
    // Border Colors
    // ========================================================================

    /// Default border color.
    pub fn border(&self) -> Color {
        if self.is_dark() {
            Color::from_rgb(0.25, 0.25, 0.3)
        } else {
            Color::from_rgb(0.8, 0.8, 0.82)
        }
    }

    /// Subtle border (less prominent).
    pub fn border_subtle(&self) -> Color {
        if self.is_dark() {
            Color::from_rgb(0.2, 0.2, 0.22)
        } else {
            Color::from_rgb(0.85, 0.85, 0.87)
        }
    }

    // ========================================================================
    // Chart/Graph Colors
    // ========================================================================

    /// Chart background color.
    pub fn chart_background(&self) -> Color {
        if self.is_dark() {
            Color::from_rgb(0.08, 0.08, 0.1)
        } else {
            Color::from_rgb(0.98, 0.98, 0.99)
        }
    }

    /// Chart outer background.
    pub fn chart_outer_background(&self) -> Color {
        if self.is_dark() {
            Color::from_rgb(0.1, 0.1, 0.12)
        } else {
            Color::from_rgb(0.95, 0.95, 0.96)
        }
    }

    /// Chart grid lines.
    pub fn chart_grid(&self) -> Color {
        if self.is_dark() {
            Color::from_rgb(0.2, 0.2, 0.25)
        } else {
            Color::from_rgb(0.85, 0.85, 0.88)
        }
    }

    /// Chart axis labels.
    pub fn chart_label(&self) -> Color {
        if self.is_dark() {
            Color::from_rgb(0.5, 0.5, 0.5)
        } else {
            Color::from_rgb(0.4, 0.4, 0.4)
        }
    }

    /// Chart tooltip background.
    pub fn chart_tooltip_background(&self) -> Color {
        if self.is_dark() {
            Color::from_rgba(0.0, 0.0, 0.0, 0.85)
        } else {
            Color::from_rgba(1.0, 1.0, 1.0, 0.95)
        }
    }

    /// Chart highlight color (cursor, selection).
    pub fn chart_highlight(&self) -> Color {
        if self.is_dark() {
            Color::from_rgb(0.3, 0.8, 1.0)
        } else {
            Color::from_rgb(0.1, 0.5, 0.8)
        }
    }

    // ========================================================================
    // Card/Container Colors
    // ========================================================================

    /// Card background color.
    pub fn card_background(&self) -> Color {
        if self.is_dark() {
            Color::from_rgb(0.12, 0.12, 0.14)
        } else {
            Color::from_rgb(1.0, 1.0, 1.0)
        }
    }

    /// Card hover background.
    pub fn card_hover_background(&self) -> Color {
        if self.is_dark() {
            Color::from_rgb(0.15, 0.15, 0.17)
        } else {
            Color::from_rgb(0.97, 0.97, 0.98)
        }
    }

    /// Row/list item background.
    pub fn row_background(&self) -> Color {
        if self.is_dark() {
            Color::from_rgb(0.13, 0.13, 0.15)
        } else {
            Color::from_rgb(0.98, 0.98, 0.99)
        }
    }

    /// Alternating row background.
    pub fn row_background_alt(&self) -> Color {
        if self.is_dark() {
            Color::from_rgb(0.11, 0.11, 0.13)
        } else {
            Color::from_rgb(0.96, 0.96, 0.97)
        }
    }

    /// Table header background.
    pub fn table_header(&self) -> Color {
        if self.is_dark() {
            Color::from_rgb(0.18, 0.18, 0.2)
        } else {
            Color::from_rgb(0.92, 0.92, 0.94)
        }
    }

    /// Section/panel background.
    pub fn section_background(&self) -> Color {
        if self.is_dark() {
            Color::from_rgb(0.12, 0.12, 0.14)
        } else {
            Color::from_rgb(0.96, 0.96, 0.97)
        }
    }

    // ========================================================================
    // Status Colors (consistent across themes for recognition)
    // ========================================================================

    /// Connected/online status.
    pub fn status_connected(&self) -> Color {
        Color::from_rgb(0.2, 0.8, 0.2)
    }

    /// Disconnected/offline status.
    pub fn status_disconnected(&self) -> Color {
        Color::from_rgb(0.8, 0.2, 0.2)
    }

    /// Healthy status.
    pub fn status_healthy(&self) -> Color {
        Color::from_rgb(0.2, 0.8, 0.3)
    }

    /// Warning status.
    pub fn status_warning(&self) -> Color {
        Color::from_rgb(0.9, 0.7, 0.2)
    }

    /// Error/critical status.
    pub fn status_error(&self) -> Color {
        Color::from_rgb(0.9, 0.2, 0.2)
    }

    /// Unknown/inactive status.
    pub fn status_unknown(&self) -> Color {
        if self.is_dark() {
            Color::from_rgb(0.5, 0.5, 0.5)
        } else {
            Color::from_rgb(0.6, 0.6, 0.6)
        }
    }

    // ========================================================================
    // Protocol Colors (consistent for brand recognition)
    // ========================================================================

    /// TCP protocol color (blue).
    pub fn protocol_tcp(&self) -> Color {
        Color::from_rgb(0.3, 0.6, 0.9)
    }

    /// UDP protocol color (green).
    pub fn protocol_udp(&self) -> Color {
        Color::from_rgb(0.4, 0.8, 0.4)
    }

    /// ICMP protocol color (orange).
    pub fn protocol_icmp(&self) -> Color {
        Color::from_rgb(0.9, 0.5, 0.3)
    }

    /// Other protocol color (purple).
    pub fn protocol_other(&self) -> Color {
        Color::from_rgb(0.7, 0.4, 0.8)
    }

    // ========================================================================
    // Syslog Severity Colors (consistent for recognition)
    // ========================================================================

    /// Emergency/Alert severity (bright red).
    pub fn severity_emergency(&self) -> Color {
        Color::from_rgb(0.95, 0.2, 0.2)
    }

    /// Critical/Error severity (red-orange).
    pub fn severity_error(&self) -> Color {
        Color::from_rgb(0.9, 0.4, 0.3)
    }

    /// Warning severity (amber).
    pub fn severity_warning(&self) -> Color {
        Color::from_rgb(0.9, 0.7, 0.2)
    }

    /// Notice severity (blue).
    pub fn severity_notice(&self) -> Color {
        Color::from_rgb(0.4, 0.7, 0.9)
    }

    /// Info severity (green).
    pub fn severity_info(&self) -> Color {
        Color::from_rgb(0.5, 0.8, 0.5)
    }

    /// Debug severity (gray).
    pub fn severity_debug(&self) -> Color {
        if self.is_dark() {
            Color::from_rgb(0.6, 0.6, 0.6)
        } else {
            Color::from_rgb(0.5, 0.5, 0.5)
        }
    }

    /// Critical severity background (emergency, alert, critical).
    pub fn severity_critical_bg(&self) -> Color {
        if self.is_dark() {
            Color::from_rgb(0.2, 0.1, 0.1)
        } else {
            Color::from_rgb(1.0, 0.95, 0.95)
        }
    }

    /// Error severity background.
    pub fn severity_error_bg(&self) -> Color {
        if self.is_dark() {
            Color::from_rgb(0.18, 0.12, 0.1)
        } else {
            Color::from_rgb(1.0, 0.97, 0.95)
        }
    }

    /// Warning severity background.
    pub fn severity_warning_bg(&self) -> Color {
        if self.is_dark() {
            Color::from_rgb(0.18, 0.16, 0.1)
        } else {
            Color::from_rgb(1.0, 0.99, 0.95)
        }
    }

    /// Syslog row background based on severity.
    pub fn syslog_row_background(
        &self,
        is_critical: bool,
        is_error: bool,
        is_warning: bool,
    ) -> Color {
        if is_critical {
            self.severity_critical_bg()
        } else if is_error {
            self.severity_error_bg()
        } else if is_warning {
            self.severity_warning_bg()
        } else {
            self.row_background()
        }
    }

    // ========================================================================
    // Utility
    // ========================================================================

    /// Check if the current theme is dark.
    pub fn is_dark(&self) -> bool {
        self.palette().is_dark
    }
}

/// Convenience function to create ThemeColors.
pub fn colors(theme: &Theme) -> ThemeColors<'_> {
    ThemeColors::new(theme)
}
