//! Output area rendering.
//!
//! Handles the terminal rendering of the conversation output:
//! cache hit/miss, visible line clipping, scroll indicator overlay.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::Paragraph,
};

use super::OutputState;
use super::cache;
use crate::ui::theme;

/// Render the output area.
///
/// Uses a write-through cache: `segment_to_lines()` + `wrap::wrap_line()` are
/// re-done only when segments change or the terminal width changes.
pub fn render(frame: &mut Frame, area: Rect, state: &mut OutputState) {
    let visible_height = area.height as usize;
    let width = area.width as usize;
    if visible_height == 0 || width == 0 {
        return;
    }

    // Rebuild cache if stale (segments changed or width changed).
    if state.cache_gen > 0 || state.cache_width != width {
        state.cached_lines = cache::build_cache(state, width);
        state.cache_width = width;
        // Reset gen so we don't rebuild again until next mutation.
        // Use wrapping to avoid overflow on billions of mutations.
        state.cache_gen = 0;
    }

    let total = state.cached_lines.len();
    let start = if total <= visible_height {
        0
    } else {
        let max_scroll = total - visible_height;
        let s = state.scroll.min(max_scroll);
        max_scroll - s
    };

    let visible: Vec<Line> = state.cached_lines[start..]
        .iter()
        .take(visible_height)
        .cloned()
        .collect();

    let text = Text::from(visible);

    frame.render_widget(
        Paragraph::new(text)
            .wrap(ratatui::widgets::Wrap { trim: false })
            .style(Style::new().bg(theme::BG)),
        area,
    );

    // Scroll indicator
    if total > visible_height && state.scroll > 0 {
        let max_scroll = total - visible_height;
        let pct = state.scroll as f64 / max_scroll as f64;
        if pct > 0.0 && area.width > 8 {
            let indicator = format!(" {}% ", (pct * 100.0) as u32);
            let ind_area = Rect {
                x: area.x + area.width.saturating_sub(indicator.len() as u16),
                y: area.y,
                width: indicator.len() as u16,
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(Span::styled(
                    indicator,
                    Style::new()
                        .fg(theme::TEXT_BRIGHT)
                        .bg(theme::ACCENT_DIM)
                        .add_modifier(Modifier::BOLD),
                )),
                ind_area,
            );
        }
    }
}
