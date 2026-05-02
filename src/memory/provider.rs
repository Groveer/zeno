//! Memory provider trait — pluggable memory backends for zeno.
//!
//! Memory providers give the agent persistent recall across sessions.
//! One provider is active at a time alongside the always-on built-in
//! memory (MEMORY.md / USER.md). The MemoryManager enforces this limit.
//!
//! Built-in memory is always active as the first provider and cannot be removed.
//! External providers (Lua-configured, API-backed, etc.) are additive — they
//! never disable the built-in store. Only one external provider runs at a time.
//!
//! Lifecycle (called by MemoryManager):
//! - initialize() — connect, create resources, warm up
//! - system_prompt_block() — static text for the system prompt
//! - prefetch(query) — background recall before each turn
//! - sync_turn(user, assistant) — async write after each turn
//! - get_tool_schemas() — tool schemas to expose to the model
//! - handle_tool_call() — dispatch a tool call
//! - shutdown() — clean exit

use async_trait::async_trait;
use serde_json::Value;

/// Result of a memory provider tool call.
pub type ProviderResult = Result<String, ProviderError>;

/// Errors from memory provider operations.
#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum ProviderError {
    #[error("Provider not available: {0}")]
    NotAvailable(String),

    #[error("Not initialized: {0}")]
    NotInitialized(String),

    #[error("Tool not found: {0}")]
    ToolNotFound(String),

    #[error("Execution failed: {0}")]
    Execution(String),

    #[error("Invalid arguments: {0}")]
    InvalidArguments(String),

    #[error("Network error: {0}")]
    Network(String),

    #[error("Config error: {0}")]
    Config(String),
}

/// Abstract base trait for pluggable memory providers.
///
/// Implementations can be:
/// - `BuiltinMemoryProvider` — the default MEMORY.md/USER.md store (always active)
/// - `LuaMemoryProvider` — a Lua-scripted provider configured via `zn.memory_provider()`
/// - Future: native Rust providers compiled into zeno
#[async_trait]
#[allow(dead_code)]
pub trait MemoryProvider: Send + Sync {
    /// Short identifier for this provider (e.g. "builtin", "mem0", "honcho").
    fn name(&self) -> &str;

    /// Return True if this provider is configured and ready to activate.
    /// Should NOT make network calls — just check config and credentials.
    fn is_available(&self) -> bool;

    /// Initialize for a session. Called once at startup.
    /// May create resources, establish connections, etc.
    async fn initialize(&mut self, session_id: &str) -> Result<(), ProviderError>;

    /// Return text to include in the system prompt.
    /// This is for STATIC provider info (instructions, status).
    /// Prefetched recall context is injected separately via prefetch().
    fn system_prompt_block(&self) -> String {
        String::new()
    }

    /// Recall relevant context for the upcoming turn.
    /// Called before each API call. Return formatted text to inject,
    /// or empty string if nothing relevant.
    async fn prefetch(&self, query: &str) -> String {
        let _ = query;
        String::new()
    }

    /// Persist a completed turn to the backend.
    /// Called after each turn. Should be non-blocking when possible.
    async fn sync_turn(&self, user_content: &str, assistant_content: &str) {
        let _ = (user_content, assistant_content);
    }

    /// Return tool schemas this provider exposes (OpenAI function calling format).
    /// Return empty vec if this provider has no tools (context-only).
    fn get_tool_schemas(&self) -> Vec<Value> {
        Vec::new()
    }

    /// Handle a tool call for one of this provider's tools.
    /// Must return a JSON string (the tool result).
    /// Only called for tool names returned by get_tool_schemas().
    async fn handle_tool_call(&self, tool_name: &str, _args: &Value) -> ProviderResult {
        Err(ProviderError::ToolNotFound(format!(
            "Provider '{}' does not handle tool '{}'",
            self.name(),
            tool_name
        )))
    }

    /// Clean shutdown — flush queues, close connections.
    async fn shutdown(&mut self) {}

    /// Called when the built-in memory tool writes an entry.
    /// Use to mirror built-in memory writes to your backend.
    fn on_memory_write(&self, action: &str, target: &str, content: &str) {
        let _ = (action, target, content);
    }
}
