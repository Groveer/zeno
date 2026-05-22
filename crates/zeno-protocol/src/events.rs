//! Engine → UI event types.
//!
//! These events are emitted by the engine during query execution and
//! consumed by the UI for rendering. The protocol is transport-agnostic:
//! events can be sent over channels, WebSocket, or recorded for replay.

use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// EngineEvent — emitted by the engine during query execution
// ---------------------------------------------------------------------------

/// UI events emitted during a query for TUI rendering.
///
/// Each variant represents a discrete lifecycle event in the agent loop.
/// The UI processes these to render streaming output, tool progress,
/// permission prompts, and status updates.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    // ── Streaming ──────────────────────────────────────────
    /// A chunk of text from the LLM stream.
    TextDelta(String),
    /// A chunk of reasoning / thinking content from the LLM stream.
    /// Displayed separately from visible text (e.g. in a collapsible block).
    ReasoningDelta(String),
    /// Clear all output segments (e.g. after /clear command).
    ClearOutput,

    // ── Tool Lifecycle ─────────────────────────────────────
    /// A tool call is starting execution.
    ToolStart {
        name: String,
        /// One-line summary of the tool input so the user knows what it does.
        input_summary: String,
    },
    /// Tool execution succeeded.
    ToolOutput { name: String, output: String },
    /// Diff output from an edit tool — shows file change diff with +/- markers.
    ToolDiff { name: String, diff: String },
    /// Tool execution failed.
    ToolError { name: String, error: String },

    // ── User Interaction ───────────────────────────────────
    /// The LLM is asking the user a question (ask_user tool).
    AskUser {
        question: String,
        response_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<String>>>>,
    },
    /// Permission check requires user confirmation (y/n/a).
    PermissionAsk {
        tool_name: String,
        reason: String,
        input: String,
        response_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<String>>>>,
    },
    /// User granted blanket permission (typed "a"/"all"/"always").
    PermissionAllowAllSet,
    /// Steer injection was consumed by the engine (next turn includes it).
    SteerHandled,

    // ── Turn Lifecycle ─────────────────────────────────────
    /// Query completed successfully.
    QueryDone {
        text: String,
        tool_calls: u32,
        tokens: u64,
    },
    /// Query was interrupted by the user (Ctrl+C).
    Interrupted,

    // ── Status & Progress ──────────────────────────────────
    /// Live token count update during a running query.
    TokenUpdate { total_tokens: u64, turn_count: u64 },
    /// Status message (thinking, compacting, retrying, etc.).
    Status(String),
    /// Compact progress — emitted during micro/full compact operations.
    CompactProgress {
        method: String, // "micro" or "full"
        tokens_before: usize,
        tokens_after: usize,
    },

    // ── Errors ─────────────────────────────────────────────
    /// An error occurred during streaming.
    Error(String),

    // ── Media ──────────────────────────────────────────────
    /// An image was successfully pasted from clipboard (Alt+V).
    ImagePasted {
        media_type: String,
        base64_data: String,
        size_kb: usize,
    },
    /// Image paste failed (no image in clipboard or tool unavailable).
    ImagePasteFailed,
}

/// Convenience type alias for the event sender half.
pub type EngineSender = tokio::sync::mpsc::UnboundedSender<EngineEvent>;
/// Convenience type alias for the event receiver half.
pub type EngineReceiver = tokio::sync::mpsc::UnboundedReceiver<EngineEvent>;

/// Create a bounded engine event channel.
pub fn engine_channel() -> (EngineSender, EngineReceiver) {
    tokio::sync::mpsc::unbounded_channel()
}
