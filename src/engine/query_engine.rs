//! Query engine: manages conversation state and tool registry.

use crate::api::client::SupportsStreamingMessages;
use crate::config::settings::{PermissionMode, ProviderConfig, Settings};
use crate::engine::carryover::Carryover;
use crate::engine::compact::CompactConfig;
use crate::engine::cost_tracker::CostTracker;
use crate::engine::messages::{ConversationEntry, ConversationHistory};
use crate::hooks::executor::HookExecutor;
use crate::memory::manager::SharedMemoryManager;
use crate::tools::base::ToolRegistry;
use std::sync::{Arc, Mutex};

/// Holds all state for a conversation session.
pub struct QueryEngine {
    pub client: Box<dyn SupportsStreamingMessages>,
    pub model: String,
    pub system_prompt: String,
    pub history: ConversationHistory,
    pub tools: Arc<ToolRegistry>,
    pub max_turns: u32,
    pub max_tokens: u32,
    pub permission_mode: PermissionMode,
    pub cost_tracker: CostTracker,
    /// Shared cost tracker for sub-agents. Sub-agents fold their token usage
    /// into this; the main engine absorbs it into `cost_tracker` after each
    /// tool-turn that contained delegate_task calls.
    pub sub_agent_cost_tracker: Arc<Mutex<CostTracker>>,
    pub compact_config: CompactConfig,
    pub settings: Arc<Settings>,
    /// Working directory captured at session start. Used instead of
    /// `std::env::current_dir()` to avoid inconsistencies if the process
    /// cwd changes, and to prevent `unwrap_or_default()` from returning
    /// an empty path that breaks permission boundaries.
    pub cwd: std::path::PathBuf,
    /// Working memory that tracks what the agent has read/written/done.
    /// Survives across turns and is injected into compression context.
    pub carryover: Carryover,
    /// Optional hook executor for pre/post tool-use events.
    pub hook_executor: Option<HookExecutor>,
    /// When true, all permission checks are auto-approved for this session.
    /// Set when the user answers "a" (yes to all) in a permission prompt.
    /// Wrapped in Arc<Mutex<>> so it can be shared with tool execution functions.
    pub permission_allow_all: Arc<Mutex<bool>>,
    /// Pending user input injected while the LLM is running (steer).
    /// Thread-safe: the TUI writes into this slot via `steer_into_slot()`,
    /// and the engine drains it after tool results are appended (before
    /// the next API call). Multiple steers concatenate with newlines.
    pub(crate) pending_steer: Arc<Mutex<Option<String>>>,
    /// Shared MCP manager for lazy MCP server connections.
    pub mcp_manager: Option<Arc<tokio::sync::Mutex<crate::mcp::manager::McpManager>>>,
    /// Shared memory manager for external provider lifecycle (prefetch, sync, mirroring).
    pub memory_manager: Option<SharedMemoryManager>,
    /// Factory function to create API clients for sub-agents.
    /// Takes a provider name and ProviderConfig, returns a new client.
    /// If None, sub-agents use the parent's client directly.
    pub client_factory: Option<
        Arc<dyn Fn(&str, &ProviderConfig) -> Box<dyn SupportsStreamingMessages> + Send + Sync>,
    >,
    /// Sender for sub-agent progress events (from App).
    /// Cloned into ToolContext so delegate_task can report progress.
    pub sub_agent_tx:
        Option<tokio::sync::mpsc::UnboundedSender<crate::engine::sub_agent::SubAgentEvent>>,
    /// Turn counter for background skill review scheduling.
    pub turns_since_skill_review: u32,
    /// Optional cancellation token for background review tasks.
    /// When the app exits, this token is cancelled so background reviews stop.
    pub background_cancel: Option<tokio_util::sync::CancellationToken>,
    /// Shared tool result cache for read-only tools (read, glob, grep).
    /// Reduces redundant execution when the LLM calls the same tool
    /// with the same arguments within a short window.
    pub tool_cache: crate::tools::cache::SharedToolCache,
    /// Shared rate limiter for bash tool execution.
    pub rate_limiter: crate::tools::rate_limiter::SharedRateLimiter,
    /// Tool usage statistics collector.
    pub tool_stats: crate::tools::tool_stats::SharedToolStats,
}

/// Inject user text into a steer slot without interrupting the agent.
///
/// Thread-safe: callable from the TUI event loop while the engine's
/// `query_tui` is executing. The text is stashed and drained after
/// tool results are appended — the model sees it on the next iteration.
///
/// Multiple calls before the drain point concatenate with newlines.
///
/// Returns `true` if the steer was accepted, `false` if the text was empty.
pub fn steer_into_slot(slot: &Arc<Mutex<Option<String>>>, text: &str) -> bool {
    let cleaned = text.trim();
    if cleaned.is_empty() {
        return false;
    }
    let mut guard = slot.lock().expect("steer_into_slot: mutex poisoned");
    match guard.as_mut() {
        Some(existing) => {
            existing.push('\n');
            existing.push_str(cleaned);
        }
        None => *guard = Some(cleaned.to_string()),
    }
    true
}

