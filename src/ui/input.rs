//! Multi-line input widget with horizontal scrolling and command/path completion popup.
//!
//! Supports cursor movement, backspace, Shift+Enter for newlines, Enter to submit,
//! Ctrl+D submission, and a popup completion menu for slash commands and file paths.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph},
};
use std::borrow::Cow;
use std::path::{Path, PathBuf};

use crate::config::paths;

use super::theme;

/// All slash commands supported by the TUI, with short aliases first.
const COMMANDS: &[&str] = &[
    "/clear", "/compact", "/cost", "/exit", "/goal", "/help", "/mcp", "/memory", "/model", "/quit",
    "/resume", "/search", "/skills", "/tools",
];

/// Maximum number of items shown in the completion popup.
const MAX_POPUP_ITEMS: usize = 5;

/// Maximum number of history entries to persist to disk.
const MAX_PERSISTED_HISTORY: usize = 2000;

// ── Completion popup state ──

/// Completion type to distinguish between command and path completions.
#[derive(Debug, Clone, PartialEq)]
enum CompletionType {
    /// Slash command completion (e.g., "/he" -> "/help")
    Command,
    /// File path completion (e.g., "src/" -> ["src/main.rs", "src/lib.rs"])
    Path {
        /// The token prefix being completed (e.g., "src/")
        prefix: String,
        /// Start byte position of the path token in the input text
        start: usize,
    },
}

/// State for the command/path completion popup.
pub struct CompletionPopup {
    /// Matching items for the current prefix (commands or paths).
    pub matches: Vec<String>,
    /// Currently selected index (0-based).
    pub selected: usize,
    /// Scroll offset for navigating beyond MAX_POPUP_ITEMS.
    pub scroll: usize,
    /// Type of completion (command or path).
    completion_type: CompletionType,
}

impl CompletionPopup {
    fn new_command(matches: Vec<String>) -> Self {
        Self {
            matches,
            selected: 0,
            scroll: 0,
            completion_type: CompletionType::Command,
        }
    }

    fn new_path(matches: Vec<String>, prefix: String, start: usize) -> Self {
        Self {
            matches,
            selected: 0,
            scroll: 0,
            completion_type: CompletionType::Path { prefix, start },
        }
    }

    /// Move selection up.
    fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
            if self.selected < self.scroll {
                self.scroll = self.selected;
            }
        }
    }

    /// Move selection down.
    fn move_down(&mut self) {
        if self.selected + 1 < self.matches.len() {
            self.selected += 1;
            if self.selected >= self.scroll + MAX_POPUP_ITEMS {
                self.scroll = self.selected - MAX_POPUP_ITEMS + 1;
            }
        }
    }

    /// Get the currently selected item.
    fn selected_item(&self) -> Option<&str> {
        self.matches.get(self.selected).map(|s| s.as_str())
    }

    /// Visible slice of matches for rendering.
    fn visible_slice(&self) -> &[String] {
        let end = (self.scroll + MAX_POPUP_ITEMS).min(self.matches.len());
        &self.matches[self.scroll..end]
    }

    /// Get the completion type.
    fn completion_type(&self) -> &CompletionType {
        &self.completion_type
    }
}

/// The input widget state.
pub struct InputState {
    /// Full text buffer (may contain \n).
    pub text: String,
    /// Byte position of cursor within `text`.
    pub cursor: usize,
    /// Set to true when the user submits (Enter without Shift/Alt).
    pub submitted: bool,
    /// Whether the widget is active (accepting input).
    pub active: bool,
    /// Active completion popup (None when not showing).
    pub popup: Option<CompletionPopup>,
    /// History of previously submitted inputs.
    input_history: Vec<String>,
    /// Current position in history (0 = newest, len = beyond oldest).
    /// When None, user is editing a new (unsaved) line.
    history_index: Option<usize>,
    /// Stashed draft when user first pressed Up away from the current line.
    draft: Option<String>,
}

impl InputState {
    pub fn new() -> Self {
        let history = load_history();
        Self {
            text: String::new(),
            cursor: 0,
            submitted: false,
            active: true,
            popup: None,
            input_history: history,
            history_index: None,
            draft: None,
        }
    }

    /// Process a key event. Returns true if the key was consumed.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        // If popup is open, intercept keys for popup navigation
        if self.popup.is_some() {
            return self.handle_popup_key(key);
        }

