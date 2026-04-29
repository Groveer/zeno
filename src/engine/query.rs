//! Core tool-aware conversation loop.
//!
//! Phase 1 version: no tool support, single turn per user message.
//! The loop structure is in place for Phase 2 tool integration.

use std::io::Write;
use std::pin::Pin;

use futures::{Stream, StreamExt};

use crate::api::types::{ApiError, StopReason, StreamEvent, Usage};
use crate::engine::query_engine::QueryEngine;

/// Result of a completed query turn.
#[derive(Debug)]
pub struct QueryResult {
    pub text: String,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

impl QueryEngine {
    /// Run a user query through the LLM with streaming output.
    ///
    /// In Phase 1 this is a single turn (no tool loop).
    /// In Phase 2+, tool_use triggers execution and continues the loop.
    pub async fn query(&mut self, user_input: &str) -> Result<QueryResult, ApiError> {
        self.history.push_user(user_input);

        let mut turn = 0;
        let mut final_text = String::new();
        let mut final_usage = Usage::default();
        let mut final_stop = StopReason::EndTurn;

        loop {
            turn += 1;
            if turn > self.max_turns {
                break;
            }

            let messages = self.history.to_api_messages();

            let stream = self
                .client
                .stream_messages(
                    &self.model,
                    &self.system_prompt,
                    &messages,
                    &[], // Phase 1: no tools
                    self.max_tokens,
                )
                .await?;

            let mut assistant_text = String::new();
            let mut current_tool_uses: Vec<(String, String, String)> = Vec::new(); // (id, name, input_json)

            // Consume the stream — pin for async iteration
            let mut pinned: Pin<Box<dyn Stream<Item = Result<StreamEvent, ApiError>> + Send>> =
                Box::pin(stream);

            while let Some(event) = pinned.as_mut().next().await {
                match event? {
                    StreamEvent::TextDelta(delta) => {
                        print!("{}", delta);
                        let _ = std::io::stdout().flush();
                        assistant_text.push_str(&delta);
                    }
                    StreamEvent::ToolUseStart { id, name } => {
                        current_tool_uses.push((id, name, String::new()));
                    }
                    StreamEvent::ToolUseDelta { id, delta_json } => {
                        if let Some(entry) = current_tool_uses.iter_mut().find(|(i, _, _)| *i == id)
                        {
                            entry.2.push_str(&delta_json);
                        }
                    }
                    StreamEvent::MessageComplete { stop_reason, usage } => {
                        final_usage = usage;
                        final_stop = stop_reason.clone();
                    }
                    StreamEvent::Error(e) => {
                        return Err(ApiError::Stream(e));
                    }
                }
            }

            final_text.clone_from(&assistant_text);
            self.history.push_assistant(&assistant_text);

            // Phase 1: no tool loop — break after first turn
            // Phase 2: if !current_tool_uses.is_empty() { execute tools; continue; }
            break;
        }

        Ok(QueryResult {
            text: final_text,
            stop_reason: final_stop,
            usage: final_usage,
        })
    }
}
