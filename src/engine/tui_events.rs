//! UI events sent from the engine to the TUI during streaming.
//!
//! Used when running in ratatui interactive mode — the engine
//! sends these events through a channel instead of printing directly.
//!
//! This module re-exports the canonical types from `zeno-protocol` and
//! provides backward-compatible type aliases.

// Re-export protocol types — all new code should use `zeno_protocol::EngineEvent` directly.
pub use zeno_protocol::events::{EngineEvent, EngineSender};
