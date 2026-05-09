//! Auxiliary task types, provider aliases, and routing configuration.
//!
//! Defines the auxiliary task enum, provider normalization/aliases,
//! and provides the routing logic that resolves a task to a concrete
//! provider/model pair.
//!
//! Routing functions (`resolve_provider`, `build_provider_chain`, etc.)
//! are actively used. Some task variants and error variants are only
//! constructed at runtime via `config()` lookup or are reserved for
//! future task types.
#![allow(
    dead_code,
    reason = "planned task variants (SessionSearch) and error variant (UnsupportedParameter)"
)]

use std::collections::HashMap;

use crate::config::settings::{AuxiliaryTaskConfig, Settings};

// ---------------------------------------------------------------------------
// Provider aliases (mirrors hermes-agent `_PROVIDER_ALIASES`)
// ---------------------------------------------------------------------------

/// Normalized provider name aliases. Users may type short/informal names;
/// these map to the canonical internal provider IDs.
static PROVIDER_ALIASES: &[(&str, &str)] = &[
    ("google", "gemini"),
    ("google-gemini", "gemini"),
    ("google-ai-studio", "gemini"),
    ("x-ai", "xai"),
    ("x.ai", "xai"),
    ("grok", "xai"),
    ("glm", "zai"),
    ("z-ai", "zai"),
    ("z.ai", "zai"),
    ("zhipu", "zai"),
    ("kimi", "kimi-coding"),
    ("moonshot", "kimi-coding"),
    ("kimi-cn", "kimi-coding-cn"),
    ("moonshot-cn", "kimi-coding-cn"),
    ("gmi-cloud", "gmi"),
    ("gmicloud", "gmi"),
    ("minimax-china", "minimax-cn"),
    ("minimax_cn", "minimax-cn"),
    ("claude", "anthropic"),
    ("claude-code", "anthropic"),
    ("codex", "openai-codex"),
    ("main", ""), // resolved dynamically to active provider
];

/// Normalize a provider name: trim, lowercase, resolve aliases.
///
/// - "auto" / "" → "auto"
/// - "custom:xxx" → "custom" (strip prefix, use xxx as base_url indicator)
/// - "codex" → "openai-codex"
/// - "main" → the user's active provider
/// - Otherwise, apply `_PROVIDER_ALIASES` table.
pub fn normalize_provider(provider: &str, active_provider: &str) -> String {
    let normalized = provider.trim().to_lowercase();

    if normalized.is_empty() || normalized == "auto" {
        return "auto".into();
    }

    // "custom:xxx" → use "custom"
    if let Some(suffix) = normalized.strip_prefix("custom:") {
        if suffix.is_empty() {
            return "custom".into();
        }
        // The suffix is treated as the base_url hint; caller handles it
        return "custom".into();
    }

    // "main" → resolve to active provider
    if normalized == "main" {
        let main = active_provider.trim().to_lowercase();
        if !main.is_empty() && main != "auto" && main != "main" {
            return main;
        }
        return "custom".into();
    }

    // Alias lookup
    for &(alias, canonical) in PROVIDER_ALIASES {
        if normalized == alias {
            // "main" alias resolved above; empty-string aliases skip
            if canonical.is_empty() {
                let main = active_provider.trim().to_lowercase();
                if !main.is_empty() && main != "auto" && main != "main" {
                    return main;
                }
                return "custom".into();
            }
            return canonical.into();
        }
    }

    normalized
}

// ---------------------------------------------------------------------------
// Task Types
// ---------------------------------------------------------------------------

/// Auxiliary tasks that can be offloaded to cheaper/faster models.
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub enum AuxiliaryTask {
    /// Compress long conversation history into a summary.
    Compression,
    /// Analyze an image (screenshot, CAPTCHA, etc.).
    Vision,
    /// Extract and summarize web page content.
    WebExtract,
    /// Generate a session title.
    TitleGeneration,
    /// Summarize matching past sessions for search.
    SessionSearch,
}

impl AuxiliaryTask {
    /// Get the task's config key name (matches `auxiliary` section in config).
    pub fn config_key(&self) -> &str {
        match self {
            Self::Compression => "compression",
            Self::Vision => "vision",
            Self::WebExtract => "web_extract",
            Self::TitleGeneration => "title_generation",
            Self::SessionSearch => "session_search",
        }
    }

