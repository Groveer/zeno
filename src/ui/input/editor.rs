//! Text editor core — low-level text manipulation primitives.
//!
//! This module provides standalone helper functions for text editing operations
//! used by `InputState`. The long-term goal is to extract a full `Editor` struct
//! here that owns the text buffer, cursor, and editing state, leaving `InputState`
//! as an orchestrator of editor + completion + history + images.

use unicode_segmentation::UnicodeSegmentation;

/// Find the nearest UTF-8 character boundary at or before `offset` in `s`.
pub fn snap_to_char_boundary(s: &str, offset: usize) -> usize {
    let offset = offset.min(s.len());
    if s.is_char_boundary(offset) {
        offset
    } else {
        // Walk backwards to the previous char boundary.
        let mut idx = offset;
        while idx > 0 && !s.is_char_boundary(idx) {
            idx -= 1;
        }
        idx
    }
}

/// Find the boundary at the start of the previous grapheme cluster before `cursor` in `s`.
/// Returns 0 if cursor is at the start. Grapheme-safe — never lands inside a
/// multi-codepoint emoji (ZWJ sequences, flags, skin-tone modifiers).
pub fn prev_grapheme_boundary(s: &str, cursor: usize) -> usize {
    if cursor == 0 {
        return 0;
    }
    // Iterate grapheme clusters; the last one that ends at/before `cursor`
    // is our target.
    let mut prev_end = 0usize;
    for (start, _grapheme) in s.grapheme_indices(true) {
        if start >= cursor {
            break;
        }
        prev_end = start;
    }
    prev_end
}

/// Find the boundary at the end of the current or next grapheme cluster after `cursor` in `s`.
/// Returns `s.len()` if cursor is at the end. Grapheme-safe — never lands inside a
/// multi-codepoint emoji.
pub fn next_grapheme_boundary(s: &str, cursor: usize) -> usize {
    let cursor = cursor.min(s.len());
    if cursor >= s.len() {
        return s.len();
    }
    for (start, grapheme) in s.grapheme_indices(true) {
        let end = start + grapheme.len();
        if end > cursor {
            return end;
        }
    }
    s.len()
}
