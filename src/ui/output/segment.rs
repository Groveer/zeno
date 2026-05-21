//! Output segment types and segment-to-lines conversion.
//!
//! Defines the [`OutputSegment`] enum and the core rendering logic
//! that converts each segment variant to styled ratatui lines.
//! This is separated from the output state and cache management
//! so that the conversion logic can be unit-tested independently.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::super::theme;
use crate::utils::{char_width, display_width};

/// A single segment of rendered output.
#[derive(Debug, Clone)]
pub enum OutputSegment {
    /// User input echo.
    UserInput(String),
    /// Question from ask_user tool — icon on first line, indented continuation.
    AskQuestion(String),
    /// Response to ask_user tool — visually indented under the question.
    AskResponse(String),
    /// Assistant text (LLM response).
    Text(String),
    /// Assistant reasoning / thinking content (displayed dimmed, separate from visible text).
    Reasoning(String),
    /// Tool invocation — executing (spinner).
    ToolExecuting(String),
    /// Tool completed — one-line result summary.
    ToolComplete(String),
    /// Tool error.
    ToolError(String),
    /// Diff output — shows file change diff with +/- markers.
    /// Rendered with distinct green/red coloring for additions/removals.
    Diff(String),
    /// Status message.
    Status(String),
    /// Rolling sub-agent progress — shows current activity per agent, updates in-place.
    SubAgentProgress(Vec<String>),
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

/// Convert a segment to styled lines.
/// Returns owned `'static` lines suitable for caching.
pub fn segment_to_lines(seg: &OutputSegment) -> Vec<Line<'static>> {
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
        OutputSegment::AskQuestion(text) => {
            let mut lines: Vec<Line<'static>> = Vec::new();
            for (i, line) in text.lines().enumerate() {
                if i == 0 {
                    lines.push(Line::from(vec![
                        Span::styled("   ".to_string(), Style::new().fg(theme::ACCENT)),
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
                    Span::styled("   ".to_string(), Style::new().fg(theme::ACCENT)),
                    Span::styled(text.clone(), Style::new().fg(theme::TEXT)),
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
            super::super::markdown::render_markdown(text)
        }
        OutputSegment::Reasoning(text) => {
            // Single-line rolling display: show the latest snippet of reasoning.
            // Reasoning text accumulates across deltas; we tail the last line for
            // a rolling-in-place effect — just enough to show the model is thinking.
            let header = Span::styled(" 󰧑 ", Style::new().fg(theme::ACCENT_DIM));

            // Take the last non-empty line for the rolling display
            let snippet = text
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or(text);
            let trimmed = snippet.trim();

            // Tail-truncate to fit roughly one line (wrap_line handles remainder).
            // 120 chars is well within typical terminal widths.
            const ROLLING_WIDTH: usize = 120;
            let display = if display_width(trimmed) <= ROLLING_WIDTH {
                trimmed.to_string()
            } else {
                let chars: Vec<char> = trimmed.chars().collect();
                let mut w = 0usize;
                let mut start = chars.len();
                for i in (0..chars.len()).rev() {
                    let next = chars.get(i + 1).copied();
                    let cw = char_width(chars[i], next);
                    if w + cw > ROLLING_WIDTH.saturating_sub(1) {
                        start = i + 1;
                        break;
                    }
                    w += cw;
                    start = i;
                }
                if start > 0 {
                    format!("…{}", chars[start..].iter().collect::<String>())
                } else {
                    trimmed.to_string()
                }
            };

            vec![Line::from(vec![
                header,
                Span::styled(display, Style::new().fg(theme::TEXT_DIM)),
            ])]
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
            let mut lines: Vec<Line<'static>> = Vec::new();
            for (i, line) in err.lines().enumerate() {
                if i == 0 {
                    lines.push(Line::from(vec![
                        Span::styled("  ".to_string(), Style::new().fg(theme::ERROR)),
                        Span::styled(line.to_string(), Style::new().fg(theme::ERROR)),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::styled("    ".to_string(), Style::default()),
                        Span::styled(line.to_string(), Style::new().fg(theme::ERROR)),
                    ]));
                }
            }
            if lines.is_empty() {
                vec![Line::from(vec![
                    Span::styled("  ".to_string(), Style::new().fg(theme::ERROR)),
                    Span::styled(err.clone(), Style::new().fg(theme::ERROR)),
                ])]
            } else {
                lines
            }
        }
        OutputSegment::Status(msg) => {
            // Split by newlines so multiline status messages render correctly
            let mut lines: Vec<Line<'static>> = Vec::new();
            for line in msg.lines() {
                lines.push(Line::from(Span::styled(
                    line.to_string(),
                    Style::new().fg(theme::TEXT_DIM),
                )));
            }
            lines
        }
        OutputSegment::SubAgentProgress(activities) => {
            // Rolling display: show the current activity per agent.
            // Styled with a distinct dim accent to differentiate from regular status.
            activities
                .iter()
                .map(|line| {
                    Line::from(vec![
                        Span::styled(" \u{F0DA} ", Style::new().fg(theme::ACCENT_DIM)),
                        Span::styled(line.clone(), Style::new().fg(theme::TEXT_DIM)),
                    ])
                })
                .collect()
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
                Span::styled("   ".to_string(), Style::new().fg(theme::WARNING)),
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
        OutputSegment::Diff(diff) => {
            // Render diff lines with +/- highlighting (green for +, red for -)
            let mut lines: Vec<Line<'static>> = Vec::new();
            for line in diff.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with('+') {
                    lines.push(Line::from(Span::styled(
                        trimmed.to_string(),
                        Style::new().fg(theme::SUCCESS),
                    )));
                } else if trimmed.starts_with('-') {
                    lines.push(Line::from(Span::styled(
                        trimmed.to_string(),
                        Style::new().fg(theme::ERROR),
                    )));
                } else {
                    lines.push(Line::from(Span::styled(
                        line.to_string(),
                        Style::new().fg(theme::TEXT_DIM),
                    )));
                }
            }
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

        let area = ratatui::layout::Rect::new(0, 0, 40, 2);
        let text = ratatui::text::Text::from(lines);
        let paragraph = ratatui::widgets::Paragraph::new(text)
            .wrap(ratatui::widgets::Wrap { trim: false })
            .style(Style::new().bg(theme::BG));

        let mut buf = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::Widget::render(paragraph, area, &mut buf);

        // New renderer: heading starts at position 0 (no "## " prefix)
        let cell = buf.cell((0, 0)).unwrap();

        assert!(
            matches!(cell.fg, Color::Rgb(_, _, _)),
            "Heading text should be RGB color, got: {:?}",
            cell.fg
        );
    }
}
