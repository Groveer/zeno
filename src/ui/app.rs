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
use crate::gateway::UiCommand;
use crate::tools::todo::TodoState;
use crate::utils::truncate;

use super::component::Component as ComponentTrait;
use super::component::safe_view;
use super::input::{self, InputState};
use super::output::{OutputSegment, OutputState};
use super::permission_overlay::{self, PermissionOverlay};
use super::side_panel::SidePanel;
use super::status_bar::{AppMode, StatusInfo};
use super::theme;
use super::title_bar::TitleBar;

/// Word-wrap text to fit within `max_width` terminal columns, respecting
/// multi-byte UTF-8 and emoji width. Breaks at word boundaries when possible.
/// Returns a list of wrapped line strings.
pub fn word_wrap(s: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 || s.is_empty() {
        return if s.is_empty() {
            vec![]
        } else {
            vec![String::new()]
        };
    }
    let mut lines: Vec<String> = Vec::new();
    for line in s.lines() {
        wrap_single_line(line, max_width, &mut lines);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Wrap a single logical line (no embedded newlines) into multiple physical lines.
fn wrap_single_line(line: &str, max_width: usize, out: &mut Vec<String>) {
    let mut current = String::new();
    let mut current_width: usize = 0;

    for word in line.split_whitespace() {
        let word_w = crate::utils::display_width(word);

        // If the word itself exceeds max_width, break it character-by-character
        if word_w > max_width {
            // Flush what we have so far
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
                current_width = 0;
            }
            // Break the long word character by character
            let mut chars = word.chars().peekable();
            while let Some(ch) = chars.next() {
                let next = chars.peek().copied();
                let cw = crate::utils::char_width(ch, next);
                if current_width + cw > max_width && !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                    current_width = 0;
                }
                current.push(ch);
                current_width += cw;
                // Skip VS16 if consumed as part of emoji sequence
                if next == Some('\u{FE0F}')
                    && let Some(vs16) = chars.next()
                {
                    current.push(vs16);
                }
            }
            continue;
        }

        // Factor in the space separator between words
        let sep_w = if current.is_empty() { 0usize } else { 1 };
        let new_width = current_width + sep_w + word_w;

        if new_width <= max_width {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
            current_width = new_width;
        } else {
            // Word doesn't fit on current line — start a new line
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            current.push_str(word);
            current_width = word_w;
        }
    }

    if !current.is_empty() {
        out.push(current);
    }
}

/// The main TUI application state.
pub struct App {
    pub(crate) input: InputState,
    pub(crate) output: OutputState,
    mode: AppMode,
    pub(crate) status: StatusInfo,
    /// Title bar component.
    title_bar: TitleBar,
    /// Side panel component (todo list).
    side_panel: SidePanel,
    /// Shared todo state reference (for poll_engine_status generation tracking).
    todo_state: Option<std::sync::Arc<tokio::sync::Mutex<TodoState>>>,
    /// Cached todo state generation — poll_engine_status compares against this.
    todo_gen: u64,
    pending_query: Option<String>,
    /// Channel receiver for UiCommands from Gateway.
    cmd_rx: mpsc::UnboundedReceiver<UiCommand>,
    /// Sender for engine events to Gateway (for image paste, etc.).
    gateway_event_tx: mpsc::UnboundedSender<crate::engine::tui_events::EngineEvent>,
    /// Permission overlay component (manages permission/ask_user queue).
    permission_overlay: PermissionOverlay,
    should_quit: bool,
    /// Cancellation token shared with the running LLM task.
    cancel_token: CancellationToken,
    /// RAII guard for the config file watcher (dropped on session ends).
    _watcher_guard: Option<crate::config::watcher::WatcherGuard>,
    /// Cancellation token for background tasks (curator, review).
    background_cancel_token: CancellationToken,
    /// Set to true whenever something changed that requires a re-render.
    render_dirty: bool,
    /// Queue of user messages typed while the agent is running.
    steer_queue: Vec<String>,
    /// Shared reference to the engine's steer slot.
    steer_slot: Option<std::sync::Arc<std::sync::Mutex<Option<String>>>>,
    /// Width of the side panel (todo list) in terminal columns.
    side_panel_width: u16,
    /// Whether the user is currently dragging the side panel divider.
    side_panel_dragging: bool,
    /// X coordinate of the side panel divider (set during render, used for hit-testing).
    divider_x: u16,
    /// Images extracted from input markers on submit, waiting for the main loop.
    pending_image_blocks: Vec<(String, String)>,
}

