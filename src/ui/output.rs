//! Scrollable output area for conversation history.

//!

//! Displays assistant text, tool calls, and tool results with

//! distinct styling.  Supports scrolling through history.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::Paragraph,
};

use super::theme;

/// A single segment of rendered output.
#[derive(Debug, Clone)]
pub enum OutputSegment {
    /// User input echo.
    UserInput(String),
    /// Response to ask_user tool — visually indented under the question.
    AskResponse(String),
    /// Assistant text (LLM response).
    Text(String),
    /// Tool invocation — executing (spinner).
    ToolExecuting(String),
    /// Tool completed — one-line result summary.
    ToolComplete(String),
    /// Tool error.
    ToolError(String),
    /// Status message.
    Status(String),
    /// Permission prompt — requires user confirmation (y/n/a).
    /// Rendered with distinct styling to stand out from normal output.
    PermissionPrompt {
        tool_name: String,
        reason: String,
        detail: String,
    },
    /// Error message.
    Error(String),
}

/// Scrollable output state.
pub struct OutputState {
    /// All rendered segments so far.
    pub(crate) segments: Vec<OutputSegment>,
    /// Scroll offset from the bottom (0 = bottom / newest).
    scroll: usize,
    /// Whether auto-scroll is enabled (follows new output).
    auto_scroll: bool,
    /// Cached rendered lines (built by build_cached_lines, invalidated on push/clear).
    cached_lines: Vec<Line<'static>>,
    /// Generation counter: incremented on push/clear, compared by render() to detect staleness.
    cache_gen: u64,
    /// Last width used to build cache; if width changes, we rebuild.
    cache_width: usize,
}

impl OutputState {
    pub fn new() -> Self {
        Self {
            segments: Vec::new(),
            scroll: 0,
            auto_scroll: true,
            cached_lines: Vec::new(),
            cache_gen: 0,
            cache_width: 0,
        }
    }

    /// Invalidate the line cache (call after any mutation to segments).
    fn bump_gen(&mut self) {
        self.cache_gen += 1;
    }

    pub fn push(&mut self, seg: OutputSegment) {
        self.segments.push(seg);
        self.bump_gen();
    }

    /// Call after in-place mutation of a segment (e.g. TextDelta append).
    /// Invalidates the render cache without adding a new segment.
    pub fn mark_dirty(&mut self) {
        self.bump_gen();
    }

    pub fn clear(&mut self) {
        self.segments.clear();
        self.scroll = 0;
        self.auto_scroll = true;
        self.cached_lines.clear();
        self.bump_gen();
    }

    /// Scroll up by N lines (towards older content).
    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll = self.scroll.saturating_add(lines);
        self.auto_scroll = false;
    }

    /// Scroll down by N lines (towards newer content).
    pub fn scroll_down(&mut self, lines: usize) {
        if self.scroll <= lines {
            self.scroll = 0;
            self.auto_scroll = true;
        } else {
            self.scroll -= lines;
        }
    }
}

/// Build the full cached line list from segments.
/// This is the expensive path (markdown parsing, syntect highlighting, wrapping).
/// Called only when the cache is stale.
fn build_cache(state: &OutputState, width: usize) -> Vec<Line<'static>> {
    let all_lines: Vec<Line<'static>> = state.segments.iter().flat_map(segment_to_lines).collect();
    all_lines
        .into_iter()
        .flat_map(|line| wrap_line(line, width))
        .collect()
}

