//! Device grouping and tagging functionality.

use std::collections::{HashMap, HashSet};

use iced::widget::{Column, Row, column, container, row, scrollable, text, text_input};
use iced::{Alignment, Element, Length, Theme};
use iced_anim::widget::button;

use serde::{Deserialize, Serialize};

use crate::message::{DeviceId, Message};
use crate::view::icons::{self, IconSize};

/// Predefined colors for groups (RGB 0.0-1.0).
pub const GROUP_COLORS: &[(f32, f32, f32, &str)] = &[
    (0.4, 0.6, 1.0, "Blue"),
    (0.4, 0.8, 0.4, "Green"),
    (1.0, 0.6, 0.2, "Orange"),
    (0.9, 0.4, 0.6, "Pink"),
    (0.7, 0.5, 0.9, "Purple"),
    (0.9, 0.8, 0.2, "Yellow"),
    (0.3, 0.8, 0.8, "Cyan"),
    (0.8, 0.5, 0.5, "Coral"),
];

/// A device group for organizing and filtering devices.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceGroup {
    /// Unique group identifier.
    pub id: u32,
    /// Display name.
    pub name: String,
    /// Color index into GROUP_COLORS.
    pub color_index: usize,
    /// Optional description.
    #[serde(default)]
    pub description: String,
}

impl DeviceGroup {
    /// Create a new group.
    pub fn new(id: u32, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            color_index: (id as usize) % GROUP_COLORS.len(),
            description: String::new(),
        }
    }

    /// Create a new group with a specific color.
    pub fn with_color(id: u32, name: impl Into<String>, color_index: usize) -> Self {
        Self {
            id,
            name: name.into(),
            color_index: color_index % GROUP_COLORS.len(),
            description: String::new(),
        }
    }

    /// Get the RGB color for this group.
    pub fn color(&self) -> (f32, f32, f32) {
        let (r, g, b, _) = GROUP_COLORS[self.color_index % GROUP_COLORS.len()];
        (r, g, b)
    }

    /// Get the color name.
    pub fn color_name(&self) -> &'static str {
        GROUP_COLORS[self.color_index % GROUP_COLORS.len()].3
    }
}

/// State for managing device groups.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct GroupsState {
    /// All defined groups.
    pub groups: HashMap<u32, DeviceGroup>,
    /// Device to group assignments (device ID string -> group IDs).
    pub assignments: HashMap<String, HashSet<u32>>,
    /// Next group ID to assign.
    #[serde(default)]
    next_id: u32,
    /// Currently selected group filter (None = show all).
    #[serde(skip)]
    pub filter: Option<u32>,
    /// New group form: name input.
    #[serde(skip)]
    pub new_group_name: String,
    /// New group form: color index.
    #[serde(skip)]
    pub new_group_color: usize,
    /// Whether the group management panel is open.
    #[serde(skip)]
    pub panel_open: bool,
    /// Group being edited (if any).
    #[serde(skip)]
    pub editing_group: Option<u32>,
    /// Edit form: name.
    #[serde(skip)]
    pub edit_name: String,
    /// Edit form: color index.
    #[serde(skip)]
    pub edit_color: usize,
}

