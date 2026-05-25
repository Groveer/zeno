//! Multi-line input widget with horizontal scrolling and command/path completion popup.
//!
//! Supports cursor movement, backspace, Shift+Enter for newlines, Enter to submit,
//! Ctrl+D submission, and a popup completion menu for slash commands and file paths.
//!
//! ## Sub-modules
//!
//! - [`completion`] — `CompletionPopup` state and navigation
//! - [`history`] — input history persistence (identity-scoped load/save from disk)
//! - [`clipboard`] — clipboard image reading for Alt+V paste

pub mod clipboard;
pub mod completion;
pub mod editor;
pub mod history;

use completion::CompletionPopup;
use completion::CompletionType;
use completion::MAX_POPUP_ITEMS;

use crate::gateway::UiCommand;
use crate::ui::status_bar::AppMode;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::component::Component;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph},
};
use std::borrow::Cow;
use std::path::{Path, PathBuf};

use super::theme;

/// Actions returned by `InputState::dispatch_key()` for App to route.
///
/// Each variant describes what the user intended, decoupled from the
/// mode-specific handling that App performs.
#[derive(Debug)]
pub enum InputAction {
    /// Key was consumed by the editor (no routing needed).
    Consumed,
    /// User submitted a query (Idle mode Enter).
    SubmitQuery {
        text: String,
        images: Vec<(String, String)>,
    },
    /// User typed while agent was running (Running mode Enter) — inject steer.
    Steer(String),
    /// User responded to ask_user / permission prompt (WaitingInput mode Enter).
    Respond { text: String },
    /// Ctrl+D with empty input — hard quit.
    HardQuit,
}

/// All slash commands supported by the TUI, with short aliases first.
const COMMANDS: &[&str] = &[
    "/clear",
    "/compact",
    "/cost",
    "/exit",
    "/goal",
    "/help",
    "/hooks",
    "/identity",
    "/mcp",
    "/memory",
    "/model",
    "/quit",
    "/restore",
    "/search",
    "/skills",
    "/tools",
];

/// Marker character used in the text buffer to represent an inline image token.
/// Renders as a styled "[img]" span in the input area. Deletable like normal chars.
pub const IMAGE_MARKER: char = '\u{FFFC}'; // Object Replacement Character

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
    /// Image data keyed by marker position — each IMAGE_MARKER in text has a
    /// corresponding (media_type, base64_data) entry here, in the same order
    /// as they appear in the text buffer.
    pub(crate) images: Vec<(String, String)>,
    /// Available identity names for `/identity` argument completion.
    /// Populated by `App` from settings at startup and on config reload.
    pub(crate) identity_names: Vec<String>,
    /// Active identity for scoped input history.
    /// When set, history is saved/loaded from `input_history/{identity}.json`.
    active_identity: Option<String>,
    /// Ghost text (inline autosuggestion) from input history.
    /// Stores only the suffix to append — the part the user hasn't typed yet.
    /// Displayed as dim shadow text after the cursor. Tab accepts it.
    ghost_text: Option<String>,
    /// Current app mode, synced from App for use in view() and dispatch_key().
    app_mode: AppMode,
}

