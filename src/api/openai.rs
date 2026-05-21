//! OpenAI-compatible API client with streaming support.
//!
//! Handles the format differences between OpenAI's tool calling format
//! and our internal ContentBlock-based message format.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::{Stream, StreamExt, stream};
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

        // Check Content-Type for non-streaming fallback.
        // Some OpenAI-compatible providers (e.g. Gemini proxy) intermittently
        // ignore `stream: true` and return a plain JSON response instead of SSE.
        // When that happens, parse the body as a complete non-streaming response.
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();

        if content_type.contains("text/event-stream") {
            Ok(parse_openai_sse_stream(response))
        } else {
            let body = response.text().await.map_err(ApiError::Request)?;
            Ok(parse_openai_nonstreaming(&body))
        }
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
    // Some providers (e.g. Gemini proxy) send delta content AND usage in the
    // same SSE chunk. Buffer the MessageComplete and emit it on [DONE].
    let pending_complete: Arc<Mutex<Option<StreamEvent>>> = Arc::new(Mutex::new(None));
    let pending_complete_clone = pending_complete.clone();

    let mapped = event_stream.flat_map(move |event| {
        let id_map = id_map_clone.clone();
        let pending_reason = pending_reason_clone.clone();
        let pending_complete = pending_complete_clone.clone();

        let evs = async move {
            let event = match event {
                Ok(evt) => evt,
                Err(e) => {
                    return vec![Err(ApiError::Stream(format!("SSE error: {}", e)))];
                }
            };

            if event.data == "[DONE]" {
                let mut flush = Vec::new();
                if let Ok(mut pc) = pending_complete.lock()
                    && let Some(event) = pc.take()
                {
                    flush.push(Ok(event));
                }
                if let Ok(mut m) = id_map.lock() {
                    m.clear();
                }
                if let Ok(mut pr) = pending_reason.lock()
                    && let Some((reason, _idx)) = pr.take()
                {
                    let stop_reason = parse_stop_reason(&reason);
                    flush.push(Ok(StreamEvent::MessageComplete {
                        stop_reason,
                        usage: Usage::default(),
                    }));
                }
                return flush;
            }
            if event.data.is_empty() {
                return Vec::new();
            }

            tracing::trace!(chunk_len = event.data.len().min(500), "SSE chunk received");

            match id_map.lock() {
                Ok(mut m) => {
                    parse_openai_chunk(&event.data, &mut m, &pending_reason, &pending_complete)
                }
                Err(e) => vec![Err(ApiError::Stream(format!("lock error: {}", e)))],
            }
        };
        stream::once(evs).map(stream::iter).flatten()
    });

    // After the SSE stream ends (without [DONE]), drain any pending events.
    // This handles the case where the connection is closed without the [DONE]
    // sentinel — e.g. provider timeout or non-standard stream termination.
    let drain_complete = pending_complete.clone();
    let drain_reason = pending_reason.clone();
    let drain_id_map = id_map.clone();
    let drain = stream::once(async move {
        // Check pending_complete first (higher priority — has usage info)
        if let Ok(mut pc) = drain_complete.lock()
            && let Some(event) = pc.take()
        {
            return Some(Ok(event));
        }
        // Then check pending_reason (no usage chunk was ever sent)
        if let Ok(mut pr) = drain_reason.lock()
            && let Some((reason, _idx)) = pr.take()
        {
            let stop_reason = parse_stop_reason(&reason);
            return Some(Ok(StreamEvent::MessageComplete {
                stop_reason,
                usage: Usage::default(),
            }));
        }
        // Clean up id_map
        if let Ok(mut m) = drain_id_map.lock() {
            m.clear();
        }
        None
    })
    .filter_map(|x| async move { x });

    Box::pin(mapped.chain(drain))
}

/// Parse an OpenAI-style usage JSON object into a Usage struct.
///
/// Handles the same fields as the streaming path: `prompt_tokens`,
/// `completion_tokens`, `prompt_tokens_details` (cached_tokens,
/// cache_write_tokens), and `completion_tokens_details` (reasoning_tokens).
/// Non-cached input tokens are computed as `prompt - cached_read - cached_write`.
fn parse_usage_from_value(usage: &Value) -> Usage {
    let prompt = usage
        .get("prompt_tokens")
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    let output = usage
        .get("completion_tokens")
        .and_then(|t| t.as_u64())
        .unwrap_or(0);

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

    let reasoning = usage
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(|t| t.as_u64())
        .unwrap_or(0);

    let input_non_cached = prompt.saturating_sub(cached_read + cached_write);

    Usage {
        input_tokens: input_non_cached,
        output_tokens: output,
        cache_read_input_tokens: cached_read,
        cache_creation_input_tokens: cached_write,
        reasoning_tokens: reasoning,
    }
}