        match key {
            // Submit: plain Enter
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.submitted = true;
                true
            }
            // Newline: Shift+Enter or Alt+Enter
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::SHIFT,
                ..
            }
            | KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::ALT,
                ..
            } => {
                self.insert_char('\n');
                self.update_popup();
                true
            }
            KeyEvent {
                code: KeyCode::Char('d'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } if self.text.is_empty() => {
                self.submitted = true;
                self.text = "/exit".into();
                true
            }
            // Tab: open popup or cycle
            KeyEvent {
                code: KeyCode::Tab,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.open_or_cycle_popup();
                true
            }
            // Shift+Tab: open popup or cycle backwards
            KeyEvent {
                code: KeyCode::BackTab,
                ..
            } => {
                self.open_or_cycle_popup_back();
                true
            }
            // Navigation
            KeyEvent {
                code: KeyCode::Left,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                if self.cursor > 0 {
                    self.cursor = self.prev_char_boundary();
                }
                self.update_popup();
                true
            }
            KeyEvent {
                code: KeyCode::Right,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                if self.cursor < self.text.len() {
                    self.cursor = self.next_char_boundary();
                }
                self.update_popup();
                true
            }
            // History navigation: Up = older, Down = newer
            KeyEvent {
                code: KeyCode::Up,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                // If multi-line text, move cursor up one line.
                // If already on the first line, fall through to history navigation.
                if self.text.contains('\n') && self.move_cursor_up() {
                    return true;
                }
                if self.input_history.is_empty() {
                    return false;
                }
                // First press: stash current draft and move to newest history
                if self.history_index.is_none() {
                    let draft = if self.text.is_empty() {
                        None
                    } else {
                        Some(self.text.clone())
                    };
                    self.draft = draft;
                    self.history_index = Some(0);
                } else if let Some(idx) = self.history_index {
                    // Move towards older entries
                    if idx + 1 < self.input_history.len() {
                        self.history_index = Some(idx + 1);
                    }
                }
                if let Some(idx) = self.history_index {
                    self.text = self.input_history[idx].clone();
                    self.cursor = self.text.len();
                }
                self.update_popup();
                true
            }
            KeyEvent {
                code: KeyCode::Down,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                // If multi-line text, move cursor down one line.
                // If already on the last line, fall through to history navigation.
                if self.text.contains('\n') && self.move_cursor_down() {
                    return true;
                }
                match self.history_index {
                    None => false, // Not in history, let app scroll output
                    Some(0) => {
                        // Back to draft / new line
                        self.history_index = None;
                        self.text = self.draft.take().unwrap_or_default();
                        self.cursor = self.text.len();
                        self.update_popup();
                        true
                    }
                    Some(idx) => {
                        self.history_index = Some(idx - 1);
                        self.text = self.input_history[idx - 1].clone();
                        self.cursor = self.text.len();
                        self.update_popup();
                        true
                    }
                }
            }
            KeyEvent {
                code: KeyCode::Home,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.move_cursor_to_line_start();
                self.update_popup();
                true
            }
            KeyEvent {
                code: KeyCode::End,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.move_cursor_to_line_end();
                self.update_popup();
                true
            }
            // Editing
            KeyEvent {
                code: KeyCode::Backspace,
                ..
            } => {
                if self.cursor > 0 {
                    let prev = self.prev_char_boundary();
                    self.text.drain(prev..self.cursor);
                    self.cursor = prev;
                }
                self.update_popup();
                true
            }
            // Ctrl+W: delete previous word (backwards until word boundary)
            KeyEvent {
                code: KeyCode::Char('w'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.delete_word_backwards();
                self.update_popup();
                true
            }
            KeyEvent {
                code: KeyCode::Delete,
                ..
            } => {
                if self.cursor < self.text.len() {
                    let next = self.next_char_boundary();
                    self.text.drain(self.cursor..next);
                }
                self.update_popup();
                true
            }
            // Text input
            KeyEvent {
                code: KeyCode::Char(c),
                modifiers: KeyModifiers::NONE | KeyModifiers::SHIFT,
                ..
            } => {
                self.insert_char(c);
                self.update_popup();
                true
            }
            _ => false,
        }
    }

    /// Clear after submission. Saves the current text to input history.
    pub fn reset(&mut self) {
        // Save to history (skip empty, slash commands, and duplicates of the most recent entry)
        let trimmed = self.text.trim().to_string();
        if !trimmed.is_empty()
            && !trimmed.starts_with('/')
            && self.input_history.first().map(|s| s.as_str()) != Some(&trimmed)
        {
            self.input_history.insert(0, trimmed);
        }

        self.text.clear();
        self.cursor = 0;
        self.submitted = false;
        self.popup = None;
        self.history_index = None;
        self.draft = None;

        // Persist history to disk after each submission
        save_history(&self.input_history);
    }

    /// Reset input state without saving to history.
    /// Used for ask_user responses and other non-user-initiated inputs
    /// that should not pollute the history navigation (Up/Down).
    pub fn reset_without_history(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.submitted = false;
        self.popup = None;
        self.history_index = None;
        self.draft = None;
    }

    // ── Multi-line cursor helpers ──

    /// Return (row, col_byte) of the cursor position in the text.
    /// col_byte is the byte offset within the current line.
    ///
    /// Panics if self.cursor is not on a UTF-8 char boundary (should never happen).
    fn cursor_row_col(&self) -> (usize, usize) {
        let cursor = self.cursor.min(self.text.len());
        // Defensive: snap cursor to the nearest char boundary if somehow misaligned.
        let cursor = snap_to_char_boundary(&self.text, cursor);
        let before = &self.text[..cursor];
        let row = before.chars().filter(|&c| c == '\n').count();
        let last_newline = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
        let col = cursor - last_newline;
        (row, col)
    }

    /// Return the byte offset of the start of the given row (0-indexed).
    fn line_start_byte(&self, row: usize) -> usize {
        let mut current_row = 0;
        let mut byte = 0;
        for (i, c) in self.text.char_indices() {
            if current_row == row {
                return i;
            }
            if c == '\n' {
                current_row += 1;
            }
            byte = i + c.len_utf8();
        }
        if current_row == row {
            byte
        } else {
            self.text.len()
        }
    }

    /// Return the byte offset just past the end of the given row (0-indexed),
    /// which is either the \n byte + 1 or the end of text.
    fn line_end_byte(&self, row: usize) -> usize {
        let mut current_row = 0;
        for (i, c) in self.text.char_indices() {
            if c == '\n' && current_row == row {
                return i; // position of \n (not past it)
            }
            if c == '\n' {
                current_row += 1;
            }
        }
        self.text.len()
    }

    /// Return the content of a given row (0-indexed), without the trailing \n.
    fn line_content(&self, row: usize) -> &str {
        let start = self.line_start_byte(row);
        let end = self.line_end_byte(row);
        &self.text[start..end]
    }

    /// Move cursor up one line (multi-line navigation).
    /// Returns false if the cursor was already on the first line (caller should fall through).
    fn move_cursor_up(&mut self) -> bool {
        let (row, col_byte) = self.cursor_row_col();
        if row == 0 {
            return false;
        }
        let prev_line_start = self.line_start_byte(row - 1);
        let prev_line_end = self.line_end_byte(row - 1);
        let prev_line_len = prev_line_end - prev_line_start;
        let target = prev_line_start + col_byte.min(prev_line_len);
        self.cursor = snap_to_char_boundary(&self.text, target);
        true
    }

    /// Move cursor down one line (multi-line navigation).
    /// Returns false if the cursor was already on the last line (caller should fall through).
    fn move_cursor_down(&mut self) -> bool {
        let (row, col_byte) = self.cursor_row_col();
        let total_rows = self.line_count();
        if row + 1 >= total_rows {
            return false;
        }
        let next_line_start = self.line_start_byte(row + 1);
        let next_line_end = self.line_end_byte(row + 1);
        let next_line_len = next_line_end - next_line_start;
        let target = next_line_start + col_byte.min(next_line_len);
        self.cursor = snap_to_char_boundary(&self.text, target);
        true
    }

    /// Move cursor to the start of the current line (Home).
    fn move_cursor_to_line_start(&mut self) {
        let (row, _) = self.cursor_row_col();
        self.cursor = self.line_start_byte(row);
    }

    /// Move cursor to the end of the current line (End).
    fn move_cursor_to_line_end(&mut self) {
        let (row, _) = self.cursor_row_col();
        self.cursor = self.line_end_byte(row);
    }

    // ── Popup logic ──

    /// Open the popup or cycle selection if already open.
    fn open_or_cycle_popup(&mut self) {
        if self.popup.is_some() {
            // Cycle: move selection down
            if let Some(ref mut popup) = self.popup {
                popup.move_down();
            }
            return;
        }

        // Command completion first (text starts with /)
        if self.text.starts_with('/') && self.cursor == self.text.len() {
            let matches = Self::find_command_matches(&self.text);
            if !matches.is_empty() {
                self.popup = Some(CompletionPopup::new_command(matches));
                return;
            }
        }

        // Path completion fallback
        if let Some((prefix, start)) = self.extract_path_prefix() {
            let matches = Self::find_path_matches(&prefix);
            if !matches.is_empty() {
                self.popup = Some(CompletionPopup::new_path(matches, prefix, start));
            }
        }
    }

    /// Open the popup or cycle selection backwards if already open.
    fn open_or_cycle_popup_back(&mut self) {
        if self.popup.is_some() {
            // Cycle: move selection up
            if let Some(ref mut popup) = self.popup {
                popup.move_up();
            }
            return;
        }

        // Command completion first (text starts with /)
        if self.text.starts_with('/') && self.cursor == self.text.len() {
            let matches = Self::find_command_matches(&self.text);
            if !matches.is_empty() {
                self.popup = Some(CompletionPopup::new_command(matches));
                return;
            }
        }

        // Path completion fallback
        if let Some((prefix, start)) = self.extract_path_prefix() {
            let matches = Self::find_path_matches(&prefix);
            if !matches.is_empty() {
                self.popup = Some(CompletionPopup::new_path(matches, prefix, start));
            }
        }
    }

    /// Handle keys when popup is open.
    fn handle_popup_key(&mut self, key: KeyEvent) -> bool {
        match key {
            // Navigate popup
            KeyEvent {
                code: KeyCode::Up,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                if let Some(ref mut popup) = self.popup {
                    popup.move_up();
                }
                true
            }
            KeyEvent {
                code: KeyCode::Down,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                if let Some(ref mut popup) = self.popup {
                    popup.move_down();
                }
                true
            }
            KeyEvent {
                code: KeyCode::Tab,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                // Tab cycles down
                if let Some(ref mut popup) = self.popup {
                    popup.move_down();
                }
                true
            }
            KeyEvent {
                code: KeyCode::BackTab,
                ..
            } => {
                // Shift+Tab cycles up
                if let Some(ref mut popup) = self.popup {
                    popup.move_up();
                }
                true
            }
            // Confirm selection
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.confirm_popup();
                true
            }
            // Dismiss popup
            KeyEvent {
                code: KeyCode::Esc, ..
            } => {
                self.popup = None;
                true
            }
            // Continue typing — dismiss popup and process char
            KeyEvent {
                code: KeyCode::Char(c),
                modifiers: KeyModifiers::NONE | KeyModifiers::SHIFT,
                ..
            } => {
                self.popup = None;
                self.insert_char(c);
                self.update_popup();
                true
            }
            KeyEvent {
                code: KeyCode::Backspace,
                ..
            } => {
                self.popup = None;
                if self.cursor > 0 {
                    let prev = self.prev_char_boundary();
                    self.text.drain(prev..self.cursor);
                    self.cursor = prev;
                }
                self.update_popup();
                true
            }
            _ => {
                self.popup = None;
                false
            }
        }
    }

    /// Confirm the popup selection: apply the selected completion.
    fn confirm_popup(&mut self) {
        if let Some(ref popup) = self.popup
            && let Some(selected) = popup.selected_item()
        {
            match popup.completion_type() {
                CompletionType::Command => {
                    // Command: replace entire text
                    self.text = selected.to_string();
                    self.cursor = self.text.len();
                }
                CompletionType::Path { prefix: _, start } => {
                    // Path: replace only the path token portion
                    let start = *start;
                    let end = self.cursor;
                    self.text.replace_range(start..end, selected);
                    self.cursor = start + selected.len();
                }
            }
        }
        self.popup = None;
    }

    /// Update popup matches based on current text (auto-show/hide).
    fn update_popup(&mut self) {
        // Command completion first (text starts with /)
        if self.text.starts_with('/') && self.cursor == self.text.len() {
            let matches = Self::find_command_matches(&self.text);
            if matches.is_empty() {
                self.popup = None;
            } else if let Some(ref mut popup) = self.popup {
                let prev = popup.selected_item().map(String::from);
                popup.matches = matches;
                popup.scroll = 0;
                if let Some(ref prev) = prev {
                    if let Some(idx) = popup.matches.iter().position(|c| c == prev) {
                        popup.selected = idx;
                        if popup.selected >= MAX_POPUP_ITEMS {
                            popup.scroll = popup.selected - MAX_POPUP_ITEMS + 1;
                        }
                    } else {
                        popup.selected = 0;
                    }
                } else {
                    popup.selected = 0;
                }
            } else {
                self.popup = Some(CompletionPopup::new_command(matches));
            }
            return;
        }

        // Path completion
        if let Some((prefix, start)) = self.extract_path_prefix() {
            let matches = Self::find_path_matches(&prefix);
            if matches.is_empty() {
                self.popup = None;
            } else if let Some(ref mut popup) = self.popup {
                let prev = popup.selected_item().map(String::from);
                popup.matches = matches;
                popup.scroll = 0;
                if let Some(ref prev) = prev {
                    if let Some(idx) = popup.matches.iter().position(|c| c == prev) {
                        popup.selected = idx;
                        if popup.selected >= MAX_POPUP_ITEMS {
                            popup.scroll = popup.selected - MAX_POPUP_ITEMS + 1;
                        }
                    } else {
                        popup.selected = 0;
                    }
                } else {
                    popup.selected = 0;
                }
            } else {
                self.popup = Some(CompletionPopup::new_path(matches, prefix, start));
            }
        } else {
            self.popup = None;
        }
    }

    /// Find slash commands matching the given prefix.
    fn find_command_matches(prefix: &str) -> Vec<String> {
        COMMANDS
            .iter()
            .filter(|c| c.starts_with(prefix))
            .map(|c| c.to_string())
            .collect()
    }

    /// Extract the path token being typed at the cursor position.
    ///
    /// Scans backwards from cursor to find the start of a path-like token.
    /// A path token is a sequence of non-whitespace characters containing `/`
    /// or starting with `.` or `~`.
    ///
    /// Returns `(prefix, start_byte)` if a path prefix is detected.
    fn extract_path_prefix(&self) -> Option<(String, usize)> {
        if self.cursor == 0 || self.text.is_empty() {
            return None;
        }

        let before_cursor = &self.text[..self.cursor];
        let line_start = before_cursor.rfind('\n').map(|i| i + 1).unwrap_or(0);
        let line_before_cursor = &before_cursor[line_start..];

        if line_before_cursor.is_empty() {
            return None;
        }

        // Scan backwards from cursor to find the start of the current token
        let bytes = line_before_cursor.as_bytes();
        let mut pos = bytes.len();
        // Skip trailing whitespace
        while pos > 0 && bytes[pos - 1].is_ascii_whitespace() {
            pos -= 1;
        }
        if pos == 0 {
            return None;
        }
        // Now scan back through non-whitespace to find token start
        let token_end = pos;
        while pos > 0 && !bytes[pos - 1].is_ascii_whitespace() {
            pos -= 1;
        }
        let token = &line_before_cursor[pos..token_end];

        // Check if this token looks like a path
        if token.contains('/') || token.starts_with('.') || token.starts_with('~') {
            Some((token.to_string(), line_start + pos))
        } else {
            None
        }
    }

    /// Find file/directory entries matching a path prefix.
    ///
    /// Handles `~` expansion, relative paths, and partial directory names.
    fn find_path_matches(prefix: &str) -> Vec<String> {
        let expanded;
        let (dir_part, file_prefix) = if let Some(stripped) = prefix.strip_prefix("~/") {
            expanded = dirs::home_dir()
                .map(|h| h.join(stripped))
                .unwrap_or_else(|| PathBuf::from(stripped));
            let p = &expanded;
            match p.parent() {
                Some(parent) if p.file_name().is_some() => (
                    parent.to_path_buf(),
                    p.file_name().unwrap().to_string_lossy().to_string(),
                ),
                _ => (p.clone(), String::new()),
            }
        } else if prefix.ends_with('/') {
            // Entered a directory separator — list the directory contents
            let dir = if prefix == "/" {
                PathBuf::from("/")
            } else {
                PathBuf::from(prefix)
            };
            (dir, String::new())
        } else {
            let p = Path::new(prefix);
            match p.parent() {
                Some(parent) if p.file_name().is_some() => (
                    parent.to_path_buf(),
                    p.file_name().unwrap().to_string_lossy().to_string(),
                ),
                Some(parent) => (parent.to_path_buf(), String::new()),
                None => (PathBuf::from("."), prefix.to_string()),
            }
        };

        // Resolve relative to cwd
        let dir_resolved = if dir_part.is_absolute() {
            dir_part.clone()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(&dir_part)
        };

        let entries = match std::fs::read_dir(&dir_resolved) {
            Ok(e) => e,
            Err(_) => return Vec::new(),
        };

        let mut results: Vec<String> = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            // Skip hidden files unless prefix starts with .
            if name.starts_with('.') && !file_prefix.starts_with('.') {
                continue;
            }
            if !name.starts_with(&file_prefix) {
                continue;
            }

            let is_dir = entry.path().is_dir();
            // Build the replacement string
            let replacement = if prefix.ends_with('/') {
                // We're inside a directory — build full path
                format!("{}{}", prefix, name)
            } else {
                // Replace the partial name
                match dir_part.to_str() {
                    Some(".") => name.clone(),
                    Some("") => name.clone(),
                    Some(d) => format!("{}/{}", d, name),
                    None => name.clone(),
                }
            };
            let display = if is_dir {
                format!("{}/", replacement)
            } else {
                replacement
            };
            results.push(display);
        }

        results.sort();
        results
    }

    // ── helpers ──

    /// Return the display-column offset of the cursor on the current line (accounts for CJK etc.).
    pub fn cursor_display_col(&self) -> u16 {
        let (row, col_byte) = self.cursor_row_col();
        let line = self.line_content(row);
        let byte_end = col_byte.min(line.len());
        crate::utils::display_width(&line[..byte_end]) as u16
    }

    /// Return the cursor row index (0-based).
    pub fn cursor_row(&self) -> usize {
        self.cursor_row_col().0
    }

    /// Total number of display rows in the text.
    pub fn line_count(&self) -> usize {
        self.text.chars().filter(|&c| c == '\n').count() + 1
    }

    fn insert_char(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor = self.next_char_boundary();
    }

    /// Insert a string at the cursor position (used for bracketed paste).
    /// Newlines in the pasted text are kept as-is (not treated as submit).
    pub fn insert_str(&mut self, s: &str) {
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    /// Delete the word before the cursor (Ctrl+W behavior).
    ///
    /// Scans backwards from cursor, first deleting any trailing whitespace,
    /// then deleting the word preceding it — equivalent to how readline /
    /// bash handle Ctrl+W (delete back to the previous whitespace boundary).
    fn delete_word_backwards(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let before = &self.text[..self.cursor];
        // Find the deletion start by scanning backwards:
        // 1. Skip any whitespace immediately before the cursor
        // 2. Then skip the word (non-whitespace) characters before that
        let mut pos = before.len();
        // Skip trailing whitespace
        while pos > 0 && before.as_bytes()[pos - 1].is_ascii_whitespace() {
            pos -= 1;
        }
        // Skip word characters
        while pos > 0 && !before.as_bytes()[pos - 1].is_ascii_whitespace() {
            pos -= 1;
        }
        self.text.drain(pos..self.cursor);
        self.cursor = pos;
    }

    fn prev_char_boundary(&self) -> usize {
        let mut idx = self.cursor.saturating_sub(1);
        while idx > 0 && !self.text.is_char_boundary(idx) {
            idx -= 1;
        }
        idx
    }

    fn next_char_boundary(&self) -> usize {
        let mut idx = self.cursor + 1;
        while idx < self.text.len() && !self.text.is_char_boundary(idx) {
            idx += 1;
        }
        idx.min(self.text.len())
    }
}

