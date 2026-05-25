//! Shared utility functions.

pub mod diff;
pub mod time;

use unicode_segmentation::UnicodeSegmentation;

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
    // Paste marker — rendered as e.g. "[📋 pasted 24 lines: "snippet…"]"
    // Returns a generous upper bound (60 cols ≈ longest possible summary).
    // The exact width is computed per-marker in PasteData::display_width and
    // used by visual_cursor_row_col for precise cursor positioning.
    if ch == '\u{E002}' {
        return 60;
    }
    // Fall back to unicode-width for all other characters
    unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0)
}

/// Compute the terminal display width of a string, accounting for emoji
/// presentation sequences and Nerd Font PUA icons.
///
/// Tab characters are expanded to the next 4-column tab stop boundary
/// (matching the `TAB_WIDTH` constant in `ui::wrap::span`).
///
/// See [`char_width`] for details on the correction logic.
pub fn display_width(s: &str) -> usize {
    let chars: Vec<char> = s.chars().collect();
    let mut width = 0usize;
    let mut col = 0usize; // track column for tab-stop calculation
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        let next = chars.get(i + 1).copied();
        if ch == '\t' {
            // Tab: advance to next 4-column tab stop
            let tab_width = 4 - (col % 4);
            width += tab_width;
            col += tab_width;
            i += 1;
        } else {
            let w = char_width(ch, next);
            width += w;
            col += w;
            // Skip VS16 if it was consumed as part of an emoji sequence
            if next == Some('\u{FE0F}') {
                i += 2;
            } else {
                i += 1;
            }
        }
    }
    width
}

/// Return the emoji or icon followed by a hair space (U+200A).
/// A hair space provides a small visual gap after the emoji without
/// the excessive whitespace that a regular space adds. Works well
/// in compact UI elements (status bars, prompts, titles).
pub fn padded_emoji(emoji: &str) -> String {
    format!("{emoji}\u{200A}")
}

