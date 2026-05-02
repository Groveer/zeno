//! SSE stream parser — converts raw SSE bytes into StreamEvents.

use std::pin::Pin;

use eventsource_stream::Eventsource;
use futures::{Stream, StreamExt};

use crate::api::types::{ApiError, StopReason, StreamEvent, Usage};

/// Parse a raw byte stream from an API response into `StreamEvent`s.
///
/// Expects SSE format with `event:` and `data:` fields as per Anthropic's API.
pub fn parse_sse_stream(
    response: reqwest::Response,
) -> Pin<Box<dyn Stream<Item = Result<StreamEvent, ApiError>> + Send>> {
    let event_stream = response.bytes_stream().eventsource();

    let mapped = event_stream.filter_map(|event| async move {
        match event {
            Ok(evt) => parse_sse_event(&evt.event, &evt.data),
            Err(e) => Some(Err(ApiError::Stream(format!("SSE parsing error: {}", e)))),
        }
    });

    Box::pin(mapped)
}

/// Parse a single SSE event into a StreamEvent (or None for keep-alive / ignored).
fn parse_sse_event(event_type: &str, data: &str) -> Option<Result<StreamEvent, ApiError>> {
    if data.is_empty() || data == "[DONE]" {
        return None;
    }

    match event_type {
        "content_block_delta" => parse_content_block_delta(data),
        "content_block_start" => parse_content_block_start(data),
        "message_delta" => parse_message_delta(data),
        "message_start" => parse_message_start(data),
        "message_stop" => None,
        "ping" => None,
        "" => {
            if data.starts_with('{') {
                parse_content_block_delta(data)
            } else {
                None
            }
        }
        _ => {
            tracing::debug!(event_type = %event_type, "Ignoring SSE event type");
            None
        }
    }
}

fn parse_content_block_delta(data: &str) -> Option<Result<StreamEvent, ApiError>> {
    let v: serde_json::Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(e) => return Some(Err(ApiError::Json(e))),
    };

    let delta = v.get("delta")?;

    // Text delta
    if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
        return Some(Ok(StreamEvent::TextDelta(text.to_string())));
    }

    // Tool use input JSON delta
    if delta.get("type").and_then(|t| t.as_str()) == Some("input_json_delta")
        && let Some(json_str) = delta.get("partial_json").and_then(|j| j.as_str())
    {
        let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
        let id = format!("tool_{}", index);
        return Some(Ok(StreamEvent::ToolUseDelta {
            id,
            delta_json: json_str.to_string(),
        }));
    }

    None
}

fn parse_content_block_start(data: &str) -> Option<Result<StreamEvent, ApiError>> {
    let v: serde_json::Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(e) => return Some(Err(ApiError::Json(e))),
    };

    let content_block = v.get("content_block")?;

    if content_block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
        let id = content_block.get("id")?.as_str()?.to_string();
        let name = content_block.get("name")?.as_str()?.to_string();
        return Some(Ok(StreamEvent::ToolUseStart {
            id,
            name,
            input_json: None,
        }));
    }

    None
}

fn parse_message_start(data: &str) -> Option<Result<StreamEvent, ApiError>> {
    let v: serde_json::Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(e) => return Some(Err(ApiError::Json(e))),
    };

    // Anthropic message_start: { "message": { "usage": { "input_tokens": N, "output_tokens": 0,
    //   "cache_read_input_tokens": N, "cache_creation_input_tokens": N } } }
    let usage = v
        .get("message")
        .and_then(|m| m.get("usage"))
        .or_else(|| v.get("usage")); // fallback: top-level usage

    if let Some(u) = usage {
        let input = u.get("input_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
        let output = u.get("output_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
        if input > 0 {
            let cached_read = u
                .get("cache_read_input_tokens")
                .and_then(|t| t.as_u64())
                .unwrap_or(0);
            let cached_write = u
                .get("cache_creation_input_tokens")
                .and_then(|t| t.as_u64())
                .unwrap_or(0);
            // Anthropic's input_tokens includes cache read + cache write.
            // Subtract both to get non-cached input, consistent with the
            // OpenAI path (openai.rs) which does
            // input_non_cached = prompt - cache_read - cache_write.
            // CostTracker::total_tokens() then correctly reconstructs the
            // original total as (input - cache) + output + cache.
            let input_non_cached = input.saturating_sub(cached_read + cached_write);
            return Some(Ok(StreamEvent::UsageUpdate(Usage {
                input_tokens: input_non_cached,
                output_tokens: output,
                cache_read_input_tokens: cached_read,
                cache_creation_input_tokens: cached_write,
                reasoning_tokens: 0,
            })));
        }
    }

    None
}

fn parse_message_delta(data: &str) -> Option<Result<StreamEvent, ApiError>> {
    let v: serde_json::Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(e) => return Some(Err(ApiError::Json(e))),
    };

    let delta = v.get("delta")?;
    let stop_reason = match delta.get("stop_reason").and_then(|s| s.as_str()) {
        Some("end_turn") => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        Some(s) => StopReason::StopSequence(s.to_string()),
        None => return None,
    };

    // Anthropic message_delta.usage only contains output_tokens (cumulative).
    // input_tokens is not present here — it was in message_start.
    let usage = v
        .get("usage")
        .map(|u| Usage {
            input_tokens: 0, // input_tokens not in message_delta
            output_tokens: u.get("output_tokens").and_then(|t| t.as_u64()).unwrap_or(0),
            cache_read_input_tokens: 0, // not in message_delta
            cache_creation_input_tokens: 0,
            reasoning_tokens: 0,
        })
        .unwrap_or_default();

    Some(Ok(StreamEvent::MessageComplete { stop_reason, usage }))
}
