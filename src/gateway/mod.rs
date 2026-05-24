//! Gateway layer: event routing + command dispatch + session management.
//!
//! The Gateway sits between the Engine (async) and UI (sync main loop).
//! It receives events from the engine, maps them to `UiCommand`s, and
//! sends them to the UI via a `Transport`.
//!
//! ## Concurrency Model
//!
//! - `handle_engine_event(&self)` — called from Engine spawned task, read-only
//!   on event_map + write to channel sender (`&self` safe)
//! - `handle_input(&mut self)` — called from sync main loop, modifies handler
//!   state / engine references
//!
//! ## Architecture
//!
//! Gateway is split into two parts:
//! - **Gateway**: pure event routing (EngineEvent → UiCommand)
//! - **CommandRouter**: slash command execution (business logic)
//!
//! This separation keeps Gateway focused on event routing while
//! CommandRouter owns the service dependencies for slash commands.

pub mod commands;
pub mod events;
pub mod sub_agent;
pub mod transport;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use tokio::sync::mpsc;

use crate::engine::sub_agent::SubAgentEvent;
use crate::engine::tui_events::EngineEvent;
use crate::ui::status_bar::AppMode;

use self::transport::Transport;

// ---------------------------------------------------------------------------
// UiCommand — Gateway → UI Components
// ---------------------------------------------------------------------------

/// Commands sent from Gateway to UI components via the transport layer.
///
/// Each variant targets a specific component and operation.
/// The root `App` component dispatches these to child components
/// in its `update()` method.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Protocol fields (name, size_kb) and pre-wired variants (Scroll*, Input*, HideOverlay)
pub enum UiCommand {
    // ── OutputPanel ──────────────────────────────────────
    /// Append text to the output (streaming delta).
    AppendText(String),
    /// Append reasoning/thinking content to the output.
    AppendReasoning(String),
    /// Clear all output segments.
    ClearOutput,
    /// A tool call is starting execution.
    ToolStart {
        name: String,
        input_summary: String,
    },
    /// A tool call completed successfully.
    ToolComplete {
        name: String,
        output: String,
    },
    /// Diff output from an edit tool.
    ToolDiff {
        name: String,
        diff: String,
    },
    /// A tool call errored.
    ToolError {
        name: String,
        error: String,
    },
    /// Scroll the output area by a delta (positive = up).
    ScrollBy(i32),
    /// Scroll to the bottom of the output.
    ScrollToBottom,

    // ── StatusBar ────────────────────────────────────────
    /// Set the application mode.
    SetMode(AppMode),
    /// Update status bar information.
    UpdateStatus(StatusInfo),
    /// Update the token count.
    UpdateTokens(u64),
    /// Update the turn count.
    UpdateTurnCount(u64),
    /// Set the current model name.
    SetModel(String),

    // ── PermissionOverlay ────────────────────────────────
    /// Show a tool permission request.
    ShowPermission {
        tool_name: String,
        reason: String,
        detail: String,
        response_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<String>>>>,
    },
    /// Show an ask_user question.
    ShowAskUser {
        question: String,
        response_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<String>>>>,
    },
    /// Hide the current overlay (permission/ask_user).
    HideOverlay,

    // ── InputPanel ───────────────────────────────────────
    /// Set input text (e.g. from command history).
    SetInputText(String),
    /// Set input placeholder text (e.g. for permission prompts).
    SetInputPlaceholder(String),
    /// Focus the input panel.
    FocusInput,
    /// Blur the input panel.
    BlurInput,

    // ── Steer ────────────────────────────────────────────
    /// Clear the steer queue (when query ends or steer is consumed).
    ClearSteerQueue,

    // ── Sub-agent indicators ─────────────────────────────
    SubAgentStarted {
        summary: String,
    },
    /// Rolling sub-agent progress line (replaces in-place, last 2 per agent).
    SubAgentProgress {
        task_index: usize,
        line: String,
    },

    // ── Steer ────────────────────────────────────────────
    SteerSlot {
        steer_count: usize,
    },

    // ── InputPanel ───────────────────────────────────────
    /// Switch identity scope for input history.
    SetInputIdentity(Option<String>),