impl InputState {
    /// Create InputState with an optional active identity for scoped history.
    pub fn with_identity(identity: Option<String>) -> Self {
        let history = history::load_history(identity.as_deref());
        let active_identity = identity;
        Self {
            text: String::new(),
            cursor: 0,
            submitted: false,
            active: true,
            popup: None,
            input_history: history,
            history_index: None,
            draft: None,
            images: Vec::new(),
            identity_names: Vec::new(),
            ghost_text: None,
            app_mode: AppMode::default(),
            active_identity,
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
            // Tab: open popup or cycle, or accept ghost text
            KeyEvent {
                code: KeyCode::Tab,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                if self.popup.is_some() {
                    // Popup is open — cycle selection
                    if let Some(ref mut popup) = self.popup {
                        popup.move_down();
                    }
                } else {
                    // No popup — try to open one
                    self.update_popup();
                    // If still no popup, accept ghost text
                    if self.popup.is_none() {
                        self.accept_ghost_text();
                    }
                }
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
                    self.cursor = self.prev_grapheme_boundary();
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
                    self.cursor = self.next_grapheme_boundary();
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
                code: KeyCode::Char('a'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.move_cursor_to_line_start();
                self.update_popup();
                true
            }
            KeyEvent {
                code: KeyCode::Char('e'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.move_cursor_to_line_end();
                self.update_popup();
                true
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
                    let prev = self.prev_grapheme_boundary();
                    // If deleting an image marker, remove the corresponding image data
                    if self.text[prev..self.cursor] == *"\u{FFFC}" {
                        let idx = self.text[..prev]
                            .chars()
                            .filter(|&c| c == '\u{FFFC}')
                            .count();
                        if idx < self.images.len() {
                            self.images.remove(idx);
                        }
                    }
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
                    // If deleting an image marker, remove the corresponding image data
                    let c = self.text[self.cursor..].chars().next().unwrap();
                    if c == '\u{FFFC}' {
                        let idx = self.text[..self.cursor]
                            .chars()
                            .filter(|&c| c == '\u{FFFC}')
                            .count();
                        if idx < self.images.len() {
                            self.images.remove(idx);
                        }
                    }
                    let next = self.next_grapheme_boundary();
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

    /// Process a key event and return an InputAction for App to route.
    ///
    /// This is the component-level key handler: it processes the key through
    /// the editor, then checks the `submitted` flag to determine the action.
    /// Global shortcuts (Ctrl+D, Ctrl+C, PageUp/Down, Alt+V) are handled by
    /// App before calling this method.
    pub fn dispatch_key(&mut self, key: KeyEvent, mode: AppMode) -> InputAction {
        // Ctrl+D with empty text → hard quit (global, but detected here since
        // it also sets submitted + text="/exit" in handle_key)
        if matches!(
            key,
            KeyEvent {
                code: KeyCode::Char('d'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }
        ) && self.text.is_empty()
        {
            return InputAction::HardQuit;
        }

        match mode {
            AppMode::Running => {
                let _ = self.handle_key(key);
                if self.submitted {
                    let text = self.text.trim().to_string();
                    self.reset();
                    if !text.is_empty() {
                        InputAction::Steer(text)
                    } else {
                        InputAction::Consumed
                    }
                } else {
                    InputAction::Consumed
                }
            }
            AppMode::WaitingInput => {
                let _ = self.handle_key(key);
                if self.submitted {
                    let text = self.text.trim().to_string();
                    self.reset_without_history();
                    InputAction::Respond { text }
                } else {
                    InputAction::Consumed
                }
            }
            AppMode::Idle => {
                let _ = self.handle_key(key);
                if self.submitted {
                    let (images, text) = self.extract_images();
                    let text = text.trim().to_string();
                    self.reset();
                    if text.is_empty() && images.is_empty() {
                        InputAction::Consumed
                    } else {
                        InputAction::SubmitQuery { text, images }
                    }
                } else {
                    InputAction::Consumed
                }
            }
        }
    }

    /// Clear after submission. Saves the current text to input history.
    pub fn reset(&mut self) {
        // Save to history (skip empty, slash commands, and duplicates of the most recent entry)
        // Strip image markers since the corresponding image data won't persist.
        let trimmed: String = self.text.chars().filter(|&c| c != IMAGE_MARKER).collect();
        let trimmed = trimmed.trim().to_string();

        let is_history_entry = !trimmed.is_empty() && !trimmed.starts_with('/');

        // Update in-memory history (for this session's Up/Down navigation)
        if is_history_entry && self.input_history.first().map(|s| s.as_str()) != Some(&trimmed) {
            self.input_history.insert(0, trimmed.clone());
        }

        self.text.clear();
        self.cursor = 0;
        self.submitted = false;
        self.popup = None;
        self.history_index = None;
        self.draft = None;
        self.images.clear();
        self.ghost_text = None;

        // Persist: re-read latest disk state, merge new entry, save.
        // This prevents overwriting entries from other concurrent Zeno instances.
        if is_history_entry {
            let identity = self.active_identity.as_deref();
            let mut on_disk = history::load_history(identity);
            if on_disk.first().map(|s| s.as_str()) != Some(&trimmed) {
                on_disk.insert(0, trimmed);
            }
            history::save_history(&on_disk, identity);
        }
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
        self.ghost_text = None;
    }

    // Multi-line cursor helpers

    /// Return (row, col_byte) of the cursor position in the text.
    /// col_byte is the byte offset within the current line.
    ///
    /// Panics if self.cursor is not on a UTF-8 char boundary (should never happen).
    fn cursor_row_col(&self) -> (usize, usize) {
        let cursor = self.cursor.min(self.text.len());
        // Defensive: snap cursor to the nearest char boundary if somehow misaligned.
        let cursor = editor::snap_to_char_boundary(&self.text, cursor);
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
        self.cursor = editor::snap_to_char_boundary(&self.text, target);
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
        self.cursor = editor::snap_to_char_boundary(&self.text, target);
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

    // Popup logic

    /// Open the popup or cycle selection backwards if already open.
    fn open_or_cycle_popup_back(&mut self) {
        if self.popup.is_some() {
            // Cycle: move selection up
            if let Some(ref mut popup) = self.popup {
                popup.move_up();
            }
            return;
        }
        // No popup open — delegate to update_popup to create one
        self.update_popup();
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
                self.compute_ghost_text();
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
                    let prev = self.prev_grapheme_boundary();
                    self.text.drain(prev..self.cursor);
                    self.cursor = prev;
                }
                self.update_popup();
                true
            }
            _ => {
                self.popup = None;
                self.compute_ghost_text();
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
                CompletionType::CommandArg { cmd_len } => {
                    // Argument: replace text from cmd_len to cursor
                    let cmd_len = *cmd_len;
                    self.text.replace_range(cmd_len..self.cursor, selected);
                    self.cursor = cmd_len + selected.len();
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
        self.compute_ghost_text();
    }

    /// Compute ghost text (inline autosuggestion) from input history.
    ///
    /// Searches `input_history` for the most recent entry that starts with the
    /// current text (and is longer). Stores the suffix as `ghost_text`.
    /// Only works when the cursor is at the end of single-line text.
    fn compute_ghost_text(&mut self) {
        // Only show ghost text when:
        // - text is non-empty (no suggestion for empty input)
        // - cursor is at end
        // - no popup is active
        // - not in history navigation mode
        if self.text.is_empty()
            || self.cursor != self.text.len()
            || self.popup.is_some()
            || self.history_index.is_some()
        {
            self.ghost_text = None;
            return;
        }

        // Don't show ghost text for slash commands (they have their own popup)
        if self.text.starts_with('/') {
            self.ghost_text = None;
            return;
        }

        // Find the most recent history entry that starts with current text
        for entry in &self.input_history {
            if entry.len() > self.text.len() && entry.starts_with(&self.text) {
                self.ghost_text = Some(entry[self.text.len()..].to_string());
                return;
            }
        }
        self.ghost_text = None;
    }

    /// Accept the current ghost text (Tab completion).
    /// Appends the ghost suffix to the text and moves cursor to end.
    fn accept_ghost_text(&mut self) {
        if let Some(suffix) = self.ghost_text.take() {
            self.text.push_str(&suffix);
            self.cursor = self.text.len();
        }
    }

    /// Update popup matches based on current text (auto-show/hide).
    fn update_popup(&mut self) {
        if !self.text.starts_with('/') || self.cursor != self.text.len() {
            // Not a slash command at cursor — try path completion
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
            self.compute_ghost_text();
            return;
        }

        // Slash command at cursor — check for argument completion first
        if let Some(matches) = self.find_command_arg_matches() {
            let cmd_len = matches.1;
            let items = matches.0;
            if let Some(ref mut popup) = self.popup {
                let prev = popup.selected_item().map(String::from);
                popup.matches = items;
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
                self.popup = Some(CompletionPopup::new_command_arg(items, cmd_len));
            }
            self.compute_ghost_text();
            return;
        }

        // Full command completion
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
        self.compute_ghost_text();
    }

    /// Find slash commands matching the given prefix.
    fn find_command_matches(prefix: &str) -> Vec<String> {
        COMMANDS
            .iter()
            .filter(|c| c.starts_with(prefix))
            .map(|c| c.to_string())
            .collect()
    }

    /// Find argument completions for commands that support them.
    /// Returns `Some((matches, cmd_len))` if the current text is a command
    /// with an argument prefix, or `None` if no argument completion applies.
    ///
    /// `cmd_len` is the byte length of the command prefix including the
    /// trailing space (e.g., "/identity ".len() = 10).
    fn find_command_arg_matches(&self) -> Option<(Vec<String>, usize)> {
        // Only works when cursor is at end
        if self.cursor != self.text.len() {
            return None;
        }
        let text = &self.text;

        // /identity [arg]
        let prefix = "/identity ";
        if let Some(arg_prefix) = text.strip_prefix(prefix) {
            let matches: Vec<String> = self
                .identity_names
                .iter()
                .filter(|n| n.starts_with(arg_prefix))
                .cloned()
                .collect();
            if !matches.is_empty() || !arg_prefix.is_empty() {
                return Some((matches, prefix.len()));
            }
        }

        None
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

    // helpers

    /// Total number of display rows in the text (physical `\n` lines only).
    pub fn line_count(&self) -> usize {
        self.text.chars().filter(|&c| c == '\n').count() + 1
    }

    /// Total visual lines accounting for both `\n` and line wrapping at the given width.
    pub fn visual_line_count(&self, text_area_width: u16) -> usize {
        let text = &self.text;
        let mut count = 1usize;
        let mut display_col: u16 = 0;

        for (byte_idx, c) in text.char_indices() {
            if c == '\n' {
                count += 1;
                display_col = 0;
                continue;
            }

            let rest = &text[byte_idx + c.len_utf8()..];
            let next = rest.chars().next();
            let cw = crate::utils::char_width(c, next) as u16;

            if display_col + cw > text_area_width && display_col > 0 {
                count += 1;
                display_col = cw;
            } else {
                display_col += cw;
            }
        }

        count
    }

    /// Return (visual_row, display_col) of the cursor, accounting for line wrapping.
    /// `text_area_width` is the available width in display columns for text content
    /// (excluding the prompt prefix).
    pub fn visual_cursor_row_col(&self, text_area_width: u16) -> (usize, usize) {
        let cursor = self.cursor.min(self.text.len());
        let cursor = editor::snap_to_char_boundary(&self.text, cursor);

        let mut visual_row = 0usize;
        let mut display_col: u16 = 0;

        for (byte_idx, c) in self.text.char_indices() {
            if byte_idx >= cursor {
                break;
            }

            if c == '\n' {
                visual_row += 1;
                display_col = 0;
                continue;
            }

            let rest = &self.text[byte_idx + c.len_utf8()..];
            let next = rest.chars().next();
            let cw = crate::utils::char_width(c, next) as u16;

            if display_col + cw > text_area_width && display_col > 0 {
                visual_row += 1;
                display_col = cw;
            } else {
                display_col += cw;
            }
        }

        (visual_row, display_col as usize)
    }

    /// Remove the last image marker from text and images.
    pub fn remove_last_image(&mut self) {
        if let Some(pos) = self.text.rfind(IMAGE_MARKER) {
            let idx = self.text[..pos]
                .chars()
                .filter(|&c| c == IMAGE_MARKER)
                .count();
            self.text.drain(pos..pos + IMAGE_MARKER.len_utf8());
            if idx < self.images.len() {
                self.images.remove(idx);
            }
        }
    }

    fn insert_char(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor = self.next_grapheme_boundary();
    }

    /// Insert an image marker at the cursor position.
    /// The marker renders as a styled `[img]` span and is deletable like normal text.
    /// Corresponding image data is stored in the `images` Vec.
    pub fn insert_image(&mut self, media_type: String, base64_data: String) {
        // Insert the marker character
        self.text.insert(self.cursor, IMAGE_MARKER);
        // Count existing markers before the new one to determine index
        let before = &self.text[..self.cursor];
        let idx = before.chars().filter(|&c| c == IMAGE_MARKER).count();
        self.images.insert(idx, (media_type, base64_data));
        self.cursor += IMAGE_MARKER.len_utf8();
    }

    /// Extract all image data from the text buffer, removing markers.
    /// Returns (image_data, cleaned_text_without_markers).
    pub fn extract_images(&mut self) -> (Vec<(String, String)>, String) {
        let images = std::mem::take(&mut self.images);
        let cleaned: String = self.text.chars().filter(|&c| c != IMAGE_MARKER).collect();
        (images, cleaned)
    }

    /// Number of image markers currently in the text buffer.
    pub fn image_count(&self) -> usize {
        self.images.len()
    }

    /// After text mutation (backspace, delete_word, etc.), re-sync the images list
    /// so it has exactly as many entries as there are IMAGE_MARKER chars in text.
    /// Entries are matched in order: images[i] corresponds to the i-th marker.
    fn sync_images(&mut self) {
        let count = self.text.chars().filter(|&c| c == IMAGE_MARKER).count();
        while self.images.len() > count {
            self.images.pop();
        }
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
        self.sync_images();
    }

    fn prev_grapheme_boundary(&self) -> usize {
        editor::prev_grapheme_boundary(&self.text, self.cursor)
    }

    fn next_grapheme_boundary(&self) -> usize {
        editor::next_grapheme_boundary(&self.text, self.cursor)
    }

    /// Switch identity and reload history scope.
    /// Saves current history, switches identity, loads that identity's history.
    pub fn set_identity(&mut self, identity: Option<String>) {
        // Save current history before switching
        let old_id = self.active_identity.as_deref();
        history::save_history(&self.input_history, old_id);

        self.active_identity = identity;
        self.input_history = history::load_history(self.active_identity.as_deref());
        self.history_index = None;
    }
}

// ── Component trait implementation ─────────────────────────────────────────

impl Component for InputState {
    fn mount(&mut self) {
        // History is loaded in new() — no additional mount work needed
    }

    fn unmount(&mut self) {
        // Persist input history to disk on shutdown
        let identity = self.active_identity.as_deref();
        history::save_history(&self.input_history, identity);
    }

    fn update(&mut self, cmd: UiCommand) {
        match cmd {
            UiCommand::PasteImage {
                media_type,
                base64_data,
                ..
            } => {
                self.insert_image(media_type, base64_data);
            }
            UiCommand::SetMode(mode) => {
                self.app_mode = mode;
            }
            UiCommand::SetInputIdentity(identity) => {
                self.set_identity(identity);
            }
            _ => {} // Ignore non-input commands
        }
    }

    fn view(&mut self, area: Rect, frame: &mut Frame) {
        render(frame, area, self, &self.app_mode);
    }

    fn needs_render(&self) -> bool {
        true // Input area is cheap to render and changes on every keypress
    }
}

// Rendering

/// Split a single physical line into visual segments that fit within `max_width`.
fn wrap_physical_line<'a>(line: &'a str, max_width: u16) -> Vec<&'a str> {
    let full_width = crate::utils::display_width(line) as u16;
    if full_width <= max_width {
        return vec![line];
    }

    let mut segments: Vec<&str> = Vec::new();
    let mut seg_start: usize = 0;
    let mut display_col: u16 = 0;

    for (byte_idx, c) in line.char_indices() {
        let rest = &line[byte_idx + c.len_utf8()..];
        let next = rest.chars().next();
        let cw = crate::utils::char_width(c, next) as u16;

        if display_col + cw > max_width && display_col > 0 {
            segments.push(&line[seg_start..byte_idx]);
            seg_start = byte_idx;
            display_col = 0;
        }

        display_col += cw;
    }

    // Last segment (or full line if no wrapping occurred)
    if seg_start < line.len() {
        segments.push(&line[seg_start..]);
    } else if seg_start == 0 && line.is_empty() {
        segments.push(line);
    }

    segments
}

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
                        "\u{f054} ",
                        Cow::Borrowed("type to steer agent\u{2026}"),
                        theme::TEXT_DIM,
                    )
                } else {
                    (
                        theme::WARNING,
                        "\u{f054} ",
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
    // Prompt icons ( /  / ) have unicode-width=1 (PUA), + trailing space = 2 total.
    // Use 2, NOT display_width() which overrides PUA→2 for Nerd Font text content.
    let prompt_display_width: u16 = 2u16;
    let text_area_width = content_area.width.saturating_sub(prompt_display_width);

    // Build lines from text, wrapping each physical line to fit text_area_width
    let all_lines: Vec<&str> = display_text.split('\n').collect();
    let mut lines: Vec<Line> = Vec::new();
    let mut is_first_visual_line = true;

    for line_str in &all_lines {
        let visual_segments = wrap_physical_line(line_str, text_area_width);

        for segment in &visual_segments {
            // Build spans for this segment, splitting by image markers
            let spans = build_line_spans(segment, text_color);

            // First visual line gets the prompt prefix; continuation lines are indented
            let mut prefix_spans: Vec<Span<'static>> = if is_first_visual_line {
                is_first_visual_line = false;
                vec![Span::styled(prompt, Style::new().fg(theme::ACCENT_DIM))]
            } else {
                vec![Span::styled(
                    " ".repeat(prompt_display_width as usize),
                    Style::new().fg(theme::ACCENT_DIM),
                )]
            };
            prefix_spans.extend(spans);
            lines.push(Line::from(prefix_spans));
        }
    }

    // Append ghost text (inline autosuggestion) to the last line
    if let Some(ref ghost) = state.ghost_text
        && !ghost.is_empty()
        && let Some(last_line) = lines.last_mut()
    {
        last_line.spans.push(Span::styled(
            ghost.clone(),
            Style::new().fg(theme::TEXT_DIM),
        ));
    }

    let p = Paragraph::new(Text::from(lines)).style(Style::new().bg(theme::BG).fg(theme::TEXT));

    frame.render_widget(p, content_area);

    // Draw border with inline image indicator
    let image_suffix = if state.image_count() > 0 {
        format!(" {} ", state.image_count())
    } else {
        String::new()
    };
    draw_border(frame, area, border_color, &image_suffix);

    // Draw completion popup if active
    if let Some(ref popup) = state.popup {
        render_popup(frame, area, popup);
    }
}

/// Split a line by IMAGE_MARKER chars, returning spans with `[img]` styled inline.
fn build_line_spans(line: &str, text_color: Color) -> Vec<Span<'static>> {
    if !line.contains(IMAGE_MARKER) {
        return vec![Span::styled(line.to_string(), Style::new().fg(text_color))];
    }

    let mut spans = Vec::new();
    let mut remaining = line;
    while let Some(pos) = remaining.find(IMAGE_MARKER) {
        // Text before the marker
        if pos > 0 {
            spans.push(Span::styled(
                remaining[..pos].to_string(),
                Style::new().fg(text_color),
            ));
        }
        // The marker itself → render as styled "[img]"
        spans.push(Span::styled(
            "[img]",
            Style::new()
                .fg(theme::ACCENT)
                .bg(theme::BG)
                .add_modifier(Modifier::BOLD),
        ));
        remaining = &remaining[pos + IMAGE_MARKER.len_utf8()..];
    }
    // Trailing text after the last marker
    if !remaining.is_empty() {
        spans.push(Span::styled(
            remaining.to_string(),
            Style::new().fg(text_color),
        ));
    }
    spans
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

fn draw_border(frame: &mut Frame, area: Rect, color: Color, suffix: &str) {
    let style = Style::new().fg(color);
    let suffix_style = Style::new().fg(theme::WARNING);

    // Top border with optional right-aligned suffix
    let border_width = area.width as usize;
    let suffix_display_width = crate::utils::display_width(suffix);
    let border_chars = "─".repeat(border_width.saturating_sub(suffix_display_width));
    let line = if suffix.is_empty() {
        Line::from(vec![Span::styled(border_chars, style)])
    } else {
        Line::from(vec![
            Span::styled(border_chars, style),
            Span::styled(suffix.to_string(), suffix_style),
        ])
    };

    let top = Rect {
        y: area.y,
        height: 1,
        ..area
    };
    frame.render_widget(Paragraph::new(line), top);
}

#[cfg(test)]
mod tests {
    use super::editor;
    use super::*;

    #[test]
    fn test_popup_opens_on_slash() {
        let mut input = InputState::with_identity(None);
        input.text = "/".into();
        input.cursor = 1;
        input.update_popup();
        assert!(input.popup.is_some(), "Popup should open for '/'");
        let popup = input.popup.as_ref().unwrap();
        assert_eq!(popup.matches.len(), COMMANDS.len());
    }

    #[test]
    fn test_popup_filters_on_typing() {
        let mut input = InputState::with_identity(None);
        input.text = "/he".into();
        input.cursor = 3;
        input.update_popup();
        assert!(input.popup.is_some());
        let popup = input.popup.as_ref().unwrap();
        assert_eq!(popup.matches, vec!["/help"]);
    }

    #[test]
    fn test_popup_no_match() {
        let mut input = InputState::with_identity(None);
        input.text = "/xyz".into();
        input.cursor = 4;
        input.update_popup();
        assert!(input.popup.is_none(), "No popup for no matches");
    }

    #[test]
    fn test_popup_not_for_non_slash() {
        let mut input = InputState::with_identity(None);
        input.text = "hello".into();
        input.cursor = 5;
        input.update_popup();
        assert!(input.popup.is_none());
    }

    #[test]
    fn test_popup_navigation() {
        let mut input = InputState::with_identity(None);
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
        let mut input = InputState::with_identity(None);
        input.text = "/he".into();
        input.cursor = 3;
        input.update_popup();
        input.confirm_popup();
        assert_eq!(input.text, "/help");
        assert!(input.popup.is_none());
    }

    #[test]
    fn test_popup_dismiss_on_non_slash_edit() {
        let mut input = InputState::with_identity(None);
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
        let mut input = InputState::with_identity(None);
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
        let mut input = InputState::with_identity(None);
        input.text = "./".into();
        input.cursor = 2;
        input.update_popup();
        // Should list cwd entries
        assert!(input.popup.is_some(), "Popup should open for './'");
    }

    #[test]
    fn test_path_completion_confirm_replaces_token() {
        // Confirming a path completion should only replace the path token, not entire text
        let mut input = InputState::with_identity(None);
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
            ..InputState::with_identity(None)
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
            ..InputState::with_identity(None)
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
            ..InputState::with_identity(None)
        };
        let result = input.extract_path_prefix();
        assert!(result.is_none());
    }

    #[test]
    fn test_multiline_insert_newline() {
        let mut input = InputState::with_identity(None);
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
        let mut input = InputState::with_identity(None);
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
        let mut input = InputState::with_identity(None);
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
        let mut input = InputState::with_identity(None);
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
        let mut input = InputState::with_identity(None);

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
        let mut input = InputState::with_identity(None);
        input.text = "hello world".into();
        input.cursor = input.text.len();

        let key = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);
        let consumed = input.handle_key(key);
        assert!(consumed, "Ctrl+W should be consumed");
        assert_eq!(input.text, "hello ");
    }

    // ── Editor primitive tests ──────────────────────────────────────────

    #[test]
    fn test_editor_prev_grapheme_boundary_ascii() {
        // ASCII text — boundaries are byte-aligned
        assert_eq!(editor::prev_grapheme_boundary("hello", 5), 4); // 'o'
        assert_eq!(editor::prev_grapheme_boundary("hello", 1), 0); // 'h'
        assert_eq!(editor::prev_grapheme_boundary("hello", 0), 0); // start
    }

    #[test]
    fn test_editor_prev_grapheme_boundary_cjk() {
        // '你好世界' bytes: 你=3, 好=3, 世=3, 界=3 (total 12)
        let s = "你好世界";
        assert_eq!(editor::prev_grapheme_boundary(s, 12), 9); // '界' start
        assert_eq!(editor::prev_grapheme_boundary(s, 9), 6); // '世' start
        assert_eq!(editor::prev_grapheme_boundary(s, 6), 3); // '好' start
        assert_eq!(editor::prev_grapheme_boundary(s, 3), 0); // '你' start
        assert_eq!(editor::prev_grapheme_boundary(s, 0), 0); // start

        // Mid-byte positions (inside a CJK char) should snap to the char's start
        assert_eq!(editor::prev_grapheme_boundary(s, 10), 9); // inside 界, snap to 界 start
        assert_eq!(editor::prev_grapheme_boundary(s, 8), 6); // inside 世, snap to 世 start
        assert_eq!(editor::prev_grapheme_boundary(s, 5), 3); // inside 好, snap to 好 start
    }

    #[test]
    fn test_editor_prev_grapheme_boundary_mixed() {
        let s = "hello过world";
        // '过' = 3 bytes, total = 5 + 3 + 5 = 13
        assert_eq!(editor::prev_grapheme_boundary(s, 13), 12); // 'd'
        assert_eq!(editor::prev_grapheme_boundary(s, 12), 11); // 'l'
        assert_eq!(editor::prev_grapheme_boundary(s, 8), 5); // after 过 → 过 start
        assert_eq!(editor::prev_grapheme_boundary(s, 7), 5); // inside 过 → 过 start
        assert_eq!(editor::prev_grapheme_boundary(s, 6), 5); // inside 过 → 过 start
        assert_eq!(editor::prev_grapheme_boundary(s, 5), 4); // 'o'
        assert_eq!(editor::prev_grapheme_boundary(s, 4), 3); // 'l'
    }

    #[test]
    fn test_editor_next_grapheme_boundary_ascii() {
        assert_eq!(editor::next_grapheme_boundary("hello", 0), 1); // 'h' → after 'h'
        assert_eq!(editor::next_grapheme_boundary("hello", 4), 5); // 'o' → end
        assert_eq!(editor::next_grapheme_boundary("hello", 5), 5); // at end
    }

    #[test]
    fn test_editor_next_grapheme_boundary_cjk() {
        let s = "你好世界";
        assert_eq!(editor::next_grapheme_boundary(s, 0), 3); // '你' → after 你
        assert_eq!(editor::next_grapheme_boundary(s, 3), 6); // '好' → after 好
        assert_eq!(editor::next_grapheme_boundary(s, 6), 9); // '世' → after 世
        assert_eq!(editor::next_grapheme_boundary(s, 9), 12); // '界' → end
        assert_eq!(editor::next_grapheme_boundary(s, 12), 12); // at end

        // Mid-byte positions should advance to the end of the current char
        assert_eq!(editor::next_grapheme_boundary(s, 7), 9); // inside 世 → 世 end
        assert_eq!(editor::next_grapheme_boundary(s, 4), 6); // inside 好 → 好 end
    }

    #[test]
    fn test_editor_next_grapheme_boundary_mixed() {
        let s = "hello过world";
        assert_eq!(editor::next_grapheme_boundary(s, 0), 1); // 'h' → 1
        assert_eq!(editor::next_grapheme_boundary(s, 5), 8); // 过 → after 过 (bytes 5-7)
        assert_eq!(editor::next_grapheme_boundary(s, 6), 8); // inside 过 → after 过
        assert_eq!(editor::next_grapheme_boundary(s, 8), 9); // 'w'
        assert_eq!(editor::next_grapheme_boundary(s, 13), 13); // end
    }

    #[test]
    fn test_editor_next_grapheme_boundary_past_end() {
        assert_eq!(editor::next_grapheme_boundary("hi", 100), 2); // len=2, min with len
        assert_eq!(editor::next_grapheme_boundary("hi", 3), 2); // past end
    }

    // ── CJK cursor_row_col edge cases ──────────────────────────────────

    #[test]
    fn test_cursor_row_col_cjk_single_line() {
        let mut input = InputState::with_identity(None);
        input.text = "你好世界".into();

        // At the start of the first CJK char
        input.cursor = 0;
        let (row, col) = input.cursor_row_col();
        assert_eq!(row, 0);
        assert_eq!(col, 0);

        // After "你" (bytes 0-2), byte 3
        input.cursor = 3;
        let (row, col) = input.cursor_row_col();
        assert_eq!(row, 0);
        assert_eq!(col, 3);

        // End of text
        input.cursor = input.text.len();
        let (row, col) = input.cursor_row_col();
        assert_eq!(row, 0);
        assert_eq!(col, 12);
    }

    #[test]
    fn test_cursor_row_col_cjk_inside_char_no_panic() {
        // Regression: cursor_row_col must never panic when cursor is inside
        // a multi-byte character — snap_to_char_boundary handles it.
        let mut input = InputState::with_identity(None);
        input.text = "你好世界".into();

        // Manually set cursor to mid-byte positions inside CJK chars
        // 你=bytes 0-2, 好=bytes 3-5, 世=bytes 6-8, 界=bytes 9-11
        for bad_cursor in [1, 2, 4, 5, 7, 8, 10, 11] {
            input.cursor = bad_cursor;
            let (row, col) = input.cursor_row_col(); // must not panic
            assert!(row == 0, "row should be 0 for bad cursor {bad_cursor}");
            // col should be a valid byte offset (snapped to prev boundary)
            assert!(
                input.text.is_char_boundary(col),
                "col {col} is not a char boundary (bad cursor was {bad_cursor})"
            );
        }
    }

    #[test]
    fn test_cursor_row_col_cjk_multiline() {
        let mut input = InputState::with_identity(None);
        input.text = "你好\n世界".into(); // 你=3, 好=3, \n=1, 世=3, 界=3

        // End of first line "你好"
        input.cursor = 6;
        assert_eq!(input.cursor_row_col(), (0, 6));

        // Start of second line "世界"
        input.cursor = 7;
        assert_eq!(input.cursor_row_col(), (1, 0));

        // After "世" on line 1
        input.cursor = 10;
        assert_eq!(input.cursor_row_col(), (1, 3));

        // End of text
        input.cursor = 13;
        assert_eq!(input.cursor_row_col(), (1, 6));

        // Inside multi-byte char on line 1 — must not panic
        input.cursor = 8;
        let (row, col) = input.cursor_row_col();
        assert_eq!(row, 1);
        assert!(
            input.text.is_char_boundary(col),
            "col {col} is not a char boundary"
        );
    }

    #[test]
    fn test_cursor_row_col_mixed_ascii_cjk() {
        let mut input = InputState::with_identity(None);
        input.text = "abc你好xyz\n第二行".into();
        // abc=3, 你=3, 好=3, xyz=3 → total 12 bytes before \n
        // \n=1, 第=3, 二=3, 行=3 → total 10 bytes

        // Cursor at end of first line
        input.cursor = 12;
        assert_eq!(input.cursor_row_col(), (0, 12));

        // Cursor at start of second line
        input.cursor = 13;
        assert_eq!(input.cursor_row_col(), (1, 0));

        // Cursor after "第二" on second line
        input.cursor = 19;
        assert_eq!(input.cursor_row_col(), (1, 6));
    }

    #[test]
    fn test_cursor_row_col_empty_lines() {
        let mut input = InputState::with_identity(None);
        input.text = "\n\n".into(); // three empty lines

        input.cursor = 0;
        assert_eq!(input.cursor_row_col(), (0, 0));

        input.cursor = 1;
        assert_eq!(input.cursor_row_col(), (1, 0));

        input.cursor = 2;
        assert_eq!(input.cursor_row_col(), (2, 0));
    }

    // ── CJK cursor up/down ──────────────────────────────────────────────

    #[test]
    fn test_move_cursor_up_down_cjk_only_lines() {
        let mut input = InputState::with_identity(None);
        // 你好 (6 bytes) 世界 (6 bytes) — two lines of CJK
        input.text = "你好\n世界".into();

        // Start at end of line 1
        input.cursor = input.text.len(); // 13
        assert_eq!(input.cursor_row_col(), (1, 6));

        // Move up — should maintain column position, snapped to char boundary
        assert!(input.move_cursor_up());
        let (row, col) = input.cursor_row_col();
        assert_eq!(row, 0);
        // Line 0 is 6 bytes long; col 6 is valid (end of "你好")
        assert_eq!(col, 6);

        // Move up at top — no-op
        assert!(!input.move_cursor_up());

        // Move down
        assert!(input.move_cursor_down());
        assert_eq!(input.cursor_row_col(), (1, 6));
    }

    #[test]
    fn test_move_cursor_up_down_cjk_short_upper_line() {
        let mut input = InputState::with_identity(None);
        // Upper line shorter than lower — snap to shorter line's end
        input.text = "hi\n你好世界".into(); // "hi" (2), "\n" (1), "你好世界" (12)

        // On lower line, column 12 (past end of "hi")
        input.cursor = 15; // "你好世界" end
        assert_eq!(input.cursor_row_col(), (1, 12));

        // Move up — column should clamp to upper line length (2)
        assert!(input.move_cursor_up());
        assert_eq!(input.cursor_row_col(), (0, 2));

        // Move down — target col=2 on lower line → byte 5 (inside '你'), snap to 3 (你 start) → col=0
        assert!(input.move_cursor_down());
        let (row, col) = input.cursor_row_col();
        assert_eq!(row, 1);
        assert_eq!(col, 0); // snapped to start of '你'
    }

    #[test]
    fn test_move_cursor_up_down_cjk_short_lower_line() {
        let mut input = InputState::with_identity(None);
        // Upper line longer than lower
        input.text = "你好世界\nhi".into(); // "你好世界" (12), "\n" (1), "hi" (2)

        // On lower line, column 2
        input.cursor = 15;
        assert_eq!(input.cursor_row_col(), (1, 2));

        // Move up — go to upper line, target col=2 falls inside '你' (bytes 0-2), snap to 0
        assert!(input.move_cursor_up());
        assert_eq!(input.cursor_row_col(), (0, 0));

        // Move down — target col=0 on lower line
        assert!(input.move_cursor_down());
        assert_eq!(input.cursor_row_col(), (1, 0));
    }

    #[test]
    fn test_move_cursor_up_down_mixed_width_multiline() {
        // Three lines: ASCII, CJK, mixed
        // "abc" (3) + "\n" (1) + "def" (3) + "你" (3) + "好" (3) + "\n" (1) + "ghi" (3) + "世" (3) + "界" (3) + "jkl" (3)
        // bytes: 0-2, 3, 4-6, 7-9, 10-12, 13, 14-16, 17-19, 20-22, 23-25 = 26 total
        let text = "abc\ndef你好\nghi世界jkl";
        let mut input = InputState::with_identity(None);
        input.text = text.into();

        // Start on line 2, end of line
        input.cursor = input.text.len(); // 26
        assert_eq!(input.cursor_row_col(), (2, 12)); // col 12 = end of "ghi世界jkl"

        // Move up to line 1
        assert!(input.move_cursor_up());
        let (row, col) = input.cursor_row_col();
        assert_eq!(row, 1);
        assert!(
            input.text.is_char_boundary(input.cursor),
            "cursor {} is not a char boundary (col={col})",
            input.cursor,
        );

        // Move up to line 0
        assert!(input.move_cursor_up());
        let (row, col) = input.cursor_row_col();
        assert_eq!(row, 0);
        assert!(
            input.text.is_char_boundary(input.cursor),
            "cursor {} is not a char boundary (col={col})",
            input.cursor,
        );
    }

    #[test]
    fn test_move_cursor_up_down_cjk_mid_char_col() {
        // When moving down from a line where the cursor column falls inside
        // a CJK char on the next line, snap_to_char_boundary should correct it.
        let mut input = InputState::with_identity(None);
        // Upper line:   "abcde" (5 bytes) — cursor at col 4 (inside 'de')
        // Lower line:   "你" (3 bytes) — col 4 is past end
        input.text = "abcde\n你".into();

        // On upper line at col 4
        input.cursor = 4;
        assert_eq!(input.cursor_row_col(), (0, 4));

        // Move down — col 4 on lower line (len=3), clamp to 3
        assert!(input.move_cursor_down());
        let (row, col) = input.cursor_row_col();
        assert_eq!(row, 1);
        assert_eq!(col, 3, "should clamp to end of '你'");

        // Move up — back to col 3 on upper line
        assert!(input.move_cursor_up());
        assert_eq!(input.cursor_row_col(), (0, 3));

        // Move down again — col 3 on lower line, which is end of '你'
        assert!(input.move_cursor_down());
        assert_eq!(input.cursor_row_col(), (1, 3));
    }

    #[test]
    fn test_move_cursor_up_down_snap_no_panic() {
        // Comprehensive test: exercise all multi-byte boundary scenarios
        // that previously caused panics.
        let text = "你好\nhello\n世界\nworld";
        let mut input = InputState::with_identity(None);
        input.text = text.into();

        // Walk cursor through every byte position to ensure no panic
        for byte_pos in 0..=text.len() {
            input.cursor = byte_pos;
            // Up/down should never panic regardless of cursor position
            let _ = input.move_cursor_up();
            let _ = input.move_cursor_down();
            // After any movement, cursor must be on a char boundary
            assert!(
                input.text.is_char_boundary(input.cursor),
                "cursor {} is not on a char boundary after move (start was {})",
                input.cursor,
                byte_pos,
            );
        }
    }

    // ── CJK Home / End ──────────────────────────────────────────────────

    #[test]
    fn test_home_end_cjk_line() {
        let mut input = InputState::with_identity(None);
        input.text = "hello\n你好世界\nxyz".into();

        // Home on CJK line
        input.cursor = 10; // middle of 你好世界 line
        input.move_cursor_to_line_start();
        assert_eq!(input.cursor, 6); // start of "你好世界"

        // End on CJK line
        input.move_cursor_to_line_end();
        assert_eq!(input.cursor, 18); // 6 + 12

        // Home on ASCII line after CJK
        input.cursor = 20; // middle of "xyz"
        input.move_cursor_to_line_start();
        assert_eq!(input.cursor, 19); // start of "xyz"

        input.move_cursor_to_line_end();
        assert_eq!(input.cursor, 22); // end of "xyz"
    }

    // ── CJK Left / Right arrow via handle_key ───────────────────────────

    #[test]
    fn test_left_right_arrow_cjk() {
        let mut input = InputState::with_identity(None);
        input.text = "你好".into();

        // Start at end
        input.cursor = 6;

        // Left — should go to start of '好' (byte 3)
        let key = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        assert!(input.handle_key(key));
        assert_eq!(input.cursor, 3, "Left should go to byte 3 (好 start)");

        // Left — should go to start of '你' (byte 0)
        let key = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        assert!(input.handle_key(key));
        assert_eq!(input.cursor, 0, "Left should go to byte 0 (你 start)");

        // Left at start — no-op
        let key = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        assert!(input.handle_key(key));
        assert_eq!(input.cursor, 0);

        // Right — should go to start of '好' (byte 3)
        let key = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        assert!(input.handle_key(key));
        assert_eq!(input.cursor, 3, "Right should go to byte 3 (好 start)");

        // Right — should go past '好' to end (byte 6)
        let key = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        assert!(input.handle_key(key));
        assert_eq!(input.cursor, 6, "Right should go to byte 6 (end)");

        // Right at end — no-op
        let key = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        assert!(input.handle_key(key));
        assert_eq!(input.cursor, 6);

        // Mid-byte position — Left should snap to char start
        input.cursor = 5; // inside '好' (bytes 3-5)
        let key = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        assert!(input.handle_key(key));
        assert_eq!(
            input.cursor, 3,
            "Left from inside '好' should snap to '好' start"
        );
    }

    #[test]
    fn test_left_right_arrow_mixed_cjk_ascii() {
        let mut input = InputState::with_identity(None);
        input.text = "abc你好def".into(); // bytes: a(0) b(1) c(2) 你(3-5) 好(6-8) d(9) e(10) f(11)

        // Start at end (byte 12)
        input.cursor = input.text.len();

        // Walk left through every character boundary
        // 12→11(f), 11→10(e), 10→9(d), 9→6(好), 6→3(你), 3→2(c), 2→1(b), 1→0(a)
        let expected_left = [11, 10, 9, 6, 3, 2, 1, 0];
        for &exp in &expected_left {
            let key = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
            assert!(input.handle_key(key));
            assert_eq!(
                input.cursor, exp,
                "Left should go to byte {exp}, got {}",
                input.cursor
            );
            assert!(
                input.text.is_char_boundary(input.cursor),
                "cursor {} not on char boundary",
                input.cursor
            );
        }

        // Walk right back — each step moves to the next grapheme cluster end
        // 0→1(a), 1→2(b), 2→3(c), 3→6(你), 6→9(好), 9→10(d), 10→11(e), 11→12(f→end)
        let expected_right = [1, 2, 3, 6, 9, 10, 11, 12];
        for &exp in &expected_right {
            let key = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
            assert!(input.handle_key(key));
            assert_eq!(
                input.cursor, exp,
                "Right should go to byte {exp}, got {}",
                input.cursor
            );
        }
    }

    // ── CJK Backspace / Delete ──────────────────────────────────────────

    #[test]
    fn test_backspace_cjk() {
        let mut input = InputState::with_identity(None);
        input.text = "你好".into();
        input.cursor = input.text.len(); // 6

        // Backspace should delete '好' (bytes 3-5)
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        assert!(input.handle_key(key));
        assert_eq!(input.text, "你");
        assert_eq!(input.cursor, 3);

        // Backspace should delete '你' (bytes 0-2)
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        assert!(input.handle_key(key));
        assert_eq!(input.text, "");
        assert_eq!(input.cursor, 0);

        // Backspace at start — no-op
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        assert!(input.handle_key(key));
        assert_eq!(input.text, "");
        assert_eq!(input.cursor, 0);
    }

    #[test]
    fn test_backspace_mixed_cjk_ascii() {
        let mut input = InputState::with_identity(None);
        input.text = "abc你好xyz".into();
        input.cursor = input.text.len(); // 12 = 3+6+3

        // Delete 'z'
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        assert!(input.handle_key(key));
        assert_eq!(input.text, "abc你好xy");
        assert_eq!(input.cursor, 11);

        // Delete 'y'
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        assert!(input.handle_key(key));
        assert_eq!(input.text, "abc你好x");
        assert_eq!(input.cursor, 10);

        // Delete 'x'
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        assert!(input.handle_key(key));
        assert_eq!(input.text, "abc你好");
        assert_eq!(input.cursor, 9);

        // Delete '好' (multi-byte CJK)
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        assert!(input.handle_key(key));
        assert_eq!(input.text, "abc你");
        assert_eq!(input.cursor, 6);

        // Delete '你'
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        assert!(input.handle_key(key));
        assert_eq!(input.text, "abc");
        assert_eq!(input.cursor, 3);
    }

    #[test]
    fn test_delete_cjk() {
        let mut input = InputState::with_identity(None);
        input.text = "你好".into();
        input.cursor = 0;

        // Delete should remove '你' (bytes 0-2)
        let key = KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE);
        assert!(input.handle_key(key));
        assert_eq!(input.text, "好");
        assert_eq!(input.cursor, 0);

        // Delete should remove '好' (bytes 0-2)
        let key = KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE);
        assert!(input.handle_key(key));
        assert_eq!(input.text, "");
        assert_eq!(input.cursor, 0);

        // Delete at end — no-op
        let key = KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE);
        assert!(input.handle_key(key));
        assert_eq!(input.text, "");
        assert_eq!(input.cursor, 0);
    }

    #[test]
    fn test_insert_cjk_characters() {
        let mut input = InputState::with_identity(None);
        input.text = "".into();
        input.cursor = 0;

        // Insert a CJK character
        input.insert_char('你');
        assert_eq!(input.text, "你");
        assert_eq!(input.cursor, 3);

        // Insert another
        input.insert_char('好');
        assert_eq!(input.text, "你好");
        assert_eq!(input.cursor, 6);

        // Insert ASCII
        input.insert_char('a');
        assert_eq!(input.text, "你好a");
        assert_eq!(input.cursor, 7);
    }

    #[test]
    fn test_backspace_cjk_at_valid_boundary() {
        // Backspace correctly deletes multi-byte CJK characters
        let mut input = InputState::with_identity(None);
        input.text = "你好世界".into();

        // Cursor after '界' (end of text)
        input.cursor = 12;
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        assert!(input.handle_key(key));
        assert_eq!(input.text, "你好世");
        assert_eq!(input.cursor, 9, "cursor should be at start of '世'");

        // Backspace again — removes '世'
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        assert!(input.handle_key(key));
        assert_eq!(input.text, "你好");
        assert_eq!(input.cursor, 6);
    }

    #[test]
    fn test_move_cursor_up_down_cjk_no_panic() {
        // Regression: moving cursor up/down between lines with different
        // character widths (ASCII vs CJK) used to place cursor inside a
        // multi-byte char boundary, causing a panic.
        // 你好世界 = 12 bytes, "hello" = 5 bytes
        let mut input = InputState::with_identity(None);
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
        assert_eq!(editor::snap_to_char_boundary(s, 5), 5); // 'h','e','l','l','o' = ascii
        assert_eq!(editor::snap_to_char_boundary(s, 6), 5); // inside '过', snap back
        assert_eq!(editor::snap_to_char_boundary(s, 7), 5); // inside '过', snap back
        assert_eq!(editor::snap_to_char_boundary(s, 8), 8); // 'w', on boundary
        assert_eq!(editor::snap_to_char_boundary(s, 100), s.len()); // past end
    }

    #[test]
    fn test_ghost_text_basic() {
        let mut input = InputState::with_identity(None);
        input.input_history = vec!["hello world".into(), "hello there".into()];
        input.text = "hel".into();
        input.cursor = 3;
        input.update_popup(); // also computes ghost text
        assert_eq!(input.ghost_text.as_deref(), Some("lo world"));
    }

    #[test]
    fn test_ghost_text_most_recent_first() {
        let mut input = InputState::with_identity(None);
        // History is ordered newest-first
        input.input_history = vec!["hello there".into(), "hello world".into()];
        input.text = "hel".into();
        input.cursor = 3;
        input.update_popup();
        // Should match "hello there" (most recent)
        assert_eq!(input.ghost_text.as_deref(), Some("lo there"));
    }

    #[test]
    fn test_ghost_text_no_match() {
        let mut input = InputState::with_identity(None);
        input.input_history = vec!["goodbye world".into()];
        input.text = "hel".into();
        input.cursor = 3;
        input.update_popup();
        assert!(input.ghost_text.is_none());
    }

    #[test]
    fn test_ghost_text_empty_input() {
        let mut input = InputState::with_identity(None);
        input.input_history = vec!["hello".into()];
        input.text = "".into();
        input.cursor = 0;
        input.update_popup();
        assert!(input.ghost_text.is_none());
    }

    #[test]
    fn test_ghost_text_exact_match() {
        let mut input = InputState::with_identity(None);
        input.input_history = vec!["hello".into()];
        input.text = "hello".into();
        input.cursor = 5;
        input.update_popup();
        // Exact match — no ghost text (no suffix to show)
        assert!(input.ghost_text.is_none());
    }

    #[test]
    fn test_ghost_text_tab_accept() {
        let mut input = InputState::with_identity(None);
        input.input_history = vec!["hello world".into()];
        input.text = "hel".into();
        input.cursor = 3;
        input.update_popup();
        assert_eq!(input.ghost_text.as_deref(), Some("lo world"));

        // Simulate Tab key
        let key = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        input.handle_key(key);
        assert_eq!(input.text, "hello world");
        assert_eq!(input.cursor, 11);
        assert!(input.ghost_text.is_none());
    }

    #[test]
    fn test_ghost_text_no_slash_commands() {
        let mut input = InputState::with_identity(None);
        input.input_history = vec!["/help me".into()];
        input.text = "/he".into();
        input.cursor = 3;
        input.update_popup();
        // Slash commands should use popup, not ghost text
        assert!(input.ghost_text.is_none());
    }

    #[test]
    fn test_ghost_text_cursor_not_at_end() {
        let mut input = InputState::with_identity(None);
        input.input_history = vec!["hello world".into()];
        input.text = "hel".into();
        input.cursor = 2; // not at end
        input.update_popup();
        assert!(input.ghost_text.is_none());
    }

    #[test]
    fn test_ghost_text_clears_on_reset() {
        let mut input = InputState::with_identity(None);
        input.input_history = vec!["hello world".into()];
        input.text = "hel".into();
        input.cursor = 3;
        input.update_popup();
        assert!(input.ghost_text.is_some());

        input.reset();
        assert!(input.ghost_text.is_none());
    }

    #[test]
    fn test_ghost_text_typing_updates() {
        let mut input = InputState::with_identity(None);
        input.input_history = vec!["hello world".into()];

        input.text = "h".into();
        input.cursor = 1;
        input.update_popup();
        assert_eq!(input.ghost_text.as_deref(), Some("ello world"));

        input.text = "he".into();
        input.cursor = 2;
        input.update_popup();
        assert_eq!(input.ghost_text.as_deref(), Some("llo world"));

        input.text = "hel".into();
        input.cursor = 3;
        input.update_popup();
        assert_eq!(input.ghost_text.as_deref(), Some("lo world"));

        input.text = "hello w".into();
        input.cursor = 7;
        input.update_popup();
        assert_eq!(input.ghost_text.as_deref(), Some("orld"));
    }

    #[test]
    fn test_ghost_text_full_flow_with_handle_key() {
        // Simulate the real user flow using handle_key
        let mut input = InputState::with_identity(None);
        input.input_history = vec!["hello world".into()];

        // Type "hel" using handle_key
        for c in ['h', 'e', 'l'] {
            let key = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
            input.handle_key(key);
        }
        assert_eq!(input.text, "hel");
        assert_eq!(input.cursor, 3);
        assert_eq!(
            input.ghost_text.as_deref(),
            Some("lo world"),
            "ghost text should show after typing 'hel'"
        );

        // Tab to accept ghost text
        let key = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        input.handle_key(key);
        assert_eq!(input.text, "hello world");
        assert_eq!(input.cursor, 11);
        assert!(
            input.ghost_text.is_none(),
            "ghost text should be cleared after Tab"
        );
    }

    #[test]
    fn test_up_down_history_with_ghost_text() {
        let mut input = InputState::with_identity(None);
        // Simulate having submitted "hello world" before
        input.input_history = vec!["hello world".into()];

        // Type "hel"
        for c in ['h', 'e', 'l'] {
            let key = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
            input.handle_key(key);
        }
        assert_eq!(input.text, "hel");
        assert_eq!(input.ghost_text.as_deref(), Some("lo world"));

        // Press Up — should go to history
        let key = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        input.handle_key(key);
        assert_eq!(input.text, "hello world", "Up should show history entry");
        assert_eq!(input.history_index, Some(0));
        assert!(
            input.ghost_text.is_none(),
            "no ghost text during history nav"
        );

        // Press Down — should go back to draft
        let key = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        input.handle_key(key);
        assert_eq!(input.text, "hel", "Down should restore draft");
        assert!(input.history_index.is_none());
        assert_eq!(
            input.ghost_text.as_deref(),
            Some("lo world"),
            "ghost text should reappear after Down"
        );
    }

    #[test]
    fn test_ghost_text_after_submit_and_retype() {
        // Simulate: submit "hello world", then type "hel" -> ghost text should show
        let mut input = InputState::with_identity(None);
        // Clear history loaded from disk to isolate the test
        input.input_history.clear();

        // Type and submit "hello world"
        for c in "hello world".chars() {
            input.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(input.text, "hello world");
        input.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(input.submitted);

        // Reset (simulating what the app does after submission)
        input.reset();
        assert_eq!(input.text, "");
        assert_eq!(input.input_history, vec!["hello world"]);

        // Type "hel"
        for c in ['h', 'e', 'l'] {
            input.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(input.text, "hel");
        assert_eq!(
            input.ghost_text.as_deref(),
            Some("lo world"),
            "ghost text should show after retyping a prefix of history"
        );
    }
}