impl GroupsState {
    /// Create a new groups state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new group and return its ID.
    pub fn create_group(&mut self, name: impl Into<String>) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        let group = DeviceGroup::new(id, name);
        self.groups.insert(id, group);
        id
    }

    /// Create a new group with a specific color.
    pub fn create_group_with_color(&mut self, name: impl Into<String>, color_index: usize) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        let group = DeviceGroup::with_color(id, name, color_index);
        self.groups.insert(id, group);
        id
    }

    /// Delete a group.
    pub fn delete_group(&mut self, group_id: u32) {
        self.groups.remove(&group_id);
        // Remove all assignments to this group
        for assignments in self.assignments.values_mut() {
            assignments.remove(&group_id);
        }
        // Clear filter if it was set to this group
        if self.filter == Some(group_id) {
            self.filter = None;
        }
    }

    /// Rename a group.
    pub fn rename_group(&mut self, group_id: u32, new_name: impl Into<String>) {
        if let Some(group) = self.groups.get_mut(&group_id) {
            group.name = new_name.into();
        }
    }

    /// Change a group's color.
    pub fn set_group_color(&mut self, group_id: u32, color_index: usize) {
        if let Some(group) = self.groups.get_mut(&group_id) {
            group.color_index = color_index % GROUP_COLORS.len();
        }
    }

    /// Assign a device to a group.
    pub fn assign_device(&mut self, device_id: &DeviceId, group_id: u32) {
        let key = device_id.to_string();
        self.assignments.entry(key).or_default().insert(group_id);
    }

    /// Remove a device from a group.
    pub fn unassign_device(&mut self, device_id: &DeviceId, group_id: u32) {
        let key = device_id.to_string();
        if let Some(assignments) = self.assignments.get_mut(&key) {
            assignments.remove(&group_id);
            if assignments.is_empty() {
                self.assignments.remove(&key);
            }
        }
    }

    /// Toggle device assignment to a group.
    pub fn toggle_assignment(&mut self, device_id: &DeviceId, group_id: u32) {
        let key = device_id.to_string();
        let assignments = self.assignments.entry(key).or_default();
        if assignments.contains(&group_id) {
            assignments.remove(&group_id);
        } else {
            assignments.insert(group_id);
        }
    }

    /// Get groups for a device.
    pub fn device_groups(&self, device_id: &DeviceId) -> Vec<&DeviceGroup> {
        let key = device_id.to_string();
        if let Some(group_ids) = self.assignments.get(&key) {
            group_ids
                .iter()
                .filter_map(|id| self.groups.get(id))
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Check if a device is in a specific group.
    pub fn device_in_group(&self, device_id: &DeviceId, group_id: u32) -> bool {
        let key = device_id.to_string();
        self.assignments
            .get(&key)
            .map(|ids| ids.contains(&group_id))
            .unwrap_or(false)
    }

    /// Check if a device passes the current group filter.
    pub fn device_passes_filter(&self, device_id: &DeviceId) -> bool {
        match self.filter {
            None => true,
            Some(group_id) => self.device_in_group(device_id, group_id),
        }
    }

    /// Set the group filter.
    pub fn set_filter(&mut self, group_id: Option<u32>) {
        self.filter = group_id;
    }

    /// Toggle the group filter.
    pub fn toggle_filter(&mut self, group_id: u32) {
        if self.filter == Some(group_id) {
            self.filter = None;
        } else {
            self.filter = Some(group_id);
        }
    }

    /// Get all groups sorted by name.
    pub fn sorted_groups(&self) -> Vec<&DeviceGroup> {
        let mut groups: Vec<_> = self.groups.values().collect();
        groups.sort_by(|a, b| a.name.cmp(&b.name));
        groups
    }

    /// Get the number of devices in each group.
    pub fn group_device_counts(&self) -> HashMap<u32, usize> {
        let mut counts = HashMap::new();
        for group_ids in self.assignments.values() {
            for &group_id in group_ids {
                *counts.entry(group_id).or_insert(0) += 1;
            }
        }
        counts
    }

    /// Open the group management panel.
    pub fn open_panel(&mut self) {
        self.panel_open = true;
        self.editing_group = None;
        self.new_group_name.clear();
        self.new_group_color = 0;
    }

    /// Close the group management panel.
    pub fn close_panel(&mut self) {
        self.panel_open = false;
        self.editing_group = None;
    }

    /// Start editing a group.
    pub fn start_editing(&mut self, group_id: u32) {
        if let Some(group) = self.groups.get(&group_id) {
            self.editing_group = Some(group_id);
            self.edit_name = group.name.clone();
            self.edit_color = group.color_index;
        }
    }

    /// Save edits to the current group.
    pub fn save_edit(&mut self) {
        if let Some(group_id) = self.editing_group {
            let new_name = self.edit_name.trim().to_string();
            let new_color = self.edit_color;
            if !new_name.is_empty() {
                self.rename_group(group_id, new_name);
                self.set_group_color(group_id, new_color);
            }
            self.editing_group = None;
        }
    }

    /// Cancel editing.
    pub fn cancel_edit(&mut self) {
        self.editing_group = None;
    }

    /// Add a new group from the form.
    pub fn add_group_from_form(&mut self) {
        let name = self.new_group_name.trim().to_string();
        let color = self.new_group_color;
        if !name.is_empty() {
            self.create_group_with_color(name, color);
            self.new_group_name.clear();
            self.new_group_color = 0;
        }
    }
}

/// Render a color indicator box.
pub fn color_indicator(color: (f32, f32, f32), size: f32) -> Element<'static, Message> {
    container(text(""))
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .style(move |theme: &Theme| {
            let colors = crate::view::theme::colors(theme);
            container::Style {
                background: Some(iced::Background::Color(iced::Color::from_rgb(
                    color.0, color.1, color.2,
                ))),
                border: iced::Border {
                    color: colors.border(),
                    width: 1.0,
                    radius: 3.0.into(),
                },
                ..Default::default()
            }
        })
        .into()
}

