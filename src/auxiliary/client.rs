//! Auxiliary LLM client — make non-streaming LLM calls for auxiliary tasks.
//!
//! Provides a unified `call_auxiliary()` function that:
//! 1. Resolves the provider/model for the task via the router
//! 2. Makes a non-streaming API call (with client caching)
//! 3. Handles error retries: 402 (payment), 401/403 (auth), 429/5xx (connection)
//! 4. Auto-retries without temperature when provider rejects it
//! 5. Auto-retries with max_completion_tokens when provider rejects max_tokens
//! 6. Validates response structure before returning
//!
//! `call_auxiliary()` is actively used by compressor and web_fetch.
//! `call_vision()` is reserved for future vision integration.

use std::time::Duration;

use crate::api::retry::{RetryConfig, is_retryable_status_default, retry_with_backoff};
use crate::config::settings::{AuxiliaryTaskConfig, Settings};

use super::cache;
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
/// that may include image_url parts for vision models.
/// Used by `auxiliary::vision` for image analysis.
pub async fn call_auxiliary_raw(
    settings: &Settings,
    task: AuxiliaryTask,
    raw_messages: Vec<serde_json::Value>,
) -> Result<AuxiliaryResult, AuxiliaryError> {
    let resolved = super::router::resolve_provider(task, settings)?;
    call_resolved_raw(&resolved, &raw_messages, None, None).await
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
    // Build the API messages as JSON values
    let api_messages: Vec<serde_json::Value> = messages
        .iter()
        .map(|m| {
            serde_json::json!({
                "role": m.role,
                "content": m.content,
            })
        })
        .collect();

    call_resolved_raw(
        provider,
        &api_messages,
        temperature_override,
        max_tokens_override,
    )
    .await
}

/// Make a non-streaming call with raw JSON messages (supports both text and vision).
///
/// Retry strategy (in order):
/// 1. **Connection/server errors (429/5xx)**: retry the same request with exponential
///    backoff via the shared `retry_with_backoff` from `api::retry`.
/// 2. **Parameter adaptation**: if the provider rejects `temperature` or `max_tokens`,
///    retry with the parameter removed or replaced (domain-specific, not in retry.rs).
/// 3. **Provider fallback**: if all retries on this provider fail with a retryable error,
///    the caller (`call_with_fallback`) tries the next provider in the chain.
pub(super) async fn call_resolved_raw(
    provider: &ResolvedProvider,
    api_messages: &[serde_json::Value],
    temperature_override: Option<f64>,
    max_tokens_override: Option<u32>,
) -> Result<AuxiliaryResult, AuxiliaryError> {
    let timeout = Duration::from_secs_f64(provider.timeout);

    // Get or create a cached HTTP client
    let client = cache::get_or_create(&provider.provider_name, &provider.base_url, timeout)
        .map_err(AuxiliaryError::ApiCall)?;

    // Determine effective temperature
    let effective_temp = effective_temperature(
        &provider.model,
        temperature_override.or(Some(provider.temperature)),
    );

    // Determine max_tokens
    let max_tokens = max_tokens_override.unwrap_or(provider.max_tokens);

    // Build the request body
    let mut body = serde_json::json!({
        "model": provider.model,
        "messages": api_messages,
    });

    // Add temperature if the model accepts it
    if let Some(temp) = effective_temp {
        body["temperature"] = serde_json::json!(temp);
    }

    // Add max_tokens (some providers use max_completion_tokens instead)
    if max_tokens > 0 {
        body["max_tokens"] = serde_json::json!(max_tokens);
    }

    // Merge per-task extra_body
    if !provider.extra_body.is_empty() {
        for (k, v) in &provider.extra_body {
            body[k] = v.clone();
        }
    }

    let url = match provider.api_type {
        crate::config::settings::ApiType::Anthropic => {
            format!("{}/v1/messages", provider.base_url.trim_end_matches('/'))
        }
        crate::config::settings::ApiType::OpenAi => format!(
            "{}/chat/completions",
            provider.base_url.trim_end_matches('/')
        ),
        crate::config::settings::ApiType::OpenAiResponses => {
            format!("{}/v1/responses", provider.base_url.trim_end_matches('/'))
        }
    };

    let retry_config = RetryConfig::for_auxiliary();
    let label = format!("auxiliary/{}", provider.provider_name);

    // Use shared retry_with_backoff for connection/server errors.
    // This replaces the previous behavior of trying once and immediately
    // falling through to the next provider in the fallback chain.
    retry_with_backoff(
        &retry_config,
        &label,
        |err: &AuxiliaryError| is_auxiliary_retryable(err),
        || {
            let client = client.clone();
            let url = url.clone();
            let api_key = provider.api_key.clone();
            let api_type = provider.api_type;
            let body = body.clone();
            async move {
                match do_api_call(&client, &url, &api_key, &api_type, &body).await {
                    Ok(result) => Ok(result),
                    Err((err, retry_no_temp, retry_mc)) => {
                        // --- Parameter-adaptation retries (domain-specific) ---
                        // These are NOT in the shared retry module because they
                        // modify the request body before retrying.
                        if retry_no_temp
                            && let Some(result) = retry_without_temperature(
                                &client, &url, &api_key, &api_type, &body, max_tokens,
                            )
                            .await
                        {
                            return Ok(result);
                        }
                        if retry_mc
                            && let Some(result) = retry_with_max_completion_tokens(
                                &client, &url, &api_key, &api_type, &body, max_tokens,
                            )
                            .await
                        {
                            return Ok(result);
                        }
                        // Not a parameter issue, or parameter retries also failed.
                        // If it's a retryable connection error, the outer
                        // retry_with_backoff will re-invoke this closure.
                        Err(err)
                    }
                }
            }
        },
    )
    .await
}

