//! OpenAI-compatible API client with streaming support.
//!
//! Handles the format differences between OpenAI's tool calling format
//! and our internal ContentBlock-based message format.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::{Stream, StreamExt};
use reqwest::Client;
use serde_json::{json, Value};

use crate::api::client::SupportsStreamingMessages;
use crate::api::types::{ApiError, ContentBlock, Message, Role, StreamEvent, StopReason, Usage};

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
        tools: &[Value],
        max_tokens: u32,
    ) -> Value {
        let mut api_messages = Vec::new();

        if !system.is_empty() {
            api_messages.push(json!({
                "role": "system",
                "content": system,
            }));
        }

        for m in messages {
            let role_str = match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };

            let has_tool_result = m
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. }));

            if has_tool_result {
                for block in &m.content {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } = block
                    {
                        let mut msg = json!({
                            "role": "tool",
                            "tool_call_id": tool_use_id,
                            "content": content,
                        });
                        if let Some(true) = is_error {
                            msg["metadata"] = json!({ "is_error": true });
                        }
                        api_messages.push(msg);
                    }
                }
            } else if role_str == "assistant" {
                let text_parts: Vec<String> = m
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect();
                let tool_uses: Vec<&ContentBlock> = m
                    .content
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
                    .collect();

                let mut msg = json!({ "role": "assistant" });

                if !text_parts.is_empty() {
                    msg["content"] = json!(text_parts.join(""));
                } else {
                    msg["content"] = json!(null);
                }

                if !tool_uses.is_empty() {
                    let calls: Vec<Value> = tool_uses
                        .iter()
                        .enumerate()
                        .map(|(idx, block)| {
                            if let ContentBlock::ToolUse { id, name, input } = block {
                                json!({
                                    "id": id,
                                    "type": "function",
                                    "function": {
                                        "name": name,
                                        "arguments": input.to_string(),
                                    },
                                    "index": idx,
                                })
                            } else {
                                unreachable!()
                            }
                        })
                        .collect();
                    msg["tool_calls"] = json!(calls);
                }

                api_messages.push(msg);
            } else {
                let text: String = m
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                api_messages.push(json!({
                    "role": "user",
                    "content": text,
                }));
            }
        }

        let mut body = json!({
            "model": model,
            "messages": api_messages,
            "stream": true,
        });

        if max_tokens > 0 {
            body["max_completion_tokens"] = json!(max_tokens);
        }

        if !tools.is_empty() {
            body["tools"] = json!(tools);
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
        tools: &[Value],
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

        Ok(parse_openai_sse_stream(response))
    }
}

/// Parse OpenAI-format SSE stream into StreamEvents.
/// Uses Arc<Mutex<>> to track tool call index -> id mapping across chunks (Send-safe).
fn parse_openai_sse_stream(
    response: reqwest::Response,
) -> Pin<Box<dyn Stream<Item = Result<StreamEvent, ApiError>> + Send>> {
    let event_stream = response.bytes_stream().eventsource();
    let id_map: Arc<Mutex<HashMap<u64, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let id_map_clone = id_map.clone();

    let mapped = event_stream.filter_map(move |event| {
        let id_map = id_map_clone.clone();
        async move {
            let event = match event {
                Ok(evt) => evt,
                Err(e) => {
                    return Some(Err(ApiError::Stream(format!("SSE error: {}", e))));
                }
            };

            if event.data == "[DONE]" {
                if let Ok(mut m) = id_map.lock() {
                    m.clear();
                }
                return Some(Ok(StreamEvent::MessageComplete {
                    stop_reason: StopReason::EndTurn,
                    usage: Usage::default(),
                }));
            }
            if event.data.is_empty() {
                return None;
            }

            tracing::trace!("SSE chunk: {}", &event.data[..event.data.len().min(500)]);

            match id_map.lock() {
                Ok(mut m) => parse_openai_chunk(&event.data, &mut m),
                Err(e) => Some(Err(ApiError::Stream(format!("lock error: {}", e)))),
            }
        }
    });

    Box::pin(mapped)
}

fn parse_openai_chunk(
    data: &str,
    id_map: &mut HashMap<u64, String>,
) -> Option<Result<StreamEvent, ApiError>> {
    let v: serde_json::Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(e) => return Some(Err(ApiError::Json(e))),
    };

    // Extract usage if present (final chunk)
    if let Some(usage) = v.get("usage") {
        let input = usage.get("prompt_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
        let output = usage.get("completion_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
        if input > 0 || output > 0 {
            let finish = v
                .get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("finish_reason"))
                .and_then(|f| f.as_str());
            let stop_reason = match finish {
                Some("stop") => StopReason::EndTurn,
                Some("tool_calls") => StopReason::ToolUse,
                Some("length") => StopReason::MaxTokens,
                _ => StopReason::EndTurn,
            };
            return Some(Ok(StreamEvent::MessageComplete {
                stop_reason,
                usage: Usage { input_tokens: input, output_tokens: output },
            }));
        }
    }

    let choices = v.get("choices")?.as_array()?;
    if choices.is_empty() {
        return None;
    }

    let choice = &choices[0];
    let delta = choice.get("delta")?;

    // Tool calls — handle the case where name AND arguments arrive in one chunk
    if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tool_calls {
            let index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0);

            // Track ID from first chunk
            if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                if !id.is_empty() {
                    id_map.insert(index, id.to_string());
                }
            }

            let func = tc.get("function");
            let name = func.and_then(|f| f.get("name")).and_then(|n| n.as_str()).unwrap_or("");
            let arguments = func.and_then(|f| f.get("arguments")).and_then(|a| a.as_str()).unwrap_or("");

            // ToolUseStart — with optional input_json when args come in same chunk
            if !name.is_empty() {
                let id = id_map.get(&index).cloned().unwrap_or_default();
                let input_json = if !arguments.is_empty() {
                    Some(arguments.to_string())
                } else {
                    None
                };
                return Some(Ok(StreamEvent::ToolUseStart {
                    id,
                    name: name.to_string(),
                    input_json,
                }));
            }

            // ToolUseDelta (when arguments come in a separate chunk from name)
            if !arguments.is_empty() {
                let id = id_map
                    .get(&index)
                    .cloned()
                    .unwrap_or_else(|| format!("call_{}", index));
                return Some(Ok(StreamEvent::ToolUseDelta {
                    id,
                    delta_json: arguments.to_string(),
                }));
            }
        }
    }

    // Text content
    if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
        if !content.is_empty() {
            return Some(Ok(StreamEvent::TextDelta(content.to_string())));
        }
    }

    // Check finish_reason (without usage, as fallback)
    if let Some(finish_reason) = choice.get("finish_reason").and_then(|f| f.as_str()) {
        match finish_reason {
            "stop" => {
                return Some(Ok(StreamEvent::MessageComplete {
                    stop_reason: StopReason::EndTurn,
                    usage: Usage::default(),
                }));
            }
            "tool_calls" => {
                return Some(Ok(StreamEvent::MessageComplete {
                    stop_reason: StopReason::ToolUse,
                    usage: Usage::default(),
                }));
            }
            "length" => {
                return Some(Ok(StreamEvent::MessageComplete {
                    stop_reason: StopReason::MaxTokens,
                    usage: Usage::default(),
                }));
            }
            _ => {}
        }
    }

    None
}