/// A lightweight group tag for rendering (owned data).
#[derive(Debug, Clone)]
pub struct GroupTag {
    pub name: String,
    pub color: (f32, f32, f32),
}

impl GroupTag {
    /// Create from a DeviceGroup reference.
    pub fn from_group(group: &DeviceGroup) -> Self {
        Self {
            name: group.name.clone(),
            color: group.color(),
        }
    }
}

/// Render group tags for a device (small colored badges).
/// Takes owned GroupTag data to avoid lifetime issues.
pub fn device_group_tags(groups: Vec<GroupTag>) -> Element<'static, Message> {
    if groups.is_empty() {
        return text("").into();
    }

    let count = groups.len();
    let mut tag_row = Row::new().spacing(4);

    for group in groups.into_iter().take(3) {
        let color = group.color;
        let tag =
            container(text(group.name).size(9))
                .padding([2, 6])
                .style(move |_theme: &Theme| container::Style {
                    background: Some(iced::Background::Color(iced::Color::from_rgba(
                        color.0, color.1, color.2, 0.3,
                    ))),
                    border: iced::Border {
                        color: iced::Color::from_rgb(color.0, color.1, color.2),
                        width: 1.0,
                        radius: 3.0.into(),
                    },
                    text_color: Some(iced::Color::from_rgb(color.0, color.1, color.2)),
                    ..Default::default()
                });
        tag_row = tag_row.push(tag);
    }

    if count > 3 {
        tag_row = tag_row.push(text(format!("+{}", count - 3)).size(9));
    }

    tag_row.into()
}

/// Render the group filter bar for the dashboard.
pub fn group_filter_bar(state: &GroupsState) -> Element<'_, Message> {
    let groups = state.sorted_groups();

    if groups.is_empty() {
        return row![].into();
    }

    let label = text("Groups:").size(12);
    let mut filter_row = row![label].spacing(8).align_y(Alignment::Center);

    // "All" button
    let all_btn = button(text("All").size(11))
        .on_press(Message::SetGroupFilter(None))
        .style(if state.filter.is_none() {
            iced::widget::button::primary
        } else {
            iced::widget::button::secondary
        });
    filter_row = filter_row.push(all_btn);

    // Group buttons
    for group in groups {
        let color = group.color();
        let is_active = state.filter == Some(group.id);

        let btn_content = row![color_indicator(color, 10.0), text(&group.name).size(11)]
            .spacing(4)
            .align_y(Alignment::Center);

        let btn = button(btn_content)
            .on_press(Message::SetGroupFilter(Some(group.id)))
            .style(if is_active {
                iced::widget::button::primary
            } else {
                iced::widget::button::secondary
            });

        filter_row = filter_row.push(btn);
    }

    // Manage groups button
    let manage_btn = button(
        row![icons::settings(IconSize::Small), text("Manage").size(11)]
            .spacing(4)
            .align_y(Alignment::Center),
    )
    .on_press(Message::OpenGroupsPanel)
    .style(iced::widget::button::secondary);

    filter_row = filter_row.push(manage_btn);

    filter_row.into()
}

