//! Completion popup for slash commands, command arguments, and file paths.
//!
//! Displays a floating popup in the input area when the user types `/`,
//! provides filtering/selection via Tab/Up/Down keys.

/// Maximum number of items shown in the completion popup.
pub const MAX_POPUP_ITEMS: usize = 5;

/// Completion type to distinguish between command and path completions.
#[derive(Debug, Clone, PartialEq)]
pub enum CompletionType {
    /// Slash command completion (e.g., "/he" -> "/help")
    Command,
    /// Command argument completion (e.g., "/identity de" -> "/identity dev").
    /// `cmd_len` is the byte length of the command prefix including trailing space
    /// (e.g., "/identity ".len() = 10). The popup replaces text from `cmd_len` to cursor.
    CommandArg { cmd_len: usize },
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
    pub fn new_command(matches: Vec<String>) -> Self {
        Self {
            matches,
            selected: 0,
            scroll: 0,
            completion_type: CompletionType::Command,
        }
    }

    pub fn new_path(matches: Vec<String>, prefix: String, start: usize) -> Self {
        Self {
            matches,
            selected: 0,
            scroll: 0,
            completion_type: CompletionType::Path { prefix, start },
        }
    }

    pub fn new_command_arg(matches: Vec<String>, cmd_len: usize) -> Self {
        Self {
            matches,
            selected: 0,
            scroll: 0,
            completion_type: CompletionType::CommandArg { cmd_len },
        }
    }

    /// Move selection up.
    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
            if self.selected < self.scroll {
                self.scroll = self.selected;
            }
        }
    }

    /// Move selection down.
    pub fn move_down(&mut self) {
        if self.selected + 1 < self.matches.len() {
            self.selected += 1;
            if self.selected >= self.scroll + MAX_POPUP_ITEMS {
                self.scroll = self.selected - MAX_POPUP_ITEMS + 1;
            }
        }
    }

    /// Get the currently selected item.
    pub fn selected_item(&self) -> Option<&str> {
        self.matches.get(self.selected).map(|s| s.as_str())
    }

    /// Visible slice of matches for rendering.
    pub fn visible_slice(&self) -> &[String] {
        let end = (self.scroll + MAX_POPUP_ITEMS).min(self.matches.len());
        &self.matches[self.scroll..end]
    }

    /// Get the completion type.
    pub fn completion_type(&self) -> &CompletionType {
        &self.completion_type
    }
}
