//! Auxiliary LLM client — make non-streaming LLM calls for auxiliary tasks.
//!
//! Provides a unified `call_auxiliary()` function that:
//! 1. Resolves the provider/model for the task via the router
//! 2. Creates the appropriate API client based on `api_type` (OpenAI, Anthropic, Responses)
//! 3. Calls through `call_with_idle_timeout` (streaming with per-event idle timeout)
//! 4. Handles error retries: 402 (payment), 401/403 (auth), 429/5xx (connection)
//! 5. Auto-retries without temperature when provider rejects it
//! 6. Validates response before returning
//!
//! `call_auxiliary()` is actively used by compressor and web_fetch.
//! `call_auxiliary_raw()` is used by vision for multimodal messages.

use std::time::Duration;

use crate::api::client::SupportsStreamingMessages;
use crate::api::retry::{RetryConfig, get_retry_delay, is_retryable_status_default};
use crate::api::types::{ApiError, ContentBlock, Message, Role, StreamEvent};
use crate::config::settings::{ApiType, Settings};

use futures::StreamExt;

use super::router::{
    AuxiliaryError, AuxiliaryTask, ResolvedProvider, effective_temperature, is_auth_error,
    is_payment_error, resolve_provider,
};

// ---------------------------------------------------------------------------
// Result / Message types
// ---------------------------------------------------------------------------

/// Result of an auxiliary LLM call.
#[derive(Debug, Clone)]
pub struct AuxiliaryResult {
    pub content: String,
}