/// Render the group management panel.
pub fn groups_panel(state: &GroupsState) -> Element<'_, Message> {
    let header = row![
        text("Manage Groups").size(18),
        button(icons::close(IconSize::Small))
            .on_press(Message::CloseGroupsPanel)
            .style(iced::widget::button::secondary)
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    // New group form
    let new_group_form = render_new_group_form(state);

    // List of existing groups
    let groups_list = render_groups_list(state);

    let content = column![header, new_group_form, groups_list]
        .spacing(15)
        .padding(20);

    container(scrollable(content))
        .width(Length::Fixed(400.0))
        .height(Length::Fill)
        .style(|theme: &Theme| {
            let colors = crate::view::theme::colors(theme);
            container::Style {
                background: Some(iced::Background::Color(colors.card_background())),
                border: iced::Border {
                    color: colors.border(),
                    width: 1.0,
                    radius: 8.0.into(),
                },
                ..Default::default()
            }
        })
        .into()
}

/// Render the new group form.
fn render_new_group_form(state: &GroupsState) -> Element<'_, Message> {
    let name_input = text_input("New group name...", &state.new_group_name)
        .on_input(Message::SetNewGroupName)
        .padding(8)
        .width(Length::Fixed(200.0));

    // Color picker
    let mut color_row = Row::new().spacing(4);
    for (i, &(r, g, b, _)) in GROUP_COLORS.iter().enumerate() {
        let is_selected = state.new_group_color == i;
        let color_btn = button(color_indicator((r, g, b), 16.0))
            .on_press(Message::SetNewGroupColor(i))
            .padding(2)
            .style(if is_selected {
                iced::widget::button::primary
            } else {
                iced::widget::button::secondary
            });
        color_row = color_row.push(color_btn);
    }

    let add_btn = button(text("Add Group").size(12))
        .on_press(Message::AddGroup)
        .style(iced::widget::button::primary);

    column![
        text("Create New Group").size(14),
        row![name_input, add_btn]
            .spacing(10)
            .align_y(Alignment::Center),
        row![text("Color:").size(11), color_row]
            .spacing(8)
            .align_y(Alignment::Center)
    ]
    .spacing(8)
    .into()
}

/// Render the list of existing groups.
fn render_groups_list(state: &GroupsState) -> Element<'_, Message> {
    let groups = state.sorted_groups();
    let counts = state.group_device_counts();

    if groups.is_empty() {
        return container(text("No groups defined yet").size(12))
            .padding(20)
            .into();
    }

    let mut list = Column::new().spacing(8);

    for group in groups {
        let device_count = counts.get(&group.id).copied().unwrap_or(0);
        list = list.push(render_group_row(state, group, device_count));
    }

    column![text("Existing Groups").size(14), list]
        .spacing(10)
        .into()
}