/// Parse a non-streaming OpenAI response into a synthetic stream of events.
///
/// Some providers (e.g. Gemini proxy) intermittently ignore `stream: true`
/// and return a plain JSON response. This converts that single-response JSON
/// into the same event sequence that the SSE path would produce:
///   ReasoningDelta (if present) → TextDelta → ToolUseStart(s) → MessageComplete
fn parse_openai_nonstreaming(
    body: &str,
) -> Pin<Box<dyn Stream<Item = Result<StreamEvent, ApiError>> + Send>> {
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return Box::pin(stream::once(async move { Err(ApiError::Json(e)) })),
    };

    let mut events: Vec<Result<StreamEvent, ApiError>> = Vec::new();

    let choice = match v.get("choices").and_then(|c| c.get(0)) {
        Some(c) => c,
        None => return Box::pin(stream::iter(events)),
    };

    // In non-streaming responses, content is in `message`, not `delta`.
    let message = match choice.get("message") {
        Some(m) => m,
        None => return Box::pin(stream::iter(events)),
    };

    // Reasoning content (Gemini proxy puts it in message, same as delta).
    // If `reasoning_content` is present and non-empty, emit it and skip `content`
    // to avoid duplicating thinking text as normal output (see streaming path).
    if let Some(rc) = message.get("reasoning_content").and_then(|c| c.as_str())
        && !rc.is_empty()
    {
        events.push(Ok(StreamEvent::ReasoningDelta(rc.to_string())));
    } else if let Some(content) = message.get("content").and_then(|c| c.as_str())
        && !content.is_empty()
    {
        events.push(Ok(StreamEvent::TextDelta(content.to_string())));
    }

    // Tool calls — non-streaming responses have complete tool_calls in message
    if let Some(tool_calls) = message.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tool_calls {
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
            let args = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .map(|a| {
                    if a.is_string() {
                        a.as_str().unwrap().to_string()
                    } else {
                        a.to_string()
                    }
                });

            events.push(Ok(StreamEvent::ToolUseStart {
                id,
                name,
                input_json: args.map(|a| a.to_string()),
            }));
        }
    }

    // Stop reason
    let stop_reason = match choice.get("finish_reason").and_then(|f| f.as_str()) {
        Some("stop") => StopReason::EndTurn,
        Some("tool_calls") => StopReason::ToolUse,
        Some("length") => StopReason::MaxTokens,
        Some(s) => StopReason::StopSequence(s.to_string()),
        None => StopReason::EndTurn,
    };

    // Usage (same format as streaming chunks)
    let usage = v
        .get("usage")
        .map(parse_usage_from_value)
        .unwrap_or_default();

    events.push(Ok(StreamEvent::MessageComplete { stop_reason, usage }));

    Box::pin(stream::iter(events))
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
    pending_complete: &Mutex<Option<StreamEvent>>,
) -> Vec<Result<StreamEvent, ApiError>> {
    let mut events = Vec::new();
    let v: serde_json::Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(e) => return vec![Err(ApiError::Json(e))],
    };

    // Extract finish_reason from choices[0] if present (used both for
    // emitting MessageComplete and for caching when usage is separate).
    let finish_reason = v
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("finish_reason"))
        .and_then(|f| f.as_str());

    // Extract usage if present (final chunk)
    if let Some(usage_val) = v.get("usage") {
        let prompt = usage_val
            .get("prompt_tokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        let output = usage_val
            .get("completion_tokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        if prompt > 0 || output > 0 {
            let usage = parse_usage_from_value(usage_val);
            // Use finish_reason from this chunk if present, otherwise drain
            // the pending_reason cache (providers like DeepSeek send
            // finish_reason in a prior chunk and usage in a later one).
            let stop_reason = match finish_reason {
                Some("stop") => StopReason::EndTurn,
                Some("tool_calls") => StopReason::ToolUse,
                Some("length") => StopReason::MaxTokens,
                _ => match pending_reason.lock() {
                    Ok(mut pr) => pr.take().map(|(r, _)| parse_stop_reason(&r)),
                    Err(_) => None,
                }
                .unwrap_or(StopReason::EndTurn),
            };

            let complete_event = StreamEvent::MessageComplete { stop_reason, usage };

            // Check if this chunk's delta also has content/reasoning that
            // needs to be emitted first (non-standard but common with Gemini
            // proxies that send usage + content in the same SSE chunk).
            let delta_has_content = v
                .get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("delta"))
                .is_some_and(|d| {
                    d.get("content")
                        .and_then(|c| c.as_str())
                        .is_some_and(|s| !s.is_empty())
                        || d.get("reasoning_content")
                            .and_then(|c| c.as_str())
                            .is_some_and(|s| !s.is_empty())
                        || d.get("tool_calls").is_some()
                });

            // Buffer the MessageComplete — delta content will be emitted
            // on this pass, and the buffer is drained on [DONE].
            if delta_has_content {
                if let Ok(mut pc) = pending_complete.lock() {
                    *pc = Some(complete_event);
                }
            } else {
                events.push(Ok(complete_event));
                return events;
            }
        }
    }

    // Choices-empty chunk with usage=null — skip (DeepSeek sends this
    // between finish_reason and [DONE]).
    let choices = match v.get("choices").and_then(|c| c.as_array()) {
        Some(c) if !c.is_empty() => c,
        _ => return events,
    };

    let choice = &choices[0];
    let delta = match choice.get("delta") {
        Some(d) => d,
        None => return events,
    };

    // Tool calls — handle the case where name AND arguments arrive in one chunk.
    // Some providers batch multiple entries for the same tool call index
    // into a single SSE chunk (e.g. name in one entry, arguments in another).
    // We must NOT return early — collect all entries and merge by index.
    if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
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

        let mut start_by_index: HashMap<u64, (String, String)> = HashMap::new();
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
                .map(|a| {
                    if a.is_string() {
                        a.as_str().unwrap().to_string()
                    } else {
                        a.to_string()
                    }
                })
                .unwrap_or_default();

            // Merge: if we already have partial args for this index, append
            if !name.is_empty() {
                let merged = start_by_index
                    .remove(&index)
                    .map(|(_, prev)| prev + &arguments)
                    .unwrap_or_else(|| arguments.clone());
                start_by_index.insert(index, (name.to_string(), merged));
            // Merge: append to existing delta or start fresh
            } else if !arguments.is_empty() {
                let merged = delta_by_index
                    .remove(&index)
                    .map(|prev| prev + &arguments)
                    .unwrap_or_else(|| arguments.clone());
                delta_by_index.insert(index, merged);
            }
        }

        // Emit ToolUseStart events (name present) — these are time-sensitive
        let mut start_indices: Vec<_> = start_by_index.keys().cloned().collect();
        start_indices.sort();
        for idx in start_indices {
            if let Some((name, args)) = start_by_index.remove(&idx) {
                let id = id_map.get(&idx).cloned().unwrap_or_default();
                let input_json = if args.is_empty() { None } else { Some(args) };
                events.push(Ok(StreamEvent::ToolUseStart {
                    id,
                    name,
                    input_json,
                }));
            }
        }

        // Emit ToolUseDelta events (no name, just arguments)
        let mut delta_indices: Vec<_> = delta_by_index.keys().cloned().collect();
        delta_indices.sort();
        for idx in delta_indices {
            if let Some(args) = delta_by_index.remove(&idx) {
                let id = id_map
                    .get(&idx)
                    .cloned()
                    .unwrap_or_else(|| format!("call_{}", idx));
                events.push(Ok(StreamEvent::ToolUseDelta {
                    id,
                    delta_json: args,
                }));
            }
        }
    }

    // Reasoning content (DeepSeek/Kimi/Gemini thinking mode).
    // Some providers (e.g. certain DeepSeek proxies / OpenRouter) send the
    // same thinking text in BOTH `reasoning_content` AND `content` in the
    // same chunk for backward compatibility.  If `reasoning_content` is
    // present, skip the `content` field to avoid duplicating thinking text
    // as normal output (it will appear only as the dimmed rolling line).
    if let Some(rc) = delta.get("reasoning_content").and_then(|c| c.as_str())
        && !rc.is_empty()
    {
        events.push(Ok(StreamEvent::ReasoningDelta(rc.to_string())));
    } else if let Some(content) = delta.get("content").and_then(|c| c.as_str())
        && !content.is_empty()
    {
        // Normal text content — only emitted when there's no reasoning
        // in this chunk, so thinking text is never duplicated as output.
        events.push(Ok(StreamEvent::TextDelta(content.to_string())));
    }

    if let Some(reason) = finish_reason
        && !reason.is_empty()
        && let Ok(mut pr) = pending_reason.lock()
    {
        pr.replace((
            reason.to_string(),
            choice
                .get("index")
                .and_then(|i| i.as_u64())
                .unwrap_or(0)
                .to_string(),
        ));
    }

    events
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

    fn make_pending_complete() -> Mutex<Option<StreamEvent>> {
        Mutex::new(None)
    }

    #[test]
    fn test_parse_openai_chunk_text_delta() {
        let mut id_map = HashMap::new();
        let pending = make_pending();
        let pending_complete = make_pending_complete();
        let data = r#"{"choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":null}]}"#;
        let result = parse_openai_chunk(data, &mut id_map, &pending, &pending_complete);
        assert!(!result.is_empty(), "Expected at least one event");
        match &result[0] {
            Ok(StreamEvent::TextDelta(text)) => assert_eq!(text, "hello"),
            other => panic!("Expected TextDelta, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_openai_chunk_tool_use_start() {
        let mut id_map = HashMap::new();
        let pending = make_pending();
        let pending_complete = make_pending_complete();
        let data = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_abc","function":{"name":"read","arguments":"{\"path\":\"main.rs\"}"}}]},"finish_reason":null}]}"#;
        let result = parse_openai_chunk(data, &mut id_map, &pending, &pending_complete);
        assert!(!result.is_empty(), "Expected at least one event");
        match &result[0] {
            Ok(StreamEvent::ToolUseStart {
                id,
                name,
                input_json,
            }) => {
                assert_eq!(id, "call_abc");
                assert_eq!(name, "read");
                assert_eq!(input_json, &Some("{\"path\":\"main.rs\"}".into()));
            }
            other => panic!("Expected ToolUseStart, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_openai_chunk_tool_use_delta() {
        let mut id_map = HashMap::new();
        id_map.insert(0, "call_abc".into());
        let pending = make_pending();
        let pending_complete = make_pending_complete();
        let data = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":\"main.rs\"}"}}]},"finish_reason":null}]}"#;
        let result = parse_openai_chunk(data, &mut id_map, &pending, &pending_complete);
        assert!(!result.is_empty(), "Expected at least one event");
        match &result[0] {
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
        let pending_complete = make_pending_complete();
        let data = r#"{"usage":{"prompt_tokens":10,"completion_tokens":20},"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;
        let result = parse_openai_chunk(data, &mut id_map, &pending, &pending_complete);
        assert!(!result.is_empty(), "Expected at least one event");
        match &result[0] {
            Ok(StreamEvent::MessageComplete { stop_reason, usage }) => {
                assert_eq!(stop_reason, &StopReason::EndTurn);
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
        let pending_complete = make_pending_complete();
        let data = r#"{"usage":{"prompt_tokens":100,"completion_tokens":30,"prompt_tokens_details":{"cached_tokens":40,"cache_write_tokens":10},"completion_tokens_details":{"reasoning_tokens":5}},"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;
        let result = parse_openai_chunk(data, &mut id_map, &pending, &pending_complete);
        assert!(!result.is_empty(), "Expected at least one event");
        match &result[0] {
            Ok(StreamEvent::MessageComplete { stop_reason, usage }) => {
                assert_eq!(stop_reason, &StopReason::ToolUse);
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
        let pending_complete = make_pending_complete();

        // First chunk: finish_reason without usage
        let chunk1 = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;
        let result1 = parse_openai_chunk(chunk1, &mut id_map, &pending, &pending_complete);
        assert!(
            result1.is_empty(),
            "finish-reason-only chunk caches reason (no events)"
        );

        // Second chunk: usage without finish_reason
        let chunk2 = r#"{"usage":{"prompt_tokens":5,"completion_tokens":10},"choices":[{"index":0,"delta":{}}]}"#;
        let result2 = parse_openai_chunk(chunk2, &mut id_map, &pending, &pending_complete);
        assert!(!result2.is_empty(), "Expected at least one event");
        match &result2[0] {
            Ok(StreamEvent::MessageComplete { stop_reason, usage }) => {
                assert_eq!(stop_reason, &StopReason::EndTurn);
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
        let pending_complete = make_pending_complete();
        let result = parse_openai_chunk("not json", &mut id_map, &pending, &pending_complete);
        assert!(!result.is_empty(), "Expected at least one event (error)");
        assert!(result[0].is_err(), "Expected error for invalid JSON");
    }

    #[test]
    fn test_parse_openai_chunk_empty_choices() {
        let mut id_map = HashMap::new();
        let pending = make_pending();
        let pending_complete = make_pending_complete();
        let data = r#"{"choices":[],"usage":null}"#;
        let result = parse_openai_chunk(data, &mut id_map, &pending, &pending_complete);
        assert!(result.is_empty(), "Empty choices should produce no events");
    }
}
