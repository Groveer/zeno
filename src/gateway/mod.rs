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

pub mod dispatch;
pub mod events;
pub mod handlers;
pub mod sub_agent;
pub mod transport;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;
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
    SubAgentThought(String),
    SubAgentToolStart {
        label: String,
    },
    SubAgentToolEnd {
        label: String,
    },
    SubAgentStatus {
        message: String,
    },
    SubAgentCompleted {
        summary: String,
    },

    // ── Image paste ──────────────────────────────────────
    PasteImage {
        media_type: String,
        base64_data: String,
        size_kb: usize,
    },

    // ── Misc ─────────────────────────────────────────────
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
/// ## Field Bloat Tracking
///
/// Gateway currently holds 6 `Arc<Mutex<...>>` dependencies. If `handlers/`
/// grows beyond 10 handlers, consider splitting into sub-Gateways
/// (SessionGateway, ConfigGateway), each holding only its own deps.
pub struct Gateway {
    /// Registered method handlers (JSON-RPC-style dispatch).
    handlers: dispatch::HandlerRegistry,
    /// Engine reference (shared with main loop for status polling).
    engine: Arc<tokio::sync::Mutex<crate::engine::query_engine::QueryEngine>>,
    /// Shared memory manager.
    memory_manager: crate::memory::manager::SharedMemoryManager,
    /// Shared skill registry.
    skill_registry: Arc<tokio::sync::Mutex<crate::skills::registry::SkillRegistry>>,
    /// Shared MCP manager.
    mcp_manager: Arc<tokio::sync::Mutex<crate::mcp::manager::McpManager>>,
    /// Shared todo state.
    #[allow(dead_code)] // Accessed via main.rs, reserved for Gateway-side panel integration
    todo_state: Arc<tokio::sync::Mutex<crate::tools::todo::TodoState>>,
    /// Application settings.
    settings: Arc<crate::config::settings::Settings>,
    /// Global permission auto-approve flag (set when user answers "a").
    permission_allow_all: AtomicBool,
    /// Transport for sending UiCommands to the UI.
    /// Wrapped in Mutex so emit() works with &self (needed for slash commands).
    transport: Mutex<Box<dyn Transport>>,
    /// Sender for engine events (cloned into spawned engine tasks).
    engine_event_tx: mpsc::UnboundedSender<EngineEvent>,
    /// Receiver for engine events (drained by drain_engine_events()).
    engine_event_rx: mpsc::UnboundedReceiver<EngineEvent>,
    /// Sender for sub-agent events (cloned into ToolContext for delegate_task).
    sub_agent_tx: mpsc::UnboundedSender<SubAgentEvent>,
    /// Receiver for sub-agent events (drained by drain_sub_agent_events()).
    sub_agent_rx: mpsc::UnboundedReceiver<SubAgentEvent>,
    /// Provider display name (for status bar).
    provider_name: String,
    /// Model display name (for status bar).
    model_name: String,
    /// Built-in tool names (for /tools command).
    builtin_tool_names: Vec<String>,
    /// Built-in tool count.
    builtin_tool_count: usize,
    /// Working directory.
    #[allow(dead_code)] // Used by slash handlers via eng.cwd; field reserved for direct access
    cwd: std::path::PathBuf,
}