/// Snap a byte offset to the nearest UTF-8 char boundary at or before it.
fn snap_to_char_boundary(s: &str, offset: usize) -> usize {
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

// ── Rendering ──

/// Render the input area.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    state: &InputState,
    mode: &super::status_bar::AppMode,
) {
    let (border_color, prompt, display_text, text_color): (Color, &str, Cow<str>, Color) =
        match mode {
            super::status_bar::AppMode::Running => {
                // Allow typing while running — user input is "steered" into the
                // agent loop on the next turn. Show a different prompt to indicate
                // mid-run input mode.
                if state.text.is_empty() {
                    (
                        theme::WARNING,
                        "\u{21e2} ",
                        Cow::Borrowed("type to steer agent\u{2026}"),
                        theme::TEXT_DIM,
                    )
                } else {
                    (
                        theme::WARNING,
                        "\u{21e2} ",
                        Cow::Borrowed(&state.text),
                        theme::TEXT,
                    )
                }
            }
            super::status_bar::AppMode::WaitingInput => {
                (theme::ACCENT, " ", Cow::Borrowed(&state.text), theme::TEXT)
            }
            super::status_bar::AppMode::Idle => {
                if state.active {
                    (theme::ACCENT, " ", Cow::Borrowed(&state.text), theme::TEXT)
                } else {
                    (theme::BORDER, " ", Cow::Borrowed(&state.text), theme::TEXT)
                }
            }
        };

    let content_area = Rect {
        y: area.y + 1,
        height: area.height.saturating_sub(1),
        ..area
    };

    // Available width for text (excluding prompt)
    let prompt_display_width: u16 = crate::utils::display_width(&prompt) as u16;
    let text_area_width = content_area.width.saturating_sub(prompt_display_width);

    // Compute horizontal scroll offset for the current line
    let cursor_col = state.cursor_display_col();
    let h_scroll = if cursor_col >= text_area_width {
        cursor_col - text_area_width + 1 // +1 so cursor is at least 1 col from right edge
    } else {
        0u16
    };

    // Compute vertical scroll offset for multi-line
    let cursor_row = state.cursor_row() as u16;
    let v_scroll = if cursor_row >= content_area.height {
        cursor_row - content_area.height + 1
    } else {
        0u16
    };

    // Build lines from text, applying horizontal scroll offset per line
    let all_lines: Vec<&str> = display_text.split('\n').collect();
    let lines: Vec<Line> = all_lines
        .iter()
        .enumerate()
        .filter(|(i, _)| *i >= v_scroll as usize)
        .map(|(i, line_str)| {
            let scrolled_content = if h_scroll > 0 {
                // Need to skip h_scroll display columns
                let mut col: u16 = 0;
                let mut byte_start = 0;
                let chars: Vec<char> = line_str.chars().collect();
                for (ci, (byte_idx, c)) in line_str.char_indices().enumerate() {
                    let next = chars.get(ci + 1).copied();
                    let w = crate::utils::char_width(c, next) as u16;
                    if col + w > h_scroll {
                        // This character straddles the scroll boundary
                        byte_start = byte_idx;
                        break;
                    }
                    col += w;
                    byte_start = byte_idx + c.len_utf8();
                }
                &line_str[byte_start..]
            } else {
                *line_str
            };

            // Only first line gets the prompt prefix; continuation lines are indented
            if i == 0 {
                Line::from(vec![
                    Span::styled(prompt, Style::new().fg(theme::ACCENT_DIM)),
                    Span::styled(scrolled_content.to_string(), Style::new().fg(text_color)),
                ])
            } else {
                Line::from(vec![
                    Span::styled(
                        " ".repeat(prompt_display_width as usize),
                        Style::new().fg(theme::ACCENT_DIM),
                    ),
                    Span::styled(scrolled_content.to_string(), Style::new().fg(text_color)),
                ])
            }
        })
        .collect();

    let p = Paragraph::new(Text::from(lines)).style(Style::new().bg(theme::BG).fg(theme::TEXT));

    frame.render_widget(p, content_area);

    // Draw border
    draw_border(frame, area, border_color);

    // Draw completion popup if active
    if let Some(ref popup) = state.popup {
        render_popup(frame, area, popup);
    }
}

