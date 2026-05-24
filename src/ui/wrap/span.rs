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

    // ── Full pipeline tests: wrap_spans → Paragraph rendering ─────────────
    //
    // These verify that our character-level wrapping (wrap_spans) produces
    // correct line counts and content.  Lines are pre-wrapped to fit within
    // `max_width` — Paragraph should NOT be asked to re-wrap them (the
    // `render` function passes them without `.wrap()` for this reason).

    #[test]
    fn test_wrap_full_pipeline_ascii() {
        // 20 ASCII characters, wrapped to width 10 → 2 lines of 10 each.
        let spans = vec![Span::styled("ABCDEFGHIJKLMNOPQRST", Style::default())];
        let wrapped = wrap_spans(&spans, 10);
        assert_eq!(wrapped.len(), 2, "wrap_spans: 20 chars at 10 → 2 lines");
        assert_eq!(concat_spans(&wrapped[0]), "ABCDEFGHIJ");
        assert_eq!(concat_spans(&wrapped[1]), "KLMNOPQRST");
    }

    #[test]
    fn test_wrap_full_pipeline_cjk() {
        // 5 CJK chars (width 2 each = total 10) at width 8 → 4+1.
        let spans = vec![Span::styled("你好世界！", Style::default())];
        let wrapped = wrap_spans(&spans, 8);
        assert_eq!(wrapped.len(), 2, "5 CJK at width 8 → 2 lines");
        assert_eq!(concat_spans(&wrapped[0]), "你好世界", "Line 0: 4 CJK (w=8)");
        assert_eq!(concat_spans(&wrapped[1]), "！", "Line 1: 1 CJK (w=2)");
    }

    #[test]
    fn test_wrap_full_pipeline_cjk_exact() {
        // 4 mixed chars (width 8 total) at width 8 → 1 line.
        let spans = vec![Span::styled("ABCD世界", Style::default())];
        let wrapped = wrap_spans(&spans, 8);
        assert_eq!(wrapped.len(), 1, "width-8 at width 8 → 1 line");
        assert_eq!(concat_spans(&wrapped[0]), "ABCD世界");
    }

    #[test]
    fn test_wrap_full_pipeline_emoji() {
        // 5 emoji (width 2 each = total 10) wrapped at width 6 → 3+2.
        let spans = vec![Span::styled("😀😀😀😀😀", Style::default())];
        let wrapped = wrap_spans(&spans, 6);
        assert_eq!(wrapped.len(), 2, "5 emoji at width 6 → 2 lines");
        assert_eq!(concat_spans(&wrapped[0]), "😀😀😀", "Line 0: 3 emoji");
        assert_eq!(concat_spans(&wrapped[1]), "😀😀", "Line 1: 2 emoji");
    }

    #[test]
    fn test_wrap_full_pipeline_exact_fit() {
        // Line with exactly width 10 at width 10 — single line.
        let spans = vec![Span::styled("1234567890", Style::default())];
        let wrapped = wrap_spans(&spans, 10);
        assert_eq!(wrapped.len(), 1, "exact fit → 1 line");
        assert_eq!(concat_spans(&wrapped[0]), "1234567890");
    }

    #[test]
    fn test_wrap_full_pipeline_one_past_exact() {
        // 11 chars at width 10 → 10+1.
        let spans = vec![Span::styled("ABCDEFGHIJK", Style::default())];
        let wrapped = wrap_spans(&spans, 10);
        assert_eq!(wrapped.len(), 2, "11 chars at 10 → 2 lines");
        assert_eq!(concat_spans(&wrapped[0]), "ABCDEFGHIJ");
        assert_eq!(concat_spans(&wrapped[1]), "K");
    }

    #[test]
    fn test_wrap_full_pipeline_mixed_with_prefix() {
        // Simulates a UserInput line: prefix + trailing text.
        // Prefix "◆ " (w=2) + 18 chars (w=18) = width 20 at area width 10.
        let spans = vec![
            Span::styled("◆ ", Style::default()),
            Span::styled("ABCDEFGHIJKLMNOPQR", Style::default()),
        ];
        let wrapped = wrap_spans(&spans, 10);
        // "◆ " (w=2) + "ABCDEFGH" (8) = width 10. Then "IJKLMNOPQR" (10).
        assert_eq!(wrapped.len(), 2, "prefix+18 at 10 → 2 lines");
        let s0 = concat_spans(&wrapped[0]);
        assert!(s0.starts_with("◆ "), "Line 0 has prefix, got: {:?}", s0);
        assert_eq!(s0.chars().count(), 10, "◆ ABCDEFGH = 10 chars");
        assert_eq!(concat_spans(&wrapped[1]), "IJKLMNOPQR");
    }
}
