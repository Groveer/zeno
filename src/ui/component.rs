//! Component trait for the TUI component tree.
//!
//! Each UI component follows a unified update/view lifecycle:
//! - `update(cmd)` — process a UiCommand and mutate internal state
//! - `view(area, frame)` — render to the terminal frame
//!
//! Components are composed into a tree rooted at `App`, which
//! dispatches UiCommands to child components and computes layout.

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::{Frame, layout::Rect};

use crate::gateway::UiCommand;

/// Component trait — the core abstraction for UI components.
///
/// Each component:
/// - Owns its state (no shared mutable state between siblings)
/// - Receives updates via `update(UiCommand)`
/// - Renders via `view(area, frame)`
/// - Can opt into mouse/resize events by overriding the default methods
pub trait Component {
    /// Mount hook (default: no-op).
    #[allow(dead_code, reason = "called via trait object dispatch")]
    fn mount(&mut self) {}

    /// Unmount hook (default: no-op).
    #[allow(dead_code, reason = "called via trait object dispatch")]
    fn unmount(&mut self) {}

    /// Process a UiCommand — mutate internal state.
    fn update(&mut self, cmd: UiCommand);

    /// Handle a keyboard event (default: no-op).
    #[allow(dead_code, reason = "called via trait object dispatch")]
    fn handle_key(&mut self, _key: KeyEvent) {}

    /// Handle a mouse event (default: no-op).
    #[allow(dead_code, reason = "called via trait object dispatch")]
    fn handle_mouse(&mut self, _mouse: MouseEvent) {}

    /// Handle terminal resize (default: no-op).
    #[allow(dead_code, reason = "called via trait object dispatch")]
    fn handle_resize(&mut self, _width: u16, _height: u16) {}

    /// Render the component to the frame within the given area.
    fn view(&mut self, area: Rect, frame: &mut Frame);

    /// Whether the component needs a re-render.
    #[allow(dead_code, reason = "called via trait object dispatch")]
    fn needs_render(&self) -> bool;

    /// Clear the dirty flag after rendering.
    #[allow(dead_code, reason = "called via trait object dispatch")]
    fn clear_dirty(&mut self) {}
}

/// Safe view wrapper — catches panics during component rendering.
///
/// If a component's `view()` panics, renders a red error placeholder
/// instead of crashing the entire TUI (which would leave the terminal
/// in raw mode with no cursor).
pub fn safe_view(component: &mut dyn Component, area: Rect, frame: &mut Frame) {
    let label = std::any::type_name_of_val(&component);
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        component.view(area, frame);
    })) {
        Ok(()) => {}
        Err(e) => {
            let err_msg = format!("[Component crashed: {label}: {e:?}]");
            frame.render_widget(
                ratatui::widgets::Paragraph::new(err_msg)
                    .style(ratatui::style::Style::new().fg(ratatui::style::Color::Red)),
                area,
            );
            tracing::error!("Component view() panic: {label}: {e:?}");
        }
    }
}