impl QueryEngine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: Box<dyn SupportsStreamingMessages>,
        model: String,
        system_prompt: String,
        history: ConversationHistory,
        tools: Arc<ToolRegistry>,
        max_turns: u32,
        max_tokens: u32,
        permission_mode: PermissionMode,
        settings: Arc<Settings>,
        cwd: std::path::PathBuf,
    ) -> Self {
        let compact_config = CompactConfig {
            threshold_ratio: settings.llm.compact_threshold,
            enabled: settings.llm.compact_threshold > 0.0,
            ..CompactConfig::default()
        };
        Self {
            client,
            model,
            system_prompt,
            history,
            tools,
            max_turns,
            max_tokens,
            permission_mode,
            cost_tracker: CostTracker::default(),
            sub_agent_cost_tracker: Arc::new(Mutex::new(CostTracker::default())),
            compact_config,
            settings,
            cwd,
            carryover: Carryover::default(),
            hook_executor: None,
            permission_allow_all: Arc::new(Mutex::new(false)),
            pending_steer: Arc::new(Mutex::new(None)),
            mcp_manager: None,
            memory_manager: None,
            client_factory: None,
            sub_agent_tx: None,
            turns_since_skill_review: 0,
            background_cancel: None,
            tool_cache: crate::tools::cache::new_shared(),
            rate_limiter: crate::tools::rate_limiter::new_shared(),
            tool_stats: crate::tools::tool_stats::new_shared(),
        }
    }

    // -----------------------------------------------------------------------
    // Runtime configuration setters
    // -----------------------------------------------------------------------

    /// Update the active model for future turns.
    pub fn set_model(&mut self, model: String) {
        self.model = model;
    }

    /// Switch to a different provider. Looks up the provider config from
    /// settings and updates the model to the provider's default_model.
    ///
    /// Note: the API client swap requires a `create_client` factory function
    /// (not yet implemented). Currently only the model field is updated.
    #[allow(dead_code, reason = "called via Lua config script")]
    pub fn set_provider(&mut self, provider: &str) -> Result<(), String> {
        let config = self
            .settings
            .providers
            .get(provider)
            .ok_or_else(|| format!("Provider '{}' not found in config", provider))?;
        // TODO: implement crate::api::create_client(config) factory function
        // self.client = crate::api::create_client(config)?;
        self.model = config.default_model.clone();
        Ok(())
    }

    /// Update the system prompt for future turns.
    #[allow(dead_code, reason = "called via Lua config script")]
    pub fn set_system_prompt(&mut self, prompt: String) {
        self.system_prompt = prompt;
    }

    /// Update the maximum number of agentic turns per user input.
    /// Enforces a minimum of 1.
    #[allow(dead_code, reason = "called via Lua config script")]
    pub fn set_max_turns(&mut self, max_turns: u32) {
        self.max_turns = max_turns.max(1);
    }

    // -----------------------------------------------------------------------
    // Mid-run user input (steer)
    // -----------------------------------------------------------------------

    /// Return the pending steer text (if any) and clear the slot.
    /// Called from the engine thread after appending tool results.
    pub fn drain_steer(&self) -> Option<String> {
        let mut guard = self
            .pending_steer
            .lock()
            .expect("pending_steer: mutex poisoned");
        guard.take()
    }

    /// Clear any pending steer (e.g. on interrupt).
    pub fn clear_steer(&self) {
        let mut guard = self
            .pending_steer
            .lock()
            .expect("pending_steer: mutex poisoned");
        *guard = None;
    }

    // -----------------------------------------------------------------------
    // Session restore
    // -----------------------------------------------------------------------

    /// Replace the in-memory conversation history with pre-existing entries.
    /// Sanitizes the loaded messages and resets the cost tracker.
    #[allow(dead_code, reason = "called via Lua config script")]
    pub fn load_messages(&mut self, entries: Vec<ConversationEntry>) {
        self.history = ConversationHistory::from_entries(entries);
        self.history.sanitize();
        self.cost_tracker = CostTracker::default();
    }

    /// Resolve the effective max_output_tokens for API calls.
    ///
    /// Returns `Some(value)` when the user or provider has explicitly set a
    /// limit, and `None` when in auto mode. When `None`, OpenAI-compatible
    /// clients omit `max_completion_tokens` from the request body, letting
    /// the provider use its own default — matching Hermes Agent behavior.
    pub fn effective_max_tokens(&self) -> Option<u32> {
        let provider_mo = self
            .settings
            .providers
            .get(&self.settings.active_provider)
            .and_then(|pc| pc.max_output_tokens);

        crate::config::model_context::resolve_max_output_tokens(self.max_tokens, provider_mo)
    }

    /// Resolve the effective context window for the current model.
    pub fn effective_context_window(&self) -> u32 {
        crate::config::model_context::resolve_context_window(
            &self.model,
            &self.settings.model_contexts,
        )
    }
}
