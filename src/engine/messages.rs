//! Conversation history management.
//!
//! Supports multi-content-block messages (text + tool_use + tool_result).

use crate::api::types::{ContentBlock, Message, Role};

/// A single entry in the conversation history.
#[derive(Debug, Clone)]
pub struct ConversationEntry {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

/// Manages conversation history.
#[derive(Debug, Clone, Default)]
pub struct ConversationHistory {
    entries: Vec<ConversationEntry>,
}

impl ConversationHistory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a user message (text only).
    pub fn push_user(&mut self, text: &str) {
        self.entries.push(ConversationEntry {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: text.into(),
            }],
        });
    }

    /// Add an assistant message with multiple content blocks.
    /// Used when the assistant produces text + tool_use in one turn.
    pub fn push_assistant_blocks(&mut self, blocks: Vec<ContentBlock>) {
        self.entries.push(ConversationEntry {
            role: Role::Assistant,
            content: blocks,
        });
    }

    /// Add an assistant message (text only, backwards compat).
    pub fn push_assistant(&mut self, text: &str) {
        self.push_assistant_blocks(vec![ContentBlock::Text {
            text: text.into(),
        }]);
    }

    /// Add a tool result message (user role with tool_result content blocks).
    pub fn push_tool_results(&mut self, results: Vec<ContentBlock>) {
        self.entries.push(ConversationEntry {
            role: Role::User,
            content: results,
        });
    }

    /// Convert the full history to API `Message` format.
    pub fn to_api_messages(&self) -> Vec<Message> {
        self.entries
            .iter()
            .map(|e| Message {
                role: e.role.clone(),
                content: e.content.clone(),
            })
            .collect()
    }

    /// Number of entries in history.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the history is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Clear all history.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}
