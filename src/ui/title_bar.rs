//! Title bar component — shows version and keyboard shortcuts.
//!
//! Implements the `Component` trait for integration into the component tree.

use ratatui::{Frame, layout::Rect, style::Style, widgets::Paragraph};

use crate::gateway::UiCommand;

use super::component::Component;
use super::theme;

/// Title bar component — stateless, always renders the same content.
pub struct TitleBar;

impl Component for TitleBar {
    fn update(&mut self, _cmd: UiCommand) {}

    fn view(&mut self, area: Rect, frame: &mut Frame) {
        let title = format!(
            " zeno {} (Ctrl+D to quit, Ctrl+C to interrupt) ",
            env!("CARGO_PKG_VERSION")
        );
        frame.render_widget(
            Paragraph::new(title).style(Style::new().fg(theme::TEXT_BRIGHT).bg(theme::ACCENT_DIM)),
            area,
        );
    }

    fn needs_render(&self) -> bool {
        false // Title bar is cheap and always drawn by App
    }
}
