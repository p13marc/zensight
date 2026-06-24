//! The persistent application shell: a left nav rail + a top bar (breadcrumb +
//! connection status + alert badge) that wrap every screen. Rendered once in
//! `App::view`, so navigation chrome and global status are consistent on every
//! page instead of each view rolling its own header.
//!
//! See docs/plans/gui/02-app-shell-and-navigation.md.

use iced::widget::{button, column, container, row, text};
use iced::{Alignment, Background, Border, Element, Length, Theme};

use crate::app::CurrentView;
use crate::message::Message;
use crate::view::dashboard::ConnectionState;
use crate::view::icons::{self, IconSize};
use crate::view::theme;
use crate::view::tokens::{font, space};

/// One nav-rail entry.
struct NavItem {
    label: &'static str,
    message: Message,
    icon: fn(IconSize) -> Element<'static, Message>,
    /// Is this entry the active section for `current`?
    active: bool,
}

fn nav_items(current: CurrentView) -> Vec<NavItem> {
    use CurrentView::*;
    // Device is a drill-down of the dashboard, so it keeps Dashboard active.
    let dash_active = matches!(current, Dashboard | Device);
    vec![
        NavItem {
            label: "Dashboard",
            message: Message::OpenDashboard,
            icon: icons::chart,
            active: dash_active,
        },
        NavItem {
            label: "Topology",
            message: Message::OpenTopology,
            icon: icons::network,
            active: matches!(current, Topology),
        },
        NavItem {
            label: "Alerts",
            message: Message::OpenAlerts,
            icon: icons::alert,
            active: matches!(current, Alerts),
        },
        NavItem {
            label: "Security",
            message: Message::OpenSecurity,
            icon: icons::info,
            active: matches!(current, Security),
        },
        NavItem {
            label: "Sensors",
            message: Message::OpenSensors,
            icon: icons::subscription,
            active: matches!(current, Sensors),
        },
        NavItem {
            label: "Expectations",
            message: Message::OpenExpectations,
            icon: icons::check,
            active: matches!(current, Expectations),
        },
        NavItem {
            label: "Settings",
            message: Message::OpenSettings,
            icon: icons::settings,
            active: matches!(current, Settings),
        },
    ]
}

/// The left navigation rail.
fn nav_rail(current: CurrentView) -> Element<'static, Message> {
    let mut col = column![text("ZenSight").size(font::EMPHASIS),]
        .spacing(space::SM)
        .padding(space::SM)
        .width(Length::Fixed(168.0));

    for item in nav_items(current) {
        let content = row![
            (item.icon)(IconSize::Medium),
            text(item.label).size(font::BODY)
        ]
        .spacing(space::SM)
        .align_y(Alignment::Center);
        let btn = button(content)
            .on_press(item.message)
            .width(Length::Fill)
            .padding([space::XS, space::SM])
            .style(if item.active {
                iced::widget::button::primary
            } else {
                iced::widget::button::text
            });
        col = col.push(btn);
    }

    container(col)
        .height(Length::Fill)
        .style(|theme: &Theme| container::Style {
            background: Some(Background::Color(theme::colors(theme).section_background())),
            border: Border {
                color: theme::colors(theme).border_subtle(),
                width: 1.0,
                radius: 0.0.into(),
            },
            ..Default::default()
        })
        .into()
}

/// The breadcrumb (left side of the top bar): `Section` or `Dashboard › <device>`.
fn breadcrumb<'a>(current: CurrentView, device: Option<&'a str>) -> Element<'a, Message> {
    let section = match current {
        CurrentView::Dashboard | CurrentView::Device => "Dashboard",
        CurrentView::Topology => "Topology",
        CurrentView::Alerts => "Alerts",
        CurrentView::Security => "Security",
        CurrentView::Sensors => "Sensors",
        CurrentView::Expectations => "Expectations",
        CurrentView::Settings => "Settings",
    };

    if let (CurrentView::Device, Some(name)) = (current, device) {
        // Dashboard segment is clickable; the device leaf is plain text.
        let root = button(text("Dashboard").size(font::BODY))
            .on_press(Message::OpenDashboard)
            .padding(0)
            .style(iced::widget::button::text);
        row![
            root,
            text(" › ").size(font::BODY).style(dim),
            text(name.to_string()).size(font::BODY),
        ]
        .align_y(Alignment::Center)
        .into()
    } else {
        text(section).size(font::SECTION).into()
    }
}

