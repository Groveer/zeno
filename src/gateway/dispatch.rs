//! Method dispatch — handler registration and method routing.
//!
//! Provides a `MethodHandler` trait and `register_handler()` API on `Gateway`,
//! enabling clean separation of command/query handlers from the Gateway core.
//!
//! This is the Zeno equivalent of Hermes-ink's `@method` decorator pattern:
//! each handler registers against a method name (e.g. "session.create"),
//! and `dispatch()` routes incoming method calls to the appropriate handler.
//!
//! ## Architecture Note
//!
//! Slash commands (`/help`, `/model`, etc.) use `dispatch_slash()` directly
//! on Gateway (see `handlers/slash.rs`). The `MethodHandler` pattern is for
//! JSON-RPC-style method dispatch, planned for future external TUI frontends
//! (Node.js ink TUI via `StdioTransport`).

use serde_json::Value;

/// Error type for method handler operations.
#[derive(Debug)]
#[allow(dead_code)]
pub enum MethodError {
    /// Handler not found for the given method name.
    NotFound(String),
    /// Invalid parameters passed to the handler.
    InvalidParams(String),
    /// Handler execution failed.
    ExecutionError(String),
}

impl std::fmt::Display for MethodError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MethodError::NotFound(name) => write!(f, "method not found: {name}"),
            MethodError::InvalidParams(msg) => write!(f, "invalid params: {msg}"),
            MethodError::ExecutionError(msg) => write!(f, "execution error: {msg}"),
        }
    }
}

impl std::error::Error for MethodError {}

/// A registered method handler, similar to Hermes `@method` decorators.
///
/// Each handler processes a named method call with JSON parameters
/// and returns a JSON result or an error.
///
/// Implementations must be `Send` so the handler map can be shared
/// across async boundaries (e.g., spawned engine tasks).
#[allow(dead_code)]
pub trait MethodHandler: Send {
    /// Execute the method with the given parameters.
    fn handle(&mut self, params: &Value) -> Result<Value, MethodError>;
}

/// Convenience type for the handler registry.
pub type HandlerRegistry = std::collections::HashMap<String, Box<dyn MethodHandler>>;
