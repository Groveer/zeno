//! The streaming API client trait — all providers implement this.

use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;

use crate::api::types::{ApiError, Message, StreamEvent};

/// A provider that can stream chat messages.
#[async_trait]
pub trait SupportsStreamingMessages: Send + Sync {
    /// Send a streaming request and return a stream of events.
    async fn stream_messages(
        &self,
        model: &str,
        system: &str,
        messages: &[Message],
        tools: &[serde_json::Value],
        max_tokens: u32,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ApiError>> + Send>>, ApiError>;
}