/// The connection status indicator (right side of the top bar).
fn connection_status<'a>(connection: ConnectionState) -> Element<'a, Message> {
    let (icon, label) = match connection {
        ConnectionState::Connected => (icons::connected(IconSize::Small), "Connected"),
        ConnectionState::Connecting => (icons::disconnected(IconSize::Small), "Connecting…"),
        ConnectionState::Disconnected => (icons::disconnected(IconSize::Small), "Disconnected"),
    };
    let label = text(label).size(font::CAPTION).style(move |theme: &Theme| {
        let c = theme::colors(theme);
        let color = match connection {
            ConnectionState::Connected => c.status_connected(),
            ConnectionState::Connecting => c.warning(),
            ConnectionState::Disconnected => c.status_disconnected(),
        };
        text::Style { color: Some(color) }
    });
    row![icon, label]
        .spacing(space::XS)
        .align_y(Alignment::Center)
        .into()
}

/// The top bar: breadcrumb (left) · alert badge + freshness + connection (right).
#[allow(clippy::too_many_arguments)]
fn top_bar<'a>(
    current: CurrentView,
    device: Option<&'a str>,
    connection: ConnectionState,
    alert_count: usize,
    last_update_ms: Option<i64>,
    now_ms: i64,
) -> Element<'a, Message> {
    let spacer = container(text("")).width(Length::Fill);

    let mut right = row![].spacing(space::MD).align_y(Alignment::Center);
    if alert_count > 0 {
        right = right.push(
            button(
                row![
                    icons::alert(IconSize::Small),
                    text(format!("{alert_count}")).size(font::CAPTION)
                ]
                .spacing(space::XS)
                .align_y(Alignment::Center),
            )
            .on_press(Message::OpenAlerts)
            .padding([space::XS, space::SM])
            .style(iced::widget::button::danger),
        );
    }
    // Global data-freshness verdict (Live / Stale / Paused + "as of HH:MM:SS").
    let connected = matches!(connection, ConnectionState::Connected);
    right = right.push(crate::view::freshness::freshness_indicator(
        connected,
        last_update_ms,
        now_ms,
    ));
    right = right.push(connection_status(connection));

    container(
        row![breadcrumb(current, device), spacer, right]
            .align_y(Alignment::Center)
            .spacing(space::MD),
    )
    .width(Length::Fill)
    .padding([space::SM, space::MD])
    .style(|theme: &Theme| container::Style {
        background: Some(Background::Color(theme::colors(theme).section_background())),
        border: Border {
            color: theme::colors(theme).border_subtle(),
            width: 1.0,
            radius: 0.0.into(),
        },
        ..Default::default()
    })
    .into()
}

/// Wrap a page `content` in the persistent shell (nav rail + top bar).
#[allow(clippy::too_many_arguments)]
pub fn app_shell<'a>(
    current: CurrentView,
    device: Option<&'a str>,
    connection: ConnectionState,
    alert_count: usize,
    last_update_ms: Option<i64>,
    now_ms: i64,
    content: Element<'a, Message>,
) -> Element<'a, Message> {
    row![
        nav_rail(current),
        column![
            top_bar(
                current,
                device,
                connection,
                alert_count,
                last_update_ms,
                now_ms
            ),
            container(content).width(Length::Fill).height(Length::Fill),
        ]
        .width(Length::Fill)
    ]
    .into()
}

fn dim(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(theme::colors(theme).text_dimmed()),
    }
}