    // ── Image paste ──────────────────────────────────────
    PasteImage {
        media_type: String,
        base64_data: String,
        size_kb: usize,
    },

    // ── Misc ─────────────────────────────────────────────
    /// Mark query as done (token count for status bar update).
    QueryDone {
        tokens: u64,
    },
    /// Show an error message in the output.
    ShowError(String),
    /// Show a status message in the output.
    ShowStatus(String),
}

// Re-export StatusInfo so gateway consumers don't need to reach into ui::status_bar
pub use crate::ui::status_bar::StatusInfo;

// ---------------------------------------------------------------------------
// Gateway
// ---------------------------------------------------------------------------

/// Gateway: event routing + command dispatch + session management.
///
/// ## Architecture
///
/// Gateway is split into two parts:
/// - **Gateway**: pure event routing (EngineEvent → UiCommand)
/// - **CommandRouter**: slash command execution (business logic)
///
/// This separation keeps Gateway focused on event routing while
/// CommandRouter owns the service dependencies for slash commands.
pub struct Gateway {
    /// Engine reference (shared with main loop for status polling).
    engine: Arc<tokio::sync::Mutex<crate::engine::query_engine::QueryEngine>>,
    /// Transport for sending UiCommands to the UI.
    /// Wrapped in Mutex so emit() works with &self (needed for slash commands).
    transport: std::sync::Mutex<Box<dyn Transport>>,
    /// Sender for engine events (cloned into spawned engine tasks).
    engine_event_tx: mpsc::UnboundedSender<EngineEvent>,
    /// Receiver for engine events (drained by drain_engine_events()).
    engine_event_rx: mpsc::UnboundedReceiver<EngineEvent>,
    /// Sender for sub-agent events (cloned into ToolContext for delegate_task).
    sub_agent_tx: mpsc::UnboundedSender<SubAgentEvent>,
    /// Receiver for sub-agent events (drained by drain_sub_agent_events()).
    sub_agent_rx: mpsc::UnboundedReceiver<SubAgentEvent>,
    /// Direct reference to engine's permission_allow_all AtomicBool.
    /// Set lock-free by drain_engine_events() without needing the engine lock,
    /// avoiding a race where the engine holds its lock waiting for a permission
    /// oneshot reply while we try to set the flag.
    permission_allow_all: Arc<AtomicBool>,
    /// Command router for slash commands.
    /// Owns service dependencies (memory, skills, mcp, settings).
    commands: commands::CommandRouter,
}

impl Gateway {
    pub async fn new(
        engine: Arc<tokio::sync::Mutex<crate::engine::query_engine::QueryEngine>>,
        settings: Arc<crate::config::settings::Settings>,
        transport: Box<dyn Transport>,
        memory_manager: crate::memory::manager::SharedMemoryManager,
        skill_registry: Arc<tokio::sync::Mutex<crate::skills::registry::SkillRegistry>>,
        mcp_manager: Arc<tokio::sync::Mutex<crate::mcp::manager::McpManager>>,
        provider_name: String,
        model_name: String,
        builtin_tool_names: Vec<String>,
    ) -> Self {
        // Create engine event channel — Gateway owns both ends.
        let (engine_event_tx, engine_event_rx) = mpsc::unbounded_channel();
        // Create sub-agent event channel — separate from engine events.
        let (sub_agent_tx, sub_agent_rx) = mpsc::unbounded_channel();

        // Create command router with service dependencies.
        let commands = commands::CommandRouter::new(
            engine.clone(),
            settings,
            memory_manager,
            skill_registry,
            mcp_manager,
            engine_event_tx.clone(),
            provider_name,
            model_name,
            builtin_tool_names,
        );

        // Extract a direct reference to engine's `permission_allow_all` (Arc<AtomicBool>)
        // so Gateway can set it lock-free without acquiring the engine lock.
        // This avoids a race where the engine holds its lock waiting for a permission
        // oneshot reply while Gateway tries to set the allow-all flag.
        // Using the lock-free accessor so this never touches the engine mutex.
        let permission_allow_all = engine.lock().await.permission_allow_all_ref();

        Self {
            engine,
            transport: std::sync::Mutex::new(transport),
            engine_event_tx,
            engine_event_rx,
            sub_agent_tx,
            sub_agent_rx,
            permission_allow_all,
            commands,
        }
    }

