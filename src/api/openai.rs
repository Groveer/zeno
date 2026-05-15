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
use serde_json::{Value, json};

use crate::api::client::SupportsStreamingMessages;
use crate::api::types::{ApiError, ContentBlock, Message, Role, StopReason, StreamEvent, Usage};

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
        max_tokens: Option<u32>,
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
                // OpenAI requires each tool_result as a separate "tool" role message.
                // Text blocks in the same user message are steer/guidance text —
                // emit them as a separate user message after all tool results.
                let mut has_text_blocks = false;
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
                    } else if matches!(block, ContentBlock::Text { .. }) {
                        has_text_blocks = true;
                    }
                }
                // Emit any text blocks (steer/guidance) as a separate user
                // message so the model can see them. Without this, steer text
                // appended via append_steer_to_last_tool_result() would be
                // silently dropped.
                if has_text_blocks {
                    let text: String = m
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    if !text.is_empty() {
                        api_messages.push(json!({
                            "role": "user",
                            "content": text,
                        }));
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

                // Echo reasoning_content back for providers that require it
                // (DeepSeek/Kimi thinking mode). Only include when the message
                // actually has reasoning_content — don't inject for providers
                // that don't use it.
                if let Some(ref rc) = m.reasoning_content {
                    if rc.is_empty() {
                        // DeepSeek V4 Pro rejects empty string — pad with space
                        msg["reasoning_content"] = json!(" ");
                    } else {
                        msg["reasoning_content"] = json!(rc);
                    }
                }

                api_messages.push(msg);
            } else {
                // User message without tool results — may contain text + image blocks
                let has_image = m
                    .content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Image { .. }));

                if has_image {
                    // OpenAI multimodal format: content is an array of parts
                    let mut parts: Vec<Value> = Vec::new();
                    for block in &m.content {
                        match block {
                            ContentBlock::Text { text } => {
                                parts.push(json!({
                                    "type": "text",
                                    "text": text,
                                }));
                            }
                            ContentBlock::Image {
                                media_type, data, ..
                            } => {
                                let data_url = format!("data:{};base64,{}", media_type, data);
                                parts.push(json!({
                                    "type": "image_url",
                                    "image_url": {
                                        "url": data_url,
                                    },
                                }));
                            }
                            _ => {}
                        }
                    }
                    api_messages.push(json!({
                        "role": "user",
                        "content": parts,
                    }));
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
        }

        let mut body = json!({
            "model": model,
            "messages": api_messages,
            "stream": true,
            "stream_options": { "include_usage": true },
        });

        // Only include max_completion_tokens when explicitly set.
        // When None (auto mode), omit it entirely — the provider's
        // default output limit applies, matching Hermes behavior.
        if let Some(mt) = max_tokens {
            body["max_completion_tokens"] = json!(mt);
        }

        if !tools.is_empty() {
            body["tools"] = json!(tools);
            // Allow the model to return multiple tool calls in a single response.
            // Without this, some OpenAI-compatible providers default to sequential
            // tool calls, forcing the agent to wait one turn per tool and
            // dramatically increasing latency for multi-tool turns.
            body["parallel_tool_calls"] = json!(true);
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
        max_tokens: Option<u32>,
        response_format: Option<&serde_json::Value>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ApiError>> + Send>>, ApiError> {
        let url = format!("{}/chat/completions", self.base_url);
        let mut body = self.build_request_body(model, system, messages, tools, max_tokens);

        // Add response_format if provided (OpenAI-compatible only)
        if let Some(rf) = response_format {
            body["response_format"] = rf.clone();
        }
        let response = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(ApiError::Request)?;

        let status = response.status();
        if status.is_client_error() || status.is_server_error() {
            let status_code = status.as_u16();
            let body_text = response.text().await.unwrap_or_default();
            if status_code == 402 {
                return Err(ApiError::PaymentRequired);
            }
            let body = if body_text.is_empty() && status.is_server_error() {
                format!(
                    "(empty response body — server error {status_code}. \
                     Possible causes: reverse proxy cannot reach upstream, \
                     or the API endpoint URL is incorrect.)"
                )
            } else {
                body_text
            };
            return Err(ApiError::Http {
                status: status_code,
                body,
                headers: None,
            });
        }

        Ok(parse_openai_sse_stream(response))
    }
}

