//! Query engine: manages conversation state and tool registry.

use crate::api::client::SupportsStreamingMessages;
use crate::config::settings::PermissionMode;
use crate::engine::messages::ConversationHistory;
use crate::tools::base::ToolRegistry;

/// Holds all state for a conversation session.
pub struct QueryEngine {
    pub client: Box<dyn SupportsStreamingMessages>,
    pub model: String,
    pub system_prompt: String,
    pub history: ConversationHistory,
    pub tools: ToolRegistry,
    pub max_turns: u32,
    pub max_tokens: u32,
    pub permission_mode: PermissionMode,
}

impl QueryEngine {
    pub fn new(
        client: Box<dyn SupportsStreamingMessages>,
        model: String,
        system_prompt: String,
        history: ConversationHistory,
        tools: ToolRegistry,
        max_turns: u32,
        max_tokens: u32,
        permission_mode: PermissionMode,
    ) -> Self {
        Self {
            client,
            model,
            system_prompt,
            history,
            tools,
            max_turns,
            max_tokens,
            permission_mode,
        }
    }
}
