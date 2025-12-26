//! Canvas-based topology graph widget.

use iced::mouse;
use iced::widget::canvas::{self, Canvas, Frame, Geometry, Path, Stroke, Text};
use iced::{Color, Element, Length, Point, Rectangle, Renderer, Theme};

use super::{NodeType, TopologyState};
use crate::message::Message;

/// Interactive topology graph widget.
pub struct TopologyGraph;

impl TopologyGraph {
    /// Create a new topology graph.
    pub fn new(state: &TopologyState) -> Element<'_, Message> {
        Canvas::new(TopologyGraphProgram { state })
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}

/// Canvas program for the topology graph.
struct TopologyGraphProgram<'a> {
    state: &'a TopologyState,
}

/// Interaction state for the graph.
#[derive(Debug, Clone, Default)]
pub struct GraphInteraction {
    /// Whether we're currently dragging a node.
    dragging_node: Option<String>,
    /// Whether we're panning the canvas.
    panning: bool,
    /// Last mouse position for drag calculations.
    last_pos: Option<Point>,
}

impl<'a> canvas::Program<Message> for TopologyGraphProgram<'a> {
    type State = GraphInteraction;

    fn update(
        &self,
        interaction: &mut Self::State,
        event: &canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        match event {
            canvas::Event::Mouse(mouse_event) => {
                self.handle_mouse(interaction, mouse_event, bounds, cursor)
            }
            canvas::Event::Keyboard(keyboard_event) => self.handle_keyboard(keyboard_event),
            _ => None,
        }
    }

    fn draw(
        &self,
        _interaction: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let geometry = self.state.cache.draw(renderer, bounds.size(), |frame| {
            self.draw_graph(frame, bounds);
        });

        vec![geometry]
    }

    fn mouse_interaction(
        &self,
        interaction: &Self::State,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if interaction.dragging_node.is_some() || interaction.panning {
            mouse::Interaction::Grabbing
        } else if cursor.is_over(bounds) {
            // Check if hovering over a node
            if let Some(pos) = cursor.position() {
                let graph_pos = self.screen_to_graph(pos, bounds);
                if self.find_node_at(graph_pos).is_some() {
                    return mouse::Interaction::Pointer;
                }
            }
            mouse::Interaction::Grab
        } else {
            mouse::Interaction::default()
        }
    }
}

impl<'a> TopologyGraphProgram<'a> {
    /// Handle mouse events.
    fn handle_mouse(
        &self,
        interaction: &mut GraphInteraction,
        event: &mouse::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        match event {
            mouse::Event::ButtonPressed(mouse::Button::Left) => {
                if let Some(pos) = cursor.position() {
                    if !cursor.is_over(bounds) {
                        return None;
                    }

                    let graph_pos = self.screen_to_graph(pos, bounds);

                    // Check if clicking on a node
                    if let Some(node_id) = self.find_node_at(graph_pos) {
                        interaction.dragging_node = Some(node_id.clone());
                        interaction.last_pos = Some(pos);
                        return Some(canvas::Action::publish(Message::TopologySelectNode(
                            node_id,
                        )));
                    }

                    // Otherwise, start panning
                    interaction.panning = true;
                    interaction.last_pos = Some(pos);
                    return Some(canvas::Action::publish(Message::TopologyClearSelection));
                }
            }
            mouse::Event::CursorMoved { position } => {
                if let Some(last) = interaction.last_pos {
                    let dx = position.x - last.x;
                    let dy = position.y - last.y;

                    if let Some(ref node_id) = interaction.dragging_node {
                        // Update node position
                        let graph_pos = self.screen_to_graph(*position, bounds);
                        return Some(canvas::Action::publish(Message::TopologyDragNodeUpdate(
                            node_id.clone(),
                            graph_pos.x,
                            graph_pos.y,
                        )));
                    } else if interaction.panning {
                        interaction.last_pos = Some(*position);
                        return Some(canvas::Action::publish(Message::TopologyPanUpdate(
                            dx / self.state.zoom,
                            dy / self.state.zoom,
                        )));
                    }
                }
                interaction.last_pos = Some(*position);
            }
            mouse::Event::ButtonReleased(mouse::Button::Left) => {
                if let Some(ref node_id) = interaction.dragging_node.take() {
                    return Some(canvas::Action::publish(Message::TopologyDragNodeEnd(
                        node_id.clone(),
                    )));
                }
                interaction.panning = false;
            }
            mouse::Event::WheelScrolled { delta } => {
                if cursor.is_over(bounds) {
                    let scroll = match delta {
                        mouse::ScrollDelta::Lines { y, .. } => *y,
                        mouse::ScrollDelta::Pixels { y, .. } => *y / 50.0,
                    };

                    if scroll > 0.0 {
                        return Some(canvas::Action::publish(Message::TopologyZoomIn));
                    } else if scroll < 0.0 {
                        return Some(canvas::Action::publish(Message::TopologyZoomOut));
                    }
                }
            }
            _ => {}
        }

        None
    }