impl App {
    /// Maximum height the input area can grow to (in rows including border).
    const MAX_INPUT_HEIGHT: u16 = 16;

    /// Minimum height for the input area (1 border + 1 content line).
    const MIN_INPUT_HEIGHT: u16 = 3;

    /// Create App with an initial active identity for scoped input history.
    pub fn with_identity(
        cmd_rx: mpsc::UnboundedReceiver<UiCommand>,
        gateway_event_tx: mpsc::UnboundedSender<crate::engine::tui_events::EngineEvent>,
        active_identity: Option<String>,
    ) -> Self {
        let mut app = Self {
            input: InputState::with_identity(active_identity.clone()),
            output: OutputState::new(),
            title_bar: TitleBar,
            side_panel: SidePanel::new(),
            todo_state: None,
            todo_gen: 0,
            mode: AppMode::Idle,
            status: StatusInfo {
                model: String::new(),
                provider: String::new(),
                total_tokens: 0,
                context_window: 0,
                turn_count: 0,
                mcp_server_count: 0,
                skill_count: 0,
                mode: AppMode::Idle,
                steer_count: 0,
                active_identity: None,
                tick: 0,
            },
            cmd_rx,
            gateway_event_tx,
            permission_overlay: PermissionOverlay::new(),
            should_quit: false,
            pending_query: None,
            cancel_token: CancellationToken::new(),
            background_cancel_token: CancellationToken::new(),
            render_dirty: true,
            steer_queue: Vec::new(),
            steer_slot: None,
            side_panel_width: 40,
            side_panel_dragging: false,
            divider_x: 0,
            _watcher_guard: None,
            pending_image_blocks: Vec::new(),
        };

        // Mount child components — triggers any one-time initialization
        // that components may need after construction.
        app.mount_components();

        app
    }

    /// Call mount() on all child components that implement the Component trait.
    fn mount_components(&mut self) {
        self.title_bar.mount();
        self.output.mount();
        self.input.mount();
        self.side_panel.mount();
        self.status.mount();
        self.permission_overlay.mount();
    }

    /// Call unmount() on all child components on shutdown.
    /// Components can use this to release resources (timers, channels, watchers).
    pub fn unmount_components(&mut self) {
        self.title_bar.unmount();
        self.output.unmount();
        self.input.unmount();
        self.side_panel.unmount();
        self.status.unmount();
        self.permission_overlay.unmount();
    }

    /// Set the config file watcher guard (dropped on session exit).
    pub fn set_watcher_guard(&mut self, guard: crate::config::watcher::WatcherGuard) {
        self._watcher_guard = Some(guard);
    }

    /// Set the shared steer slot from the engine so the TUI can inject
    /// mid-run user input. Called once when the engine is created.
    pub fn set_steer_slot(&mut self, slot: std::sync::Arc<std::sync::Mutex<Option<String>>>) {
        self.steer_slot = Some(slot);
    }

