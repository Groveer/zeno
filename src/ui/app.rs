//! ratatui application state machine.

//!

//! Manages the TUI event loop, layout, and coordinates between

//! the input widget, output area, status bar, and engine queries.

use std::io;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::engine::query_engine::steer_into_slot;

use super::input::{self, InputState};
use super::output::{OutputSegment, OutputState};
use super::status_bar::{self, AppMode, StatusInfo};
use super::theme;

/// Truncate a string for display, safe for multi-byte UTF-8.
/// Returns a `Cow` that borrows when no truncation is needed.
fn truncate_preview(s: &str, max_chars: usize) -> std::borrow::Cow<'_, str> {
    if s.chars().count() <= max_chars {
        std::borrow::Cow::Borrowed(s)
    } else {
        let end = s.floor_char_boundary(max_chars);
        std::borrow::Cow::Owned(format!("{}…", &s[..end]))
    }
}

/// The main TUI application state.
pub struct App {
    input: InputState,
    pub(crate) output: OutputState,
    mode: AppMode,
    pub(crate) status: StatusInfo,
    pending_query: Option<String>,
    event_rx: mpsc::UnboundedReceiver<crate::engine::tui_events::UiEvent>,
    event_tx: mpsc::UnboundedSender<crate::engine::tui_events::UiEvent>,
    should_quit: bool,
    /// When AI asks a question, this holds the oneshot sender to reply.
    ask_response_tx: Option<tokio::sync::oneshot::Sender<String>>,
    /// The question being asked (for display in input placeholder).
    ask_question: Option<String>,
    /// Queue of pending permission requests waiting to be displayed.
    /// When multiple tools request permission concurrently, earlier requests
    /// are queued and processed one at a time after the user responds.
    permission_queue: Vec<PendingPermission>,
    /// Cancellation token shared with the running LLM task.
    /// Pressing Ctrl+C while Running cancels this token instead of quitting.
    cancel_token: CancellationToken,
    /// Set to true whenever something changed that requires a re-render.
    /// The main loop skips terminal::draw() when this is false and mode is Idle.
    render_dirty: bool,
    /// When true, auto-approve all permission requests without prompting.
    /// Set when the user answers "a" (yes to all) in a permission prompt.
    permission_allow_all: bool,
    /// Queue of user messages typed while the agent is running.
    /// These are sent as "steer" to the engine when the user presses Enter.
    steer_queue: Vec<String>,
    /// Shared reference to the engine's steer slot so the TUI can inject
    /// mid-run user input into the agent loop without the engine lock.
    steer_slot: Option<std::sync::Arc<std::sync::Mutex<Option<String>>>>,
}

/// A queued permission request waiting for user response.
struct PendingPermission {
    tool_name: String,
    reason: String,
    input: String,
    response_tx: tokio::sync::oneshot::Sender<String>,
}

impl App {
    /// Maximum height the input area can grow to (in rows including border).
    const MAX_INPUT_HEIGHT: u16 = 16;

    /// Minimum height for the input area (1 border + 1 content line).
    const MIN_INPUT_HEIGHT: u16 = 3;

