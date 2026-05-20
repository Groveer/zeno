//! Paste handling for the TUI input.
//!
//! Manages bracketed terminal paste events and text insertion.
//! When the terminal sends a bracketed paste event (enabled via
//! `EnableBracketedPaste` in init_terminal()), the pasted text
//! is inserted into the input buffer as-is, preserving newlines.
//!
//! ## Module Organization
//!
//! - `clipboard.rs` — clipboard image reading (Alt+V paste from system clipboard)
//! - `paste.rs` — bracketed paste event handling (terminal-emitted paste)
//!
//! ## Future
//!
//! - Paste preview: show what will be pasted before inserting
//! - Paste size warning: warn if pasting very large text blocks
//! - Multi-line paste handling: auto-indent pasted code blocks
#![allow(dead_code)]

/// Max characters accepted in a single paste event.
/// Prevents accidental paste of enormous text blocks.
const MAX_PASTE_LENGTH: usize = 100_000;

/// Result of processing a paste event.
#[derive(Debug)]
pub enum PasteResult {
    /// Text was inserted successfully.
    Inserted,
    /// Text was too long and was rejected.
    TooLong { length: usize, max: usize },
    /// No text content to paste.
    Empty,
}

/// Process a bracketed paste event and produce text suitable for insertion.
///
/// Returns `None` if the paste is empty or exceeds the maximum length.
pub fn process_paste(text: &str) -> PasteResult {
    if text.is_empty() {
        return PasteResult::Empty;
    }

    if text.len() > MAX_PASTE_LENGTH {
        return PasteResult::TooLong {
            length: text.len(),
            max: MAX_PASTE_LENGTH,
        };
    }

    PasteResult::Inserted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_process_empty_paste() {
        assert!(matches!(process_paste(""), PasteResult::Empty));
    }

    #[test]
    fn test_process_normal_paste() {
        assert!(matches!(
            process_paste("hello world"),
            PasteResult::Inserted
        ));
    }

    #[test]
    fn test_process_multi_line_paste() {
        assert!(matches!(
            process_paste("line1\nline2\nline3"),
            PasteResult::Inserted
        ));
    }

    #[test]
    fn test_process_huge_paste() {
        let huge = "x".repeat(MAX_PASTE_LENGTH + 1);
        match process_paste(&huge) {
            PasteResult::TooLong { length, max } => {
                assert_eq!(length, MAX_PASTE_LENGTH + 1);
                assert_eq!(max, MAX_PASTE_LENGTH);
            }
            _ => panic!("expected TooLong"),
        }
    }
}
