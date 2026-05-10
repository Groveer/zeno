//! Shared utility functions.

pub mod diff;

/// Compute the terminal display width of a string, accounting for emoji
/// presentation sequences.
///
/// The `unicode-width` crate reports width 1 for BMP emoji like ✏️
/// (U+270F + U+FE0F), but terminals render them as width 2 in emoji
/// presentation mode. This function detects base characters followed
/// by VS16 (U+FE0F) and reports width 2 for the pair.
pub fn display_width(s: &str) -> usize {
    let chars: Vec<char> = s.chars().collect();
    let mut width = 0usize;
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        // Check for emoji presentation: base char + VS16 (U+FE0F)
        if i + 1 < chars.len() && chars[i + 1] == '\u{FE0F}' {
            // Base char with VS16: treat as emoji (width 2), skip VS16
            width += 2;
            i += 2;
        } else if ch == '\u{FE0F}' {
            // Orphan VS16 (shouldn't happen in well-formed text): width 0
            i += 1;
        } else {
            width += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            i += 1;
        }
    }
    width
}
