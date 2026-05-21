//! Incremental streaming row builder for live text output.
//!
//! Wraps text into visual rows of at most `width` display columns,
//! processing input in fragments (streaming). Useful for rendering
//! real-time output as characters arrive, without waiting for the
//! full text.
//!
//! Based on the same approach as Codex's `live_wrap::RowBuilder`.
//!
//! # Status
//!
//! This module is forward-looking — the types are not yet wired into
//! any rendering pipeline. They are provided as a building block for
//! future streaming output support.

#![allow(dead_code)]

use std::collections::VecDeque;

use unicode_width::UnicodeWidthStr;

use super::prefix;

/// A single visual row produced by [`RowBuilder`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub text: String,
    /// True if this row ends with an explicit line break
    /// (as opposed to a hard wrap).
    pub explicit_break: bool,
}

impl Row {
    pub fn width(&self) -> usize {
        self.text.width()
    }
}

/// Incrementally wraps input text into visual rows of at most `width` cells.
///
/// Processes text in fragments via [`push_fragment`](RowBuilder::push_fragment),
/// maintaining internal buffer state so that streaming input produces properly
/// wrapped output without requiring the full text upfront.
pub struct RowBuilder {
    target_width: usize,
    /// Buffer for the current logical line (until a '\n' is seen).
    current_line: String,
    /// Output rows built so far for the current logical line and previous ones.
    rows: VecDeque<Row>,
}

impl RowBuilder {
    pub fn new(target_width: usize) -> Self {
        Self {
            target_width: target_width.max(1),
            current_line: String::new(),
            rows: VecDeque::new(),
        }
    }

    pub fn width(&self) -> usize {
        self.target_width
    }

    pub fn set_width(&mut self, width: usize) {
        self.target_width = width.max(1);
        // Rewrap everything we have
        let mut all = String::new();
        for row in self.rows.drain(..) {
            all.push_str(&row.text);
            if row.explicit_break {
                all.push('\n');
            }
        }
        all.push_str(&self.current_line);
        self.current_line.clear();
        self.push_fragment(&all);
    }

    /// Push an input fragment. May contain newlines.
    pub fn push_fragment(&mut self, fragment: &str) {
        if fragment.is_empty() {
            return;
        }
        let mut start = 0usize;
        for (i, ch) in fragment.char_indices() {
            if ch == '\n' {
                // Flush anything pending before the newline.
                if start < i {
                    self.current_line.push_str(&fragment[start..i]);
                }
                self.flush_current_line(/*explicit_break*/ true);
                start = i + ch.len_utf8();
            }
        }
        if start < fragment.len() {
            self.current_line.push_str(&fragment[start..]);
            self.wrap_current_line();
        }
    }

    /// Mark the end of the current logical line (equivalent to pushing a '\n').
    pub fn end_line(&mut self) {
        self.flush_current_line(/*explicit_break*/ true);
    }

    /// Return a snapshot of produced rows (non-draining).
    pub fn rows(&self) -> &VecDeque<Row> {
        &self.rows
    }

    /// Rows suitable for display, including the current partial line if any.
    pub fn display_rows(&self) -> Vec<Row> {
        let mut out: Vec<Row> = self.rows.iter().cloned().collect();
        if !self.current_line.is_empty() {
            out.push(Row {
                text: self.current_line.clone(),
                explicit_break: false,
            });
        }
        out
    }

    /// Drain the oldest rows that exceed `max_keep` display rows (including the
    /// current partial line, if any). Returns the drained rows in order.
    pub fn drain_commit_ready(&mut self, max_keep: usize) -> Vec<Row> {
        let display_count = self.rows.len() + if self.current_line.is_empty() { 0 } else { 1 };
        if display_count <= max_keep {
            return Vec::new();
        }
        let to_commit = display_count - max_keep;
        let commit_count = to_commit.min(self.rows.len());
        let mut drained = Vec::with_capacity(commit_count);
        for _ in 0..commit_count {
            drained.push(self.rows.pop_front().unwrap());
        }
        drained
    }

    fn flush_current_line(&mut self, explicit_break: bool) {
        self.wrap_current_line();
        if explicit_break {
            if self.current_line.is_empty() {
                self.rows.push_back(Row {
                    text: String::new(),
                    explicit_break: true,
                });
            } else {
                let mut s = String::new();
                std::mem::swap(&mut s, &mut self.current_line);
                self.rows.push_back(Row {
                    text: s,
                    explicit_break: true,
                });
            }
        }
        self.current_line.clear();
    }

    fn wrap_current_line(&mut self) {
        loop {
            if self.current_line.is_empty() {
                break;
            }
            let (prefix, suffix, taken) =
                prefix::take_prefix_by_width(&self.current_line, self.target_width);
            let suffix_start = self.current_line.len() - suffix.len();
            if taken == 0 {
                // Avoid infinite loop on pathological inputs; take one scalar.
                if let Some((i, ch)) = self.current_line.char_indices().next() {
                    let len = i + ch.len_utf8();
                    let p = self.current_line[..len].to_string();
                    self.rows.push_back(Row {
                        text: p,
                        explicit_break: false,
                    });
                    self.current_line = self.current_line[len..].to_string();
                    continue;
                }
                break;
            }
            if suffix.is_empty() {
                // Fits entirely; keep in buffer.
                break;
            } else {
                self.rows.push_back(Row {
                    text: prefix.to_string(),
                    explicit_break: false,
                });
                self.current_line = self.current_line[suffix_start..].to_string();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_row_builder_ascii() {
        let mut rb = RowBuilder::new(10);
        rb.push_fragment("hello whirl this is a test");
        assert_eq!(rb.display_rows().len(), 3);
        for row in rb.display_rows() {
            assert!(row.width() <= 10, "Row '{}' exceeds width 10", row.text);
        }
    }

    #[test]
    fn test_row_builder_emoji() {
        // 😀 is width 2. Two emoji + space = 5, at width 6 only 1 column remains
        // but CJK "你" needs 2 columns, so "你好" stays in the buffer (not flushed).
        let mut rb = RowBuilder::new(6);
        rb.push_fragment("😀😀 你好");
        let rows: Vec<Row> = rb.rows().iter().cloned().collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "😀😀 ");
    }

    #[test]
    fn test_row_builder_newline() {
        let mut rb = RowBuilder::new(10);
        rb.push_fragment("hello\nworld");
        let rows = rb.display_rows();
        assert_eq!(rows[0].text, "hello");
        assert!(rows[0].explicit_break);
        assert!(rows[1].text.contains("world"));
    }

    #[test]
    fn test_row_builder_empty() {
        let mut rb = RowBuilder::new(10);
        assert!(rb.rows().is_empty());
        rb.push_fragment("");
        assert!(rb.rows().is_empty());
    }

    #[test]
    fn test_row_builder_drain() {
        let mut rb = RowBuilder::new(5);
        rb.push_fragment("abcdefghij");
        let drained = rb.drain_commit_ready(1);
        assert!(!drained.is_empty());
    }
}
