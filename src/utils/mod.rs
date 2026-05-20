//! Shared utility functions.

pub mod diff;
pub mod time;

/// Return the terminal display width of a single character.
///
/// Handles three cases that `unicode-width` gets wrong:
/// - **Emoji presentation** (base char + VS16): `unicode-width` reports width 1
///   for BMP emoji like  (U+F040), but terminals render them as width 2.
/// - **Private Use Area** (U+E000–U+F8FF, U+F0000–U+FFFFD, U+100000–U+10FFFD):
///   `unicode-width` reports width 1, but Nerd Font icons at these codepoints
///   are rendered as width 2 by modern terminals.
pub fn char_width(ch: char, next: Option<char>) -> usize {
    // Emoji presentation sequence: base char + VS16 (U+FE0F) → width 2
    if next == Some('\u{FE0F}') {
        return 2;
    }
    // Orphan VS16 → width 0
    if ch == '\u{FE0F}' {
        return 0;
    }
    // Private Use Area characters (Nerd Font icons) → width 2
    if matches!(ch as u32, 0xE000..=0xF8FF | 0xF0000..=0xFFFFD | 0x100000..=0x10FFFD) {
        return 2;
    }
    // Image marker (U+FFFC) — rendered as "[img]" (5 display columns)
    if ch == '\u{FFFC}' {
        return 5;
    }
    // Fall back to unicode-width for all other characters
    unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0)
}

/// Compute the terminal display width of a string, accounting for emoji
/// presentation sequences and Nerd Font PUA icons.
///
/// See [`char_width`] for details on the correction logic.
pub fn display_width(s: &str) -> usize {
    let chars: Vec<char> = s.chars().collect();
    let mut width = 0usize;
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        let next = chars.get(i + 1).copied();
        width += char_width(ch, next);
        // Skip VS16 if it was consumed as part of an emoji sequence
        if next == Some('\u{FE0F}') {
            i += 2;
        } else {
            i += 1;
        }
    }
    width
}

/// Truncate a string for display, safe for multi-byte UTF-8.
/// Returns a `Cow` that borrows when no truncation is needed.
pub fn truncate(s: &str, max_chars: usize) -> std::borrow::Cow<'_, str> {
    if s.chars().count() <= max_chars {
        std::borrow::Cow::Borrowed(s)
    } else {
        let end = s.floor_char_boundary(max_chars);
        std::borrow::Cow::Owned(format!("{}…", &s[..end]))
    }
}
