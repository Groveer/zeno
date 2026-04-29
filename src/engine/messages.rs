//! Conversation history management.

use crate::api::types::Message;

/// A single turn in the conversation, stored for history and context window management.
#[derive(Debug, Clone)]
pub struct ConversationMessage {
    pub role: String,
    pub text: String,
}

/// Manages conversation history.
#[derive(Debug, Clone, Default)]
pub struct ConversationHistory {
    messages: Vec<ConversationMessage>,
}

impl ConversationHistory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a user message.
    pub fn push_user(&mut self, text: &str) {
        self.messages.push(ConversationMessage {
            role: "user".into(),
            text: text.into(),
        });
    }

    /// Add an assistant message.
    pub fn push_assistant(&mut self, text: &str) {
        self.messages.push(ConversationMessage {
            role: "assistant".into(),
            text: text.into(),
        });
    }

    /// Convert the full history to API `Message` format.
    pub fn to_api_messages(&self) -> Vec<Message> {
        self.messages
            .iter()
            .map(|m| {
                if m.role == "user" {
                    Message::user(&m.text)
                } else {
                    Message::assistant(&m.text)
                }
            })
            .collect()
    }

    /// Number of messages in history.
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Whether the history is empty.
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Clear all history.
    pub fn clear(&mut self) {
        self.messages.clear();
    }
}
