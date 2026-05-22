//! Zeno agent communication protocol.
//!
//! Defines the bidirectional event types for Engine ↔ UI communication:
//!
//! - [`EngineEvent`]: Events emitted by the engine during query execution
//!   (streaming deltas, tool lifecycle, status updates, etc.)
//! - [`Submission`]: Commands sent from UI to engine
//!   (user input, interrupts, permission responses, etc.)
//!
//! # Design Principles (inspired by Codex SQ/EQ pattern)
//!
//! - **Protocol/transport separation**: Types are serializable and transport-agnostic.
//!   They can be sent over channels, WebSocket, UDS, or recorded for replay.
//! - **Exhaustive variants**: All lifecycle events are explicitly modeled.
//! - **Replay-friendly**: Events carry enough context for session recording/replay.

pub mod events;
pub mod submission;
pub mod usage;

pub use events::EngineEvent;
pub use submission::Submission;
pub use usage::Usage;

/// Unique identifier for a turn within a session.
pub type TurnId = String;

/// Unique identifier for a tool call within a turn.
pub type CallId = String;
