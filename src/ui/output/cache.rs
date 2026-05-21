//! Cache management for rendered output lines.
//!
//! Provides the cache-building logic: converting segments to styled lines,
//! then wrapping them to fit the terminal width. The cache is invalidated
//! whenever segments change (`cache_gen > 0`) or width changes.
//!
//! Delegates actual wrapping to [`crate::ui::wrap::span`].

use ratatui::text::Line;

use super::OutputState;
use crate::ui::wrap::span::wrap_line;

/// Build the full cached line list from segments.
///
/// This is the expensive path (markdown parsing, syntect highlighting, wrapping).
/// Called only when the cache is stale (segments changed or terminal resized).
pub fn build_cache(state: &OutputState, width: usize) -> Vec<Line<'static>> {
    let all_lines: Vec<Line<'static>> = state
        .segments
        .iter()
        .flat_map(super::segment::segment_to_lines)
        .collect();
    all_lines
        .into_iter()
        .flat_map(|line| wrap_line(line, width))
        .collect()
}
