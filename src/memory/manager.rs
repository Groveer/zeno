//! Memory manager — orchestrates the built-in memory provider plus at most
//! ONE external plugin memory provider.
//!
//! Single integration point. The BuiltinMemoryProvider is always registered
//! first and cannot be removed. Only ONE external (non-builtin) provider is
//! allowed at a time — attempting to register a second external provider
//! replaces the first with a warning.
//!
//! The MemoryManager is wrapped in Arc<Mutex<>> for shared access from both
//! the main engine loop and the MemoryProviderTool bridge tools.

use std::sync::Arc;
use tokio::sync::Mutex;

use super::provider::{MemoryProvider, ProviderError, ProviderResult};
use crate::memory::store::MemoryStore;
use serde_json::Value;

/// Orchestrates the built-in memory provider plus at most ONE external provider.
pub struct MemoryManager {
    /// The always-on built-in memory store (MEMORY.md / USER.md).
    builtin_store: Arc<Mutex<MemoryStore>>,
    /// Optional external provider (mem0, honcho, lua-configured, etc.).
    external_provider: Option<Box<dyn MemoryProvider>>,
    /// Whether the external provider has been initialized.
    external_initialized: bool,
    /// Current session ID (for provider lifecycle).
    session_id: String,
    /// Cached prefetch result from queue_prefetch(), consumed by prefetch().
    prefetch_cache: Option<String>,
}

// Implement Clone by wrapping in Arc<Mutex<>> — the standard pattern for
// shared mutable state across tool execution.
pub type SharedMemoryManager = Arc<Mutex<MemoryManager>>;

impl MemoryManager {
    /// Create a new MemoryManager with the built-in store.
    pub fn new(builtin_store: Arc<Mutex<MemoryStore>>) -> Self {
        Self {
            builtin_store,
            external_provider: None,
            external_initialized: false,
            session_id: String::new(),
            prefetch_cache: None,
        }
    }

    /// Set (or replace) the external memory provider.
    /// Only ONE external provider can be active at a time.
    /// If a provider is already set, it is shut down first.
    pub async fn set_external(&mut self, provider: Box<dyn MemoryProvider>) {
        if let Some(ref mut old) = self.external_provider {
            tracing::warn!(
                old_provider = %old.name(),
                new_provider = %provider.name(),
                "Replacing external memory provider"
            );
            old.shutdown().await;
        }
        tracing::info!(provider = %provider.name(), "External memory provider set");
        self.external_provider = Some(provider);
        self.external_initialized = false;
    }

    /// Initialize all providers for a session.
    pub async fn initialize(&mut self, session_id: &str) {
        self.session_id = session_id.to_string();

        if let Some(ref mut p) = self.external_provider
            && p.is_available()
            && !self.external_initialized
        {
            match p.initialize(session_id).await {
                Ok(()) => {
                    self.external_initialized = true;
                    tracing::info!(
                        provider = %p.name(),
                        session_id = %session_id,
                        "External memory provider initialized"
                    );
                }
                Err(e) => {
                    tracing::error!(
                        provider = %p.name(),
                        error = %e,
                        "Failed to initialize external memory provider"
                    );
                }
            }
        }
    }

    /// Build the system prompt section from all providers.
    /// Combines built-in memory snapshot + external provider's system_prompt_block().
    pub fn build_system_prompt(&self) -> String {
        let mut parts = Vec::new();

        // Built-in memory snapshot
        if let Ok(store) = self.builtin_store.try_lock() {
            let mem_block = store.format_for_system_prompt("memory");
            let usr_block = store.format_for_system_prompt("user");
            let combined: Vec<&str> = [mem_block, usr_block].into_iter().flatten().collect();
            if !combined.is_empty() {
                parts.push(combined.join("\n\n"));
            }
        }

        // External provider system prompt block
        if let Some(ref p) = self.external_provider
            && self.external_initialized
        {
            let block = p.system_prompt_block();
            if !block.is_empty() {
                parts.push(block);
            }
        }

        if parts.is_empty() {
            String::new()
        } else {
            format!("## Memory\n\n{}", parts.join("\n\n"))
        }
    }

