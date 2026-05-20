//! UI events sent from the engine to the TUI during streaming.
//!
//! Used when running in ratatui interactive mode — the engine
//! sends these events through a channel instead of printing directly.
//!
//! Some enum variant fields are kept for type completeness even though
//! they are not individually read (the variant is destructured as a whole).
#![allow(dead_code, reason = "enum variant fields kept for type completeness")]

use std::sync::{Arc, Mutex};

/// UI events emitted during a query for TUI rendering.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    /// A chunk of text from the LLM stream.
    TextDelta(String),
    /// A chunk of reasoning / thinking content from the LLM stream.
    /// Displayed separately from visible text (e.g. in a collapsible block).
    ReasoningDelta(String),
    /// Clear all output segments (e.g. after /clear command).
    ClearOutput,
    /// A tool call is being executed.
    ToolStart {
        name: String,
        /// One-line summary of the tool input so the user knows what it does.
        input_summary: String,
    },
    /// Tool execution succeeded.
    ToolOutput { name: String, output: String },
    /// Diff output from an edit tool — shows file change diff with +/- markers.
    /// Sent alongside ToolOutput for richer TUI rendering.
    ToolDiff { name: String, diff: String },
    /// Tool execution failed.
    ToolError { name: String, error: String },
    /// The LLM is asking the user a question (ask_user tool).
    /// The UI should display the question and let the user type a response.
    /// When the user submits, the response should be sent via the shared sender.
    AskUser {
        question: String,
        response_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<String>>>>,
    },
    /// Permission check requires user confirmation (y/n/a).
    /// The UI should display the tool name, reason, and input, then
    /// send the user's response ("y", "n", or "a") via response_tx.
    PermissionAsk {
        tool_name: String,
        reason: String,
        input: String,
        response_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<String>>>>,
    },
    /// Query completed successfully.
    QueryDone {
        text: String,
        tool_calls: u32,
        tokens: u64,
    },
    /// Query was interrupted by the user (Ctrl+C).
    Interrupted,
    /// Live token count update during a running query.
    /// Sent after each API response is recorded so the status bar
    /// can display real-time consumption without needing the engine lock.
    TokenUpdate { total_tokens: u64, turn_count: u64 },
    /// Status message (thinking, compacting, retrying, etc.).
    Status(String),
    /// Compact progress — emitted during micro/full compact operations.
    CompactProgress {
        method: String, // "micro" or "full"
        tokens_before: usize,
        tokens_after: usize,
    },
    /// An error occurred during streaming.
    Error(String),
    /// An image was successfully pasted from clipboard (Alt+V).
    ImagePasted {
        media_type: String,
        base64_data: String,
        size_kb: usize,
    },
    /// Image paste failed (no image in clipboard or tool unavailable).
    ImagePasteFailed,
    /// User granted blanket permission (typed "a"/"all"/"always" in response
    /// to a permission prompt). Gateway sets its permission_allow_all flag.
    PermissionAllowAllSet,
    /// Steer injection was consumed by the engine (next turn includes it).
    SteerHandled,
}

/// Convenience type alias for the event sender half.
pub type EngineSender = tokio::sync::mpsc::UnboundedSender<EngineEvent>;
