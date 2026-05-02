//! Model context window and max output tokens lookup.
//!
//! The model context window table is user-configured via `zn.model_context()`
//! in init.lua. This gives users full control and avoids the need to update
//! zeno itself when new models are released.
//!
//! Resolution order for context_window:
//! 1. Settings.model_contexts table lookup (longest prefix match)
//! 2. Fallback: 128000
//!
//! Resolution order for max_output_tokens (per request):
//! 1. ProviderConfig.max_output_tokens (user explicit override) → Some(value)
//! 2. Settings.max_tokens if > 0 (user global override) → Some(value)
//! 3. Auto (both are 0/None) → None (let the provider decide its default)
//!
//! When `None` is returned, the API client omits `max_tokens` /
//! `max_completion_tokens` from the request body entirely, matching
//! Hermes Agent's behavior for OpenAI-compatible providers. Anthropic
//! requires the field, so AnthropicClient applies a fallback internally.

use std::collections::HashMap;

/// Default context window when no model prefix matches.
pub const DEFAULT_CONTEXT_WINDOW: u32 = 128_000;

/// Fallback max output tokens for Anthropic API (which requires the field).
/// Not used for OpenAI-compatible providers when in auto mode.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 32_768;

/// Look up context window by model name from the user-configured table.
///
/// Uses **longest prefix match** — the most specific entry wins.
/// Model name comparison is case-insensitive.
pub fn lookup_context_window(model: &str, model_contexts: &HashMap<String, u32>) -> u32 {
    let model_lower = model.to_lowercase();
    // Find the longest matching prefix (most specific wins)
    let mut best_len: usize = 0;
    let mut best_cw: u32 = DEFAULT_CONTEXT_WINDOW;
    for (prefix, window) in model_contexts {
        let prefix_lower = prefix.to_lowercase();
        if model_lower.starts_with(&prefix_lower) && prefix_lower.len() > best_len {
            best_len = prefix_lower.len();
            best_cw = *window;
        }
    }
    if best_len > 0 {
        return best_cw;
    }
    DEFAULT_CONTEXT_WINDOW
}

/// Resolve the effective context window for the current model.
///
/// Priority:
/// 1. `model_contexts` table lookup (longest prefix match)
/// 2. DEFAULT_CONTEXT_WINDOW
pub fn resolve_context_window(model: &str, model_contexts: &HashMap<String, u32>) -> u32 {
    lookup_context_window(model, model_contexts)
}

/// Resolve the effective max_output_tokens for an API request.
///
/// Returns `Some(value)` when the user or provider has explicitly set a
/// limit, and `None` when in auto mode (both `settings_max_tokens == 0`
/// and `provider_max_output == None`). When `None` is returned, OpenAI-
/// compatible clients omit the `max_tokens` / `max_completion_tokens`
/// field entirely, letting the provider use its own default — matching
/// Hermes Agent's behavior.
///
/// Priority:
/// 1. `provider_max_output` (from ProviderConfig.max_output_tokens) → Some
/// 2. `settings_max_tokens` if > 0 (user global override) → Some
/// 3. Auto (0 + None) → None
pub fn resolve_max_output_tokens(
    settings_max_tokens: u32,
    provider_max_output: Option<u32>,
) -> Option<u32> {
    // 1. Provider-level explicit override
    if let Some(mo) = provider_max_output {
        return Some(mo);
    }

    // 2. User global override (0 = auto, >0 = use as-is)
    if settings_max_tokens > 0 {
        return Some(settings_max_tokens);
    }

    // 3. Auto: return None — let the provider decide its default
    // This matches Hermes Agent's behavior for OpenAI-compatible providers.
    // Anthropic requires max_tokens, so AnthropicClient applies
    // DEFAULT_MAX_OUTPUT_TOKENS as a fallback internally.
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_table() -> HashMap<String, u32> {
        let mut m = HashMap::new();
        // Anthropic Claude 4.6 family (1M context)
        m.insert("claude-opus-4-7".into(), 1_000_000);
        m.insert("claude-opus-4.7".into(), 1_000_000);
        m.insert("claude-opus-4-6".into(), 1_000_000);
        m.insert("claude-sonnet-4-6".into(), 1_000_000);
        m.insert("claude-opus-4.6".into(), 1_000_000);
        m.insert("claude-sonnet-4.6".into(), 1_000_000);
        // Older Claude (200k)
        m.insert("claude".into(), 200_000);
        // OpenAI GPT-5 family
        m.insert("gpt-5.5".into(), 1_050_000);
        m.insert("gpt-5.4-nano".into(), 400_000);
        m.insert("gpt-5.4-mini".into(), 400_000);
        m.insert("gpt-5.4".into(), 1_050_000);
        m.insert("gpt-5.1-chat".into(), 128_000);
        m.insert("gpt-5".into(), 400_000);
        m.insert("gpt-4.1".into(), 1_047_576);
        m.insert("gpt-4".into(), 128_000);
        // Google Gemini (1M+)
        m.insert("gemini".into(), 1_048_576);
        // GLM / Z.AI
        m.insert("glm-5".into(), 202_752);
        m.insert("glm".into(), 202_752);
        m
    }

    #[test]
    fn test_lookup_known_model() {
        let table = make_table();
        assert_eq!(
            lookup_context_window("claude-sonnet-4-6", &table),
            1_000_000
        );
        assert_eq!(lookup_context_window("gpt-5", &table), 400_000);
        assert_eq!(lookup_context_window("glm-5.1", &table), 202_752);
    }

    #[test]
    fn test_lookup_unknown_model() {
        let table = make_table();
        assert_eq!(
            lookup_context_window("my-custom-model-v1", &table),
            DEFAULT_CONTEXT_WINDOW
        );
    }

    #[test]
    fn test_lookup_empty_table() {
        let table: HashMap<String, u32> = HashMap::new();
        assert_eq!(
            lookup_context_window("claude-sonnet-4-6", &table),
            DEFAULT_CONTEXT_WINDOW
        );
    }

    #[test]
    fn test_lookup_longest_prefix_match() {
        let table = make_table();
        // "claude-opus-4-6" (len 16) is more specific than "claude" (len 6)
        assert_eq!(
            lookup_context_window("claude-opus-4-6-20250514", &table),
            1_000_000
        );
        // Generic "claude" prefix matches older models
        assert_eq!(lookup_context_window("claude-3-haiku", &table), 200_000);
    }

    #[test]
    fn test_lookup_case_insensitive() {
        let table = make_table();
        assert_eq!(lookup_context_window("Claude-3-Haiku", &table), 200_000);
        assert_eq!(lookup_context_window("GPT-5", &table), 400_000);
    }

    #[test]
    fn test_resolve_max_output_provider_override() {
        assert_eq!(resolve_max_output_tokens(0, Some(8192)), Some(8192));
    }

    #[test]
    fn test_resolve_max_output_settings_override() {
        assert_eq!(resolve_max_output_tokens(32768, None), Some(32768));
    }

    #[test]
    fn test_resolve_max_output_auto_returns_none() {
        // When no override is set, return None (let provider decide).
        assert_eq!(resolve_max_output_tokens(0, None), None);
    }

    #[test]
    fn test_resolve_context_window_with_table() {
        let table = make_table();
        assert_eq!(resolve_context_window("gpt-5", &table), 400_000);
    }

    #[test]
    fn test_resolve_context_window_empty_table() {
        let table: HashMap<String, u32> = HashMap::new();
        assert_eq!(
            resolve_context_window("gpt-5", &table),
            DEFAULT_CONTEXT_WINDOW
        );
    }
}
