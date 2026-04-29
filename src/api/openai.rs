//! OpenAI-compatible API client with streaming support.
//!
//! Works with any OpenAI-compatible endpoint (OpenAI, Groveer, etc.)

use std::pin::Pin;

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::{Stream, StreamExt};
use reqwest::Client;

use crate::api::client::SupportsStreamingMessages;
use crate::api::types::{ApiError, Message, Role, StreamEvent};

pub struct OpenAIClient {
    http: Client,
    api_key: String,
    base_url: String,
}

impl OpenAIClient {
    pub fn new(api_key: String, base_url: String) -> Self {
        Self {
            http: Client::new(),
            api_key,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    fn build_request_body(
        &self,
        model: &str,
        system: &str,
        messages: &[Message],
        tools: &[serde_json::Value],
        max_tokens: u32,
    ) -> serde_json::Value {
        let mut api_messages = Vec::new();

        // System message
        if !system.is_empty() {
            api_messages.push(serde_json::json!({
                "role": "system",
                "content": system,
            }));
        }

        // Convert messages
        for m in messages {
            let role_str = match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            let content: Vec<serde_json::Value> = m
                .content
                .iter()
                .map(|block| serde_json::to_value(block).unwrap_or_default())
                .collect();
            api_messages.push(serde_json::json!({
                "role": role_str,
                "content": content,
            }));
        }

        let mut body = serde_json::json!({
            "model": model,
            "messages": api_messages,
            "stream": true,
        });

        if max_tokens > 0 {
            // OpenAI uses max_completion_tokens in newer API versions
            body["max_completion_tokens"] = serde_json::json!(max_tokens);
        }

        if !tools.is_empty() {
            body["tools"] = serde_json::json!(tools);
        }

        body
    }
}

#[async_trait]
impl SupportsStreamingMessages for OpenAIClient {
    async fn stream_messages(
        &self,
        model: &str,
        system: &str,
        messages: &[Message],
        tools: &[serde_json::Value],
        max_tokens: u32,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ApiError>> + Send>>, ApiError> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = self.build_request_body(model, system, messages, tools, max_tokens);

        let response = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ApiError::Request(e))?;

        let status = response.status();
        if status.is_client_error() || status.is_server_error() {
            let status_code = status.as_u16();
            let body_text = response.text().await.unwrap_or_default();
            if status_code == 402 {
                return Err(ApiError::PaymentRequired);
            }
            return Err(ApiError::Http {
                status: status_code,
                body: body_text,
            });
        }

        // OpenAI SSE format: each data line is a JSON with choices[0].delta.content
        Ok(parse_openai_sse_stream(response))
    }
}

/// Parse OpenAI-format SSE stream into StreamEvents.
fn parse_openai_sse_stream(
    response: reqwest::Response,
) -> Pin<Box<dyn Stream<Item = Result<StreamEvent, ApiError>> + Send>> {
    let event_stream = response.bytes_stream().eventsource();

    let mapped = event_stream.filter_map(|event| async move {
        match event {
            Ok(evt) => {
                if evt.data == "[DONE]" {
                    // Emit a synthetic MessageComplete for OpenAI streams
                    return Some(Ok(StreamEvent::MessageComplete {
                        stop_reason: crate::api::types::StopReason::EndTurn,
                        usage: crate::api::types::Usage::default(),
                    }));
                }
                if evt.data.is_empty() {
                    return None;
                }
                parse_openai_chunk(&evt.data)
            }
            Err(e) => Some(Err(ApiError::Stream(format!(
                "SSE parsing error: {}",
                e
            )))),
        }
    });

    Box::pin(mapped)
}

fn parse_openai_chunk(data: &str) -> Option<Result<StreamEvent, ApiError>> {
    let v: serde_json::Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(e) => return Some(Err(ApiError::Json(e))),
    };

    let choices = v.get("choices")?.as_array()?;
    if choices.is_empty() {
        return None;
    }

    let choice = &choices[0];
    let delta = choice.get("delta")?;

    // Text content
    if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
        if !content.is_empty() {
            return Some(Ok(StreamEvent::TextDelta(content.to_string())));
        }
    }

    // Tool calls
    if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tool_calls {
            let index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
            let id = tc
                .get("id")
                .and_then(|i| i.as_str())
                .unwrap_or("")
                .to_string();
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();

            if !name.is_empty() && !id.is_empty() {
                // This is a new tool call start
                return Some(Ok(StreamEvent::ToolUseStart { id, name }));
            }

            // Tool call arguments delta
            if let Some(arguments) = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str())
            {
                if !arguments.is_empty() {
                    let tool_id = tc
                        .get("id")
                        .and_then(|i| i.as_str())
                        .unwrap_or(&format!("tool_{}", index))
                        .to_string();
                    return Some(Ok(StreamEvent::ToolUseDelta {
                        id: tool_id,
                        delta_json: arguments.to_string(),
                    }));
                }
            }
        }
    }

    // Check finish_reason
    if let Some(finish_reason) = choice.get("finish_reason").and_then(|f| f.as_str()) {
        match finish_reason {
            "stop" => {
                return Some(Ok(StreamEvent::MessageComplete {
                    stop_reason: crate::api::types::StopReason::EndTurn,
                    usage: crate::api::types::Usage::default(),
                }));
            }
            "tool_calls" => {
                return Some(Ok(StreamEvent::MessageComplete {
                    stop_reason: crate::api::types::StopReason::ToolUse,
                    usage: crate::api::types::Usage::default(),
                }));
            }
            "length" => {
                return Some(Ok(StreamEvent::MessageComplete {
                    stop_reason: crate::api::types::StopReason::MaxTokens,
                    usage: crate::api::types::Usage::default(),
                }));
            }
            _ => {}
        }
    }

    // Extract usage from the response if present
    if let Some(usage) = v.get("usage") {
        let _ = usage; // We'll get usage from the final chunk
    }

    None
}