    pub fn new() -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        Self {
            input: InputState::new(),
            output: OutputState::new(),
            mode: AppMode::Idle,
            status: StatusInfo {
                model: String::new(),
                provider: String::new(),
                total_tokens: 0,
                context_window: 0,
                turn_count: 0,
                tool_count: 0,
                mode: AppMode::Idle,
                steer_count: 0,
            },
            event_rx,
            event_tx,
            should_quit: false,
            pending_query: None,
            ask_response_tx: None,
            ask_question: None,
            permission_queue: Vec::new(),
            cancel_token: CancellationToken::new(),
            render_dirty: true,
            permission_allow_all: false,
            steer_queue: Vec::new(),
            steer_slot: None,
        }
    }

    pub fn event_sender(&self) -> mpsc::UnboundedSender<crate::engine::tui_events::UiEvent> {
        self.event_tx.clone()
    }

    /// Set the shared steer slot from the engine so the TUI can inject
    /// mid-run user input. Called once when the engine is created.
    pub fn set_steer_slot(&mut self, slot: std::sync::Arc<std::sync::Mutex<Option<String>>>) {
        self.steer_slot = Some(slot);
    }

    /// Whether something changed since the last frame and a re-render is due.
    pub fn needs_render(&self) -> bool {
        self.render_dirty
    }

    /// Mark that a re-render is needed (called externally when status changes).
    pub fn mark_dirty(&mut self) {
        self.render_dirty = true;
    }

    /// Clear the dirty flag after rendering.
    pub fn clear_dirty(&mut self) {
        self.render_dirty = false;
    }

    pub fn set_status(&mut self, info: StatusInfo) {
        self.status = info;
        self.render_dirty = true;
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    /// Scroll up (mouse wheel / PageUp).
    pub fn scroll_up(&mut self, lines: usize) {
        self.output.scroll_up(lines);
        self.render_dirty = true;
    }

    /// Scroll down (mouse wheel / PageDown).
    pub fn scroll_down(&mut self, lines: usize) {
        self.output.scroll_down(lines);
        self.render_dirty = true;
    }

    /// Handle a bracketed-paste event: insert the pasted text into the input
    /// widget without triggering submit. Newlines are kept as-is.
    pub fn handle_paste(&mut self, text: String) {
        self.input.insert_str(&text);
        self.render_dirty = true;
    }

    /// Process keyboard events.
    pub fn handle_key(&mut self, key: KeyEvent) {
        self.render_dirty = true;
        // Global shortcuts — Ctrl+D = immediate hard quit, regardless of mode.
        // Unlike Ctrl+C which first interrupts then quits, Ctrl+D always exits
        // immediately even while running or waiting for input.
        if matches!(
            key,
            KeyEvent {
                code: KeyCode::Char('d'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }
        ) {
            self.should_quit = true;
            // Also cancel any running LLM task so the engine lock is
            // released quickly.  Without this, the quit path would have
            // to wait for the query_tui loop to notice the cancellation
            // at its next checkpoint, causing a multi-second hang.
            self.cancel_token.cancel();
            return;
        }

        match key {
            KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                match self.mode {
                    AppMode::Running | AppMode::WaitingInput => {
                        // In Running mode: if there's text in the input, clear it
                        // first (don't accidentally interrupt the agent just because
                        // the user was typing). Second Ctrl+C actually interrupts.
                        if self.mode == AppMode::Running && !self.input.text.is_empty() {
                            self.input.reset();
                            return;
                        }
                        // Interrupt the LLM task instead of quitting.
                        // Works in both Running and WaitingInput modes so
                        // Ctrl+C can escape permission prompts, ask_user, etc.
                        self.cancel_token.cancel();
                        // If we were waiting for a permission/ask_user response,
                        // drop the response channel so the receiver gets Err.
                        if self.mode == AppMode::WaitingInput {
                            self.ask_response_tx.take();
                            self.ask_question.take();
                            self.permission_queue.clear();
                        }
                    }
                    _ => {
                        self.should_quit = true;
                        self.cancel_token.cancel();
                    }
                }
                return;
            }
            KeyEvent {
                code: KeyCode::PageUp,
                ..
            } => {
                self.output.scroll_up(10);
                return;
            }
            KeyEvent {
                code: KeyCode::PageDown,
                ..
            } => {
                self.output.scroll_down(10);
                return;
            }
            _ => {}
        }

        if self.mode == AppMode::Running {
            // Allow the user to type while the agent is running.
            // On Enter, the text is "steered" into the engine's pending
            // input slot so the model sees it on the next turn.
            let consumed = self.input.handle_key(key);

            // Scroll fallback (same as idle mode)
            if !consumed {
                match key {
                    KeyEvent {
                        code: KeyCode::Up,
                        modifiers: KeyModifiers::NONE,
                        ..
                    } => {
                        self.output.scroll_up(3);
                    }
                    KeyEvent {
                        code: KeyCode::Down,
                        modifiers: KeyModifiers::NONE,
                        ..
                    } => {
                        self.output.scroll_down(3);
                    }
                    _ => {}
                }
            }

            if self.input.submitted {
                let text = self.input.text.trim().to_string();
                self.input.reset();

                if !text.is_empty() {
                    // Inject into the engine's steer slot
                    if let Some(ref slot) = self.steer_slot {
                        steer_into_slot(slot, &text);
                    }
                    self.steer_queue.push(text.clone());
                    self.status.steer_count = self.steer_queue.len();
                    self.output.push(OutputSegment::Status(format!(
                        "⇢ Steered: {} (will be injected on next turn)",
                        truncate_preview(&text, 60)
                    )));
                }
            }
            return;
        }

        let consumed = self.input.handle_key(key);

        // If input didn't consume Up/Down, scroll output instead.
        // This handles keyboard Up/Down when input has no history entry
        // to navigate to (e.g., empty history, or at the boundary).
        if !consumed {
            match key {
                KeyEvent {
                    code: KeyCode::Up,
                    modifiers: KeyModifiers::NONE,
                    ..
                } => {
                    self.output.scroll_up(3);
                }
                KeyEvent {
                    code: KeyCode::Down,
                    modifiers: KeyModifiers::NONE,
                    ..
                } => {
                    self.output.scroll_down(3);
                }
                _ => {}
            }
        }

        if self.input.submitted {
            let text = self.input.text.trim().to_string();

            if text.is_empty() {
                self.input.reset();
                return;
            }

            // If we were waiting for an ask_user or permission response, send it back
            if self.mode == AppMode::WaitingInput {
                // Don't save ask_user / permission responses to input history
                self.input.reset_without_history();

                // Let the query engine handle the "allow all" status message
                // (it sends UiEvent::Status back to us), so avoid duplication here.
                let lower = text.trim().to_lowercase();
                if matches!(lower.as_str(), "a" | "all" | "always") {
                    self.permission_allow_all = true;
                }

                // Show the user's response in the output area.
                // Use AskResponse if we have a pending ask question (ask_user tool),
                // otherwise UserInput for permission responses.
                if self.ask_question.is_some() {
                    self.output.push(OutputSegment::AskResponse(text.clone()));
                } else {
                    self.output.push(OutputSegment::UserInput(text.clone()));
                }

                if let Some(tx) = self.ask_response_tx.take() {
                    let _ = tx.send(text);
                }
                self.ask_question = None;

                // Drain queued permission requests: auto-approve if allow_all
                // is set, otherwise show the next one to the user.
                if !self.drain_permission_queue() {
                    self.mode = AppMode::Running;
                }
                return;
            }

            // Regular user input — save to history
            self.input.reset();

            if text == "/exit" || text == "/quit" {
                self.should_quit = true;
                return;
            }

            // /clear is handled by cli::commands (needs engine lock to clear history).
            // Only clear the TUI output here, the engine history is cleared in the command handler.

            self.output.push(OutputSegment::UserInput(text.clone()));
            self.pending_query = Some(text);
            self.mode = AppMode::Running;
        }
    }

    /// Process engine events (called every tick).
    pub fn process_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            self.render_dirty = true;
            use crate::engine::tui_events::UiEvent;
            match event {
                UiEvent::ClearOutput => {
                    self.output.clear();
                }
                UiEvent::TextDelta(delta) => {
                    let mut pushed = false;
                    if let Some(OutputSegment::Text(existing)) = self.output.segments.last_mut() {
                        existing.push_str(&delta);
                        pushed = true;
                    }
                    if !pushed {
                        self.output.push(OutputSegment::Text(delta));
                    } else {
                        self.output.mark_dirty();
                    }
                }
                UiEvent::ToolStart {
                    name,
                    input_summary,
                } => {
                    let display = if input_summary.is_empty() {
                        name
                    } else {
                        input_summary
                    };
                    self.output.push(OutputSegment::ToolExecuting(display));
                }
                UiEvent::ToolOutput { name: _, output } => {
                    // Replace the last ToolExecuting with ToolComplete,
                    // preserving the operation description.
                    if let Some(last) = self
                        .output
                        .segments
                        .iter_mut()
                        .rev()
                        .find(|s| matches!(s, OutputSegment::ToolExecuting(_)))
                    {
                        let summary =
                            match std::mem::replace(last, OutputSegment::Status(String::new())) {
                                OutputSegment::ToolExecuting(op) => format!("{} → {}", op, output),
                                _ => output.clone(),
                            };
                        *last = OutputSegment::ToolComplete(summary);
                    }
                }
                UiEvent::ToolError { name: _, error } => {
                    // Replace the last ToolExecuting with error,
                    // preserving the operation description.
                    if let Some(last) = self
                        .output
                        .segments
                        .iter_mut()
                        .rev()
                        .find(|s| matches!(s, OutputSegment::ToolExecuting(_)))
                    {
                        let summary =
                            match std::mem::replace(last, OutputSegment::Status(String::new())) {
                                OutputSegment::ToolExecuting(op) => format!("{} → {}", op, error),
                                _ => error.clone(),
                            };
                        *last = OutputSegment::ToolError(summary);
                    } else {
                        self.output.push(OutputSegment::ToolError(error));
                    }
                }
                UiEvent::AskUser {
                    question,
                    response_tx,
                } => {
                    self.output
                        .push(OutputSegment::AskQuestion(question.clone()));
                    // Extract the oneshot sender from the Arc<Mutex>
                    let tx = {
                        let mut guard = response_tx.lock().unwrap();
                        guard.take()
                    };
                    self.ask_response_tx = tx;
                    self.ask_question = Some(question);
                    self.mode = AppMode::WaitingInput;
                }
                UiEvent::PermissionAsk {
                    tool_name,
                    reason,
                    input,
                    response_tx,
                } => {
                    let tx = {
                        let mut guard = response_tx.lock().unwrap();
                        guard.take()
                    };
                    if let Some(tx) = tx {
                        // If "allow all" was already granted, auto-approve without prompting
                        if self.permission_allow_all {
                            let _ = tx.send("y".into());
                            continue;
                        }
                        // If we're already waiting for a permission response,
                        // queue this request instead of overwriting.
                        if self.mode == AppMode::WaitingInput {
                            self.permission_queue.push(PendingPermission {
                                tool_name,
                                reason,
                                input,
                                response_tx: tx,
                            });
                        } else {
                            // Display permission prompt in TUI
                            self.output.push(OutputSegment::PermissionPrompt {
                                tool_name: tool_name.clone(),
                                reason: reason.clone(),
                                detail: input.clone(),
                            });
                            self.ask_response_tx = Some(tx);
                            self.ask_question =
                                Some(format!("[permission] {} — allow?", tool_name));
                            self.mode = AppMode::WaitingInput;
                        }
                    }
                }
                UiEvent::QueryDone {
                    text,
                    tool_calls: _,
                    tokens,
                } => {
                    if !text.is_empty() {
                        self.output.push(OutputSegment::Text(text));
                    }
                    self.status.total_tokens = tokens;
                    // QueryDone is now only sent when the query loop fully exits,
                    // so we always switch back to Idle regardless of tool_calls count.
                    self.mode = AppMode::Idle;
                    self.steer_queue.clear();
                    self.status.steer_count = 0;
                }
                UiEvent::Status(msg) => {
                    self.output.push(OutputSegment::Status(msg));
                }
                UiEvent::Error(err) => {
                    self.output.push(OutputSegment::Error(err));
                    self.mode = AppMode::Idle;
                }
                UiEvent::Interrupted => {
                    self.output.push(OutputSegment::Status(
                        "⏸  Interrupted — press Ctrl+C again to quit.".into(),
                    ));
                    self.mode = AppMode::Idle;
                    self.steer_queue.clear();
                    self.status.steer_count = 0;
                }
                UiEvent::TokenUpdate {
                    total_tokens,
                    turn_count,
                } => {
                    self.status.total_tokens = total_tokens;
                    self.status.turn_count = turn_count;
                }
                UiEvent::CompactProgress {
                    method,
                    tokens_before,
                    tokens_after,
                } => {
                    self.output.push(OutputSegment::Status(format!(
                        "󰏖 compact: {} ({}  {} tokens)",
                        method, tokens_before, tokens_after
                    )));
                }
            }
        }
    }

    /// Render the full TUI.
    pub fn render(&mut self, frame: &mut Frame) {
        // Sync mode to status bar before rendering
        self.status.mode = self.mode;

        let full = frame.area();

        // Calculate dynamic input height based on text line count
        let text_lines = self.input.line_count() as u16;
        // input height = 1 (border) + text_lines, capped at MAX_INPUT_HEIGHT
        let desired_input_height = (Self::MIN_INPUT_HEIGHT + text_lines.saturating_sub(1))
            .min(Self::MAX_INPUT_HEIGHT)
            .min(full.height.saturating_sub(2)); // leave room for status bar + at least 1 output line
        let input_height = desired_input_height.max(Self::MIN_INPUT_HEIGHT);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(input_height),
                Constraint::Length(1),
            ])
            .split(full);

        let output_area = chunks[0];
        let input_area = chunks[1];
        let status_area = chunks[2];

        // Title bar
        let title = format!(
            " zeno {} (Ctrl+D to quit, Ctrl+C to interrupt) ",
            env!("CARGO_PKG_VERSION")
        );
        let title_area = Rect {
            y: output_area.y,
            height: 1,
            ..output_area
        };
        frame.render_widget(
            ratatui::widgets::Paragraph::new(title).style(
                ratatui::style::Style::new()
                    .fg(theme::TEXT_BRIGHT)
                    .bg(theme::ACCENT_DIM),
            ),
            title_area,
        );

        // Output area (skip title row)
        let output_render_area = Rect {
            y: output_area.y + 1,
            height: output_area.height.saturating_sub(1),
            ..output_area
        };

        if self.output.segments.is_empty() && self.mode == AppMode::Idle {
            let hint = "Ask a question or type /help for available commands.";
            frame.render_widget(
                ratatui::widgets::Paragraph::new(hint)
                    .style(ratatui::style::Style::new().fg(theme::TEXT_DIM)),
                output_render_area,
            );
        } else {
            super::output::render(frame, output_render_area, &mut self.output);
        }

        // Input area
        input::render(frame, input_area, &self.input, &self.mode);

        // Status bar
        status_bar::render(frame, status_area, &self.status);

        // Cursor — compute position accounting for multi-line text and scrolling
        if self.mode == AppMode::Idle
            || self.mode == AppMode::WaitingInput
            || self.mode == AppMode::Running
        {
            let cursor_col = self.input.cursor_display_col();

            // Available text width (subtract prompt width "◆ " or "> " = 2 display cols)
            let prompt_width: u16 = 2u16; // "◆ " or "> "
            let text_width = input_area.width.saturating_sub(prompt_width);

            // Horizontal scroll: same logic as in render()
            let h_scroll = if cursor_col >= text_width {
                cursor_col - text_width + 1
            } else {
                0u16
            };

            let cursor_x = input_area.x + prompt_width + cursor_col.saturating_sub(h_scroll);

            // Vertical: cursor row relative to the content area, accounting for v_scroll
            let cursor_row = self.input.cursor_row() as u16;
            let content_height = input_area.height.saturating_sub(1); // minus border
            let v_scroll = if cursor_row >= content_height {
                cursor_row - content_height + 1
            } else {
                0u16
            };
            let cursor_y = input_area.y + 1 + cursor_row.saturating_sub(v_scroll);

            frame.set_cursor_position((cursor_x, cursor_y));
        }
    }

    pub fn take_pending_query(&mut self) -> Option<String> {
        self.pending_query.take()
    }

    pub fn is_running(&self) -> bool {
        self.mode == AppMode::Running
    }

    pub fn mode(&self) -> AppMode {
        self.mode
    }

    /// Create a fresh cancellation token for a new LLM query.
    /// Must be called when a new query starts so that cancelling one
    /// query doesn't accidentally cancel a future one.
    pub fn reset_cancel_token(&mut self) -> CancellationToken {
        self.cancel_token = CancellationToken::new();
        self.cancel_token.clone()
    }

    /// Cancel the current running query (used by the exit path).
    pub fn cancel_running(&self) {
        self.cancel_token.cancel();
    }

    /// Drain the permission queue. When `permission_allow_all` is set,
    /// auto-approve all queued requests. Otherwise, show the next one
    /// and stay in WaitingInput mode.
    /// Returns `true` if it's still waiting for user input (a queued
    /// request was promoted to active), `false` if nothing is pending.
    fn drain_permission_queue(&mut self) -> bool {
        // Auto-approve all queued requests if "allow all" was granted
        while self.permission_allow_all {
            if let Some(next) = self.permission_queue.first() {
                if !next.input.is_empty() {
                    // Show what was auto-approved
                    self.output.push(OutputSegment::Status(format!(
                        "󰌾 [{}] {} (auto-approved)",
                        next.tool_name, next.reason
                    )));
                }
                let queued = self.permission_queue.remove(0);
                let _ = queued.response_tx.send("y".into());
            } else {
                return false;
            }
        }
        // Not allow-all: promote the next queued request to active
        if !self.permission_queue.is_empty() {
            let next = self.permission_queue.remove(0); // FIFO
            self.output.push(OutputSegment::PermissionPrompt {
                tool_name: next.tool_name.clone(),
                reason: next.reason.clone(),
                detail: next.input.clone(),
            });
            self.ask_response_tx = Some(next.response_tx);
            self.ask_question = Some(format!("[permission] {} — allow?", next.tool_name));
            return true;
        }
        false
    }
}

/// Initialize the terminal for ratatui.
pub fn init_terminal() -> io::Result<ratatui::DefaultTerminal> {
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(
        io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        // Full mouse capture: wheel events arrive as ScrollUp/ScrollDown,
        // so they don't conflict with Up/Down keyboard navigation.
        // Text selection: hold Shift + drag to bypass capture (Kitty default).
        crossterm::event::EnableMouseCapture,
        // Bracketed paste: multi-line paste arrives as a single Event::Paste
        // instead of individual Enter keys that trigger submit/steer.
        crossterm::event::EnableBracketedPaste
    )?;
    ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(io::stdout()))
}

/// Restore the terminal.
pub fn restore_terminal(terminal: &mut ratatui::DefaultTerminal) -> io::Result<()> {
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::event::DisableBracketedPaste,
        crossterm::event::DisableMouseCapture,
        crossterm::terminal::LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}