/// Render the output area.
///
/// Uses a write-through cache: `segment_to_lines()` + `wrap_line()` are
/// re-done only when segments change or the terminal width changes.
pub fn render(frame: &mut Frame, area: Rect, state: &mut OutputState) {
    let visible_height = area.height as usize;
    let width = area.width as usize;
    if visible_height == 0 || width == 0 {
        return;
    }

    // Rebuild cache if stale (segments changed or width changed).
    if state.cache_gen > 0 || state.cache_width != width {
        state.cached_lines = build_cache(state, width);
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

    frame.render_widget(Paragraph::new(text).style(Style::new().bg(theme::BG)), area);

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

use crate::utils::display_width;

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
            // Uses display_width logic: base char + VS16 = width 2
            let mut byte_end = remaining.len();
            let mut used_width = 0;
            let chars: Vec<char> = remaining.chars().collect();
            let mut ci = 0;
            #[allow(clippy::explicit_counter_loop)]
            for (i, ch) in remaining.char_indices() {
                let w = if ci + 1 < chars.len() && chars[ci + 1] == '\u{FE0F}' {
                    // Emoji presentation sequence: width 2, will skip VS16 next iter
                    2
                } else if ch == '\u{FE0F}' {
                    // VS16 was already counted with its base char
                    0
                } else {
                    unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0)
                };
                if used_width + w > room {
                    byte_end = i;
                    break;
                }
                used_width += w;
                byte_end = remaining.len();
                ci += 1;
            }

            if byte_end == 0 {
                // Single char wider than room — force it onto current line
                let ch = remaining.chars().next().unwrap();
                byte_end = ch.len_utf8();
                used_width = if let Some(next) = remaining.chars().nth(1) {
                    if next == '\u{FE0F}' {
                        2
                    } else {
                        unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0)
                    }
                } else {
                    unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0)
                };
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

