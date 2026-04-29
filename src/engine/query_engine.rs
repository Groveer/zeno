//! Query engine: manages conversation state and turn count.
//!
//! The actual streaming loop lives in `query.rs`. This module provides
//! the state container that wraps a client + history + config.

use crate::api::client::SupportsStreamingMessages;
use crate::engine::messages::ConversationHistory;

/// Holds all state for a conversation session.
pub struct QueryEngine {
    pub client: Box<dyn SupportsStreamingMessages>,
    pub model: String,
    pub system_prompt: String,
    pub history: ConversationHistory,
    pub max_turns: u32,
    pub max_tokens: u32,
}

impl QueryEngine {
    pub fn new(
        client: Box<dyn SupportsStreamingMessages>,
        model: String,
        system_prompt: String,
        history: ConversationHistory,
        max_turns: u32,
        max_tokens: u32,
    ) -> Self {
        Self {
            client,
            model,
            system_prompt,
            history,
            max_turns,
            max_tokens,
        }
    }
}