impl Gateway {
    pub fn new(
        engine: Arc<tokio::sync::Mutex<crate::engine::query_engine::QueryEngine>>,
        settings: Arc<crate::config::settings::Settings>,
        transport: Box<dyn Transport>,
        memory_manager: crate::memory::manager::SharedMemoryManager,
        skill_registry: Arc<tokio::sync::Mutex<crate::skills::registry::SkillRegistry>>,
        mcp_manager: Arc<tokio::sync::Mutex<crate::mcp::manager::McpManager>>,
        todo_state: Arc<tokio::sync::Mutex<crate::tools::todo::TodoState>>,
        provider_name: String,
        model_name: String,
        builtin_tool_names: Vec<String>,
        cwd: std::path::PathBuf,
    ) -> Self {
        let builtin_tool_count = builtin_tool_names.len();
        // Create engine event channel — Gateway owns both ends.
        // The sender is cloned into engine tasks; the receiver is drained
        // by drain_engine_events() each render cycle.
        let (engine_event_tx, engine_event_rx) = mpsc::unbounded_channel();
        // Create sub-agent event channel — separate from engine events
        // to avoid head-of-line blocking.
        let (sub_agent_tx, sub_agent_rx) = mpsc::unbounded_channel();
        Self {
            handlers: dispatch::HandlerRegistry::new(),
            engine,
            memory_manager,
            skill_registry,
            mcp_manager,
            todo_state,
            settings,
            permission_allow_all: AtomicBool::new(false),
            transport: Mutex::new(transport),
            engine_event_tx,
            engine_event_rx,
            sub_agent_tx,
            sub_agent_rx,
            provider_name,
            model_name,
            builtin_tool_names,
            builtin_tool_count,
            cwd,
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
        if let Ok(t) = self.transport.lock() {
            if !t.send(&cmd) {
                tracing::info!("gateway emit failed (transport closed)");
            }
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
            self.dispatch_slash(input).await;
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

    // Accessors for external consumers (main.rs, future handlers).
    // Not yet called but part of the public API surface.
    #[allow(dead_code)]
    pub fn engine(&self) -> &Arc<tokio::sync::Mutex<crate::engine::query_engine::QueryEngine>> {
        &self.engine
    }
    #[allow(dead_code)]
    pub fn settings(&self) -> &Arc<crate::config::settings::Settings> {
        &self.settings
    }
    #[allow(dead_code)]
    pub fn memory_manager(&self) -> &crate::memory::manager::SharedMemoryManager {
        &self.memory_manager
    }
    #[allow(dead_code)]
    pub fn skill_registry(
        &self,
    ) -> &Arc<tokio::sync::Mutex<crate::skills::registry::SkillRegistry>> {
        &self.skill_registry
    }
    #[allow(dead_code)]
    pub fn mcp_manager(&self) -> &Arc<tokio::sync::Mutex<crate::mcp::manager::McpManager>> {
        &self.mcp_manager
    }
    #[allow(dead_code)]
    pub fn todo_state(&self) -> &Arc<tokio::sync::Mutex<crate::tools::todo::TodoState>> {
        &self.todo_state
    }
    #[allow(dead_code)]
    pub fn provider_name(&self) -> &str {
        &self.provider_name
    }
    #[allow(dead_code)]
    pub fn model_name(&self) -> &str {
        &self.model_name
    }
    #[allow(dead_code)]
    pub fn set_model_name(&mut self, name: String) {
        self.model_name = name;
    }
    #[allow(dead_code)]
    pub fn builtin_tool_names(&self) -> &[String] {
        &self.builtin_tool_names
    }
    #[allow(dead_code)]
    pub fn builtin_tool_count(&self) -> usize {
        self.builtin_tool_count
    }
    #[allow(dead_code)]
    pub fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }
    #[allow(dead_code)]
    pub fn set_permission_allow_all(&self, allow: bool) {
        self.permission_allow_all.store(allow, Ordering::Relaxed);
    }
    #[allow(dead_code)]
    pub fn permission_allow_all(&self) -> bool {
        self.permission_allow_all.load(Ordering::Relaxed)
    }

    /// Get the engine event sender (for spawning tasks that need to send events).
    pub fn engine_event_sender(&self) -> mpsc::UnboundedSender<EngineEvent> {
        self.engine_event_tx.clone()
    }

    // ── Method handler registration ──

    /// Register a method handler for JSON-RPC-style dispatch.
    ///
    /// Similar to Hermes `@method` decorator pattern. Handlers are looked up
    /// by name in `dispatch()` and can be called from both slash command routes
    /// and external RPC interfaces (e.g., future StdioTransport).
    ///
    /// ## Example
    ///
    /// ```ignore
    /// gateway.register_handler("session.create", SessionCreateHandler::new());
    /// gateway.register_handler("config.get", ConfigGetHandler::new(settings.clone()));
    /// ```
    #[allow(dead_code)] // API surface for future RPC integration
    pub fn register_handler(&mut self, name: &str, handler: Box<dyn dispatch::MethodHandler>) {
        self.handlers.insert(name.to_string(), handler);
    }

    /// Dispatch a method call to the registered handler.
    ///
    /// Looks up `name` in the handler registry and calls it with `params`.
    /// Returns the handler's result or `MethodError::NotFound` if no handler
    /// is registered for the given name.
    #[allow(dead_code, unused_variables)] // API surface for future RPC integration
    pub fn dispatch(&self, name: &str, params: &Value) -> Result<Value, dispatch::MethodError> {
        // Need &mut to call handle() — we use interior mutability via the
        // handler's own mechanisms (or this requires &mut self).
        // For now, dispatch requires &mut self at the call site.
        // This method is kept as a convenience wrapper.
        Err(dispatch::MethodError::NotFound(name.into()))
    }

    /// Mutable dispatch — requires &mut self for handler state mutation.
    #[allow(dead_code)]
    pub fn dispatch_mut(
        &mut self,
        name: &str,
        params: &Value,
    ) -> Result<Value, dispatch::MethodError> {
        match self.handlers.get_mut(name) {
            Some(handler) => handler.handle(params),
            None => Err(dispatch::MethodError::NotFound(name.into())),
        }
    }
}