/// Render the completion popup above the input area.
fn render_popup(frame: &mut Frame, input_area: Rect, popup: &CompletionPopup) {
    let visible = popup.visible_slice();
    if visible.is_empty() {
        return;
    }

    let popup_height = visible.len() as u16 + 2; // +2 for border
    // Dynamic width: longest item + padding, capped
    let max_item_width = visible.iter().map(|s| s.len()).max().unwrap_or(8) as u16;
    let popup_width = (max_item_width + 4).min(input_area.width.saturating_sub(2)); // +4 for " " + padding

    // Position: above input area, left-aligned
    let popup_area = Rect {
        x: input_area.x + 2, // align with prompt
        y: input_area.y.saturating_sub(popup_height),
        width: popup_width,
        height: popup_height,
    };

    // Clear the area behind the popup
    frame.render_widget(Clear, popup_area);

    // Build popup content
    let lines: Vec<Line> = visible
        .iter()
        .enumerate()
        .map(|(i, cmd)| {
            let is_selected = (i + popup.scroll) == popup.selected;
            let style = if is_selected {
                Style::new()
                    .fg(theme::TEXT_BRIGHT)
                    .bg(theme::ACCENT_DIM)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(theme::TEXT_DIM)
            };
            let marker = if is_selected { " " } else { "  " };
            Line::from(vec![Span::styled(format!("{}{}", marker, cmd), style)])
        })
        .collect();

    let popup_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(theme::ACCENT))
        .style(Style::new().bg(theme::BG));

    let p = Paragraph::new(lines).block(popup_block);
    frame.render_widget(p, popup_area);
}

