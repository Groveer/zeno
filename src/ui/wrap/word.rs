//! Plain-text word-wrapping for terminal display.
//!
//! Splits text at word boundaries, respecting emoji/CJK display widths.
//! Single words wider than `max_width` are broken character-by-character.

use crate::utils::display_width;

use super::prefix;

/// Word-wrap text to fit within `max_width` terminal columns, respecting
/// multi-byte UTF-8 and emoji width. Breaks at word boundaries when possible.
/// Returns a list of wrapped line strings.
///
/// # Edge cases
///
/// - Empty input (`""`) returns an empty `Vec` (nothing to wrap).
/// - Zero `max_width` returns `vec![""]` (degenerate single empty line).
///
/// # Example
///
/// ```
/// let lines = word_wrap("hello world", 5);
/// assert_eq!(lines, vec!["hello", "world"]);
/// ```
pub fn word_wrap(s: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 || s.is_empty() {
        return if s.is_empty() {
            vec![]
        } else {
            vec![String::new()]
        };
    }
    let mut lines: Vec<String> = Vec::new();
    for line in s.lines() {
        wrap_single_line(line, max_width, &mut lines);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Wrap a single logical line (no embedded newlines) into multiple physical lines.
fn wrap_single_line(line: &str, max_width: usize, out: &mut Vec<String>) {
    let mut current = String::new();
    let mut current_width: usize = 0;

    for word in line.split_whitespace() {
        let word_w = display_width(word);

        // If the word itself exceeds max_width, break it character-by-character
        if word_w > max_width {
            // Flush what we have so far
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
                current_width = 0;
            }
            // Break the long word character by character using prefix_by_width
            let mut remaining = word;
            while !remaining.is_empty() {
                let (prefix, rest, taken) = prefix::take_prefix_by_width(remaining, max_width);
                if taken == 0 {
                    // Single char wider than room; take at least one char.
                    let ch = remaining.chars().next().unwrap();
                    out.push(ch.to_string());
                    remaining = &remaining[ch.len_utf8()..];
                } else if rest.is_empty() {
                    // Fits entirely; put in current buffer.
                    current = prefix.to_string();
                    current_width = taken;
                    break;
                } else {
                    out.push(prefix.to_string());
                    remaining = rest;
                }
            }
            continue;
        }

        // Factor in the space separator between words
        let sep_w = if current.is_empty() { 0usize } else { 1 };
        let new_width = current_width + sep_w + word_w;

        if new_width <= max_width {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
            current_width = new_width;
        } else {
            // Word doesn't fit on current line — start a new line
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            current.push_str(word);
            current_width = word_w;
        }
    }

    if !current.is_empty() {
        out.push(current);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_word_wrap_basic() {
        assert_eq!(word_wrap("hello world", 100), vec!["hello world"]);
    }

    #[test]
    fn test_word_wrap_break() {
        assert_eq!(
            word_wrap("hello world this is a test", 10),
            vec!["hello", "world this", "is a test"]
        );
    }

    #[test]
    fn test_word_wrap_empty() {
        let empty: Vec<String> = Vec::new();
        assert_eq!(word_wrap("", 10), empty);
    }

    #[test]
    fn test_word_wrap_zero_width() {
        assert_eq!(word_wrap("hello", 0), vec![String::new()]);
    }

    #[test]
    fn test_word_wrap_long_word() {
        let result = word_wrap("abcdefghijklmnop", 8);
        for line in &result {
            assert!(
                display_width(line) <= 8,
                "line '{line}' exceeds width 8 (width={})",
                display_width(line)
            );
        }
    }

    #[test]
    fn test_word_wrap_emoji() {
        // 😀 is width 2. "😀😀" is width 4, "😀😀😀" is width 6.
        let result = word_wrap("😀😀😀 xyz", 5);
        for line in &result {
            assert!(
                display_width(line) <= 5,
                "line '{line}' exceeds width 5 (width={})",
                display_width(line)
            );
        }
    }

    #[test]
    fn test_word_wrap_newline_preserved() {
        let result = word_wrap("hello\nworld", 100);
        assert_eq!(result, vec!["hello", "world"]);
    }
}