    /// Handle keyboard events.
    fn handle_keyboard(&self, event: &iced::keyboard::Event) -> Option<canvas::Action<Message>> {
        use iced::keyboard::{Event, Key, key::Named};

        if let Event::KeyPressed { key, .. } = event {
            match key {
                Key::Character(c) if c.as_str() == "+" || c.as_str() == "=" => {
                    return Some(canvas::Action::publish(Message::TopologyZoomIn));
                }
                Key::Character(c) if c.as_str() == "-" => {
                    return Some(canvas::Action::publish(Message::TopologyZoomOut));
                }
                Key::Named(Named::Escape) => {
                    return Some(canvas::Action::publish(Message::TopologyClearSelection));
                }
                Key::Character(c) if c.as_str() == "0" => {
                    return Some(canvas::Action::publish(Message::TopologyZoomReset));
                }
                _ => {}
            }
        }

        None
    }

    /// Draw the graph.
    fn draw_graph(&self, frame: &mut Frame, bounds: Rectangle) {
        let center = Point::new(bounds.width / 2.0, bounds.height / 2.0);

        // Draw background
        frame.fill(
            &Path::rectangle(Point::ORIGIN, bounds.size()),
            Color::from_rgb(0.08, 0.08, 0.1),
        );

        // Draw edges first (behind nodes)
        for edge in &self.state.edges {
            self.draw_edge(frame, edge, center);
        }

        // Draw nodes
        for node in self.state.nodes.values() {
            self.draw_node(frame, node, center);
        }

        // Draw "empty state" message if no nodes
        if self.state.nodes.is_empty() {
            let text = Text {
                content: "No hosts detected. Waiting for sysinfo telemetry...".to_string(),
                position: center,
                color: Color::from_rgb(0.5, 0.5, 0.5),
                size: 16.0.into(),
                align_x: iced::alignment::Horizontal::Center.into(),
                align_y: iced::alignment::Vertical::Center.into(),
                ..Text::default()
            };
            frame.fill_text(text);
        }

        // Draw zoom indicator
        let zoom_text = Text {
            content: format!("Zoom: {}%", (self.state.zoom * 100.0) as i32),
            position: Point::new(10.0, bounds.height - 20.0),
            color: Color::from_rgb(0.4, 0.4, 0.4),
            size: 12.0.into(),
            ..Text::default()
        };
        frame.fill_text(zoom_text);
    }

    /// Draw a single node.
    fn draw_node(&self, frame: &mut Frame, node: &super::Node, center: Point) {
        let pos = self.apply_transform(node.position, center);
        // Node radius scales with zoom but has a minimum size
        let radius = (25.0 * self.state.zoom).max(15.0);

        // Node color based on type and health
        let base_color = match node.node_type {
            NodeType::Host => {
                if node.is_healthy {
                    Color::from_rgb(0.3, 0.6, 0.9)
                } else {
                    Color::from_rgb(0.9, 0.3, 0.3)
                }
            }
            NodeType::Router => Color::from_rgb(0.4, 0.8, 0.4),
            NodeType::Switch => Color::from_rgb(0.8, 0.6, 0.3),
            NodeType::Unknown => Color::from_rgb(0.5, 0.5, 0.5),
        };

        // Highlight if selected
        let is_selected = self.state.selected_node.as_ref() == Some(&node.id);
        let is_highlighted = !self.state.search_query.is_empty()
            && node
                .label
                .to_lowercase()
                .contains(&self.state.search_query.to_lowercase());

        // Draw selection ring
        if is_selected {
            let ring = Path::circle(pos, radius + 5.0);
            frame.stroke(
                &ring,
                Stroke::default()
                    .with_color(Color::from_rgb(1.0, 0.8, 0.2))
                    .with_width(3.0),
            );
        } else if is_highlighted {
            let ring = Path::circle(pos, radius + 4.0);
            frame.stroke(
                &ring,
                Stroke::default()
                    .with_color(Color::from_rgb(0.9, 0.5, 0.9))
                    .with_width(2.0),
            );
        }

        // Draw node circle
        let circle = Path::circle(pos, radius);
        frame.fill(&circle, base_color);

        // Draw pinned indicator
        if node.pinned {
            let pin = Path::circle(Point::new(pos.x + radius * 0.7, pos.y - radius * 0.7), 5.0);
            frame.fill(&pin, Color::from_rgb(1.0, 0.5, 0.2));
        }

        // Draw label
        let label = Text {
            content: node.label.clone(),
            position: Point::new(pos.x, pos.y + radius + 14.0),
            color: Color::WHITE,
            size: (14.0 * self.state.zoom).max(11.0).into(),
            align_x: iced::alignment::Horizontal::Center.into(),
            ..Text::default()
        };
        frame.fill_text(label);

        // Draw CPU/Memory mini-stats if available (only when zoomed in enough)
        if self.state.zoom >= 0.6 {
            if let Some(cpu) = node.cpu_usage {
                let cpu_text = Text {
                    content: format!("CPU: {:.0}%", cpu),
                    position: Point::new(pos.x, pos.y - 5.0),
                    color: Color::WHITE,
                    size: (10.0 * self.state.zoom).max(9.0).into(),
                    align_x: iced::alignment::Horizontal::Center.into(),
                    ..Text::default()
                };
                frame.fill_text(cpu_text);
            }

            if let Some(mem) = node.memory_usage {
                let mem_text = Text {
                    content: format!("Mem: {:.0}%", mem),
                    position: Point::new(pos.x, pos.y + 5.0),
                    color: Color::WHITE,
                    size: (10.0 * self.state.zoom).max(9.0).into(),
                    align_x: iced::alignment::Horizontal::Center.into(),
                    ..Text::default()
                };
                frame.fill_text(mem_text);
            }
        }
    }