fn draw_border(frame: &mut Frame, area: Rect, color: Color) {
    let style = Style::new().fg(color);

    // Top border
    let top = Rect {
        y: area.y,
        height: 1,
        ..area
    };
    frame.render_widget(
        Paragraph::new(Line::from("─".repeat(area.width as usize))).style(style),
        top,
    );
}

// ── Session history persistence ──

/// Load input history from disk. Returns an empty Vec if the file doesn't
/// exist or is corrupted, so the user never loses the ability to type.
fn load_history() -> Vec<String> {
    let path = paths::session_history_path();
    if !path.exists() {
        return Vec::new();
    }
    match std::fs::read_to_string(&path) {
        Ok(json) => match serde_json::from_str::<Vec<String>>(&json) {
            Ok(history) => history,
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "Failed to parse session history, starting fresh");
                Vec::new()
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "Failed to read session history, starting fresh");
            Vec::new()
        }
    }
}

/// Save input history to disk. Truncates to MAX_PERSISTED_HISTORY entries.
fn save_history(history: &[String]) {
    let path = paths::session_history_path();
    let truncated: Vec<&str> = history
        .iter()
        .take(MAX_PERSISTED_HISTORY)
        .map(|s| s.as_str())
        .collect();
    match serde_json::to_string(&truncated) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, &json) {
                tracing::warn!(error = %e, path = %path.display(), "Failed to save session history");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to serialize session history");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_popup_opens_on_slash() {
        let mut input = InputState::new();
        input.text = "/".into();
        input.cursor = 1;
        input.update_popup();
        assert!(input.popup.is_some(), "Popup should open for '/'");
        let popup = input.popup.as_ref().unwrap();
        assert_eq!(popup.matches.len(), COMMANDS.len());
    }

    #[test]
    fn test_popup_filters_on_typing() {
        let mut input = InputState::new();
        input.text = "/he".into();
        input.cursor = 3;
        input.update_popup();
        assert!(input.popup.is_some());
        let popup = input.popup.as_ref().unwrap();
        assert_eq!(popup.matches, vec!["/help"]);
    }

    #[test]
    fn test_popup_no_match() {
        let mut input = InputState::new();
        input.text = "/xyz".into();
        input.cursor = 4;
        input.update_popup();
        assert!(input.popup.is_none(), "No popup for no matches");
    }

    #[test]
    fn test_popup_not_for_non_slash() {
        let mut input = InputState::new();
        input.text = "hello".into();
        input.cursor = 5;
        input.update_popup();
        assert!(input.popup.is_none());
    }

    #[test]
    fn test_popup_navigation() {
        let mut input = InputState::new();
        input.text = "/".into();
        input.cursor = 1;
        input.update_popup();
        let popup = input.popup.as_mut().unwrap();
        assert_eq!(popup.selected, 0);
        popup.move_down();
        assert_eq!(popup.selected, 1);
        popup.move_up();
        assert_eq!(popup.selected, 0);
        // Up at top should stay at 0
        popup.move_up();
        assert_eq!(popup.selected, 0);
    }

    #[test]
    fn test_popup_confirm_command() {
        let mut input = InputState::new();
        input.text = "/he".into();
        input.cursor = 3;
        input.update_popup();
        input.confirm_popup();
        assert_eq!(input.text, "/help");
        assert!(input.popup.is_none());
    }

    #[test]
    fn test_popup_dismiss_on_non_slash_edit() {
        let mut input = InputState::new();
        input.text = "/".into();
        input.cursor = 1;
        input.update_popup();
        assert!(input.popup.is_some());
        // Backspace to empty — should dismiss
        input.text.clear();
        input.cursor = 0;
        input.update_popup();
        assert!(input.popup.is_none());
    }

    #[test]
    fn test_path_completion_basic() {
        // Test path detection: "src/" should be detected as path
        let mut input = InputState::new();
        input.text = "look at src/".into();
        input.cursor = input.text.len();
        input.update_popup();
        // If src/ directory exists and has entries, popup should open
        // (this test depends on the working directory having a src/ dir)
        if std::path::Path::new("src").is_dir() {
            assert!(input.popup.is_some(), "Popup should open for 'src/' path");
            let popup = input.popup.as_ref().unwrap();
            assert!(!popup.matches.is_empty());
            // All matches should start with "src/"
            for m in &popup.matches {
                assert!(
                    m.starts_with("src/"),
                    "Match '{}' should start with 'src/'",
                    m
                );
            }
        }
    }

    #[test]
    fn test_path_completion_dot_prefix() {
        // "./" should trigger path completion
        let mut input = InputState::new();
        input.text = "./".into();
        input.cursor = 2;
        input.update_popup();
        // Should list cwd entries
        assert!(input.popup.is_some(), "Popup should open for './'");
    }

    #[test]
    fn test_path_completion_confirm_replaces_token() {
        // Confirming a path completion should only replace the path token, not entire text
        let mut input = InputState::new();
        input.text = "read file src/".into();
        input.cursor = input.text.len(); // cursor at end
        // Simulate a path popup manually
        input.popup = Some(CompletionPopup::new_path(
            vec!["src/main.rs".into(), "src/lib.rs".into()],
            "src/".into(),
            10, // start of "src/" ("read file " = 10 bytes)
        ));
        input.confirm_popup();
        assert_eq!(input.text, "read file src/main.rs");
        assert_eq!(input.cursor, 21); // "read file src/main.rs" = 21 bytes
    }

    #[test]
    fn test_extract_path_prefix_basic() {
        let input = InputState {
            text: "look at src/main".into(),
            cursor: 16,
            ..InputState::new()
        };
        let result = input.extract_path_prefix();
        assert!(result.is_some());
        let (prefix, start) = result.unwrap();
        assert_eq!(prefix, "src/main");
        assert_eq!(start, 8); // "look at " is 8 bytes
    }

    #[test]
    fn test_extract_path_prefix_tilde() {
        let input = InputState {
            text: "cat ~/Doc".into(),
            cursor: 9,
            ..InputState::new()
        };
        let result = input.extract_path_prefix();
        assert!(result.is_some());
        let (prefix, _) = result.unwrap();
        assert_eq!(prefix, "~/Doc");
    }

    #[test]
    fn test_extract_path_prefix_no_path() {
        let input = InputState {
            text: "hello world".into(),
            cursor: 11,
            ..InputState::new()
        };
        let result = input.extract_path_prefix();
        assert!(result.is_none());
    }

    #[test]
    fn test_multiline_insert_newline() {
        let mut input = InputState::new();
        // Simulate Shift+Enter
        input.text = "hello".into();
        input.cursor = 5;
        input.insert_char('\n');
        assert_eq!(input.text, "hello\n");
        assert_eq!(input.cursor, 6);
        input.insert_char('w');
        input.insert_char('o');
        input.insert_char('r');
        input.insert_char('l');
        input.insert_char('d');
        assert_eq!(input.text, "hello\nworld");
    }

    #[test]
    fn test_cursor_row_col() {
        let mut input = InputState::new();
        input.text = "hello\nworld\n!".into();
        input.cursor = 0;
        assert_eq!(input.cursor_row_col(), (0, 0));

        input.cursor = 5; // just before \n
        assert_eq!(input.cursor_row_col(), (0, 5));

        input.cursor = 6; // just after \n, start of "world"
        assert_eq!(input.cursor_row_col(), (1, 0));

        input.cursor = 11; // just before second \n
        assert_eq!(input.cursor_row_col(), (1, 5));

        input.cursor = 12; // start of "!"
        assert_eq!(input.cursor_row_col(), (2, 0));

        input.cursor = 13; // end of text
        assert_eq!(input.cursor_row_col(), (2, 1));
    }

    #[test]
    fn test_move_cursor_up_down() {
        let mut input = InputState::new();
        input.text = "hello\nworld\n!".into();

        // Start at end of line 2 (after "!")
        input.cursor = 13;
        assert_eq!(input.cursor_row_col(), (2, 1));

        // Move up — should go to col 1 of "world"
        input.move_cursor_up();
        assert_eq!(input.cursor_row_col(), (1, 1));

        // Move up — should go to col 1 of "hello"
        input.move_cursor_up();
        assert_eq!(input.cursor_row_col(), (0, 1));

        // Move up at top — returns false, cursor stays
        assert!(!input.move_cursor_up());
        assert_eq!(input.cursor_row_col(), (0, 1));

        // Now go down
        input.cursor = 0;
        input.move_cursor_down();
        assert_eq!(input.cursor_row_col(), (1, 0));
    }

    #[test]
    fn test_home_end_multiline() {
        let mut input = InputState::new();
        input.text = "hello\nworld".into();
        input.cursor = 8; // middle of "world"

        input.move_cursor_to_line_start();
        assert_eq!(input.cursor, 6); // start of "world"

        input.move_cursor_to_line_end();
        assert_eq!(input.cursor, 11); // end of "world"

        // Now on first line
        input.cursor = 3;
        input.move_cursor_to_line_start();
        assert_eq!(input.cursor, 0);

        input.move_cursor_to_line_end();
        assert_eq!(input.cursor, 5); // end of "hello" (before \n)
    }

    #[test]
    fn test_delete_word_backwards() {
        let mut input = InputState::new();

        // "hello world" with cursor at end — should delete "world"
        input.text = "hello world".into();
        input.cursor = input.text.len();
        input.delete_word_backwards();
        assert_eq!(input.text, "hello ");
        assert_eq!(input.cursor, 6);

        // "hello" with cursor at end — single word, should delete it all
        input.delete_word_backwards();
        assert_eq!(input.text, "");
        assert_eq!(input.cursor, 0);

        // Cursor at start — no-op
        input.text = "test".into();
        input.cursor = 0;
        input.delete_word_backwards();
        assert_eq!(input.text, "test");
        assert_eq!(input.cursor, 0);

        // Cursor in the middle of a word — should delete back to previous whitespace
        input.text = "hello world".into();
        input.cursor = 8; // between 'o' and 'r' in "world"
        input.delete_word_backwards();
        assert_eq!(input.text, "hello rld");
        assert_eq!(input.cursor, 6);

        // Only whitespace before cursor
        input.text = "   hello".into();
        input.cursor = 3; // after leading spaces
        input.delete_word_backwards();
        assert_eq!(input.text, "hello");
        assert_eq!(input.cursor, 0);
    }

    #[test]
    fn test_ctrl_w_key_event() {
        let mut input = InputState::new();
        input.text = "hello world".into();
        input.cursor = input.text.len();

        let key = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);
        let consumed = input.handle_key(key);
        assert!(consumed, "Ctrl+W should be consumed");
        assert_eq!(input.text, "hello ");
    }

    #[test]
    fn test_move_cursor_up_down_cjk_no_panic() {
        // Regression: moving cursor up/down between lines with different
        // character widths (ASCII vs CJK) used to place cursor inside a
        // multi-byte char boundary, causing a panic.
        // 你好世界 = 12 bytes, "hello" = 5 bytes
        let mut input = InputState::new();
        input.text = "你好世界\nhello".into();
        // Cursor at end of "hello" on line 1
        input.cursor = input.text.len(); // 18
        assert_eq!(input.cursor_row_col(), (1, 5));

        // Move up — cursor should snap to a valid char boundary on line 0
        // "你好世界" is 12 bytes, col 5 would be inside '世' (bytes 6-8).
        // snap_to_char_boundary should correct this.
        assert!(input.move_cursor_up());
        let (row, col) = input.cursor_row_col(); // must not panic
        assert_eq!(row, 0);
        // col may be 3（好 ends at byte 6) or 6（世 ends at byte 9)
        assert!(col <= 12);

        // Move back down
        assert!(input.move_cursor_down());
        let (row, col) = input.cursor_row_col();
        assert_eq!(row, 1);
        assert!(col <= 5);
    }

    #[test]
    fn test_snap_to_char_boundary() {
        // '过' = bytes 0-2, so byte 1 is inside the char
        let s = "hello过world";
        assert_eq!(snap_to_char_boundary(s, 5), 5); // 'h','e','l','l','o' = ascii
        assert_eq!(snap_to_char_boundary(s, 6), 5); // inside '过', snap back
        assert_eq!(snap_to_char_boundary(s, 7), 5); // inside '过', snap back
        assert_eq!(snap_to_char_boundary(s, 8), 8); // 'w', on boundary
        assert_eq!(snap_to_char_boundary(s, 100), s.len()); // past end
    }
}