/// Render a single group row.
fn render_group_row<'a>(
    state: &'a GroupsState,
    group: &'a DeviceGroup,
    device_count: usize,
) -> Element<'a, Message> {
    if state.editing_group == Some(group.id) {
        // Edit mode
        let name_input = text_input("Group name...", &state.edit_name)
            .on_input(Message::SetEditGroupName)
            .padding(6)
            .width(Length::Fixed(150.0));

        let mut color_row = Row::new().spacing(2);
        for (i, &(r, g, b, _)) in GROUP_COLORS.iter().enumerate() {
            let is_selected = state.edit_color == i;
            let color_btn = button(color_indicator((r, g, b), 12.0))
                .on_press(Message::SetEditGroupColor(i))
                .padding(1)
                .style(if is_selected {
                    iced::widget::button::primary
                } else {
                    iced::widget::button::secondary
                });
            color_row = color_row.push(color_btn);
        }

        let save_btn = button(text("Save").size(11))
            .on_press(Message::SaveGroupEdit)
            .style(iced::widget::button::primary);

        let cancel_btn = button(text("Cancel").size(11))
            .on_press(Message::CancelGroupEdit)
            .style(iced::widget::button::secondary);

        container(
            column![
                row![name_input, save_btn, cancel_btn]
                    .spacing(6)
                    .align_y(Alignment::Center),
                color_row
            ]
            .spacing(6),
        )
        .padding(8)
        .style(|theme: &Theme| {
            let colors = crate::view::theme::colors(theme);
            container::Style {
                background: Some(iced::Background::Color(colors.table_header())),
                border: iced::Border {
                    color: colors.border(),
                    width: 1.0,
                    radius: 4.0.into(),
                },
                ..Default::default()
            }
        })
        .into()
    } else {
        // Display mode
        let color = group.color();

        let group_info = row![
            color_indicator(color, 14.0),
            text(&group.name).size(13),
            text(format!("({} devices)", device_count))
                .size(11)
                .style(|theme: &Theme| text::Style {
                    color: Some(crate::view::theme::colors(theme).text_dimmed()),
                })
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let edit_btn = button(icons::edit(IconSize::Small))
            .on_press(Message::EditGroup(group.id))
            .style(iced::widget::button::secondary);

        let delete_btn = button(icons::close(IconSize::Small))
            .on_press(Message::DeleteGroup(group.id))
            .style(iced::widget::button::danger);

        container(
            row![group_info, edit_btn, delete_btn]
                .spacing(8)
                .align_y(Alignment::Center),
        )
        .padding(8)
        .width(Length::Fill)
        .style(|theme: &Theme| {
            let colors = crate::view::theme::colors(theme);
            container::Style {
                background: Some(iced::Background::Color(colors.row_background())),
                border: iced::Border {
                    color: colors.border(),
                    width: 1.0,
                    radius: 4.0.into(),
                },
                ..Default::default()
            }
        })
        .into()
    }
}

