//! Side panel component — shows the todo/task list.
//!
//! Renders a right-side panel with the current task plan,
//! progress bar, and individual task items.
//! Implements the `Component` trait for integration into the component tree.

use std::sync::Arc;

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph},
};
use tokio::sync::Mutex;

use crate::gateway::UiCommand;
use crate::tools::todo::TodoState;

use super::app::word_wrap;
use super::component::Component;
use super::theme;

/// Returns true if the todo side panel should be visible.
/// The panel is hidden when there are no tasks, or when
/// the task list is empty or all tasks are completed.
fn has_active_todo(state: &Arc<Mutex<TodoState>>) -> bool {
    match state.try_lock() {
        Ok(s) => {
            if s.tasks.is_empty() {
                return false;
            }
            let all_completed = s.tasks.iter().all(|t| t.status == "completed");
            !all_completed
        }
        Err(_) => {
            // Contended lock means something is happening — show the panel
            true
        }
    }
}

/// Side panel component — displays the todo/task list.
pub struct SidePanel {
    /// Shared todo state reference.
    todo_state: Option<Arc<Mutex<TodoState>>>,
    /// Whether the side panel needs re-rendering.
    dirty: bool,
}

impl SidePanel {
    pub fn new() -> Self {
        Self {
            todo_state: None,
            dirty: true,
        }
    }

    /// Set the shared todo state for the side panel.
    pub fn set_todo_state(&mut self, state: Arc<Mutex<TodoState>>) {
        self.todo_state = Some(state);
        self.dirty = true;
    }

    /// Whether the side panel should be visible (has active todos).
    pub fn is_visible(&self) -> bool {
        self.todo_state.as_ref().is_some_and(has_active_todo)
    }

    /// Render the right side panel showing the todo list.
    fn render(&mut self, frame: &mut Frame, area: Rect) {
        let state_arc = match &self.todo_state {
            Some(s) => s,
            None => return,
        };

        // Try to lock; if contended, show a brief message
        let state = match state_arc.try_lock() {
            Ok(s) => s,
            Err(_) => {
                let block = Block::default()
                    .title(" Tasks ")
                    .borders(Borders::LEFT)
                    .border_style(Style::new().fg(theme::BORDER));
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        " loading...",
                        Style::new().fg(theme::TEXT_DIM),
                    ))
                    .block(block),
                    area,
                );
                return;
            }
        };

        let total = state.tasks.len();
        let completed = state
            .tasks
            .iter()
            .filter(|t| t.status == "completed")
            .count();

        // Create block early so we can measure the actual content width
        let block = Block::default()
            .borders(Borders::LEFT)
            .border_style(Style::new().fg(theme::BORDER));
        let inner_width = block.inner(area).width as usize;

        let mut lines: Vec<Line<'static>> = Vec::new();

        // Title line
        lines.push(Line::from(vec![
            Span::styled(" ", Style::new().fg(theme::ACCENT)),
            Span::styled(
                "Tasks",
                Style::new()
                    .fg(theme::TEXT_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(""));

        // Plan name
        if !state.plan.is_empty() {
            let plan_width = inner_width.saturating_sub(4);
            for wrapped_line in word_wrap(&state.plan, plan_width) {
                lines.push(Line::from(Span::styled(
                    wrapped_line,
                    Style::new().fg(theme::ACCENT),
                )));
            }
            lines.push(Line::from(""));
        }

        // Text-based progress bar
        if total > 0 {
            let fraction = format!("{}/{}", completed, total);
            let bar_width = inner_width.saturating_sub(4 + fraction.len());
            let filled = if bar_width > 0 && total > 0 {
                (completed * bar_width) / total
            } else {
                0
            };
            let empty = bar_width.saturating_sub(filled);
            let bar = format!(
                " [{}{}] {}",
                "█".repeat(filled),
                "░".repeat(empty),
                fraction
            );
            lines.push(Line::from(vec![Span::styled(
                bar,
                Style::new().fg(theme::SUCCESS).bg(theme::SURFACE),
            )]));
            lines.push(Line::from(""));
        }

        // Task items
        let desc_width = inner_width.saturating_sub(8);
        for task in &state.tasks {
            let (checkbox, color) = match task.status.as_str() {
                "completed" => ("✓", theme::TEXT_DIM),
                "in_progress" => ("", theme::ACCENT),
                _ => ("○", theme::TEXT),
            };
            let wrapped = word_wrap(&task.description, desc_width);
            for (i, line) in wrapped.iter().enumerate() {
                if i == 0 {
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!(" {} ", checkbox),
                            Style::new().fg(color).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(line.clone(), Style::new().fg(color)),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::styled("   ", Style::new().fg(color)),
                        Span::styled(line.clone(), Style::new().fg(color)),
                    ]));
                }
            }
        }
        if total == 0 {
            lines.push(Line::from(Span::styled(
                " (no tasks)",
                Style::new().fg(theme::TEXT_DIM),
            )));
        }

        // Render with left border
        frame.render_widget(
            Paragraph::new(Text::from(lines))
                .block(block)
                .style(Style::new().bg(theme::BG)),
            area,
        );
    }
}

impl Component for SidePanel {
    fn update(&mut self, _cmd: UiCommand) {
        // SidePanel doesn't receive UiCommands directly — it polls TodoState
        // via poll_engine_status(). Mark dirty so poll changes are reflected.
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
