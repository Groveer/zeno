//! Style-preserving span wrapping for ratatui TUI rendering.
//!
//! Splits [`ratatui::text::Span`] lists at character boundaries, preserving
//! each span's [`Style`] across wrapped lines. This is used by the output
//! cache to ensure syntax highlighting and other styling survive line breaks.

use ratatui::text::{Line, Span};

use crate::utils::char_width;

/// Default tab width for display calculations.
pub const TAB_WIDTH: usize = 4;

/// Wrap a single styled line to fit within `max_width` terminal columns.
/// Returns multiple lines if the original line is too wide.
///
/// Walks spans character-by-character using [`char_width`] (which handles
/// VS16 emoji presentation, PUA icons, etc.) so that emoji and CJK wide
/// characters are correctly measured.
///
/// Each output line preserves the original styling from its source spans.
/// When a span is split across a line boundary, the continuation uses the
/// same style.
pub fn wrap_spans(spans: &[Span<'static>], max_width: usize) -> Vec<Vec<Span<'static>>> {
    if max_width == 0 || spans.is_empty() {
        return if spans.is_empty() {
            vec![Vec::new()]
        } else {
            vec![spans.to_vec()]
        };
    }

    // Fast path: if the total width fits, return as-is.
    // Compute width across all spans with proper tab-stop tracking (column
    // position carries over between spans, just like the char-by-char path).
    let mut total_width: usize = 0;
    let mut col: usize = 0;
    for span in spans {
        for ch in span.content.as_ref().chars() {
            if ch == '\t' {
                let tw = TAB_WIDTH - (col % TAB_WIDTH);
                total_width += tw;
                col += tw;
            } else {
                // Pass None for next-char peek — only affects VS16 sequences
                // which are rare at span boundaries; the slight overcount is
                // harmless for the fast-path "fits? → early return" check.
                let w = char_width(ch, None);
                total_width += w;
                col += w;
            }
        }
    }
    if total_width <= max_width {
        return vec![spans.to_vec()];
    }

    // Walk through spans, splitting at max_width boundaries.
    // Track `col` (column position) so that tabs expand to the next
    // tab-stop boundary, matching the fast-path calculation.
    let mut result: Vec<Vec<Span<'static>>> = Vec::new();
    let mut current_line: Vec<Span<'static>> = Vec::new();
    let mut current_width: usize = 0;
    let mut col: usize = 0;

    for span in spans {
        let span_str: &str = span.content.as_ref();
        let span_style = span.style;

        let mut remaining = span_str;
        while !remaining.is_empty() {
            // If current line is already full, start a new one
            if current_width >= max_width {
                result.push(std::mem::take(&mut current_line));
                current_width = 0;
                col = 0;
            }

            let room = max_width - current_width;

            // Find how many chars fit in `room` columns.
            // Uses `char_width()` which already handles VS16 emoji presentation
            // sequences (base + U+FE0F → width 2) — no separate VS16 skip needed.
            let mut byte_end = 0;
            let mut used_width = 0;
            let mut peekable = remaining.chars().peekable();
            while let Some(ch) = peekable.next() {
                let next = peekable.peek().copied();
                // Tabs expand to the next tab-stop boundary (positional).
                let w = if ch == '\t' {
                    TAB_WIDTH - (col % TAB_WIDTH)
                } else {
                    char_width(ch, next)
                };
                if used_width + w > room {
                    break;
                }
                byte_end += ch.len_utf8();
                used_width += w;
                col += w;
            }

            if byte_end == 0 {
                // Single char wider than room — force it onto current line
                let ch = remaining.chars().next().unwrap();
                byte_end = ch.len_utf8();
                let next = remaining.chars().nth(1);
                used_width = if ch == '\t' {
                    TAB_WIDTH - (col % TAB_WIDTH)
                } else {
                    char_width(ch, next)
                };
                col += used_width;
            }

            let (chunk, rest) = remaining.split_at(byte_end);
            current_line.push(Span::styled(chunk.to_string(), span_style));
            current_width += used_width;
            remaining = rest;
        }
    }

    if !current_line.is_empty() {
        result.push(current_line);
    }

    if result.is_empty() {
        result.push(Vec::new());
    }

    result
}

/// Convenience: wrap a ratatui `Line` into multiple `Line`s by display width.
/// Delegates to [`wrap_spans`].
pub fn wrap_line(line: Line<'static>, max_width: usize) -> Vec<Line<'static>> {
    let wrapped = wrap_spans(&line.spans, max_width);
    wrapped.into_iter().map(Line::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;

    #[test]
    fn test_wrap_spans_empty() {
        let result = wrap_spans(&[], 10);
        assert_eq!(result.len(), 1);
        assert!(result[0].is_empty());
    }

    #[test]
    fn test_wrap_spans_zero_width() {
        let spans = vec![Span::styled("hello", Style::default())];
        let result = wrap_spans(&spans, 0);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].len(), 1);
    }

    #[test]
    fn test_wrap_spans_short_line() {
        let spans = vec![Span::styled("hello", Style::default())];
        let result = wrap_spans(&spans, 10);
        assert_eq!(result.len(), 1);
        assert_eq!(concat_spans(&result[0]), "hello");
    }

    #[test]
    fn test_wrap_spans_emoji() {
        // 😀 is width 2. Two emoji = width 4, which fits in width 4.
        let spans = vec![Span::styled("😀😀", Style::default())];
        let result = wrap_spans(&spans, 4);
        assert_eq!(result.len(), 1);
        assert_eq!(concat_spans(&result[0]), "😀😀");
    }

    #[test]
    fn test_wrap_spans_emoji_split() {
        // 😀 width 2, so 3 emoji = width 6. At width 4: 😀😀 on line1, 😀 on line2.
        let spans = vec![Span::styled("😀😀😀", Style::default())];
        let result = wrap_spans(&spans, 4);
        assert_eq!(result.len(), 2);
        assert_eq!(concat_spans(&result[0]), "😀😀");
        assert_eq!(concat_spans(&result[1]), "😀");
    }

    #[test]
    fn test_wrap_spans_multiple_spans() {
        let spans = vec![
            Span::styled("hello ", Style::default()),
            Span::styled("world", Style::default()),
        ];
        let result = wrap_spans(&spans, 6);
        assert_eq!(result.len(), 2);
        assert_eq!(concat_spans(&result[0]), "hello ");
        assert_eq!(concat_spans(&result[1]), "world");
    }

    #[test]
    fn test_wrap_line_convenience() {
        let line = Line::from("hello world");
        let result = wrap_line(line, 5);
        assert_eq!(result.len(), 3);
        assert_eq!(concat_line(&result[0]), "hello");
        assert_eq!(concat_line(&result[1]), " worl");
        assert_eq!(concat_line(&result[2]), "d");
    }

    fn concat_spans(spans: &[Span<'static>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect::<String>()
    }

    fn concat_line(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }
}