    // ── Event handling ──

    /// Drain engine events from the channel and map to UiCommands.
    ///
    /// Called from the main loop each render cycle. Non-blocking.
    /// Each EngineEvent is mapped to zero or more UiCommands via event_map(),
    /// then sent to the UI via the transport.
    pub fn drain_engine_events(&mut self) {
        const MAX_BATCH: usize = 256;
        for _ in 0..MAX_BATCH {
            match self.engine_event_rx.try_recv() {
                Ok(event) => {
                    // Lock-free permission_allow_all: set atomically without
                    // acquiring the engine lock. This avoids a race where the
                    // engine holds its lock waiting for a permission oneshot
                    // reply while we try to set the allow-all flag.
                    if matches!(event, EngineEvent::PermissionAllowAllSet) {
                        self.permission_allow_all
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                    }

                    let commands = self.event_map(event);
                    for cmd in commands {
                        self.emit(cmd);
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
    }

    /// Send a UiCommand via the transport.
    ///
    /// If the channel is closed (UI dropped receiver), logs at info level.
    /// This is expected during shutdown — the main loop sets should_quit=true
    /// and breaks, dropping the receiver. Engine tasks may still be sending.
    pub fn emit(&self, cmd: UiCommand) {
        if let Ok(t) = self.transport.lock()
            && !t.send(&cmd)
        {
            tracing::info!("gateway emit failed (transport closed)");
        }
    }

    /// Drain sub-agent events from the channel, map to UiCommands, and emit.
    ///
    /// Called from the main loop each render cycle. Non-blocking.
    /// Sub-agent events are kept separate from engine events to avoid
    /// head-of-line blocking (sub-agent progress shouldn't delay the
    /// main engine output stream).
    pub fn drain_sub_agent_events(&mut self) {
        const MAX_BATCH: usize = 128;
        for _ in 0..MAX_BATCH {
            match self.sub_agent_rx.try_recv() {
                Ok(event) => {
                    let commands = self.handle_sub_agent_event(event);
                    for cmd in commands {
                        self.emit(cmd);
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
    }

    /// Get the sub-agent event sender (for wiring into engine/ToolContext).
    pub fn sub_agent_sender(&self) -> mpsc::UnboundedSender<SubAgentEvent> {
        self.sub_agent_tx.clone()
    }

    // ── Input handling (called from sync main loop) ──

    /// Handle user input from the main loop.
    ///
    /// If input starts with '/', dispatches as slash command.
    /// Otherwise, spawns an engine query task.
    /// `cancel_token` is created by the App for each new query.
    pub async fn handle_input(
        &mut self,
        input: &str,
        images: Vec<(String, String)>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        if input.starts_with('/') {
            // Delegate to command router
            match self.commands.dispatch(input).await {
                commands::CommandResult::Handled(cmds) => {
                    for cmd in cmds {
                        self.emit(cmd);
                    }
                }
                commands::CommandResult::PassThrough => {
                    self.submit_query(input, images, cancel_token).await;
                }
            }
        } else {
            self.submit_query(input, images, cancel_token).await;
        }
    }

    /// Submit a user query to the engine (spawns async task).
    async fn submit_query(
        &self,
        input: &str,
        images: Vec<(String, String)>,
        cancel: tokio_util::sync::CancellationToken,
    ) {
        let engine = self.engine.clone();
        let sender = self.engine_event_tx.clone();
        let text = input.to_string();
        tokio::spawn(async move {
            let mut eng = engine.lock().await;
            if let Err(e) = eng.query_tui(&text, images, &sender, cancel).await {
                let _ = sender.send(EngineEvent::Error(e.to_string()));
            }
        });
    }

    /// Get the engine event sender (for spawning tasks that need to send events).
    pub fn engine_event_sender(&self) -> mpsc::UnboundedSender<EngineEvent> {
        self.engine_event_tx.clone()
    }
}
