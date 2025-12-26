//! Canvas-based topology graph widget.

use iced::mouse;
use iced::widget::canvas::{self, Canvas, Frame, Geometry, Path, Stroke, Text};
use iced::{Color, Element, Length, Point, Rectangle, Renderer, Theme};

use super::{NodeType, TopologyState};
use crate::message::Message;

/// Interactive topology graph widget.
pub struct TopologyGraph;

impl TopologyGraph {
    /// Create a topology graph element.
    pub fn view(state: &TopologyState) -> Element<'_, Message> {
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
                align_y: iced::alignment::Vertical::Center,
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
        screen_to_graph_coords(screen_pos, bounds, self.state.zoom, self.state.pan)
    }

    /// Find the node at the given graph position.
    fn find_node_at(&self, pos: Point) -> Option<String> {
        const HIT_RADIUS: f32 = 25.0; // Same as node radius
        find_node_at_position(pos, &self.state.nodes, HIT_RADIUS)
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

/// Convert screen coordinates to graph coordinates.
///
/// This is the core coordinate transformation used for hit testing.
/// - `screen_pos`: cursor position in window coordinates
/// - `bounds`: the canvas bounds (position and size)
/// - `zoom`: current zoom level
/// - `pan`: current pan offset (x, y)
pub fn screen_to_graph_coords(
    screen_pos: Point,
    bounds: Rectangle,
    zoom: f32,
    pan: (f32, f32),
) -> Point {
    // Convert from window coordinates to canvas-relative coordinates
    let canvas_x = screen_pos.x - bounds.x;
    let canvas_y = screen_pos.y - bounds.y;
    let center_x = bounds.width / 2.0;
    let center_y = bounds.height / 2.0;
    Point::new(
        (canvas_x - center_x) / zoom - pan.0,
        (canvas_y - center_y) / zoom - pan.1,
    )
}

/// Find a node at the given graph position.
///
/// Returns the node ID if a node is found within the hit radius.
pub fn find_node_at_position(
    pos: Point,
    nodes: &std::collections::HashMap<super::NodeId, super::Node>,
    hit_radius: f32,
) -> Option<String> {
    for node in nodes.values() {
        let dx = pos.x - node.position.0;
        let dy = pos.y - node.position.1;
        let distance = (dx * dx + dy * dy).sqrt();

        if distance <= hit_radius {
            return Some(node.id.clone());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::topology::{Node, NodeType};
    use std::collections::HashMap;

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1500), "1.5 KB");
        assert_eq!(format_bytes(1_500_000), "1.5 MB");
        assert_eq!(format_bytes(1_500_000_000), "1.5 GB");
    }

    // ========================================================================
    // Coordinate conversion tests
    // ========================================================================

    #[test]
    fn test_screen_to_graph_center_click() {
        // Canvas at origin (0,0), size 800x600
        let bounds = Rectangle::new(Point::new(0.0, 0.0), iced::Size::new(800.0, 600.0));
        // Click at center of canvas
        let screen_pos = Point::new(400.0, 300.0);
        let zoom = 1.0;
        let pan = (0.0, 0.0);

        let graph_pos = screen_to_graph_coords(screen_pos, bounds, zoom, pan);

        // Center of canvas should map to origin in graph coords
        assert!(
            (graph_pos.x).abs() < 0.001,
            "Expected x=0, got {}",
            graph_pos.x
        );
        assert!(
            (graph_pos.y).abs() < 0.001,
            "Expected y=0, got {}",
            graph_pos.y
        );
    }

    #[test]
    fn test_screen_to_graph_with_canvas_offset() {
        // Canvas NOT at origin - offset by (100, 50) to simulate header/sidebar
        let bounds = Rectangle::new(Point::new(100.0, 50.0), iced::Size::new(800.0, 600.0));
        // Click at center of canvas (in window coords: 100 + 400 = 500, 50 + 300 = 350)
        let screen_pos = Point::new(500.0, 350.0);
        let zoom = 1.0;
        let pan = (0.0, 0.0);

        let graph_pos = screen_to_graph_coords(screen_pos, bounds, zoom, pan);

        // Should still map to origin since we clicked at canvas center
        assert!(
            (graph_pos.x).abs() < 0.001,
            "Expected x=0, got {}",
            graph_pos.x
        );
        assert!(
            (graph_pos.y).abs() < 0.001,
            "Expected y=0, got {}",
            graph_pos.y
        );
    }

    #[test]
    fn test_screen_to_graph_with_zoom() {
        let bounds = Rectangle::new(Point::new(0.0, 0.0), iced::Size::new(800.0, 600.0));
        // Click 100 pixels right of center
        let screen_pos = Point::new(500.0, 300.0);
        let zoom = 2.0; // 200% zoom
        let pan = (0.0, 0.0);

        let graph_pos = screen_to_graph_coords(screen_pos, bounds, zoom, pan);

        // At 200% zoom, 100 screen pixels = 50 graph units
        assert!(
            (graph_pos.x - 50.0).abs() < 0.001,
            "Expected x=50, got {}",
            graph_pos.x
        );
        assert!(
            (graph_pos.y).abs() < 0.001,
            "Expected y=0, got {}",
            graph_pos.y
        );
    }

    #[test]
    fn test_screen_to_graph_with_pan() {
        let bounds = Rectangle::new(Point::new(0.0, 0.0), iced::Size::new(800.0, 600.0));
        // Click at center
        let screen_pos = Point::new(400.0, 300.0);
        let zoom = 1.0;
        let pan = (100.0, 50.0); // Panned right 100, down 50

        let graph_pos = screen_to_graph_coords(screen_pos, bounds, zoom, pan);

        // Pan shifts the view, so center click maps to (-pan.x, -pan.y)
        assert!(
            (graph_pos.x - (-100.0)).abs() < 0.001,
            "Expected x=-100, got {}",
            graph_pos.x
        );
        assert!(
            (graph_pos.y - (-50.0)).abs() < 0.001,
            "Expected y=-50, got {}",
            graph_pos.y
        );
    }

    #[test]
    fn test_screen_to_graph_combined_offset_zoom_pan() {
        // Comprehensive test with all transformations
        let bounds = Rectangle::new(Point::new(50.0, 30.0), iced::Size::new(800.0, 600.0));
        // Click at canvas center (window coords: 50 + 400 = 450, 30 + 300 = 330)
        let screen_pos = Point::new(450.0, 330.0);
        let zoom = 0.5; // 50% zoom
        let pan = (20.0, 10.0);

        let graph_pos = screen_to_graph_coords(screen_pos, bounds, zoom, pan);

        // At center with pan, should get (-pan.x, -pan.y)
        assert!(
            (graph_pos.x - (-20.0)).abs() < 0.001,
            "Expected x=-20, got {}",
            graph_pos.x
        );
        assert!(
            (graph_pos.y - (-10.0)).abs() < 0.001,
            "Expected y=-10, got {}",
            graph_pos.y
        );
    }

    // ========================================================================
    // Node hit testing
    // ========================================================================

    fn create_test_node(id: &str, x: f32, y: f32) -> Node {
        Node {
            id: id.to_string(),
            label: id.to_string(),
            position: (x, y),
            velocity: (0.0, 0.0),
            node_type: NodeType::Host,
            cpu_usage: None,
            memory_usage: None,
            network_rx: None,
            network_tx: None,
            is_healthy: true,
            pinned: false,
        }
    }

    #[test]
    fn test_find_node_at_exact_position() {
        let mut nodes = HashMap::new();
        nodes.insert("node1".to_string(), create_test_node("node1", 100.0, 100.0));

        // Click exactly on node
        let pos = Point::new(100.0, 100.0);
        let result = find_node_at_position(pos, &nodes, 25.0);

        assert_eq!(result, Some("node1".to_string()));
    }

    #[test]
    fn test_find_node_at_edge_of_radius() {
        let mut nodes = HashMap::new();
        nodes.insert("node1".to_string(), create_test_node("node1", 100.0, 100.0));

        // Click just inside the hit radius (24 pixels away)
        let pos = Point::new(124.0, 100.0);
        let result = find_node_at_position(pos, &nodes, 25.0);

        assert_eq!(result, Some("node1".to_string()));
    }

    #[test]
    fn test_find_node_miss_outside_radius() {
        let mut nodes = HashMap::new();
        nodes.insert("node1".to_string(), create_test_node("node1", 100.0, 100.0));

        // Click just outside the hit radius (26 pixels away)
        let pos = Point::new(126.0, 100.0);
        let result = find_node_at_position(pos, &nodes, 25.0);

        assert_eq!(result, None);
    }

    #[test]
    fn test_find_node_multiple_nodes() {
        let mut nodes = HashMap::new();
        nodes.insert("node1".to_string(), create_test_node("node1", 0.0, 0.0));
        nodes.insert("node2".to_string(), create_test_node("node2", 200.0, 0.0));
        nodes.insert("node3".to_string(), create_test_node("node3", 100.0, 200.0));

        // Click on node2
        let pos = Point::new(200.0, 0.0);
        let result = find_node_at_position(pos, &nodes, 25.0);

        assert_eq!(result, Some("node2".to_string()));
    }

    #[test]
    fn test_find_node_empty_graph() {
        let nodes: HashMap<String, Node> = HashMap::new();

        let pos = Point::new(100.0, 100.0);
        let result = find_node_at_position(pos, &nodes, 25.0);

        assert_eq!(result, None);
    }

    // ========================================================================
    // Integration test: screen click to node detection
    // ========================================================================

    #[test]
    fn test_click_on_node_with_canvas_offset() {
        // Simulate a real scenario: canvas is offset due to header
        let bounds = Rectangle::new(Point::new(0.0, 80.0), iced::Size::new(1200.0, 720.0));

        // Node at graph origin (0, 0)
        let mut nodes = HashMap::new();
        nodes.insert(
            "server01".to_string(),
            create_test_node("server01", 0.0, 0.0),
        );

        // At zoom 1.0, no pan, the node at (0,0) is at canvas center
        // Canvas center in window coords: (600, 80 + 360) = (600, 440)
        let zoom = 1.0;
        let pan = (0.0, 0.0);

        let screen_pos = Point::new(600.0, 440.0);
        let graph_pos = screen_to_graph_coords(screen_pos, bounds, zoom, pan);
        let result = find_node_at_position(graph_pos, &nodes, 25.0);

        assert_eq!(
            result,
            Some("server01".to_string()),
            "Should find node at center. graph_pos = ({}, {})",
            graph_pos.x,
            graph_pos.y
        );
    }

    #[test]
    fn test_click_on_node_with_zoom_and_pan() {
        let bounds = Rectangle::new(Point::new(100.0, 100.0), iced::Size::new(800.0, 600.0));

        // Node at (150, 100) in graph coordinates
        let mut nodes = HashMap::new();
        nodes.insert(
            "router01".to_string(),
            create_test_node("router01", 150.0, 100.0),
        );

        let zoom = 0.8;
        let pan = (-50.0, -30.0);

        // Calculate where the node appears on screen
        // Node graph pos: (150, 100)
        // After pan: (150 + (-50), 100 + (-30)) = (100, 70) relative to center
        // After zoom: (100 * 0.8, 70 * 0.8) = (80, 56) pixels from canvas center
        // Canvas center in window: (100 + 400, 100 + 300) = (500, 400)
        // Node screen pos: (500 + 80, 400 + 56) = (580, 456)
        let screen_pos = Point::new(580.0, 456.0);

        let graph_pos = screen_to_graph_coords(screen_pos, bounds, zoom, pan);
        let result = find_node_at_position(graph_pos, &nodes, 25.0);

        assert_eq!(
            result,
            Some("router01".to_string()),
            "Should find node. graph_pos = ({}, {}), expected (150, 100)",
            graph_pos.x,
            graph_pos.y
        );
    }
}
