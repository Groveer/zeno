//! Shared utility functions.

pub mod diff;

/// Return the terminal display width of a single character.
///
/// Handles three cases that `unicode-width` gets wrong:
/// - **Emoji presentation** (base char + VS16): `unicode-width` reports width 1
///   for BMP emoji like ✏️ (U+270F), but terminals render them as width 2.
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