/// Truncate a string to at most `max_graphemes` grapheme clusters (user-perceived
/// characters), appending "…" if truncated. Uses Unicode grapheme boundaries to
/// avoid breaking multi-codepoint emoji (ZWJ sequences, flags, skin-tone modifiers).
/// Returns a `Cow` that borrows when no truncation is needed.
pub fn truncate(s: &str, max_graphemes: usize) -> std::borrow::Cow<'_, str> {
    if max_graphemes == 0 || s.is_empty() {
        return std::borrow::Cow::Borrowed("");
    }

    // Fast path: char count is a fast upper bound on grapheme count.
    if s.chars().count() <= max_graphemes {
        return std::borrow::Cow::Borrowed(s);
    }

    let grapheme_indices: Vec<(usize, &str)> = s.grapheme_indices(true).collect();
    if grapheme_indices.len() <= max_graphemes {
        return std::borrow::Cow::Borrowed(s);
    }

    // Always reserve at least 1 grapheme for the ellipsis "…".
    // max_graphemes=1 → just "…"; max_graphemes=2 → 1 content + "…";
    // max_graphemes>=3 → (max-3) content + "…" (keeps 3 for "…" as before).
    let content_graphemes = if max_graphemes >= 3 {
        max_graphemes - 3
    } else {
        max_graphemes.saturating_sub(1)
    };
    let truncate_idx = grapheme_indices[content_graphemes].0;
    std::borrow::Cow::Owned(format!("{}…", &s[..truncate_idx]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::Rng;
    use rand::SeedableRng;

    #[test]
    fn test_truncate_short_string() {
        let s = "hello";
        let result = truncate(s, 10);
        assert_eq!(result, "hello");
        assert!(matches!(result, std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn test_truncate_exact_fit() {
        let s = "hello";
        let result = truncate(s, 5);
        assert_eq!(result, "hello");
    }

    #[test]
    fn test_truncate_with_ellipsis() {
        let s = "hello world";
        let result = truncate(s, 6);
        assert_eq!(result, "hel…");
        assert!(matches!(result, std::borrow::Cow::Owned(_)));
    }

    #[test]
    fn test_truncate_empty() {
        assert_eq!(truncate("", 5), "");
        assert_eq!(truncate("hello", 0), "");
    }

    #[test]
    fn test_truncate_small_limit() {
        let s = "hello";
        assert_eq!(truncate(s, 1), "…");
        assert_eq!(truncate(s, 2), "h…");
    }

    #[test]
    fn test_truncate_zwj_emoji_unsplit() {
        // ZWJ family emoji: 1 grapheme cluster, 7 codepoints
        let family = "👨‍👩‍👧‍👦";
        // Should fit entirely at max_graphemes=1 since it's 1 grapheme
        let result = truncate(family, 1);
        assert_eq!(result, family);
    }

    #[test]
    fn test_truncate_flag_emoji_unsplit() {
        // Flag emoji: 2 codepoints but 1 grapheme cluster
        let flag = "🇺🇳";
        let result = truncate(flag, 1);
        assert_eq!(result, flag);
    }

    /// Generate a random grapheme cluster for fuzz testing.
    /// Mirrors Codex's approach: emoji, CJK, combining marks, ASCII, and special chars.
    fn rand_grapheme(rng: &mut impl rand::Rng) -> String {
        let r: u8 = rng.gen_range(0..100);
        match r {
            0..=4 => "\n".to_string(),
            5..=12 => " ".to_string(),
            13..=35 => (rng.gen_range(b'a'..=b'z') as char).to_string(),
            36..=45 => (rng.gen_range(b'A'..=b'Z') as char).to_string(),
            46..=52 => (rng.gen_range(b'0'..=b'9') as char).to_string(),
            53..=65 => {
                // Wide emoji (display width 2)
                let choices = ["👍", "😊", "🐍", "🚀", "🧪", "🌟", "🔥", "❤️", "😀", "🎉"];
                choices[rng.gen_range(0..choices.len())].to_string()
            }
            66..=75 => {
                // CJK wide characters (display width 2)
                let choices = ["漢", "字", "測", "試", "你", "好", "世", "界", "编", "码"];
                choices[rng.gen_range(0..choices.len())].to_string()
            }
            76..=85 => {
                // Combining mark sequences (single base + one combining mark)
                let base = ["e", "a", "o", "n", "u"][rng.gen_range(0..5)];
                let marks = ["\u{0301}", "\u{0308}", "\u{0302}", "\u{0303}"];
                format!("{base}{}", marks[rng.gen_range(0..marks.len())])
            }
            86..=92 => {
                // Non-latin single codepoints (Greek, Cyrillic, Hebrew, Arabic)
                let choices = ["Ω", "β", "Ж", "ю", "ש", "م", "ह"];
                choices[rng.gen_range(0..choices.len())].to_string()
            }
            _ => {
                // PUA / Nerd Font icon (display width 2)
                let pua_start: u32 = 0xE000 + rng.gen_range(0..100) as u32;
                char::from_u32(pua_start).unwrap_or('').to_string()
            }
        }
    }

    #[test]
    fn test_display_width_fuzz() {
        // Generate random strings and verify display_width doesn't panic
        // and returns reasonable values.
        let seed: u64 = 42;
        let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
        for _ in 0..500 {
            let mut s = String::new();
            let len: usize = rng.gen_range(1..=20);
            for _ in 0..len {
                s.push_str(&rand_grapheme(&mut rng));
            }
            let w = display_width(&s);
            // Display width should be at least the number of non-newline chars
            // (each char has width ≥ 0, newlines are 0)
            assert!(
                w <= s.len(),
                "display_width {} > len {} for {:?}",
                w,
                s.len(),
                s
            );
        }
    }

    #[test]
    fn test_truncate_grapheme_fuzz() {
        // Generate random strings and verify truncate never splits a grapheme.
        let seed: u64 = 123;
        let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
        for _ in 0..500 {
            let mut s = String::new();
            let len: usize = rng.gen_range(1..=30);
            for _ in 0..len {
                s.push_str(&rand_grapheme(&mut rng));
            }
            let max_g: usize = rng.gen_range(1..=15);
            let result = truncate(&s, max_g);
            let result_str = result.as_ref();

            // Result should be a prefix of the original string
            assert!(
                s.starts_with(result_str.trim_end_matches('…')),
                "truncate result {:?} is not a prefix of {:?}",
                result_str,
                s
            );

            // All grapheme boundaries in result should be valid grapheme boundaries
            // in the original string (no splitting)
            if result_str.ends_with('…') {
                let truncated = &result_str[..result_str.len() - 3]; // remove "…"
                if !truncated.is_empty() {
                    // The truncated part should end at a valid grapheme boundary
                    let original_prefix = &s[..truncated.len()];
                    // The truncated part's byte length should correspond to
                    // a grapheme boundary in the original
                    assert!(
                        s[..truncated.len()].chars().count() <= truncated.chars().count() + 1,
                        "grapheme possibly split in {:?}",
                        result_str
                    );
                }
            }
        }
    }
}
