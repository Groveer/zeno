//! Graph panel component — shows active sub-agent spawn topology.
//!
//! Renders a right-side panel section listing open sub-agents (children
//! spawned via `delegate_task`) with their goals and status.
//! Implements the `Component` trait for integration into the component tree.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders},
};

use crate::store::agent_graph::EdgeRecord;
use crate::ui::wrap::word_wrap;
use crate::utils::padded_emoji;

use crate::gateway::UiCommand;

use super::component::Component;
use super::theme;

/// Graph panel component — displays the sub-agent spawn tree.
pub struct GraphPanel {
    /// Cached list of open child edge records for the current session.
    children: Vec<EdgeRecord>,
    /// Whether the panel needs re-rendering.
    dirty: bool,
    /// Vertical scroll offset.
    scroll_offset: usize,
}

impl GraphPanel {
    pub fn new() -> Self {
        Self {
            children: Vec::new(),
            dirty: true,
            scroll_offset: 0,
        }
    }

    /// Update the cached child list from a fresh query.
    pub fn set_children(&mut self, children: Vec<EdgeRecord>) {
        self.children = children;
        self.dirty = true;
    }

    /// Whether there are open children to display.
    pub fn has_children(&self) -> bool {
        !self.children.is_empty()
    }

    /// Scroll the panel content up.
    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
        self.dirty = true;
    }

    /// Scroll the panel content down.
    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_add(lines);
        self.dirty = true;
    }

    /// Reset scroll to top.
    pub fn reset_scroll(&mut self) {
        self.scroll_offset = 0;
        self.dirty = true;
    }

    /// Render the graph panel.
    fn render(&mut self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(theme::BORDER))
            .title(format!(
                "{} Sub-Agents ({})",
                padded_emoji("\u{f09e}"), // grid icon (nf-fa-th)
                self.children.len(),
            ))
            .title_style(Style::new().fg(theme::ACCENT).add_modifier(Modifier::BOLD));

        // Build lines from children
        let mut lines: Vec<Line> = Vec::new();

        // Measure available width for goal text: inner width minus prefix " N. ○ " padding
        let inner = block.inner(area);
        let prefix_width = 6; // " 1. " width (up to " 99. " still fits in 6)
        let icon_width = 3; // padded emoji width
        let goal_width = (inner.width as usize).saturating_sub(prefix_width + icon_width);

        // Show children with their goals
        // Note: currently only Open-status children are sent by poll_engine_status;
        // if closed children are ever displayed here, re-add the closed-style branch.
        for (i, child) in self.children.iter().enumerate() {
            let icon = padded_emoji("\u{f111}"); // empty circle (nf-fa-circle_o)
            let status_color = theme::WARNING;

            // Line 1+: word-wrapped goal
            let wrapped = word_wrap(&child.goal, goal_width);
            if wrapped.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled(format!(" {}. ", i + 1), Style::new().fg(theme::TEXT_DIM)),
                    Span::styled(icon.clone(), Style::new().fg(status_color)),
                    Span::raw(""),
                ]));
            } else {
                for (j, wline) in wrapped.iter().enumerate() {
                    if j == 0 {
                        lines.push(Line::from(vec![
                            Span::styled(format!(" {}. ", i + 1), Style::new().fg(theme::TEXT_DIM)),
                            Span::styled(icon.clone(), Style::new().fg(status_color)),
                            Span::styled(wline.clone(), Style::new().fg(theme::TEXT)),
                        ]));
                    } else {
                        lines.push(Line::from(vec![
                            Span::raw("   "), // indent to align with text after icon
                            Span::styled(wline.clone(), Style::new().fg(theme::TEXT)),
                        ]));
                    }
                }
            }

            // Line 2: indented metadata
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(
                    format!("id: {}", truncate_mid(&child.child_id, 12)),
                    Style::new().fg(theme::TEXT_DIM),
                ),
                Span::raw(" "),
                Span::styled("● open", Style::new().fg(status_color)),
            ]));
        }

        frame.render_widget(block, area);

        let text = ratatui::widgets::Paragraph::new(lines)
            .style(Style::new().bg(theme::BG))
            .scroll((self.scroll_offset as u16, 0));
        frame.render_widget(text, inner);
    }
}

impl Component for GraphPanel {
    fn update(&mut self, _cmd: UiCommand) {
        self.dirty = true;
    }

    fn view(&mut self, area: Rect, frame: &mut Frame) {
        self.render(frame, area);
        self.dirty = false;
    }

    fn needs_render(&self) -> bool {
        self.dirty
    }

    fn clear_dirty(&mut self) {
        self.dirty = false;
    }
}

/// Truncate a string from the middle for compact display.
/// Uses char-counting to safely handle multi-byte UTF-8.
fn truncate_mid(s: &str, max_len: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_len {
        return s.to_string();
    }
    let front = (max_len - 1) / 2;
    let back = max_len - 1 - front;
    let front_str: String = s.chars().take(front).collect();
    let back_str: String = s.chars().skip(char_count - back).collect();
    format!("{}…{}", front_str, back_str)
}