/// Parse OpenAI-format SSE stream into StreamEvents.
/// Uses Arc<Mutex<>> to track tool call index -> id mapping across chunks (Send-safe).
///
/// Handles providers that send `usage` and `finish_reason` in separate chunks
/// (e.g. DeepSeek: finish_reason in one chunk, usage in the next). A pending
/// finish_reason is buffered until usage arrives or the stream ends.
fn parse_openai_sse_stream(
    response: reqwest::Response,
) -> Pin<Box<dyn Stream<Item = Result<StreamEvent, ApiError>> + Send>> {
    let event_stream = response.bytes_stream().eventsource();
    let id_map: Arc<Mutex<HashMap<u64, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let id_map_clone = id_map.clone();
    // Buffer a finish_reason from a chunk that has it but no usage, so it can
    // be merged with a later usage chunk. Stores the reason string and the
    // choice index for later emission.
    let pending_reason: Arc<Mutex<Option<(String, String)>>> = Arc::new(Mutex::new(None));
    let pending_reason_clone = pending_reason.clone();

    let mapped = event_stream.filter_map(move |event| {
        let id_map = id_map_clone.clone();
        let pending_reason = pending_reason_clone.clone();
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
                // Flush any pending finish_reason (provider never sent usage)
                if let Ok(mut pr) = pending_reason.lock()
                    && let Some((reason, _idx)) = pr.take()
                {
                    let stop_reason = parse_stop_reason(&reason);
                    return Some(Ok(StreamEvent::MessageComplete {
                        stop_reason,
                        usage: Usage::default(),
                    }));
                }
                return None;
            }
            if event.data.is_empty() {
                return None;
            }

            tracing::trace!(chunk_len = event.data.len().min(500), "SSE chunk received");

            match id_map.lock() {
                Ok(mut m) => parse_openai_chunk(&event.data, &mut m, &pending_reason),
                Err(e) => Some(Err(ApiError::Stream(format!("lock error: {}", e)))),
            }
        }
    });

    Box::pin(mapped)
}

/// Convert an OpenAI finish_reason string to StopReason.
fn parse_stop_reason(reason: &str) -> StopReason {
    match reason {
        "stop" => StopReason::EndTurn,
        "tool_calls" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        s => StopReason::StopSequence(s.to_string()),
    }
}

