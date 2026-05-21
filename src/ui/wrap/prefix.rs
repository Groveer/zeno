//! Low-level width-based text splitting (private helpers).
//!
//! These functions are used internally by the wrapping layers and are not
//! part of the public API.

use crate::utils::char_width;

/// Take a prefix of `text` whose visible display width is at most `max_cols`.
/// Returns `(prefix, suffix, prefix_width)`.
///
/// Handles emoji, CJK, VS16, and PUA icons via [`char_width`].
/// Single characters wider than `max_cols` are still taken (forced break).
pub fn take_prefix_by_width(text: &str, max_cols: usize) -> (&str, &str, usize) {
    if max_cols == 0 || text.is_empty() {
        return ("", text, 0);
    }
    let mut cols = 0usize;
    let mut end_idx = 0usize;
    for (i, ch) in text.char_indices() {
        let next = text[i + ch.len_utf8()..].chars().next();
        let ch_width = char_width(ch, next);
        if cols.saturating_add(ch_width) > max_cols {
            break;
        }
        cols += ch_width;
        end_idx = i + ch.len_utf8();
        if cols == max_cols {
            break;
        }
    }
    let prefix = &text[..end_idx];
    let suffix = &text[end_idx..];
    (prefix, suffix, cols)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_take_prefix_ascii() {
        let (p, s, w) = take_prefix_by_width("hello world", 5);
        assert_eq!(p, "hello");
        assert_eq!(s, " world");
        assert_eq!(w, 5);
    }

    #[test]
    fn test_take_prefix_emoji() {
        // 😀 is width 2
        let (p, s, w) = take_prefix_by_width("😀😀", 3);
        assert_eq!(p, "😀");
        assert_eq!(w, 2);
        assert_eq!(s, "😀");
    }

    #[test]
    fn test_take_prefix_exact_fit() {
        let (p, s, w) = take_prefix_by_width("abcd", 4);
        assert_eq!(p, "abcd");
        assert_eq!(s, "");
        assert_eq!(w, 4);
    }

    #[test]
    fn test_take_prefix_empty() {
        let (p, s, w) = take_prefix_by_width("", 10);
        assert_eq!(p, "");
        assert_eq!(s, "");
        assert_eq!(w, 0);
    }

    #[test]
    fn test_take_prefix_zero_width() {
        let (p, s, w) = take_prefix_by_width("hello", 0);
        assert_eq!(p, "");
        assert_eq!(s, "hello");
        assert_eq!(w, 0);
    }
}