    /// Get the task config from settings.
    pub fn config<'a>(&self, settings: &'a Settings) -> &'a AuxiliaryTaskConfig {
        match self {
            Self::Compression => &settings.auxiliary.compression,
            Self::Vision => &settings.auxiliary.vision,
            Self::WebExtract => &settings.auxiliary.web_extract,
            Self::TitleGeneration => &settings.auxiliary.title_generation,
            Self::SessionSearch => &settings.auxiliary.session_search,
        }
    }

    /// Default temperature for this task type.
    pub fn default_temperature(&self) -> f64 {
        match self {
            Self::Compression => 0.3,
            Self::Vision => 0.3,
            Self::WebExtract => 0.3,
            Self::TitleGeneration => 0.3,
            Self::SessionSearch => 0.3,
        }
    }

    /// Default max_tokens for this task type.
    pub fn default_max_tokens(&self) -> u32 {
        match self {
            Self::Compression => 4096,
            Self::Vision => 4096,
            Self::WebExtract => 4096,
            Self::TitleGeneration => 256,
            Self::SessionSearch => 1024,
        }
    }
}

// ---------------------------------------------------------------------------
// Resolved Provider
// ---------------------------------------------------------------------------

/// A fully resolved provider+model pair for an auxiliary call.
#[derive(Debug, Clone)]
pub struct ResolvedProvider {
    pub provider_name: String,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout: f64,
    /// Per-task extra body fields.
    pub extra_body: HashMap<String, serde_json::Value>,
    /// Max output tokens for this call.
    pub max_tokens: u32,
    /// Temperature for this call.
    pub temperature: f64,
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

/// Auxiliary routing errors.
#[derive(Debug, thiserror::Error)]
pub enum AuxiliaryError {
    #[error("No provider available for task: {0:?}")]
    NoProviderAvailable(AuxiliaryTask),

    #[error("Provider '{0}' returned HTTP 402 — balance exhausted")]
    PaymentRequired(String),

    #[error("Provider '{0}' returned auth error (401/403)")]
    AuthError(String),

    #[error("Connection error to provider '{0}': {1}")]
    ConnectionError(String, String),

    #[error("API call failed: {0}")]
    ApiCall(String),

    #[error("No API key for provider: {0}")]
    NoApiKey(String),

    #[error("Invalid response from provider '{0}': {1}")]
    InvalidResponse(String, String),

