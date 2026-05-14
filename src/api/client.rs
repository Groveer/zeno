//! The streaming API client trait — all providers implement this.

use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;
use futures::StreamExt;

use crate::api::types::{ApiError, Message, StreamEvent};
/// A provider that can stream chat messages.
#[async_trait]
pub trait SupportsStreamingMessages: Send + Sync {
    /// Send a streaming request and return a stream of events.
    ///
    /// `max_tokens`: when `Some(N)`, include in the API request to cap output;
    /// when `None`, omit it and let the provider use its own default.
    /// Anthropic's Messages API requires `max_tokens` as a mandatory field,
    /// so AnthropicClient must provide a fallback when `None` is passed.
    async fn stream_messages(
        &self,
        model: &str,
        system: &str,
        messages: &[Message],
        tools: &[serde_json::Value],
        max_tokens: Option<u32>,
        response_format: Option<&serde_json::Value>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ApiError>> + Send>>, ApiError>;

    /// Non-streaming call: collect the full stream and return the assistant text.
    ///
    /// This is a convenience wrapper around `stream_messages` for use by
    /// auxiliary tasks (vision analysis, compression, sub-agents) that need
    /// a complete response rather than incremental deltas.
    async fn call_messages(
        &self,
        model: &str,
        system: &str,
        messages: &[Message],
        tools: &[serde_json::Value],
        max_tokens: Option<u32>,
        response_format: Option<&serde_json::Value>,
    ) -> Result<String, ApiError> {
        let mut stream = self
            .stream_messages(model, system, messages, tools, max_tokens, response_format)
            .await?;
        let mut full_text = String::new();
        while let Some(event) = stream.next().await {
            match event? {
                StreamEvent::TextDelta(text) => full_text.push_str(&text),
                StreamEvent::MessageComplete { .. } => break,
                _ => {} // ignore tool/metadata events
            }
        }
        Ok(full_text)
    }
}