/// Render a group assignment dropdown/menu for a device.
pub fn device_group_menu<'a>(
    device_id: &'a DeviceId,
    state: &'a GroupsState,
) -> Element<'a, Message> {
    let groups = state.sorted_groups();

    if groups.is_empty() {
        return text("No groups").size(11).into();
    }

    let device_id_owned = device_id.clone();
    let mut menu_col = Column::new().spacing(4);

    for group in groups {
        let is_assigned = state.device_in_group(&device_id_owned, group.id);
        let color = group.color();
        let group_id = group.id;
        let device_id_clone = device_id_owned.clone();
        let group_name = group.name.clone();

        let check_icon = if is_assigned {
            icons::check(IconSize::Small)
        } else {
            text("  ").size(12).into()
        };

        let item = button(
            row![
                check_icon,
                color_indicator(color, 10.0),
                text(group_name).size(11)
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        )
        .on_press(Message::ToggleDeviceGroup(device_id_clone, group_id))
        .width(Length::Fill)
        .style(if is_assigned {
            iced::widget::button::primary
        } else {
            iced::widget::button::secondary
        });

        menu_col = menu_col.push(item);
    }

    container(menu_col)
        .padding(8)
        .style(|theme: &Theme| {
            let colors = crate::view::theme::colors(theme);
            container::Style {
                background: Some(iced::Background::Color(colors.row_background())),
                border: iced::Border {
                    color: colors.border(),
                    width: 1.0,
                    radius: 4.0.into(),
                },
                ..Default::default()
            }
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zensight_common::Protocol;

    #[test]
    fn test_create_group() {
        let mut state = GroupsState::new();

        let id1 = state.create_group("Servers");
        let id2 = state.create_group("Network");

        assert_eq!(state.groups.len(), 2);
        assert_eq!(state.groups.get(&id1).unwrap().name, "Servers");
        assert_eq!(state.groups.get(&id2).unwrap().name, "Network");
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_delete_group() {
        let mut state = GroupsState::new();
        let id = state.create_group("Test");

        let device = DeviceId::new(Protocol::Snmp, "device1");
        state.assign_device(&device, id);

        assert!(state.device_in_group(&device, id));

        state.delete_group(id);

        assert!(!state.groups.contains_key(&id));
        assert!(!state.device_in_group(&device, id));
    }

    #[test]
    fn test_assign_device() {
        let mut state = GroupsState::new();
        let group_id = state.create_group("Servers");

        let device = DeviceId::new(Protocol::Snmp, "router1");

        assert!(!state.device_in_group(&device, group_id));

        state.assign_device(&device, group_id);
        assert!(state.device_in_group(&device, group_id));

        state.unassign_device(&device, group_id);
        assert!(!state.device_in_group(&device, group_id));
    }

    #[test]
    fn test_toggle_assignment() {
        let mut state = GroupsState::new();
        let group_id = state.create_group("Test");
        let device = DeviceId::new(Protocol::Syslog, "host1");

        assert!(!state.device_in_group(&device, group_id));

        state.toggle_assignment(&device, group_id);
        assert!(state.device_in_group(&device, group_id));

        state.toggle_assignment(&device, group_id);
        assert!(!state.device_in_group(&device, group_id));
    }

    #[test]
    fn test_device_groups() {
        let mut state = GroupsState::new();
        let g1 = state.create_group("Servers");
        let g2 = state.create_group("Production");

        let device = DeviceId::new(Protocol::Snmp, "web1");
        state.assign_device(&device, g1);
        state.assign_device(&device, g2);

        let groups = state.device_groups(&device);
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn test_filter() {
        let mut state = GroupsState::new();
        let g1 = state.create_group("Servers");
        let g2 = state.create_group("Network");

        let server = DeviceId::new(Protocol::Snmp, "server1");
        let router = DeviceId::new(Protocol::Snmp, "router1");

        state.assign_device(&server, g1);
        state.assign_device(&router, g2);

        // No filter - all pass
        assert!(state.device_passes_filter(&server));
        assert!(state.device_passes_filter(&router));

        // Filter by servers
        state.set_filter(Some(g1));
        assert!(state.device_passes_filter(&server));
        assert!(!state.device_passes_filter(&router));

        // Clear filter
        state.set_filter(None);
        assert!(state.device_passes_filter(&server));
        assert!(state.device_passes_filter(&router));
    }

    #[test]
    fn test_group_device_counts() {
        let mut state = GroupsState::new();
        let g1 = state.create_group("Group1");
        let g2 = state.create_group("Group2");

        for i in 0..5 {
            let device = DeviceId::new(Protocol::Snmp, format!("device{}", i));
            state.assign_device(&device, g1);
        }

        for i in 0..3 {
            let device = DeviceId::new(Protocol::Snmp, format!("other{}", i));
            state.assign_device(&device, g2);
        }

        let counts = state.group_device_counts();
        assert_eq!(counts.get(&g1), Some(&5));
        assert_eq!(counts.get(&g2), Some(&3));
    }

    #[test]
    fn test_rename_and_color() {
        let mut state = GroupsState::new();
        let id = state.create_group("Old Name");

        state.rename_group(id, "New Name");
        assert_eq!(state.groups.get(&id).unwrap().name, "New Name");

        state.set_group_color(id, 3);
        assert_eq!(state.groups.get(&id).unwrap().color_index, 3);
    }
}