    #[error("Unsupported parameter '{0}' on provider '{1}'")]
    UnsupportedParameter(String, String),
}

/// Check if an HTTP status code indicates a payment error.
pub fn is_payment_error(status: u16) -> bool {
    status == 402
}

/// Check if an HTTP status code indicates an auth error.
pub fn is_auth_error(status: u16) -> bool {
    status == 401 || status == 403
}

/// Check if an HTTP status code indicates a connection / server error
/// that might be transient and worth retrying.
///
/// Delegates to the shared `api::retry::is_retryable_status_default()`.
/// Kept as a convenience re-export for use within the auxiliary module.
pub fn is_connection_error(status: u16) -> bool {
    crate::api::retry::is_retryable_status_default(status)
}

/// Route an auxiliary task to a concrete provider.
///
/// Resolution order:
/// 1. If the task config has an explicit provider (not "auto"), use it directly.
/// 2. If "auto", try the active provider first, then other configured providers.
pub fn resolve_provider(
    task: AuxiliaryTask,
    settings: &Settings,
) -> Result<ResolvedProvider, AuxiliaryError> {
    let task_config = task.config(settings);
    let normalized = normalize_provider(&task_config.provider, &settings.active_provider);

    // Explicit provider (not "auto")
    if normalized != "auto" {
        return resolve_explicit(&normalized, task_config, settings);
    }

    // Auto: try provider chain
    let chain = build_provider_chain(settings);

    for candidate in chain {
        match try_resolve_candidate(&candidate, task_config, settings) {
            Ok(resolved) => return Ok(resolved),
            Err(AuxiliaryError::NoApiKey(_)) => continue,
            Err(e) => return Err(e),
        }
    }

    Err(AuxiliaryError::NoProviderAvailable(task))
}

/// Build the provider chain for auto routing.
pub fn build_provider_chain(settings: &Settings) -> Vec<String> {
    let mut chain = Vec::new();

    // 1. Active provider
    chain.push(settings.active_provider.clone());

    // 2. Other providers in config order
    for name in settings.providers.keys() {
        if *name != settings.active_provider && !chain.contains(name) {
            chain.push(name.clone());
        }
    }

    chain
}

/// Resolve an explicitly configured provider.
fn resolve_explicit(
    provider_name: &str,
    task_config: &AuxiliaryTaskConfig,
    settings: &Settings,
) -> Result<ResolvedProvider, AuxiliaryError> {
    let provider = settings.providers.get(provider_name).ok_or_else(|| {
        AuxiliaryError::ApiCall(format!("Provider '{}' not found in config", provider_name))
    })?;

    let api_key = crate::config::settings::resolve_api_key_opt(provider.api_key.as_deref())
        .ok_or_else(|| AuxiliaryError::NoApiKey(provider_name.to_string()))?;

    let model = if task_config.model.is_empty() {
        if provider.default_model.is_empty() {
            settings.model.clone()
        } else {
            provider.default_model.clone()
        }
    } else {
        task_config.model.clone()
    };

    let base_url = task_config
        .base_url
        .clone()
        .unwrap_or_else(|| provider.base_url.clone());

    let temperature = task_config
        .temperature
        .unwrap_or_else(|| task_config.default_temperature_for_task());

    let max_tokens = if task_config.max_tokens > 0 {
        task_config.max_tokens
    } else {
        task_config.default_max_tokens_for_task()
    };

    Ok(ResolvedProvider {
        provider_name: provider_name.to_string(),
        base_url,
        api_key,
        model,
        timeout: task_config.timeout,
        extra_body: task_config.extra_body.clone(),
        max_tokens,
        temperature,
    })
}

/// Try to resolve a candidate provider from the auto chain.
pub fn try_resolve_candidate(
    provider_name: &str,
    task_config: &AuxiliaryTaskConfig,
    settings: &Settings,
) -> Result<ResolvedProvider, AuxiliaryError> {
    let provider = settings
        .providers
        .get(provider_name)
        .ok_or_else(|| AuxiliaryError::NoApiKey(provider_name.to_string()))?;

    let api_key = crate::config::settings::resolve_api_key_opt(provider.api_key.as_deref())
        .ok_or_else(|| AuxiliaryError::NoApiKey(provider_name.to_string()))?;

    let model = if task_config.model.is_empty() {
        if provider.default_model.is_empty() {
            settings.model.clone()
        } else {
            provider.default_model.clone()
        }
    } else {
        task_config.model.clone()
    };

    let base_url = task_config
        .base_url
        .clone()
        .unwrap_or_else(|| provider.base_url.clone());

    let temperature = task_config
        .temperature
        .unwrap_or_else(|| task_config.default_temperature_for_task());

    let max_tokens = if task_config.max_tokens > 0 {
        task_config.max_tokens
    } else {
        task_config.default_max_tokens_for_task()
    };

    Ok(ResolvedProvider {
        provider_name: provider_name.to_string(),
        base_url,
        api_key,
        model,
        timeout: task_config.timeout,
        extra_body: task_config.extra_body.clone(),
        max_tokens,
        temperature,
    })
}

/// Helper: get default temperature for a task from its config key name.
impl AuxiliaryTaskConfig {
    fn default_temperature_for_task(&self) -> f64 {
        // All tasks currently use 0.3; this is a hook for future per-task defaults.
        0.3
    }