/// Execute the API call and return the result or error + retry hints.
async fn do_api_call(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    api_type: &crate::config::settings::ApiType,
    body: &serde_json::Value,
) -> Result<AuxiliaryResult, (AuxiliaryError, bool, bool)> {
    let mut req = client.post(url).header("Content-Type", "application/json");

    // Set auth header based on API type
    req = match api_type {
        crate::config::settings::ApiType::Anthropic => req
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01"),
        crate::config::settings::ApiType::OpenAi
        | crate::config::settings::ApiType::OpenAiResponses => {
            req.header("Authorization", format!("Bearer {}", api_key))
        }
    };

    let resp = req.json(body).send().await;

    match resp {
        Ok(resp) => {
            let status = resp.status();
            let status_code = status.as_u16();

            // Payment required
            if is_payment_error(status_code) {
                let provider = extract_provider_from_url(url);
                return Err((AuxiliaryError::PaymentRequired(provider), false, false));
            }

            // Auth error
            if is_auth_error(status_code) {
                let provider = extract_provider_from_url(url);
                return Err((AuxiliaryError::AuthError(provider), false, false));
            }

            // Connection / server error — uses shared retryable status check
            // (429, 500, 502, 503, 529 — same set as the main API retry loop)
            if is_retryable_status_default(status_code) {
                let provider = extract_provider_from_url(url);
                let error_body = resp.text().await.unwrap_or_default();
                return Err((
                    AuxiliaryError::ConnectionError(
                        provider,
                        format!("HTTP {}: {}", status_code, truncate_str(&error_body, 200)),
                    ),
                    false,
                    false,
                ));
            }

            // Client error — might be unsupported parameter
            if !status.is_success() {
                let error_body = resp.text().await.unwrap_or_default();
                let retry_no_temp = is_unsupported_temperature_error(&error_body);
                let retry_max_completion = is_unsupported_max_tokens_error(&error_body);

                return Err((
                    AuxiliaryError::ApiCall(format!(
                        "HTTP {} from {}: {}",
                        status_code,
                        extract_provider_from_url(url),
                        truncate_str(&error_body, 500)
                    )),
                    retry_no_temp,
                    retry_max_completion,
                ));
            }

            // Parse successful response
            let data: serde_json::Value = resp.json().await.map_err(|e| {
                (
                    AuxiliaryError::ApiCall(format!("Failed to parse response: {}", e)),
                    false,
                    false,
                )
            })?;

            // Validate response structure
            match validate_response(&data) {
                Ok(content) => Ok(AuxiliaryResult { content }),
                Err(e) => Err((e, false, false)),
            }
        }
        Err(e) => {
            // reqwest error (DNS, timeout, connection refused, etc.)
            let provider = extract_provider_from_url(url);
            if e.is_connect() || e.is_timeout() {
                Err((
                    AuxiliaryError::ConnectionError(provider, e.to_string()),
                    false,
                    false,
                ))
            } else {
                Err((
                    AuxiliaryError::ApiCall(format!("Request to {} failed: {}", provider, e)),
                    false,
                    false,
                ))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Retryability and parameter-adaptation helpers
// ---------------------------------------------------------------------------

/// Determine if an `AuxiliaryError` is retryable (transient server/connection errors).
///
/// PaymentRequired and AuthError are NOT retryable — those should trigger
/// provider fallback instead. ConnectionError (429/5xx) IS retryable.
fn is_auxiliary_retryable(err: &AuxiliaryError) -> bool {
    matches!(err, AuxiliaryError::ConnectionError(_, _))
}

/// Retry a request with the `temperature` parameter removed.
///
/// Some providers reject `temperature` for certain models (e.g. o1, kimi).
/// Returns `Some(result)` on success, `None` on failure.
async fn retry_without_temperature(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    api_type: &crate::config::settings::ApiType,
    body: &serde_json::Value,
    max_tokens: u32,
) -> Option<AuxiliaryResult> {
    tracing::info!(
        event = "auxiliary_retry",
        parameter = "temperature",
        "Auxiliary: retrying without temperature parameter"
    );
    let mut retry_body = body.clone();
    if let Some(obj) = retry_body.as_object_mut() {
        obj.remove("temperature");
    }

    match do_api_call(client, url, api_key, api_type, &retry_body).await {
        Ok(result) => Some(result),
        Err((_, _, retry_mc)) => {
            // If max_completion_tokens retry is also needed, try it
            if retry_mc {
                retry_with_max_completion_tokens(
                    client,
                    url,
                    api_key,
                    api_type,
                    &retry_body,
                    max_tokens,
                )
                .await
            } else {
                None
            }
        }
    }
}

/// Retry with `max_completion_tokens` instead of `max_tokens`.
///
/// Some providers (OpenAI o1, etc.) use `max_completion_tokens` instead of
/// `max_tokens`. Returns `Some(result)` on success, `None` on failure.
async fn retry_with_max_completion_tokens(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    api_type: &crate::config::settings::ApiType,
    body: &serde_json::Value,
    max_tokens: u32,
) -> Option<AuxiliaryResult> {
    tracing::info!(
        event = "auxiliary_retry",
        parameter = "max_completion_tokens",
        "Auxiliary: retrying with max_completion_tokens instead of max_tokens"
    );
    let mut retry_body = body.clone();
    if let Some(obj) = retry_body.as_object_mut() {
        obj.remove("max_tokens");
    }
    if max_tokens > 0 {
        retry_body["max_completion_tokens"] = serde_json::json!(max_tokens);
    }

    do_api_call(client, url, api_key, api_type, &retry_body)
        .await
        .ok()
}

// ---------------------------------------------------------------------------
// Response validation
// ---------------------------------------------------------------------------

/// Validate that an LLM response has the expected `choices[0].message.content` shape.
///
/// Fails fast with a clear error instead of letting malformed payloads propagate.
///
/// Reference: hermes-agent `_validate_llm_response`.
fn validate_response(data: &serde_json::Value) -> Result<String, AuxiliaryError> {
    if data.is_null() {
        return Err(AuxiliaryError::InvalidResponse(
            "unknown".into(),
            "LLM returned null response".into(),
        ));
    }

    let choices = match data.get("choices") {
        Some(c) => c,
        None => {
            return Err(AuxiliaryError::InvalidResponse(
                "unknown".into(),
                "Response missing 'choices' field".into(),
            ));
        }
    };

    let first_choice = match choices.as_array().and_then(|a| a.first()) {
        Some(c) => c,
        None => {
            return Err(AuxiliaryError::InvalidResponse(
                "unknown".into(),
                "Response has empty 'choices' array".into(),
            ));
        }
    };

    let message = match first_choice.get("message") {
        Some(m) => m,
        None => {
            return Err(AuxiliaryError::InvalidResponse(
                "unknown".into(),
                "Response choices[0] missing 'message' field".into(),
            ));
        }
    };

    let content = match message.get("content") {
        Some(c) if c.is_null() => {
            return Err(AuxiliaryError::InvalidResponse(
                "unknown".into(),
                "Response choices[0].message.content is null".into(),
            ));
        }
        Some(c) => c.as_str().unwrap_or("").to_string(),
        None => {
            return Err(AuxiliaryError::InvalidResponse(
                "unknown".into(),
                "Response choices[0].message missing 'content' field".into(),
            ));
        }
    };

    Ok(content)
}

// ---------------------------------------------------------------------------
// Error detection helpers
// ---------------------------------------------------------------------------

/// Check if an error message indicates an unsupported `temperature` parameter.
///
/// Reference: hermes-agent `_is_unsupported_temperature_error`.
fn is_unsupported_temperature_error(error_body: &str) -> bool {
    let lower = error_body.to_lowercase();
    lower.contains("temperature")
        && (lower.contains("unsupported")
            || lower.contains("not supported")
            || lower.contains("invalid")
            || lower.contains("unknown parameter")
            || lower.contains("unrecognized"))
}

/// Check if an error message indicates an unsupported `max_tokens` parameter.
///
/// Some providers (e.g. OpenAI o-series) reject `max_tokens` and require
/// `max_completion_tokens` instead.
fn is_unsupported_max_tokens_error(error_body: &str) -> bool {
    let lower = error_body.to_lowercase();
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

/// Extract a provider-like name from a URL for error messages.
fn extract_provider_from_url(url: &str) -> String {
    // Try to extract hostname from URL
    if let Ok(parsed) = url::Url::parse(url)
        && let Some(host) = parsed.host_str()
    {
        return host.to_string();
    }
    // Fallback: use the URL itself, truncated
    let truncated = truncate_str(url, 50);
    truncated.to_string()
}

/// Truncate a string for inclusion in error messages.
fn truncate_str(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        s
    } else {
        // Find a valid char boundary
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
    fn test_validate_response_valid() {
        let data = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "Hello world"
                }
            }]
        });
        assert_eq!(validate_response(&data).unwrap(), "Hello world");
    }

    #[test]
    fn test_validate_response_null() {
        let data = serde_json::Value::Null;
        assert!(matches!(
            validate_response(&data),
            Err(AuxiliaryError::InvalidResponse(_, _))
        ));
    }

    #[test]
    fn test_validate_response_no_choices() {
        let data = serde_json::json!({"error": "bad"});
        assert!(matches!(
            validate_response(&data),
            Err(AuxiliaryError::InvalidResponse(_, _))
        ));
    }

    #[test]
    fn test_validate_response_empty_choices() {
        let data = serde_json::json!({"choices": []});
        assert!(matches!(
            validate_response(&data),
            Err(AuxiliaryError::InvalidResponse(_, _))
        ));
    }

    #[test]
    fn test_validate_response_no_message() {
        let data = serde_json::json!({"choices": [{"finish_reason": "stop"}]});
        assert!(matches!(
            validate_response(&data),
            Err(AuxiliaryError::InvalidResponse(_, _))
        ));
    }

    #[test]
    fn test_validate_response_null_content() {
        // content: null → should error, not return Ok("")
        let data = serde_json::json!({"choices": [{"message": {"content": null}}]});
        assert!(matches!(
            validate_response(&data),
            Err(AuxiliaryError::InvalidResponse(_, _))
        ));
    }

    #[test]
    fn test_validate_response_missing_content_field() {
        // message exists but has no content key → should error
        let data = serde_json::json!({"choices": [{"message": {"role": "assistant"}}]});
        assert!(matches!(
            validate_response(&data),
            Err(AuxiliaryError::InvalidResponse(_, _))
        ));
    }

    #[test]
    fn test_is_unsupported_temperature_error() {
        assert!(is_unsupported_temperature_error(
            "unsupported parameter: temperature"
        ));
        assert!(is_unsupported_temperature_error(
            "Temperature is not supported for this model"
        ));
        assert!(!is_unsupported_temperature_error("rate limit exceeded"));
    }

    #[test]
    fn test_is_unsupported_max_tokens_error() {
        assert!(is_unsupported_max_tokens_error(
            "unsupported parameter: max_tokens"
        ));
        assert!(!is_unsupported_max_tokens_error("invalid temperature"));
    }

    #[test]
    fn test_extract_provider_from_url() {
        let provider = extract_provider_from_url("https://api.anthropic.com/v1/chat/completions");
        assert_eq!(provider, "api.anthropic.com");
    }

    #[test]
    fn test_truncate_str() {
        assert_eq!(truncate_str("hello", 10), "hello");
        assert_eq!(truncate_str("hello world", 5), "hello");
    }
}
