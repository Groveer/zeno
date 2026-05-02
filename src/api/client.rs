//! The streaming API client trait — all providers implement this.

use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;

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
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ApiError>> + Send>>, ApiError>;
}