/// Convert a segment to styled lines.
/// Returns owned `'static` lines suitable for caching.
fn segment_to_lines(seg: &OutputSegment) -> Vec<Line<'static>> {
    match seg {
        OutputSegment::UserInput(text) => {
            let mut lines: Vec<Line<'static>> = Vec::new();
            for (i, line) in text.lines().enumerate() {
                if i == 0 {
                    lines.push(Line::from(vec![
                        Span::styled("◆ ".to_string(), Style::new().fg(theme::ACCENT_DIM)),
                        Span::styled(line.to_string(), Style::new().fg(theme::TEXT_BRIGHT)),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::styled("│ ".to_string(), Style::new().fg(theme::ACCENT_DIM)),
                        Span::styled(line.to_string(), Style::new().fg(theme::TEXT_BRIGHT)),
                    ]));
                }
            }
            if lines.is_empty() {
                vec![Line::from(vec![
                    Span::styled("◆ ".to_string(), Style::new().fg(theme::ACCENT_DIM)),
                    Span::styled(text.clone(), Style::new().fg(theme::TEXT_BRIGHT)),
                ])]
            } else {
                lines
            }
        }
        OutputSegment::AskResponse(text) => {
            let mut lines: Vec<Line<'static>> = Vec::new();
            for (i, line) in text.lines().enumerate() {
                if i == 0 {
                    lines.push(Line::from(vec![
                        Span::styled("  ↳ ".to_string(), Style::new().fg(theme::ACCENT)),
                        Span::styled(line.to_string(), Style::new().fg(theme::TEXT)),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::styled("    ".to_string(), Style::default()),
                        Span::styled(line.to_string(), Style::new().fg(theme::TEXT)),
                    ]));
                }
            }
            if lines.is_empty() {
                vec![Line::from(vec![
                    Span::styled("  ↳ ".to_string(), Style::new().fg(theme::ACCENT)),
                    Span::styled(text.clone(), Style::new().fg(theme::TEXT)),
                ])]
            } else {
                lines
            }
        }
        OutputSegment::Text(text) => {
            // Render markdown to styled lines
            super::markdown::render_markdown(text)
        }
        OutputSegment::ToolExecuting(summary) => {
            vec![Line::from(vec![
                Span::styled("  ".to_string(), Style::new().fg(theme::ACCENT_DIM)),
                Span::styled(summary.clone(), Style::new().fg(theme::TEXT_DIM)),
            ])]
        }
        OutputSegment::ToolComplete(summary) => {
            // Format: "op → result" where result may be multi-line diff (\n-separated).
            // Line 1: " op → result_header"
            // Subsequent lines: diff lines with "-" (red) or "+" (green) prefix
            const ARROW_SEP: &str = " → ";
            let mut lines: Vec<Line<'static>> = Vec::new();

            if let Some(arrow_pos) = summary.find(ARROW_SEP) {
                let op = &summary[..arrow_pos];
                let result = &summary[arrow_pos + ARROW_SEP.len()..];
                let result_lines: Vec<&str> = result.lines().collect();

                if result_lines.len() == 1 {
                    // Single-line result — render on one line with color coding
                    let mut spans: Vec<Span<'static>> = vec![
                        Span::styled("  ".to_string(), Style::new().fg(theme::SUCCESS)),
                        Span::styled(op.to_string(), Style::new().fg(theme::TEXT)),
                        Span::styled(" → ".to_string(), Style::new().fg(theme::DIFF_ARROW)),
                    ];

                    let r = result_lines[0];
                    if r.starts_with('-') {
                        spans.push(Span::styled(
                            r.to_string(),
                            Style::new().fg(theme::DIFF_DEL),
                        ));
                    } else if r.starts_with('+') {
                        spans.push(Span::styled(
                            r.to_string(),
                            Style::new().fg(theme::DIFF_ADD),
                        ));
                    } else if r.starts_with("exit ") {
                        spans.push(Span::styled(r.to_string(), Style::new().fg(theme::ERROR)));
                    } else {
                        spans.push(Span::styled(
                            r.to_string(),
                            Style::new().fg(theme::TEXT_DIM),
                        ));
                    }
                    lines.push(Line::from(spans));
                } else {
                    // Multi-line result (diff): first line shows " op", then diff lines
                    lines.push(Line::from(vec![
                        Span::styled("  ".to_string(), Style::new().fg(theme::SUCCESS)),
                        Span::styled(op.to_string(), Style::new().fg(theme::TEXT)),
                    ]));

                    // Render diff lines with 4-space indent and color
                    for diff_line in &result_lines {
                        let (prefix_color, content) =
                            if let Some(stripped) = diff_line.strip_prefix('-') {
                                (theme::DIFF_DEL, stripped)
                            } else if let Some(stripped) = diff_line.strip_prefix('+') {
                                (theme::DIFF_ADD, stripped)
                            } else if diff_line.starts_with('(') || diff_line.starts_with("...") {
                                // Metadata lines like "(replace all)" or "... (N more lines omitted)"
                                (theme::TEXT_DIM, *diff_line)
                            } else {
                                (theme::TEXT_DIM, *diff_line)
                            };

                        let prefix_char = if diff_line.starts_with('-') {
                            "-"
                        } else if diff_line.starts_with('+') {
                            "+"
                        } else {
                            " "
                        };

                        lines.push(Line::from(vec![
                            Span::styled("    ".to_string(), Style::new().fg(theme::TEXT_DIM)),
                            Span::styled(
                                prefix_char.to_string(),
                                Style::new().fg(prefix_color).add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(content.to_string(), Style::new().fg(prefix_color)),
                        ]));
                    }
                }
            } else {
                // No arrow separator — simple summary
                lines.push(Line::from(vec![
                    Span::styled("  ".to_string(), Style::new().fg(theme::SUCCESS)),
                    Span::styled(summary.clone(), Style::new().fg(theme::TEXT)),
                ]));
            }

            lines
        }
        OutputSegment::ToolError(err) => {
            vec![Line::from(vec![
                Span::styled("  ".to_string(), Style::new().fg(theme::ERROR)),
                Span::styled(err.clone(), Style::new().fg(theme::ERROR)),
            ])]
        }
        OutputSegment::Status(msg) => {
            // Split by newlines so multiline status messages render correctly
            let mut lines: Vec<Line<'static>> = Vec::new();
            for line in msg.lines() {
                lines.push(Line::from(Span::styled(
                    format!(" ── {} ──", line),
                    Style::new().fg(theme::TEXT_DIM),
                )));
            }
            lines
        }
        OutputSegment::PermissionPrompt {
            tool_name,
            reason,
            detail,
        } => {
            // Render a visually distinct permission prompt block.
            // Uses WARNING color to draw attention without being alarming.
            let mut lines: Vec<Line<'static>> = Vec::new();

            // Header line: icon + [tool_name] reason
            lines.push(Line::from(vec![
                Span::styled("  ⚠ ".to_string(), Style::new().fg(theme::WARNING)),
                Span::styled(
                    format!("[{}] ", tool_name),
                    Style::new().fg(theme::WARNING).add_modifier(Modifier::BOLD),
                ),
                Span::styled(reason.clone(), Style::new().fg(theme::TEXT)),
            ]));

            // Detail lines (formatted tool input — not raw JSON).
            // Supports multi-line content (e.g. edit old_string/new_string).
            // Each line is indented to align with the header.
            if !detail.is_empty() {
                for (i, detail_line) in detail.lines().enumerate() {
                    if detail_line.starts_with("  - ") {
                        // Old string (removal) — red tint
                        lines.push(Line::from(vec![
                            Span::styled("    ".to_string(), Style::default()),
                            Span::styled(detail_line.to_string(), Style::new().fg(theme::DIFF_DEL)),
                        ]));
                    } else if detail_line.starts_with("  + ") {
                        // New string (addition) — green tint
                        lines.push(Line::from(vec![
                            Span::styled("    ".to_string(), Style::default()),
                            Span::styled(detail_line.to_string(), Style::new().fg(theme::DIFF_ADD)),
                        ]));
                    } else if i == 0 && detail.lines().count() == 1 {
                        // Single-line detail: render inline
                        lines.push(Line::from(vec![
                            Span::styled("    ".to_string(), Style::default()),
                            Span::styled(detail_line.to_string(), Style::new().fg(theme::TEXT_DIM)),
                        ]));
                    } else {
                        // Multi-line detail (continuation or plain)
                        lines.push(Line::from(vec![
                            Span::styled("    ".to_string(), Style::default()),
                            Span::styled(detail_line.to_string(), Style::new().fg(theme::TEXT_DIM)),
                        ]));
                    }
                }
            }

            // Prompt line
            lines.push(Line::from(vec![
                Span::styled("    ".to_string(), Style::default()),
                Span::styled(
                    "Allow? (y/n/a = yes to all)".to_string(),
                    Style::new().fg(theme::WARNING).add_modifier(Modifier::BOLD),
                ),
            ]));

            lines
        }
        OutputSegment::Error(err) => {
            vec![Line::from(Span::styled(
                format!("  {}", err),
                Style::new().fg(theme::ERROR).add_modifier(Modifier::BOLD),
            ))]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    #[test]
    fn test_heading_renders_with_color_in_paragraph() {
        let seg = OutputSegment::Text("## Hello World\nSome text".to_string());
        let lines = segment_to_lines(&seg);
        eprintln!("[TEST] lines: {:?}", lines);

        let area = Rect::new(0, 0, 40, 2);
        let text = Text::from(lines);
        let paragraph = Paragraph::new(text)
            .wrap(ratatui::widgets::Wrap { trim: false })
            .style(Style::new().bg(theme::BG));

        let mut buf = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::Widget::render(paragraph, area, &mut buf);

        // New renderer: heading starts at position 0 (no "## " prefix)
        let cell = buf.cell((0, 0)).unwrap();
        eprintln!(
            "[TEST] Cell (2,0): symbol={:?} fg={:?} bg={:?}",
            cell.symbol(),
            cell.fg,
            cell.bg
        );

        assert!(
            matches!(cell.fg, Color::Rgb(_, _, _)),
            "Heading text should be RGB color, got: {:?}",
            cell.fg
        );
    }
}
