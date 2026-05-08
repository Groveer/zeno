//! Hook system types.
//!
//! A hook is a Lua function registered via `zn.hook(event, fn)` in init.lua.
//! Each event type defines what context the callback receives and what it
//! can return (block, inject context, modify input, or observe-only).

/// Events that trigger hooks.
///
/// Naming convention follows hermes-agent: `pre_` / `post_` for paired events,
/// bare names for lifecycle events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookEvent {
    // --- Tool lifecycle ---
    /// Before tool execution. Can block with a reason.
    PreToolUse,
    /// After tool execution. Observe-only.
    PostToolUse,

    // --- Session lifecycle ---
    /// New session created (on startup or after /clear).
    SessionStart,
    /// Session ending (user quit, /clear).
    SessionEnd,

    // --- Agent loop ---
    /// Before LLM API request. Can inject extra context into the conversation.
    PreLlmCall,
    /// After LLM API response. Observe tokens, model, duration.
    PostLlmCall,

    // --- User interaction ---
    /// User submitted input. Can transform the input before processing.
    UserMessage,
}

/// All valid hook event names (for Lua API validation).
pub const VALID_HOOK_EVENTS: &[(&str, HookEvent)] = &[
    ("pre_tool_use", HookEvent::PreToolUse),
    ("post_tool_use", HookEvent::PostToolUse),
    ("session_start", HookEvent::SessionStart),
    ("session_end", HookEvent::SessionEnd),
    ("pre_llm_call", HookEvent::PreLlmCall),
    ("post_llm_call", HookEvent::PostLlmCall),
    ("user_message", HookEvent::UserMessage),
];

/// Result returned by a hook execution.
///
/// Each variant corresponds to a specific action the hook system should take.
/// Variants that don't match the current event type are silently ignored
/// (e.g., `Block` from a `SessionStart` hook is a no-op).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum HookResult {
    /// No-op — proceed normally.
    Continue,
    /// Block the current action (only meaningful for `PreToolUse`).
    Block { reason: String },
    /// Inject extra context before the LLM call (only for `PreLlmCall`).
    InjectContext(String),
    /// Replace user input (only for `UserMessage`).
    ModifiedInput(String),
}
