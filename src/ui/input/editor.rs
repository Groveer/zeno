//! Text editor core — low-level text manipulation primitives.
//!
//! This module provides standalone helper functions for text editing operations
//! used by `InputState`. The long-term goal is to extract a full `Editor` struct
//! here that owns the text buffer, cursor, and editing state, leaving `InputState`
//! as an orchestrator of editor + completion + history + images.

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

/// Find the previous character boundary before `cursor` in `s`.
/// Returns 0 if cursor is at the start.
pub fn prev_char_boundary(s: &str, cursor: usize) -> usize {
    let mut idx = cursor.saturating_sub(1);
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// Find the next character boundary after `cursor` in `s`.
/// Returns `s.len()` if cursor is at the end.
pub fn next_char_boundary(s: &str, cursor: usize) -> usize {
    let mut idx = cursor + 1;
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx.min(s.len())
}