    fn default_max_tokens_for_task(&self) -> u32 {
        4096
    }
}

// ---------------------------------------------------------------------------
// Models that manage temperature server-side (should NOT send temperature)
// ---------------------------------------------------------------------------

/// Returns true for models that manage temperature internally and should
/// not receive a `temperature` parameter in API calls.
///
/// Reference: hermes-agent `_is_kimi_model`, `_fixed_temperature_for_model`.
pub fn model_omits_temperature(model: &str) -> bool {
    let lower = model.trim().to_lowercase();
    let bare = lower.rsplit('/').next().unwrap_or(&lower);
    // Kimi / Moonshot models manage temperature server-side
    bare.starts_with("kimi-") || bare == "kimi"
}

/// Returns the effective temperature for a model, or None if the model
/// should not receive a temperature parameter.
pub fn effective_temperature(model: &str, requested: Option<f64>) -> Option<f64> {
    if model_omits_temperature(model) {
        return None;
    }
    Some(requested.unwrap_or(0.3))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::settings::*;

    fn make_settings() -> Settings {
        let mut providers = HashMap::new();
        providers.insert(
            "custom".into(),
            ProviderConfig {
                api_key: Some("test-key".into()),
                base_url: "https://api.example.com/v1".into(),
                default_model: "test-model".into(),
                max_output_tokens: None,
            },
        );
        providers.insert(
            "fallback".into(),
            ProviderConfig {
                api_key: Some("fallback-key".into()),
                base_url: "https://fallback.example.com/v1".into(),
                default_model: "fallback-model".into(),
                max_output_tokens: None,
            },
        );

        Settings {
            providers,
            active_provider: "custom".into(),
            model: "default-model".into(),
            ..Settings::default()
        }
    }

    #[test]
    fn test_normalize_provider_auto() {
        assert_eq!(normalize_provider("auto", "custom"), "auto");
        assert_eq!(normalize_provider("", "custom"), "auto");
    }

    #[test]
    fn test_normalize_provider_aliases() {
        assert_eq!(normalize_provider("claude", "custom"), "anthropic");
        assert_eq!(normalize_provider("Claude-Code", "custom"), "anthropic");
        assert_eq!(normalize_provider("codex", "custom"), "openai-codex");
        assert_eq!(normalize_provider("google", "custom"), "gemini");
    }

    #[test]
    fn test_normalize_provider_main() {
        assert_eq!(normalize_provider("main", "custom"), "custom");
    }

    #[test]
    fn test_normalize_provider_custom_prefix() {
        assert_eq!(
            normalize_provider("custom:http://localhost:8080", "custom"),
            "custom"
        );
    }

    #[test]
    fn test_resolve_auto_active_provider() {
        let settings = make_settings();
        let resolved = resolve_provider(AuxiliaryTask::Compression, &settings).unwrap();
        assert_eq!(resolved.provider_name, "custom");
    }

    #[test]
    fn test_resolve_explicit_provider() {
        let mut settings = make_settings();
        settings.auxiliary.compression.provider = "fallback".into();
        let resolved = resolve_provider(AuxiliaryTask::Compression, &settings).unwrap();
        assert_eq!(resolved.provider_name, "fallback");
    }

    #[test]
    fn test_resolve_no_provider() {
        let settings = Settings::default();
        let result = resolve_provider(AuxiliaryTask::Compression, &settings);
        assert!(matches!(
            result,
            Err(AuxiliaryError::NoProviderAvailable(_))
        ));
    }

    #[test]
    fn test_task_config_key() {
        assert_eq!(AuxiliaryTask::Compression.config_key(), "compression");
        assert_eq!(AuxiliaryTask::Vision.config_key(), "vision");
        assert_eq!(AuxiliaryTask::WebExtract.config_key(), "web_extract");
        assert_eq!(
            AuxiliaryTask::TitleGeneration.config_key(),
            "title_generation"
        );
        assert_eq!(AuxiliaryTask::SessionSearch.config_key(), "session_search");
    }

    #[test]
    fn test_model_omits_temperature() {
        assert!(model_omits_temperature("kimi-latest"));
        assert!(model_omits_temperature("kimi"));
        assert!(model_omits_temperature("openrouter/kimi-latest"));
        assert!(!model_omits_temperature("gpt-4o"));
        assert!(!model_omits_temperature("claude-sonnet-4"));
    }

    #[test]
    fn test_effective_temperature() {
        assert_eq!(effective_temperature("gpt-4o", Some(0.5)), Some(0.5));
        assert_eq!(effective_temperature("gpt-4o", None), Some(0.3));
        assert_eq!(effective_temperature("kimi-latest", Some(0.5)), None);
    }

    #[test]
    fn test_is_payment_error() {
        assert!(is_payment_error(402));
        assert!(!is_payment_error(401));
        assert!(!is_payment_error(500));
    }

    #[test]
    fn test_is_auth_error() {
        assert!(is_auth_error(401));
        assert!(is_auth_error(403));
        assert!(!is_auth_error(402));
    }

    #[test]
    fn test_is_connection_error() {
        assert!(is_connection_error(500));
        assert!(is_connection_error(502));
        assert!(is_connection_error(429));
        assert!(!is_connection_error(400));
        assert!(!is_connection_error(402));
    }
}