    /// Get tool schemas from the external provider (if any).
    /// Built-in memory uses the standard `memory` tool registered separately.
    pub fn get_external_tool_schemas(&self) -> Vec<Value> {
        if let Some(ref p) = self.external_provider
            && self.external_initialized
        {
            return p.get_tool_schemas();
        }
        Vec::new()
    }

    /// Handle a tool call for one of the external provider's tools.
    pub async fn handle_external_tool_call(&self, tool_name: &str, args: &Value) -> ProviderResult {
        if let Some(ref p) = self.external_provider
            && self.external_initialized
        {
            return p.handle_tool_call(tool_name, args).await;
        }
        Err(ProviderError::ToolNotFound(format!(
            "No external memory provider active for tool '{}'",
            tool_name
        )))
    }

    /// Get the name of the active external provider (if any).
    pub fn external_name(&self) -> Option<&str> {
        self.external_provider.as_ref().map(|p| p.name())
    }

    /// Prefetch memory context from the external provider before a turn.
    ///
    /// If a cached prefetch result is available (from queue_prefetch),
    /// returns it immediately. Otherwise calls the provider synchronously.
    /// Returns formatted text to inject into the system prompt, or empty string.
    pub async fn prefetch(&mut self, query: &str) -> String {
        // Return cached result if available
        if let Some(cached) = self.prefetch_cache.take() {
            return cached;
        }
        // Otherwise fetch synchronously
        if let Some(ref p) = self.external_provider
            && self.external_initialized
        {
            return p.prefetch(query).await;
        }
        String::new()
    }

    /// Queue a background prefetch on the external provider for the NEXT turn.
    /// The result will be consumed by prefetch() on the next API call.
    /// Also notifies the external provider of the current turn's context.
    pub fn queue_prefetch(&self, query: &str) {
        if let Some(ref p) = self.external_provider
            && self.external_initialized
        {
            p.queue_prefetch(query);
        }
    }

    /// Sync a completed turn to the external provider.
    pub async fn sync_turn(&self, user_content: &str, assistant_content: &str) {
        if let Some(ref p) = self.external_provider
            && self.external_initialized
        {
            p.sync_turn(user_content, assistant_content).await;
        }
    }

    /// Notify the external provider of a built-in memory write.
    pub async fn on_memory_change(&self, action: &str, target: &str, content: &str) {
        if let Some(ref p) = self.external_provider
            && self.external_initialized
        {
            p.on_memory_change(action, target, content);
        }
    }

    /// Notify the external provider of a new turn starting.
    pub fn on_turn_start(&self, turn_number: u32, message: &str) {
        if let Some(ref p) = self.external_provider
            && self.external_initialized
        {
            p.on_turn_start(turn_number, message);
        }
    }

    /// Notify the external provider of session end.
    /// Called on explicit exit, /reset, or session timeout.
    pub async fn on_session_end(&self, messages: &[Value]) {
        if let Some(ref p) = self.external_provider
            && self.external_initialized
        {
            p.on_session_end(messages).await;
        }
    }

    /// Notify the external provider of a session_id change.
    /// Fires on /resume, /branch, /reset, /new and context compression.
    pub fn on_session_switch(&self, new_session_id: &str, parent_session_id: &str, reset: bool) {
        if let Some(ref p) = self.external_provider
            && self.external_initialized
        {
            p.on_session_switch(new_session_id, parent_session_id, reset);
        }
    }

    /// Ask the external provider for text to inject into the compression
    /// summary prompt. Returns empty string if no provider or no contribution.
    pub fn on_pre_compress(&self, messages: &[Value]) -> String {
        if let Some(ref p) = self.external_provider
            && self.external_initialized
        {
            return p.on_pre_compress(messages);
        }
        String::new()
    }

    /// Shut down the external provider.
    pub async fn shutdown(&mut self) {
        if let Some(ref mut p) = self.external_provider {
            p.shutdown().await;
        }
    }
}