    /// Set the shared todo state for the side panel and poll_engine_status tracking.
    pub fn set_todo_state(&mut self, state: std::sync::Arc<tokio::sync::Mutex<TodoState>>) {
        self.side_panel.set_todo_state(state.clone());
        self.todo_state = Some(state);
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

    /// Poll external shared state (TodoState) for changes and mark dirty if needed.
    ///
    /// Uses generation counter to detect changes without per-frame locking in view().
    /// Called from the main loop each render cycle.
    ///
    /// TODO(Phase 4): Replace with tokio::sync::watch channel push notification.
    pub fn poll_engine_status(&mut self) {
        if let Some(ref state) = self.todo_state
            && let Ok(s) = state.try_lock()
        {
            let cur_gen = s.generation();
            if cur_gen != self.todo_gen {
                self.todo_gen = cur_gen;
                self.render_dirty = true;
            }
        }
        // try_lock failure: todo_state held by tool — skip this cycle.
        // If contention persists across frames, side panel misses updates.
        // Phase 4 watch channel resolves this entirely.
    }

    pub fn set_status(&mut self, info: StatusInfo) {
        self.status = info;
        self.render_dirty = true;
    }

    /// Trigger an image paste from clipboard (Alt+V).
    ///
    /// Spawns an async task to read the clipboard, then inserts an inline
    /// `[img]` marker in the input buffer. The marker is deletable like
    /// normal text.
    pub fn trigger_image_paste(&mut self) {
        let tx = self.gateway_event_tx.clone();
        tokio::spawn(async move {
            match crate::ui::input::clipboard::read_clipboard_image().await {
                Some(img) => {
                    let size_kb = img.size_bytes / 1024;
                    let (media_type, base64_data) = img.into_tuple();
                    let _ = tx.send(crate::engine::tui_events::EngineEvent::ImagePasted {
                        media_type,
                        base64_data,
                        size_kb,
                    });
                }
                None => {
                    let _ = tx.send(crate::engine::tui_events::EngineEvent::ImagePasteFailed);
                }
            }
        });
    }

    /// Take image blocks extracted from input markers on the last submit.
    pub fn take_pending_image_blocks(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.pending_image_blocks)
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
    ///
    /// Also checks the clipboard for image data — if the clipboard contains
    /// an image alongside text, it's added as a pending image to attach to
    /// the next message.
    pub fn handle_paste(&mut self, text: String) {
        self.input.insert_str(&text);
        // Also check clipboard for image data (terminal paste may carry both
        // text and image representations — e.g. wl-paste or xclip).
        let tx = self.gateway_event_tx.clone();
        tokio::spawn(async move {
            if let Some(img) = crate::ui::input::clipboard::read_clipboard_image().await {
                let size_kb = img.size_bytes / 1024;
                let (media_type, base64_data) = img.into_tuple();
                let _ = tx.send(crate::engine::tui_events::EngineEvent::ImagePasted {
                    media_type,
                    base64_data,
                    size_kb,
                });
            }
        });
        self.render_dirty = true;
    }

    /// Minimum / maximum side panel width (terminal columns).
    const SIDE_PANEL_MIN: u16 = 20;
    const SIDE_PANEL_MAX: u16 = 80;

    /// Process a mouse event for side panel drag resizing.
    pub fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent, terminal_width: u16) {
        use crossterm::event::MouseEventKind;

        match mouse.kind {
            MouseEventKind::Drag(crossterm::event::MouseButton::Left)
                if self.side_panel_dragging => {
                    // Resize: side panel width = total - mouse column position
                    let new_width = terminal_width.saturating_sub(mouse.column);
                    self.side_panel_width =
                        new_width.clamp(Self::SIDE_PANEL_MIN, Self::SIDE_PANEL_MAX);
                    self.render_dirty = true;
                }
            MouseEventKind::Down(crossterm::event::MouseButton::Left)
                // Start drag if clicking near the divider (within 2 columns)
                if mouse.column >= self.divider_x.saturating_sub(2)
                    && mouse.column <= self.divider_x + 1
                => {
                    self.side_panel_dragging = true;
                }
            MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                self.side_panel_dragging = false;
            }
            _ => {}
        }
    }

    /// Process keyboard events.
    ///
    /// Handles global shortcuts (Ctrl+D, Ctrl+C, PageUp/Down, Alt+V) directly,
    /// then delegates to `InputState::dispatch_key()` for mode-specific input handling.
    pub fn handle_key(&mut self, key: KeyEvent) {
        self.render_dirty = true;

        // ── Global shortcuts (any mode) ────────────────────────
        // Ctrl+D: immediate hard quit
        if matches!(
            key,
            KeyEvent {
                code: KeyCode::Char('d'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }
        ) {
            self.should_quit = true;
            self.cancel_token.cancel();
            return;
        }

        // Ctrl+C: clear input / interrupt / quit (depends on mode + text)
        if matches!(
            key,
            KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }
        ) {
            if !self.input.text.is_empty() {
                self.input.reset();
                return;
            }
            match self.mode {
                AppMode::Running | AppMode::WaitingInput => {
                    self.cancel_token.cancel();
                    if self.mode == AppMode::WaitingInput {
                        self.permission_overlay.clear();
                    }
                }
                _ => {
                    self.should_quit = true;
                }
            }
            return;
        }

        // PageUp/PageDown: scroll output
        if matches!(
            key,
            KeyEvent {
                code: KeyCode::PageUp,
                ..
            }
        ) {
            self.output.scroll_up(10);
            return;
        }
        if matches!(
            key,
            KeyEvent {
                code: KeyCode::PageDown,
                ..
            }
        ) {
            self.output.scroll_down(10);
            return;
        }

        // Alt+V: paste/remove image
        if matches!(
            key,
            KeyEvent {
                code: KeyCode::Char('v'),
                modifiers: KeyModifiers::ALT,
                ..
            }
        ) {
            if self.mode == AppMode::Idle {
                if self.input.image_count() > 0 {
                    self.input.remove_last_image();
                } else {
                    self.trigger_image_paste();
                }
            }
            return;
        }

        // ── Mode-specific input via InputPanel ─────────────────
        let action = self.input.dispatch_key(key, self.mode);

        // Scroll fallback: if input didn't consume Up/Down, scroll output
        if matches!(action, input::InputAction::Consumed) {
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

        // ── Route InputAction ──────────────────────────────────
        match action {
            input::InputAction::Consumed | input::InputAction::HardQuit => {
                if matches!(action, input::InputAction::HardQuit) {
                    self.should_quit = true;
                    self.cancel_token.cancel();
                }
            }
            input::InputAction::SubmitQuery { text, images } => {
                self.output.push(OutputSegment::UserInput(text.clone()));
                self.pending_image_blocks = images;
                self.pending_query = Some(text);
                self.mode = AppMode::Running;
            }
            input::InputAction::Steer(text) => {
                if let Some(ref slot) = self.steer_slot {
                    steer_into_slot(slot, &text);
                }
                self.steer_queue.push(text.clone());
                self.status.steer_count = self.steer_queue.len();
                self.output.push(OutputSegment::Status(format!(
                    "\u{f054} Steered: {} (will be injected on next turn)",
                    truncate(&text, 60)
                )));
            }
            input::InputAction::Respond { text } => {
                // Show response in output
                let is_ask = self.permission_overlay.is_ask_user_active();
                if is_ask {
                    self.output.push(OutputSegment::AskResponse(text.clone()));
                } else {
                    self.output.push(OutputSegment::UserInput(text.clone()));
                }

                // Notify Gateway for permission_allow_all
                let lower = text.trim().to_lowercase();
                if matches!(lower.as_str(), "a" | "all" | "always") {
                    let _ = self
                        .gateway_event_tx
                        .send(crate::engine::tui_events::EngineEvent::PermissionAllowAllSet);
                }

                // Delegate to overlay
                let queue_action = self.permission_overlay.respond(&text);
                match queue_action {
                    permission_overlay::QueueAction::AskUserActive
                    | permission_overlay::QueueAction::ShownNext => {}
                    _ => {
                        self.mode = AppMode::Running;
                    }
                }
            }
            input::InputAction::Cancel => {
                self.cancel_token.cancel();
                if self.mode == AppMode::WaitingInput {
                    self.permission_overlay.clear();
                }
            }
        }
    }

    /// Process a single UiCommand — dispatch to the appropriate component.
    ///
    /// This is the core of the component-based architecture:
    /// Gateway → Transport → App.update() → child components.
    pub fn update(&mut self, cmd: UiCommand) {
        self.render_dirty = true;
        match cmd {
            // ── OutputPanel — delegate to Component ──────────────
            UiCommand::AppendText(_)
            | UiCommand::AppendReasoning(_)
            | UiCommand::ClearOutput
            | UiCommand::ToolStart { .. }
            | UiCommand::ToolComplete { .. }
            | UiCommand::ToolError { .. }
            | UiCommand::ToolDiff { .. }
            | UiCommand::ScrollBy(_)
            | UiCommand::ScrollToBottom
            | UiCommand::ShowStatus(_)
            | UiCommand::SubAgentStarted { .. }
            | UiCommand::SubAgentThought(_)
            | UiCommand::SubAgentToolStart { .. }
            | UiCommand::SubAgentToolEnd { .. }
            | UiCommand::SubAgentStatus { .. }
            | UiCommand::SubAgentCompleted { .. } => {
                self.output.update(cmd);
            }

            // ── ShowError → output + mode reset ──────────────────
            UiCommand::ShowError(err) => {
                self.output.update(UiCommand::ShowError(err));
                self.mode = AppMode::Idle;
            }

            // ── StatusBar — delegate to Component trait ──────────────
            UiCommand::SetMode(mode) => {
                self.mode = mode;
                self.status.update(UiCommand::SetMode(mode));
            }
            UiCommand::UpdateStatus(info) => {
                self.status.update(UiCommand::UpdateStatus(info));
            }
            UiCommand::UpdateTokens(tokens) => {
                self.status.update(UiCommand::UpdateTokens(tokens));
            }
            UiCommand::UpdateTurnCount(turns) => {
                self.status.update(UiCommand::UpdateTurnCount(turns));
            }
            UiCommand::SetModel(model) => {
                self.status.update(UiCommand::SetModel(model));
            }

            // ── PermissionOverlay — delegate to overlay ──────────
            UiCommand::ShowPermission {
                tool_name,
                reason,
                detail,
                response_tx,
            } => {
                // Display in output
                self.output.push(OutputSegment::PermissionPrompt {
                    tool_name: tool_name.clone(),
                    reason: reason.clone(),
                    detail: detail.clone(),
                });
                // Delegate queue management to overlay (takes ownership of response_tx)
                self.permission_overlay.update(UiCommand::ShowPermission {
                    tool_name,
                    reason,
                    detail,
                    response_tx,
                });
                self.mode = AppMode::WaitingInput;
            }
            UiCommand::ShowAskUser {
                question,
                response_tx,
            } => {
                // Display in output
                self.output
                    .push(OutputSegment::AskQuestion(question.clone()));
                // Delegate queue management to overlay
                self.permission_overlay.update(UiCommand::ShowAskUser {
                    question,
                    response_tx,
                });
                self.mode = AppMode::WaitingInput;
            }
            UiCommand::HideOverlay => {
                self.permission_overlay.update(UiCommand::HideOverlay);
                if self.mode == AppMode::WaitingInput {
                    self.mode = AppMode::Running;
                }
            }

            // ── Steer ────────────────────────────────────────────
            UiCommand::ClearSteerQueue => {
                self.steer_queue.clear();
                self.status.steer_count = 0;
            }
            UiCommand::SteerSlot { steer_count } => {
                self.status.steer_count = steer_count;
            }

            // ── Input — delegate to Component trait ─────────────────
            UiCommand::PasteImage { .. }
            | UiCommand::SetInputText(_)
            | UiCommand::SetInputPlaceholder(_)
            | UiCommand::SetInputIdentity(_)
            | UiCommand::FocusInput
            | UiCommand::BlurInput => {
                self.input.update(cmd);
            }
        }
    }

    /// Drain UiCommands from the Gateway channel (non-blocking).
    ///
    /// Uses try_recv to consume up to MAX_BATCH commands per render cycle.
    /// This replaces the old process_events() method for the new architecture.
    pub fn drain_commands(&mut self) {
        const MAX_BATCH: usize = 256;
        for _ in 0..MAX_BATCH {
            match self.cmd_rx.try_recv() {
                Ok(cmd) => self.update(cmd),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    self.should_quit = true;
                    break;
                }
            }
        }
    }

    /// Render the full TUI.
    pub fn render(&mut self, frame: &mut Frame) {
        // Sync mode to status bar before rendering
        self.status.mode = self.mode;
        self.status.tick += 1;

        let full = frame.area();

        // Calculate dynamic input height based on visual (wrapped) line count
        // Prompt icons ( /  / ) have unicode-width=1 (PUA), + trailing space = 2 total.
        // We use 2, NOT display_width() which overrides PUA→2 for Nerd Font content.
        const PROMPT_WIDTH: u16 = 2u16;
        let text_area_width = full.width.saturating_sub(PROMPT_WIDTH);
        let text_lines = self.input.visual_line_count(text_area_width) as u16;
        let desired_input_height = (Self::MIN_INPUT_HEIGHT + text_lines.saturating_sub(1))
            .min(Self::MAX_INPUT_HEIGHT)
            .min(full.height.saturating_sub(2));
        let input_height = desired_input_height.max(Self::MIN_INPUT_HEIGHT);

        // Vertical layout per design spec: TitleBar | OutputPanel | InputPanel | StatusBar
        let vert_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(input_height),
                Constraint::Length(1),
            ])
            .split(full);

        let title_area = vert_chunks[0];
        let vert_output_area = vert_chunks[1];
        let input_area = vert_chunks[2];
        let status_area = vert_chunks[3];

        // Decide layout: if side panel has active todos and there's room,
        // split output area horizontally.
        let side_panel_width = self.side_panel_width;
        let has_side_panel = self.side_panel.is_visible() && full.width > side_panel_width + 20;

        let (output_area, side_area) = if has_side_panel {
            let areas = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(1), Constraint::Length(side_panel_width)])
                .split(vert_output_area);
            // Store divider x position for mouse drag hit-testing
            self.divider_x = areas[1].x;
            (areas[0], areas[1])
        } else {
            self.divider_x = 0;
            (vert_output_area, Rect::default())
        };

        // Title bar (independent constraint, not carved from output area)
        safe_view(&mut self.title_bar, title_area, frame);

        // Output area
        if self.output.segments.is_empty() && self.mode == AppMode::Idle {
            let hint = "Ask a question or type /help for available commands.";
            frame.render_widget(
                ratatui::widgets::Paragraph::new(hint)
                    .style(ratatui::style::Style::new().fg(theme::TEXT_DIM)),
                output_area,
            );
        } else {
            safe_view(&mut self.output, output_area, frame);
        }

        // Input area
        safe_view(&mut self.input, input_area, frame);

        // Status bar
        safe_view(&mut self.status, status_area, frame);

        // Side panel (todo list)
        if has_side_panel {
            safe_view(&mut self.side_panel, side_area, frame);
        }

        // Cursor
        if self.mode == AppMode::Idle
            || self.mode == AppMode::WaitingInput
            || self.mode == AppMode::Running
        {
            const PROMPT_WIDTH: u16 = 2u16;
            let text_width = input_area.width.saturating_sub(PROMPT_WIDTH);
            let (visual_row, visual_col) = self.input.visual_cursor_row_col(text_width);

            let cursor_x = input_area.x + PROMPT_WIDTH + visual_col as u16;
            let cursor_y = input_area.y + 1 + visual_row as u16;

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

    /// Cancel background tasks (curator, review) — called once on shutdown.
    pub fn cancel_background(&self) {
        self.background_cancel_token.cancel();
    }

    /// Get the background cancellation token for long-running tasks.
    pub fn background_cancel_token(&self) -> CancellationToken {
        self.background_cancel_token.clone()
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
