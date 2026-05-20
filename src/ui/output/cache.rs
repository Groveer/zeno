//! Cache management for rendered output lines.
//!
//! Provides the cache-building logic: converting segments to styled lines,
//! then wrapping them to fit the terminal width. The cache is invalidated
//! whenever segments change (`cache_gen > 0`) or width changes.

use ratatui::text::{Line, Span};

use super::OutputState;

use crate::utils::{char_width, display_width};

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

/// Wrap a single styled line to fit within `max_width` terminal columns.
/// Returns multiple lines if the original line is too wide.
fn wrap_line(line: Line<'static>, max_width: usize) -> Vec<Line<'static>> {
    // Calculate total display width of the line (emoji-aware)
    let total_width: usize = line
        .spans
        .iter()
        .map(|s| display_width(s.content.as_ref()))
        .sum();

    if total_width <= max_width || max_width == 0 {
        return vec![line];
    }

    // Walk through spans, splitting at max_width boundaries
    let mut result: Vec<Line<'static>> = Vec::new();
    let mut current_line: Vec<Span<'static>> = Vec::new();
    let mut current_width: usize = 0;

    for span in &line.spans {
        let span_str: &str = span.content.as_ref();
        let span_style = span.style;

        // Split this span's text by width (emoji-aware)
        let mut remaining = span_str;
        while !remaining.is_empty() {
            // If current line is already full, start a new one
            if current_width >= max_width {
                result.push(Line::from(std::mem::take(&mut current_line)));
                current_width = 0;
            }

            let room = max_width - current_width;

            // Find how many chars fit in `room` columns
            let mut byte_end = remaining.len();
            let mut used_width = 0;
            let chars: Vec<char> = remaining.chars().collect();
            let mut ci = 0;
            #[allow(clippy::explicit_counter_loop)]
            for (i, ch) in remaining.char_indices() {
                let next = chars.get(ci + 1).copied();
                let w = char_width(ch, next);
                if used_width + w > room {
                    byte_end = i;
                    break;
                }
                used_width += w;
                byte_end = remaining.len();
                // Skip VS16 if it was consumed as part of an emoji sequence
                if next == Some('\u{FE0F}') {
                    ci += 2;
                } else {
                    ci += 1;
                }
            }

            if byte_end == 0 {
                // Single char wider than room — force it onto current line
                let ch = remaining.chars().next().unwrap();
                byte_end = ch.len_utf8();
                let next = remaining.chars().nth(1);
                used_width = char_width(ch, next);
            }

            let (chunk, rest) = remaining.split_at(byte_end);
            current_line.push(Span::styled(chunk.to_string(), span_style));
            current_width += used_width;
            remaining = rest;
        }
    }

    if !current_line.is_empty() {
        result.push(Line::from(current_line));
    }

    if result.is_empty() {
        result.push(Line::from(""));
    }

    result
}