fn parse_openai_chunk(
    data: &str,
    id_map: &mut HashMap<u64, String>,
    pending_reason: &Mutex<Option<(String, String)>>,
) -> Option<Result<StreamEvent, ApiError>> {
    let v: serde_json::Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(e) => return Some(Err(ApiError::Json(e))),
    };

    // Extract finish_reason from choices[0] if present (used both for
    // emitting MessageComplete and for caching when usage is separate).
    let finish_reason = v
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("finish_reason"))
        .and_then(|f| f.as_str());

    // Extract usage if present (final chunk)
    if let Some(usage) = v.get("usage") {
        let prompt = usage
            .get("prompt_tokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        let output = usage
            .get("completion_tokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        if prompt > 0 || output > 0 {
            // Extract cache details from prompt_tokens_details
            let (cached_read, cached_write) = usage
                .get("prompt_tokens_details")
                .map(|d| {
                    let cr = d.get("cached_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                    let cw = d
                        .get("cache_write_tokens")
                        .and_then(|t| t.as_u64())
                        .unwrap_or(0);
                    (cr, cw)
                })
                .unwrap_or((0, 0));

            // Extract reasoning tokens from completion_tokens_details
            let reasoning = usage
                .get("completion_tokens_details")
                .and_then(|d| d.get("reasoning_tokens"))
                .and_then(|t| t.as_u64())
                .unwrap_or(0);

            // OpenAI's prompt_tokens includes cached tokens AND cache writes.
            // Subtract both to get the non-cached input portion.
            // Matches hermes-agent normalize_usage() behavior.
            let input_non_cached = prompt.saturating_sub(cached_read + cached_write);

            // output_tokens is the raw completion_tokens — reasoning_tokens
            // is a *subset* of output for display purposes only (no split).
            // Matches hermes-agent where output_tokens = completion_tokens.
            let output_total = output;

            // Use finish_reason from this chunk if present, otherwise drain
            // the pending_reason cache (providers like DeepSeek send
            // finish_reason in a prior chunk and usage in a later one).
            let stop_reason = match finish_reason {
                Some("stop") => StopReason::EndTurn,
                Some("tool_calls") => StopReason::ToolUse,
                Some("length") => StopReason::MaxTokens,
                _ => {
                    // Try pending_reason cache
                    match pending_reason.lock() {
                        Ok(mut pr) => pr.take().map(|(r, _)| parse_stop_reason(&r)),
                        Err(_) => None,
                    }
                    .unwrap_or(StopReason::EndTurn)
                }
            };
            return Some(Ok(StreamEvent::MessageComplete {
                stop_reason,
                usage: Usage {
                    input_tokens: input_non_cached,
                    output_tokens: output_total,
                    cache_read_input_tokens: cached_read,
                    cache_creation_input_tokens: cached_write,
                    reasoning_tokens: reasoning,
                },
            }));
        }
    }

    let choices = v.get("choices")?.as_array()?;
    if choices.is_empty() {
        // Choices-empty chunk with usage=null — skip (DeepSeek sends this
        // between finish_reason and [DONE]).
        return None;
    }

    let choice = &choices[0];
    let delta = choice.get("delta")?;

    // Tool calls — handle the case where name AND arguments arrive in one chunk
    // NOTE: if the chunk also has content, we can only return one event.
    // Prefer tool_use events (they are time-sensitive for tool execution),
    // but log a warning so we can detect non-standard provider behavior.
    if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
        // Check for co-occurring content (non-standard but possible)
        let content_also = delta
            .get("content")
            .and_then(|c| c.as_str())
            .is_some_and(|s| !s.is_empty());
        if content_also {
            tracing::warn!(
                event = "nonstandard_sse_chunk",
                "SSE chunk has both tool_calls and content — content will be emitted in a separate event. \
                This is non-standard; provider should send them in separate chunks."
            );
        }

        // Some providers batch multiple entries for the same tool call index
        // into a single SSE chunk (e.g. name in one entry, arguments in another).
        // We must NOT return early — collect all entries and merge by index.
        let mut start_by_index: HashMap<u64, (String, String)> = HashMap::new(); // index -> (name, merged_args)
        let mut delta_by_index: HashMap<u64, String> = HashMap::new();

        for tc in tool_calls {
            let index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0);

            // Track ID from first chunk
            if let Some(id) = tc.get("id").and_then(|i| i.as_str())
                && !id.is_empty()
            {
                id_map.insert(index, id.to_string());
            }

            let func = tc.get("function");
            let name = func
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let arguments = func
                .and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str())
                .unwrap_or("");

            if !name.is_empty() {
                // Merge: if we already have partial args for this index, append
                let merged = start_by_index
                    .remove(&index)
                    .map(|(_, prev)| prev + arguments)
                    .unwrap_or_else(|| arguments.to_string());
                start_by_index.insert(index, (name.to_string(), merged));
            } else if !arguments.is_empty() {
                // Merge: append to existing delta or start fresh
                let merged = delta_by_index
                    .remove(&index)
                    .map(|prev| prev + arguments)
                    .unwrap_or_else(|| arguments.to_string());
                delta_by_index.insert(index, merged);
            }
        }

        // Emit ToolUseStart events (name present) — these are time-sensitive
        if let Some((&idx, (name, args))) = start_by_index.iter().next() {
            let id = id_map.get(&idx).cloned().unwrap_or_default();
            let input_json = if args.is_empty() {
                None
            } else {
                Some(args.clone())
            };
            return Some(Ok(StreamEvent::ToolUseStart {
                id,
                name: name.clone(),
                input_json,
            }));
        }

        // Emit ToolUseDelta events (no name, just arguments)
        if let Some((&idx, args)) = delta_by_index.iter().next() {
            let id = id_map
                .get(&idx)
                .cloned()
                .unwrap_or_else(|| format!("call_{}", idx));
            return Some(Ok(StreamEvent::ToolUseDelta {
                id,
                delta_json: args.clone(),
            }));
        }
    }

    // Text content
    if let Some(content) = delta.get("content").and_then(|c| c.as_str())
        && !content.is_empty()
    {
        return Some(Ok(StreamEvent::TextDelta(content.to_string())));
    }

    // Reasoning content (DeepSeek/Kimi thinking mode)
    if let Some(rc) = delta.get("reasoning_content").and_then(|c| c.as_str())
        && !rc.is_empty()
    {
        return Some(Ok(StreamEvent::ReasoningDelta(rc.to_string())));
    }

    // Finish reason without usage — cache it for later merging with usage
    // chunk. Providers like DeepSeek send finish_reason in one chunk and
    // usage in a separate subsequent chunk.
    if let Some(reason) = finish_reason
        && !reason.is_empty()
        && let Ok(mut pr) = pending_reason.lock()
    {
        *pr = Some((reason.to_string(), "0".to_string()));
    }

    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // --- build_request_body ---

    #[test]
    fn test_build_request_body_system_only() {
        let client = OpenAIClient::new("test-key".into(), "http://localhost".into());
        let body =
            client.build_request_body("gpt-4", "You are a helpful assistant.", &[], &[], None);
        assert_eq!(body["model"], "gpt-4");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(
            body["messages"][0]["content"],
            "You are a helpful assistant."
        );
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn test_build_request_body_user_message() {
        let client = OpenAIClient::new("test-key".into(), "http://localhost".into());
        let msg = Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "hello".into(),
            }],
            reasoning_content: None,
        };
        let body = client.build_request_body("gpt-4", "", &[msg], &[], None);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "hello");
    }

    #[test]
    fn test_build_request_body_tool_call() {
        let client = OpenAIClient::new("test-key".into(), "http://localhost".into());
        let msg = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "thinking".into(),
                },
                ContentBlock::ToolUse {
                    id: "call_123".into(),
                    name: "read".into(),
                    input: serde_json::json!({"path": "main.rs"}),
                },
            ],
            reasoning_content: None,
        };
        let body = client.build_request_body("gpt-4", "", &[msg], &[], None);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[0]["content"], "thinking");
        let calls = msgs[0]["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "read");
    }

    #[test]
    fn test_build_request_body_tool_result() {
        let client = OpenAIClient::new("test-key".into(), "http://localhost".into());
        let msg = Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_123".into(),
                content: "file content".into(),
                is_error: None,
            }],
            reasoning_content: None,
        };
        let body = client.build_request_body("gpt-4", "", &[msg], &[], None);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(msgs[0]["tool_call_id"], "call_123");
        assert_eq!(msgs[0]["content"], "file content");
    }

    #[test]
    fn test_build_request_body_tool_result_with_steer() {
        let client = OpenAIClient::new("test-key".into(), "http://localhost".into());
        let msg = Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "call_123".into(),
                    content: "file content".into(),
                    is_error: None,
                },
                ContentBlock::Text {
                    text: "steer text".into(),
                },
            ],
            reasoning_content: None,
        };
        let body = client.build_request_body("gpt-4", "", &[msg], &[], None);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "steer text");
    }

    #[test]
    fn test_build_request_body_max_tokens_none() {
        let client = OpenAIClient::new("test-key".into(), "http://localhost".into());
        let body = client.build_request_body("gpt-4", "", &[], &[], None);
        assert!(body.get("max_completion_tokens").is_none());
    }

    #[test]
    fn test_build_request_body_max_tokens_some() {
        let client = OpenAIClient::new("test-key".into(), "http://localhost".into());
        let body = client.build_request_body("gpt-4", "", &[], &[], Some(4096));
        assert_eq!(body["max_completion_tokens"], 4096);
    }

    #[test]
    fn test_build_request_body_includes_tools() {
        let client = OpenAIClient::new("test-key".into(), "http://localhost".into());
        let tools = vec![serde_json::json!({
            "type": "function",
            "function": {
                "name": "read",
                "description": "Read files"
            }
        })];
        let body = client.build_request_body("gpt-4", "", &[], &tools, None);
        assert!(body.get("tools").is_some());
        assert_eq!(body["tools"].as_array().unwrap().len(), 1);
        assert_eq!(body["parallel_tool_calls"], true);
    }

    // --- parse_stop_reason ---

    #[test]
    fn test_parse_stop_reason_end_turn() {
        assert_eq!(parse_stop_reason("stop"), StopReason::EndTurn);
    }

    #[test]
    fn test_parse_stop_reason_tool_use() {
        assert_eq!(parse_stop_reason("tool_calls"), StopReason::ToolUse);
    }

    #[test]
    fn test_parse_stop_reason_max_tokens() {
        assert_eq!(parse_stop_reason("length"), StopReason::MaxTokens);
    }

    #[test]
    fn test_parse_stop_reason_unknown() {
        assert_eq!(
            parse_stop_reason("content_filter"),
            StopReason::StopSequence("content_filter".into())
        );
    }

    // --- parse_openai_chunk ---

    fn make_pending() -> Mutex<Option<(String, String)>> {
        Mutex::new(None)
    }

    #[test]
    fn test_parse_openai_chunk_text_delta() {
        let mut id_map = HashMap::new();
        let pending = make_pending();
        let data = r#"{"choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":null}]}"#;
        let result = parse_openai_chunk(data, &mut id_map, &pending);
        assert!(result.is_some());
        match result.unwrap() {
            Ok(StreamEvent::TextDelta(text)) => assert_eq!(text, "hello"),
            other => panic!("Expected TextDelta, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_openai_chunk_tool_use_start() {
        let mut id_map = HashMap::new();
        let pending = make_pending();
        let data = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_abc","function":{"name":"read","arguments":"{\"path\":\"main.rs\"}"}}]},"finish_reason":null}]}"#;
        let result = parse_openai_chunk(data, &mut id_map, &pending);
        assert!(result.is_some());
        match result.unwrap() {
            Ok(StreamEvent::ToolUseStart {
                id,
                name,
                input_json,
            }) => {
                assert_eq!(id, "call_abc");
                assert_eq!(name, "read");
                assert_eq!(input_json, Some("{\"path\":\"main.rs\"}".into()));
            }
            other => panic!("Expected ToolUseStart, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_openai_chunk_tool_use_delta() {
        let mut id_map = HashMap::new();
        id_map.insert(0, "call_abc".into());
        let pending = make_pending();
        let data = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":\"main.rs\"}"}}]},"finish_reason":null}]}"#;
        let result = parse_openai_chunk(data, &mut id_map, &pending);
        assert!(result.is_some());
        match result.unwrap() {
            Ok(StreamEvent::ToolUseDelta { id, delta_json }) => {
                assert_eq!(id, "call_abc");
                assert_eq!(delta_json, "{\"path\":\"main.rs\"}");
            }
            other => panic!("Expected ToolUseDelta, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_openai_chunk_usage() {
        let mut id_map = HashMap::new();
        let pending = make_pending();
        let data = r#"{"usage":{"prompt_tokens":10,"completion_tokens":20},"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;
        let result = parse_openai_chunk(data, &mut id_map, &pending);
        assert!(result.is_some());
        match result.unwrap() {
            Ok(StreamEvent::MessageComplete { stop_reason, usage }) => {
                assert_eq!(stop_reason, StopReason::EndTurn);
                assert_eq!(usage.input_tokens, 10);
                assert_eq!(usage.output_tokens, 20);
            }
            other => panic!("Expected MessageComplete, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_openai_chunk_usage_with_cache() {
        let mut id_map = HashMap::new();
        let pending = make_pending();
        let data = r#"{"usage":{"prompt_tokens":100,"completion_tokens":30,"prompt_tokens_details":{"cached_tokens":40,"cache_write_tokens":10},"completion_tokens_details":{"reasoning_tokens":5}},"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;
        let result = parse_openai_chunk(data, &mut id_map, &pending);
        assert!(result.is_some());
        match result.unwrap() {
            Ok(StreamEvent::MessageComplete { stop_reason, usage }) => {
                assert_eq!(stop_reason, StopReason::ToolUse);
                assert_eq!(usage.input_tokens, 50);
                assert_eq!(usage.cache_read_input_tokens, 40);
                assert_eq!(usage.cache_creation_input_tokens, 10);
                assert_eq!(usage.output_tokens, 30);
                assert_eq!(usage.reasoning_tokens, 5);
            }
            other => panic!("Expected MessageComplete, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_openai_chunk_deepseek_pending_reason() {
        let mut id_map = HashMap::new();
        let pending = make_pending();

        // First chunk: finish_reason without usage
        let chunk1 = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;
        let result1 = parse_openai_chunk(chunk1, &mut id_map, &pending);
        assert!(result1.is_none(), "finish-reason-only chunk caches reason");

        // Second chunk: usage without finish_reason
        let chunk2 = r#"{"usage":{"prompt_tokens":5,"completion_tokens":10},"choices":[{"index":0,"delta":{}}]}"#;
        let result2 = parse_openai_chunk(chunk2, &mut id_map, &pending);
        assert!(result2.is_some());
        match result2.unwrap() {
            Ok(StreamEvent::MessageComplete { stop_reason, usage }) => {
                assert_eq!(stop_reason, StopReason::EndTurn);
                assert_eq!(usage.input_tokens, 5);
                assert_eq!(usage.output_tokens, 10);
            }
            other => panic!("Expected MessageComplete, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_openai_chunk_invalid_json() {
        let mut id_map = HashMap::new();
        let pending = make_pending();
        let result = parse_openai_chunk("not json", &mut id_map, &pending);
        assert!(result.is_some());
        assert!(result.unwrap().is_err());
    }

    #[test]
    fn test_parse_openai_chunk_empty_choices() {
        let mut id_map = HashMap::new();
        let pending = make_pending();
        let data = r#"{"choices":[],"usage":null}"#;
        let result = parse_openai_chunk(data, &mut id_map, &pending);
        assert!(result.is_none());
    }
}