    /// Draw an edge between two nodes.
    fn draw_edge(&self, frame: &mut Frame, edge: &super::Edge, center: Point) {
        let from_node = match self.state.nodes.get(&edge.from) {
            Some(n) => n,
            None => return,
        };
        let to_node = match self.state.nodes.get(&edge.to) {
            Some(n) => n,
            None => return,
        };

        let from_pos = self.apply_transform(from_node.position, center);
        let to_pos = self.apply_transform(to_node.position, center);

        // Edge width based on bandwidth
        let base_width = 2.0;
        let max_width = 10.0;
        let bandwidth_factor = (edge.bytes as f32 / 1_000_000.0).clamp(0.0, 1.0);
        let width = base_width + bandwidth_factor * (max_width - base_width);

        // Edge color
        let is_selected = self.state.selected_edge
            == Some(
                self.state
                    .edges
                    .iter()
                    .position(|e| e.from == edge.from && e.to == edge.to)
                    .unwrap_or(usize::MAX),
            );

        let color = if is_selected {
            Color::from_rgb(1.0, 0.8, 0.2)
        } else {
            Color::from_rgb(0.4, 0.5, 0.6)
        };

        // Draw edge line
        let mut path = canvas::path::Builder::new();
        path.move_to(from_pos);
        path.line_to(to_pos);
        let edge_path = path.build();

        frame.stroke(
            &edge_path,
            Stroke::default()
                .with_color(color)
                .with_width(width * self.state.zoom),
        );

        // Draw bandwidth label at midpoint
        if edge.bytes > 0 {
            let mid = Point::new((from_pos.x + to_pos.x) / 2.0, (from_pos.y + to_pos.y) / 2.0);
            let label = Text {
                content: format_bytes(edge.bytes),
                position: Point::new(mid.x, mid.y - 8.0),
                color: Color::from_rgb(0.7, 0.7, 0.7),
                size: (10.0 * self.state.zoom).max(8.0).into(),
                align_x: iced::alignment::Horizontal::Center.into(),
                ..Text::default()
            };
            frame.fill_text(label);
        }
    }

    /// Apply zoom and pan transform to a graph position.
    fn apply_transform(&self, pos: (f32, f32), center: Point) -> Point {
        Point::new(
            center.x + (pos.0 + self.state.pan.0) * self.state.zoom,
            center.y + (pos.1 + self.state.pan.1) * self.state.zoom,
        )
    }

    /// Convert screen coordinates to graph coordinates.
    fn screen_to_graph(&self, screen_pos: Point, bounds: Rectangle) -> Point {
        let center = Point::new(bounds.width / 2.0, bounds.height / 2.0);
        Point::new(
            (screen_pos.x - center.x) / self.state.zoom - self.state.pan.0,
            (screen_pos.y - center.y) / self.state.zoom - self.state.pan.1,
        )
    }

    /// Find the node at the given graph position.
    fn find_node_at(&self, pos: Point) -> Option<String> {
        let hit_radius = 25.0; // Same as node radius

        for node in self.state.nodes.values() {
            let dx = pos.x - node.position.0;
            let dy = pos.y - node.position.1;
            let distance = (dx * dx + dy * dy).sqrt();

            if distance <= hit_radius {
                return Some(node.id.clone());
            }
        }

        None
    }
}

/// Format bytes as human-readable string.
pub fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.1} GB", bytes as f64 / 1_000_000_000.0)
    } else if bytes >= 1_000_000 {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    } else if bytes >= 1_000 {
        format!("{:.1} KB", bytes as f64 / 1_000.0)
    } else {
        format!("{} B", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1500), "1.5 KB");
        assert_eq!(format_bytes(1_500_000), "1.5 MB");
        assert_eq!(format_bytes(1_500_000_000), "1.5 GB");
    }
}