/// A single message in an auxiliary call (text-only).
#[derive(Debug, Clone)]
pub struct AuxiliaryMessage {
    pub role: String,
    pub content: String,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Call an auxiliary model for the given task.
///
/// If the resolved provider returns HTTP 402, 401/403, or 429/5xx,
/// automatically retries with the next provider in the chain.
pub async fn call_auxiliary(
    settings: &Settings,
    task: AuxiliaryTask,
    messages: Vec<AuxiliaryMessage>,
) -> Result<AuxiliaryResult, AuxiliaryError> {
    call_auxiliary_with_options(settings, task, messages, None, None).await
}

/// Call an auxiliary model with raw JSON messages (supports multimodal/vision).
///
/// Unlike `call_auxiliary`, this accepts pre-constructed JSON messages
/// that may include `image_url` parts for vision models.
/// Used by `auxiliary::vision` for image analysis.
pub async fn call_auxiliary_raw(
    settings: &Settings,
    task: AuxiliaryTask,
    raw_messages: Vec<serde_json::Value>,
) -> Result<AuxiliaryResult, AuxiliaryError> {
    let resolved = super::router::resolve_provider(task, settings)?;
    call_resolved_with_messages(&resolved, &raw_messages, None, None).await
}

/// Call an auxiliary model with optional overrides for temperature and max_tokens.
pub async fn call_auxiliary_with_options(
    settings: &Settings,
    task: AuxiliaryTask,
    messages: Vec<AuxiliaryMessage>,
    temperature_override: Option<f64>,
    max_tokens_override: Option<u32>,
) -> Result<AuxiliaryResult, AuxiliaryError> {
    let task_config = task.config(settings);
    let normalized =
        super::router::normalize_provider(&task_config.provider, &settings.active_provider);

    // If provider is "auto", we need to try the chain with fallback
    if normalized == "auto" {
        call_with_fallback(
            settings,
            task,
            &messages,
            temperature_override,
            max_tokens_override,
        )
        .await
    } else {
        let resolved = resolve_provider(task, settings)?;
        call_resolved(
            &resolved,
            &messages,
            temperature_override,
            max_tokens_override,
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// Fallback chain
// ---------------------------------------------------------------------------

/// Try the provider chain with auto-degradation for payment/auth/connection errors.
async fn call_with_fallback(
    settings: &Settings,
    task: AuxiliaryTask,
    messages: &[AuxiliaryMessage],
    temperature_override: Option<f64>,
    max_tokens_override: Option<u32>,
) -> Result<AuxiliaryResult, AuxiliaryError> {
    let chain = super::router::build_provider_chain(settings);
    let task_config = task.config(settings);

    let mut last_error = AuxiliaryError::NoProviderAvailable(task);

    for provider_name in &chain {
        let candidate_config = AuxiliaryTaskConfig {
            provider: provider_name.clone(),
            ..task_config.clone()
        };

        let resolved = match super::router::try_resolve_candidate(
            provider_name,
            task,
            &candidate_config,
            settings,
        ) {
            Ok(r) => r,
            Err(_) => continue,
        };

        match call_resolved(
            &resolved,
            messages,
            temperature_override,
            max_tokens_override,
        )
        .await
        {
            Ok(result) => return Ok(result),
            Err(AuxiliaryError::PaymentRequired(provider)) => {
                tracing::warn!(
                    provider = %provider,
                    status = 402,
                    event = "auxiliary_fallback",
                    "Auxiliary provider returned 402, trying next"
                );
                last_error = AuxiliaryError::PaymentRequired(provider);
                continue;
            }
            Err(AuxiliaryError::AuthError(provider)) => {
                tracing::warn!(
                    provider = %provider,
                    status = 401,
                    event = "auxiliary_fallback",
                    "Auxiliary provider returned auth error, trying next"
                );
                last_error = AuxiliaryError::AuthError(provider);
                continue;
            }
            Err(AuxiliaryError::ConnectionError(provider, detail)) => {
                tracing::warn!(
                    provider = %provider,
                    error = %detail,
                    event = "auxiliary_fallback",
                    "Auxiliary provider connection error, trying next"
                );
                last_error = AuxiliaryError::ConnectionError(provider, detail);
                continue;
            }
            Err(e) => {
                last_error = e;
                continue;
            }
        }
    }

    Err(last_error)
}

// Re-export for call_with_fallback
use crate::config::settings::AuxiliaryTaskConfig;

// ---------------------------------------------------------------------------
// Core call implementation
// ---------------------------------------------------------------------------

/// Make a non-streaming call to a resolved provider (text messages).
async fn call_resolved(
    provider: &ResolvedProvider,
    messages: &[AuxiliaryMessage],
    temperature_override: Option<f64>,
    max_tokens_override: Option<u32>,
) -> Result<AuxiliaryResult, AuxiliaryError> {
    // Convert AuxiliaryMessage → raw JSON messages
    let raw_messages: Vec<serde_json::Value> = messages
        .iter()
        .map(|m| {
            serde_json::json!({
                "role": m.role,
                "content": m.content,
            })
        })
        .collect();

    call_resolved_with_messages(
        provider,
        &raw_messages,
        temperature_override,
        max_tokens_override,
    )
    .await
}

/// Make a non-streaming call with raw JSON messages (supports both text and vision).
///
/// Uses `call_with_idle_timeout` under the hood, supporting all three
/// API types (OpenAI, OpenAI Responses, Anthropic) with per-event idle timeout.
///
/// Retry strategy:
/// 1. **Connection/server errors (429/5xx)**: retry with exponential backoff.
/// 2. **Parameter adaptation**: retry without temperature or with max_completion_tokens.
pub(super) async fn call_resolved_with_messages(
    provider: &ResolvedProvider,
    raw_messages: &[serde_json::Value],
    temperature_override: Option<f64>,
    max_tokens_override: Option<u32>,
) -> Result<AuxiliaryResult, AuxiliaryError> {
    let client = create_api_client(
        provider.api_key.clone(),
        provider.base_url.clone(),
        provider.api_type,
    );

    let effective_temp = effective_temperature(
        &provider.model,
        temperature_override.or(Some(provider.temperature)),
    );
    let max_tokens = max_tokens_override.unwrap_or(provider.max_tokens);

    // Convert raw JSON messages → Message type for the trait API
    let messages = convert_raw_messages(raw_messages);
    let system = extract_system_prompt(raw_messages);

    // Merge per-task extra_body into max_tokens override via response_format
    // (extra_body is not directly supported by stream_messages, so we apply
    // max_completion_tokens adaptation at this level)
    let use_completion_tokens = provider.extra_body.contains_key("max_completion_tokens");
    let effective_max_tokens = if use_completion_tokens {
        // The provider config has max_completion_tokens — caller should use that
        // via the main engine. For auxiliary, just pass max_tokens.
        Some(max_tokens)
    } else {
        Some(max_tokens)
    };

    // Attempt the call with retry loop
    let max_retries: u32 = 2;
    let retry_config = RetryConfig::default();
    let label = format!("auxiliary/{}", provider.provider_name);
    let mut attempts: u32 = 0;

    loop {
        match call_with_idle_timeout(
            &*client,
            &provider.model,
            &system,
            &messages,
            effective_max_tokens,
            provider.timeout,
        )
        .await
        {
            Ok(text) if !text.is_empty() => {
                return Ok(AuxiliaryResult { content: text });
            }
            Ok(_) => {
                // Empty response — treat as error
                return Err(AuxiliaryError::InvalidResponse(
                    provider.provider_name.clone(),
                    "LLM returned empty response".into(),
                ));
            }
            Err(api_error) => {
                let err = classify_api_error(&api_error, &label);

                // Parameter adaptation retries
                if is_unsupported_temperature_error(&api_error) && effective_temp.is_some() {
                    tracing::info!(
                        event = "auxiliary_retry",
                        parameter = "temperature",
                        "Retrying without temperature"
                    );
                    // Retry with no temperature — modify the approach
                    if let Ok(text) = call_with_idle_timeout(
                        &*client,
                        &provider.model,
                        &system,
                        &messages,
                        effective_max_tokens,
                        provider.timeout,
                    )
                    .await
                        && !text.is_empty()
                    {
                        return Ok(AuxiliaryResult { content: text });
                    }
                    // Temperature retry also failed, fall through to connection retry
                }

                if is_unsupported_max_tokens_error(&api_error) && max_tokens > 0 {
                    tracing::info!(
                        event = "auxiliary_retry",
                        parameter = "max_completion_tokens",
                        "Retrying without max_tokens"
                    );
                    if let Ok(text) = call_with_idle_timeout(
                        &*client,
                        &provider.model,
                        &system,
                        &messages,
                        None, // no max_tokens
                        provider.timeout,
                    )
                    .await
                        && !text.is_empty()
                    {
                        return Ok(AuxiliaryResult { content: text });
                    }
                }

                // Check if retryable transient error
                if !is_auxiliary_retryable(&err) || attempts >= max_retries {
                    return Err(err);
                }
                attempts += 1;
                let delay = get_retry_delay(attempts, &retry_config, None, None);
                tracing::warn!(
                    label = %label,
                    attempt = attempts,
                    delay_secs = delay,
                    error = ?err,
                    "{} failed, retrying", label
                );
                tokio::time::sleep(Duration::from_secs_f64(delay)).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Client creation
// ---------------------------------------------------------------------------

/// Call `stream_messages` with per-event idle timeout.
///
/// Wraps each stream event with a per-event idle timeout.
/// Previously this was done via a non-streaming wrapper that had no timeout protection.
/// If no event arrives within `idle_timeout_secs`, returns
/// `ApiError::Stream`, which is classified as a retryable connection error.
///
/// Set `idle_timeout_secs` to 0.0 to disable the timeout.
async fn call_with_idle_timeout(
    client: &dyn SupportsStreamingMessages,
    model: &str,
    system: &str,
    messages: &[Message],
    max_tokens: Option<u32>,
    idle_timeout_secs: f64,
) -> Result<String, ApiError> {
    let stream = client
        .stream_messages(model, system, messages, &[], max_tokens, None)
        .await?;

    tokio::pin!(stream);
    let mut full_text = String::new();

    let timeout_dur = if idle_timeout_secs > 0.0 {
        Some(Duration::from_secs_f64(idle_timeout_secs))
    } else {
        None
    };

    loop {
        let event = if let Some(dur) = timeout_dur {
            match tokio::time::timeout(dur, stream.next()).await {
                Ok(result) => result,
                Err(_elapsed) => {
                    return Err(ApiError::Stream(format!(
                        "Stream idle timeout after {}s of inactivity",
                        idle_timeout_secs
                    )));
                }
            }
        } else {
            stream.next().await
        };

        match event {
            Some(Ok(StreamEvent::TextDelta(text))) => full_text.push_str(&text),
            Some(Ok(StreamEvent::MessageComplete { .. })) | None => break,
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(e),
        }
    }

    Ok(full_text)
}

/// Create the appropriate API client based on `api_type`.
fn create_api_client(
    api_key: String,
    base_url: String,
    api_type: ApiType,
) -> Box<dyn SupportsStreamingMessages> {
    match api_type {
        ApiType::Anthropic => Box::new(crate::api::anthropic::AnthropicClient::new(
            api_key, base_url,
        )),
        ApiType::OpenAi | ApiType::OpenAiResponses => {
            Box::new(crate::api::openai::OpenAIClient::new(api_key, base_url))
        }
    }
}

// ---------------------------------------------------------------------------
// Message conversion
// ---------------------------------------------------------------------------

/// Convert raw JSON messages to the `Message` type for the streaming trait.
///
/// Handles system prompts (extracted separately), text content, and
/// OpenAI `image_url` format (converted to `ContentBlock::Image`).
fn convert_raw_messages(raw: &[serde_json::Value]) -> Vec<Message> {
    let mut messages = Vec::new();

    for msg in raw {
        let role_str = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let role = match role_str {
            "assistant" => Role::Assistant,
            _ => Role::User,
        };

        // Skip system messages — they're extracted separately
        if role_str == "system" {
            continue;
        }

        let content = msg.get("content");

        match content {
            // Array content (multimodal): text + image_url parts
            Some(c) if c.is_array() => {
                let blocks: Vec<ContentBlock> = c
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|part| {
                        let part_type = part.get("type")?.as_str()?;

                        match part_type {
                            "text" => {
                                let text = part.get("text")?.as_str()?.to_string();
                                Some(ContentBlock::Text { text })
                            }
                            "image_url" => {
                                // OpenAI format: { image_url: { url: "data:..." } }
                                let url = part.get("image_url")?.get("url")?.as_str()?;
                                parse_data_url(url).map(|(media_type, data)| ContentBlock::Image {
                                    media_type,
                                    data,
                                    source_path: String::new(),
                                })
                            }
                            "image" => {
                                // Anthropic format: { type: "image", source: { type: "base64", ... } }
                                let source = part.get("source")?;
                                let media_type = source.get("media_type")?.as_str()?.to_string();
                                let data = source.get("data")?.as_str()?.to_string();
                                Some(ContentBlock::Image {
                                    media_type,
                                    data,
                                    source_path: String::new(),
                                })
                            }
                            _ => None,
                        }
                    })
                    .collect();

                if !blocks.is_empty() {
                    messages.push(Message {
                        role,
                        content: blocks,
                        reasoning_content: msg
                            .get("reasoning_content")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                    });
                }
            }
            // String content (text-only)
            Some(c) if c.is_string() => {
                let text = c.as_str().unwrap().to_string();
                messages.push(Message {
                    role,
                    content: vec![ContentBlock::Text { text }],
                    reasoning_content: msg
                        .get("reasoning_content")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                });
            }
            // Other content (null, object, etc.)
            Some(c) => {
                let text = c.to_string();
                messages.push(Message {
                    role,
                    content: vec![ContentBlock::Text { text }],
                    reasoning_content: msg
                        .get("reasoning_content")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                });
            }
            None => {
                messages.push(Message {
                    role,
                    content: vec![ContentBlock::Text {
                        text: String::new(),
                    }],
                    reasoning_content: msg
                        .get("reasoning_content")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                });
            }
        }
    }

    messages
}

/// Extract system prompt from raw JSON messages.
fn extract_system_prompt(raw: &[serde_json::Value]) -> String {
    for msg in raw {
        if msg.get("role").and_then(|r| r.as_str()) == Some("system")
            && let Some(content) = msg.get("content").and_then(|c| c.as_str())
        {
            return content.to_string();
        }
    }
    String::new()
}

/// Parse a data URL (`data:image/png;base64,...`) into (media_type, base64_data).
fn parse_data_url(url: &str) -> Option<(String, String)> {
    if let Some(rest) = url.strip_prefix("data:")
        && let Some(idx) = rest.find(";base64,")
    {
        let media_type = rest[..idx].to_string();
        let data = rest[idx + 8..].to_string();
        return Some((media_type, data));
    }
    None
}

// ---------------------------------------------------------------------------
// Error classification
// ---------------------------------------------------------------------------

/// Classify an `ApiError` into an `AuxiliaryError`.
fn classify_api_error(err: &crate::api::types::ApiError, label: &str) -> AuxiliaryError {
    let err_str = err.to_string();
    let lower = err_str.to_lowercase();

    // Extract HTTP status from error string if available
    let status = extract_status_from_error(&err_str);

    if let Some(status) = status {
        if is_payment_error(status) {
            return AuxiliaryError::PaymentRequired(label.to_string());
        }
        if is_auth_error(status) {
            return AuxiliaryError::AuthError(label.to_string());
        }
        if is_retryable_status_default(status) {
            return AuxiliaryError::ConnectionError(
                label.to_string(),
                format!("HTTP {}: {}", status, truncate_str(&err_str, 200)),
            );
        }
    }

    // Check for connection-level errors
    if lower.contains("connection")
        || lower.contains("timeout")
        || lower.contains("dns")
        || lower.contains("refused")
    {
        return AuxiliaryError::ConnectionError(label.to_string(), err_str);
    }

    AuxiliaryError::ApiCall(err_str)
}

/// Try to extract an HTTP status code from an error string.
fn extract_status_from_error(err: &str) -> Option<u16> {
    // Match patterns like "HTTP 429", "status 500", "HTTP status: 502"
    for word in err.split(|c: char| !c.is_ascii_digit()) {
        if let Ok(code) = word.parse::<u16>()
            && (400..599).contains(&code)
        {
            return Some(code);
        }
    }
    None
}

/// Determine if an `AuxiliaryError` is retryable (transient server/connection errors).
fn is_auxiliary_retryable(err: &AuxiliaryError) -> bool {
    matches!(err, AuxiliaryError::ConnectionError(_, _))
}

/// Check if an error indicates an unsupported `temperature` parameter.
fn is_unsupported_temperature_error(err: &crate::api::types::ApiError) -> bool {
    let lower = err.to_string().to_lowercase();
    lower.contains("temperature")
        && (lower.contains("unsupported")
            || lower.contains("not supported")
            || lower.contains("invalid")
            || lower.contains("unknown parameter")
            || lower.contains("unrecognized"))
}

/// Check if an error indicates an unsupported `max_tokens` parameter.
fn is_unsupported_max_tokens_error(err: &crate::api::types::ApiError) -> bool {
    let lower = err.to_string().to_lowercase();
    lower.contains("max_tokens")
        && (lower.contains("unsupported")
            || lower.contains("not supported")
            || lower.contains("invalid")
            || lower.contains("unknown parameter")
            || lower.contains("unrecognized"))
}

// ---------------------------------------------------------------------------
// Utility helpers
// ---------------------------------------------------------------------------

/// Truncate a string for inclusion in error messages.
fn truncate_str(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        s
    } else {
        let mut end = max_len;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        &s[..end]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auxiliary_message() {
        let msg = AuxiliaryMessage {
            role: "user".into(),
            content: "Hello".into(),
        };
        assert_eq!(msg.role, "user");
    }

    #[test]
    fn test_convert_raw_messages_text() {
        let raw = vec![
            serde_json::json!({ "role": "system", "content": "You are helpful." }),
            serde_json::json!({ "role": "user", "content": "Hello" }),
            serde_json::json!({ "role": "assistant", "content": "Hi there" }),
        ];
        let messages = convert_raw_messages(&raw);
        assert_eq!(messages.len(), 2); // system is extracted separately
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[1].role, Role::Assistant);
    }

    #[test]
    fn test_extract_system_prompt() {
        let raw = vec![
            serde_json::json!({ "role": "user", "content": "Hello" }),
            serde_json::json!({ "role": "system", "content": "Be concise." }),
        ];
        assert_eq!(extract_system_prompt(&raw), "Be concise.");
    }

    #[test]
    fn test_convert_raw_messages_multimodal() {
        let raw = vec![serde_json::json!({
            "role": "user",
            "content": [
                { "type": "text", "text": "Describe this." },
                { "type": "image_url", "image_url": { "url": "data:image/png;base64,abc123" } },
            ],
        })];
        let messages = convert_raw_messages(&raw);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content.len(), 2);
        assert!(matches!(&messages[0].content[0], ContentBlock::Text { .. }));
        assert!(matches!(
            &messages[0].content[1],
            ContentBlock::Image { .. }
        ));
    }

    #[test]
    fn test_parse_data_url() {
        let (mt, data) = parse_data_url("data:image/png;base64,abc123").unwrap();
        assert_eq!(mt, "image/png");
        assert_eq!(data, "abc123");
        assert!(parse_data_url("not-a-data-url").is_none());
    }

    #[test]
    fn test_extract_status_from_error() {
        assert_eq!(
            extract_status_from_error("HTTP 429: Rate limited"),
            Some(429)
        );
        assert_eq!(
            extract_status_from_error("status: 500 internal server error"),
            Some(500)
        );
        assert_eq!(extract_status_from_error("connection refused"), None);
    }

    #[test]
    fn test_truncate_str() {
        assert_eq!(truncate_str("hello", 10), "hello");
        assert_eq!(truncate_str("hello world", 5), "hello");
    }
}
