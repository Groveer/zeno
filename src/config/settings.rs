use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::permissions::execpolicy::ExecRule;
use crate::sandbox::SandboxConfig;

// ---------------------------------------------------------------------------
// Top-level settings
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Settings {
    pub providers: HashMap<String, ProviderConfig>,
    pub active_provider: String,
    pub model: String,
    pub tools: ToolsConfig,
    pub role: RoleConfig,
    pub web_search_config: WebSearchConfig,
    pub mcp: McpConfig,
    pub permissions: PermissionMode,
    /// Paths that are exempt from permission checks.
    /// Files under these paths bypass both the CWD boundary check and
    /// the mode-based ask/deny behavior — operations are always allowed.
    /// Configured via `zn.trusted_paths({"/home/user/proj"})` in init.lua.
    #[serde(default)]
    pub trusted_paths: Vec<String>,
    pub max_turns: u32,
    pub max_tokens: u32,
    /// Model context window table: model name prefix → context window (tokens).
    /// Used for auto-detecting context window from the model name.
    /// Longer prefixes are tried first (most specific match wins).
    /// Configured via `zn.model_context("prefix", n)` in init.lua.
    #[serde(default)]
    pub model_contexts: HashMap<String, u32>,
    pub theme: String,
    pub memory: MemoryConfig,
    pub auxiliary: AuxiliaryConfig,
    pub llm: LlmConfig,
    pub log_retention_days: u64,
    /// Structured output format (response_format) for the API.
    /// When set to a JSON Schema value, the model is constrained to output
    /// JSON matching that schema. Currently supported by OpenAI-compatible APIs.
    /// Set via `zn.response_format({ type = "json_schema", json_schema = { ... } })`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<serde_json::Value>,
    /// Delegation config for sub-agents (delegate_task tool).
    #[serde(default)]
    pub delegation: DelegationConfig,
    /// Skill management config (curator, background review, lifecycle).
    #[serde(default)]
    pub skills: SkillsConfig,
    /// Engine behavior config (timeouts, auto-continue, collapse thresholds).
    #[serde(default)]
    pub engine: EngineConfig,
    /// Extra paths that are always allowed (in addition to /tmp and /var/tmp).
    #[serde(default)]
    pub safe_paths: Vec<String>,
    /// Named identities defined via `zn.identity("name", {...})` in init.lua.
    /// Each identity provides custom `identity` and `guidelines` that override
    /// the default `RoleConfig` when activated.
    #[serde(default)]
    pub identities: HashMap<String, IdentityConfig>,
    /// Currently active identity name. When set, the corresponding
    /// `IdentityConfig` overrides `RoleConfig` for system prompt building.
    /// Set via `zn.set_identity("name")` or `ZENO_IDENTITY` env var.
    #[serde(default)]
    pub active_identity: Option<String>,
    /// Sandbox configuration for secure command execution.
    /// Default: no sandbox (SandboxMode::None).
    /// Configured via `zn.sandbox({ mode = "workspace_write" })` in init.lua.
    #[serde(default)]
    pub sandbox: SandboxConfig,
    /// Execution policy rules for bash command authorization.
    /// Rules are evaluated in order — first match wins.
    /// Configured per-rule in init.lua:
    ///   zn.exec_policy({ pattern = "^git push", action = "ask",
    ///                     reason = "Confirm pushes" })
    #[serde(default)]
    pub exec_policy_rules: Vec<ExecRule>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            providers: HashMap::new(),
            active_provider: "anthropic".into(),
            model: String::new(),
            tools: ToolsConfig::default(),
            role: RoleConfig::default(),
            web_search_config: WebSearchConfig::default(),
            mcp: McpConfig::default(),
            permissions: PermissionMode::Ask,
            trusted_paths: Vec::new(),
            max_turns: 200,
            max_tokens: 0, // 0 = auto (derived from model context window)
            model_contexts: HashMap::new(),
            theme: "default".into(),
            memory: MemoryConfig::default(),
            auxiliary: AuxiliaryConfig::default(),
            llm: LlmConfig::default(),
            delegation: DelegationConfig::default(),
            skills: SkillsConfig::default(),
            log_retention_days: 3,
            response_format: None,
            engine: EngineConfig::default(),
            safe_paths: Vec::new(),
            identities: HashMap::new(),
            active_identity: None,
            sandbox: SandboxConfig::default(),
            exec_policy_rules: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

/// API protocol type for an LLM provider.
///
/// Determines the request format, authentication method, and endpoint path
/// used when communicating with the provider's API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
pub enum ApiType {
    /// OpenAI Chat Completions API format (default).
    ///
    /// - Endpoint: `POST /v1/chat/completions`
    /// - Auth: `Authorization: Bearer <key>`
    /// - Compatible with: OpenAI, DeepSeek, Ollama, Groq, Together, etc.
    #[serde(rename = "openai")]
    #[default]
    OpenAi,
    /// OpenAI Responses API format (newer OpenAI API).
    ///
    /// - Endpoint: `POST /v1/responses`
    /// - Auth: `Authorization: Bearer <key>`
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
    /// Anthropic Messages API format.
    ///
    /// - Endpoint: `POST /v1/messages`
    /// - Auth: `x-api-key` header
    /// - Compatible with: Anthropic, Anthropic-compatible proxies
    #[serde(rename = "anthropic")]
    Anthropic,
}

/// Configuration for an LLM provider (e.g. "anthropic", "openai").
///
/// The `api_key` field supports auto-detection:
/// - UPPER_SNAKE_CASE values (e.g. `"ANTHROPIC_API_KEY"`) are treated as
///   environment variable names first; if the env var doesn't exist, the
///   value is used as a literal key.
/// - Other patterns (e.g. `"sk-abc123"`) are used as literal API keys directly.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderConfig {
    /// API key or environment variable name (auto-detected).
    ///
    /// Examples:
    /// - `"ANTHROPIC_API_KEY"` → resolve from env var, fallback to literal
    /// - `"sk-ant-xxxx"` → used directly as literal API key
    pub api_key: Option<String>,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub default_model: String,
    /// Optional: max output tokens per request.
    /// If set, overrides the top-level `max_tokens` setting.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// API protocol type: "openai" (default), "openai-responses", or "anthropic".
    ///
    /// Determines the request format, auth method, and endpoint path.
    /// - `"openai"` (default) → Chat Completions API (`/v1/chat/completions`)
    /// - `"openai-responses"` → Responses API (`/v1/responses`)
    /// - `"anthropic"` → Messages API (`/v1/messages`)
    ///
    /// Most providers (OpenAI, DeepSeek, Ollama, Groq, etc.) use `"openai"`.
    /// Only Anthropic and Anthropic-compatible proxies use `"anthropic"`.
    #[serde(default)]
    pub api_type: ApiType,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            base_url: String::new(),
            default_model: String::new(),
            max_output_tokens: None,
            api_type: ApiType::OpenAi,
        }
    }
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ToolsConfig {
    pub bash: bool,
    pub use_rtk: bool,
    /// Extra environment variables injected into every bash command execution.
    /// Key-value pairs like `{"NODE_ENV": "development"}`.
    #[serde(default)]
    pub bash_env: HashMap<String, String>,
    /// Maximum lines in bash tool output before head/tail truncation.
    /// When exceeded, keeps the first ~30% and last ~70% of lines with
    /// a `[truncated — omitted N lines]` marker. Set to 0 to disable.
    ///
    /// NOTE: No field-level `#[serde(default)]` here — the struct-level
    /// `#[serde(default)]` uses `ToolsConfig::default()` which sets 500.
    /// A field-level default would override that with `usize::default()` (0).
    pub bash_max_lines: usize,
    pub read: bool,
    pub write: bool,
    pub edit: bool,
    pub glob: bool,
    pub grep: bool,
    pub web_search: bool,
    pub web_fetch: bool,
    /// Directories to skip during glob and grep traversal.
    /// Merged with a built-in default list (`.git`, `node_modules`, etc.).
    /// User additions are appended; the defaults are always included.
    #[serde(default)]
    pub skip_dirs: Vec<String>,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            bash: true,
            use_rtk: true,
            bash_env: HashMap::new(),
            bash_max_lines: 500,
            read: true,
            write: true,
            edit: true,
            glob: true,
            grep: true,
            web_search: true,
            web_fetch: true,
            skip_dirs: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Guidelines (inline text and external file references)
// ---------------------------------------------------------------------------

/// A single guideline entry — inline text or an external file reference.
///
/// When it's a file reference, both the text prefix and the file content
/// contribute to the final guidelines text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GuidelineEntry {
    /// Plain inline text.
    Text(String),
    /// Text prefix + external file path (`{ text, file_path }` in Lua).
    Ref((String, PathBuf)),
}

/// Collection of guidelines, supporting multiple sources.
///
/// Accepts either a plain string (backward compatible) or an array of entries:
/// - `"- Always validate"` (plain string)
/// - `{ "- Style guide", zn.config_dir .. "style.md" }` (file reference)
///
/// Multiple entries can be mixed:
/// ```lua
/// zn.role({
///   guidelines = {
///     "- Be concise.",
///     { "- Company rules:", zn.config_dir .. "company.md" },
///   }
/// })
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GuidelinesConfig {
    /// Single inline text (backward compatible).
    Single(String),
    /// Multiple entries — each is inline text or a file reference.
    Multi(Vec<GuidelineEntry>),
}

impl GuidelinesConfig {
    /// Resolve all entries into a single string, reading external files as needed.
    ///
    /// Relative file paths are resolved relative to `config_dir`.
    pub fn resolve(&self, config_dir: &Path) -> anyhow::Result<String> {
        match self {
            Self::Single(s) => Ok(s.clone()),
            Self::Multi(entries) => {
                let mut parts: Vec<String> = Vec::new();
                for entry in entries {
                    match entry {
                        GuidelineEntry::Text(text) => parts.push(text.clone()),
                        GuidelineEntry::Ref((text, path)) => {
                            let full_path = if path.is_relative() {
                                config_dir.join(path)
                            } else {
                                path.clone()
                            };
                            let content =
                                std::fs::read_to_string(&full_path).with_context(|| {
                                    format!(
                                        "Failed to read guidelines file: {}",
                                        full_path.display()
                                    )
                                })?;
                            let combined = if text.is_empty() {
                                content
                            } else {
                                format!("{}\n{}", text, content)
                            };
                            parts.push(combined);
                        }
                    }
                }
                Ok(parts.join("\n\n"))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Role (identity/persona)
// ---------------------------------------------------------------------------

/// Configuration for a named identity (persona).
///
/// Each identity overrides the default `RoleConfig` fields when activated.
/// Defined via `zn.identity("name", { identity = "...", guidelines = "..." })` in init.lua.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct IdentityConfig {
    /// Custom identity text (overrides `RoleConfig.identity`).
    #[serde(default)]
    pub identity: Option<String>,
    /// Custom guidelines (overrides `RoleConfig.guidelines`).
    ///
    /// Accepts either a plain string or structured entries with file references.
    #[serde(default)]
    pub guidelines: Option<GuidelinesConfig>,
}

/// Customizable role sections for the system prompt.
/// All fields are optional — `None` means use the built-in default text.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
#[derive(Default)]
pub struct RoleConfig {
    /// Core identity and role declaration (replaces the default "You are zeno..." block).
    pub identity: Option<String>,
    /// Guidelines section (replaces the default guidelines block).
    ///
    /// Accepts either a plain string or structured entries with file references.
    pub guidelines: Option<GuidelinesConfig>,
}

// ---------------------------------------------------------------------------
// Web Search
// ---------------------------------------------------------------------------

/// Configuration for the web search tool.
/// Users can customize the search backend via `zn.web_search({...})` in init.lua.
///
/// Supported providers:
/// - `searxng` (default): SearXNG meta-search engine, no API key required
/// - `brave`: Brave Search API, requires API key
/// - `tavily`: Tavily Search API, requires API key
/// - `duckduckgo`: DuckDuckGo Lite (fallback, no API key)
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct WebSearchConfig {
    /// Search provider: "searxng", "brave", "tavily", or "duckduckgo".
    pub provider: String,
    /// Base URL for the search service.
    /// For searxng: the instance URL (default: "https://searx.be")
    /// For brave/tavily: usually not needed (uses official API endpoint)
    pub url: String,
    /// API key or environment variable name (auto-detected).
    ///
    /// - UPPER_SNAKE_CASE → treated as env var name first, fallback to literal
    /// - Other patterns (e.g. `"BSA-xxxx"`) → used directly as literal key
    pub api_key: Option<String>,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            provider: "searxng".into(),
            url: String::new(), // empty = use provider default
            api_key: None,
        }
    }
}

impl WebSearchConfig {
    /// Resolve the API key with auto-detection.
    pub fn resolve_api_key(&self) -> Option<String> {
        resolve_api_key_opt(self.api_key.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_looks_like_env_var() {
        // Standard env var names → true
        assert!(looks_like_env_var("BRAVE_API_KEY"));
        assert!(looks_like_env_var("API_KEY"));
        assert!(looks_like_env_var("MY_VAR_123"));
        assert!(looks_like_env_var("A_"));
        assert!(looks_like_env_var("X2"));
        // Underscore prefix is valid per POSIX
        assert!(looks_like_env_var("_MY_KEY"));
        assert!(looks_like_env_var("__KEY"));

        // Literal API keys → false (use raw strings to avoid Rust 2024 edition prefix parsing)
        assert!(!looks_like_env_var(r#"BSA-xxxx-yyyy"#));
        assert!(!looks_like_env_var(r#"sk-abc123def456"#));
        assert!(!looks_like_env_var(r#"tvly-xxxxxxxx"#));
        assert!(!looks_like_env_var("lower_case_key"));

        // Edge cases → false
        assert!(!looks_like_env_var("A")); // too short
        assert!(!looks_like_env_var("123")); // no letters
        assert!(!looks_like_env_var("has spaces"));
        assert!(!looks_like_env_var("has.dots"));
        assert!(!looks_like_env_var("")); // empty
        assert!(looks_like_env_var("_A")); // underscore prefix + letter = valid
    }

    #[test]
    fn test_web_search_resolve_api_key_literal() {
        // Literal key (not UPPER_SNAKE_CASE) → used directly
        let cfg = WebSearchConfig {
            provider: "brave".into(),
            url: String::new(),
            api_key: Some(r#"BSA-xxxx-yyyy"#.into()),
        };
        assert_eq!(cfg.resolve_api_key(), Some(r#"BSA-xxxx-yyyy"#.into()));
    }

    #[test]
    fn test_web_search_resolve_api_key_env_name_exists() {
        // UPPER_SNAKE_CASE → try env var first
        unsafe { std::env::set_var("ZENO_TEST_WEB_KEY", "test-key-123") };
        let cfg = WebSearchConfig {
            provider: "brave".into(),
            url: String::new(),
            api_key: Some("ZENO_TEST_WEB_KEY".into()),
        };
        assert_eq!(cfg.resolve_api_key(), Some("test-key-123".into()));
        unsafe { std::env::remove_var("ZENO_TEST_WEB_KEY") };
    }

    #[test]
    fn test_web_search_resolve_api_key_env_name_fallback() {
        // UPPER_SNAKE_CASE but env var doesn't exist → fallback to literal
        let cfg = WebSearchConfig {
            provider: "brave".into(),
            url: String::new(),
            api_key: Some("NONEXISTENT_KEY_XYZ".into()),
        };
        // Env var doesn't exist → use the string itself as literal key
        assert_eq!(cfg.resolve_api_key(), Some("NONEXISTENT_KEY_XYZ".into()));
    }

    #[test]
    fn test_web_search_resolve_api_key_none() {
        let cfg = WebSearchConfig::default();
        assert!(cfg.resolve_api_key().is_none());
    }

    #[test]
    fn test_resolve_api_key_opt_literal() {
        assert_eq!(
            resolve_api_key_opt(Some(r#"sk-abc123"#)),
            Some(r#"sk-abc123"#.to_string())
        );
    }

    #[test]
    fn test_resolve_api_key_opt_env_var() {
        unsafe { std::env::set_var("ZENO_TEST_PROVIDER_KEY", "provider-key-456") };
        assert_eq!(
            resolve_api_key_opt(Some("ZENO_TEST_PROVIDER_KEY")),
            Some("provider-key-456".to_string())
        );
        unsafe { std::env::remove_var("ZENO_TEST_PROVIDER_KEY") };
    }

    #[test]
    fn test_resolve_api_key_opt_none() {
        assert_eq!(resolve_api_key_opt(None), None);
    }

    #[test]
    fn test_resolve_api_key_opt_empty() {
        assert_eq!(resolve_api_key_opt(Some("")), None);
    }

    #[test]
    fn test_resolve_provider_api_key() {
        let provider = ProviderConfig {
            api_key: Some("ANTHROPIC_API_KEY".into()),
            base_url: String::new(),
            default_model: String::new(),
            max_output_tokens: None,
            api_type: ApiType::OpenAi,
        };
        // env var doesn't exist → fallback to literal
        assert_eq!(resolve_api_key(&provider).unwrap(), "ANTHROPIC_API_KEY");
    }

    #[test]
    fn test_resolve_provider_api_key_missing() {
        let provider = ProviderConfig::default();
        assert!(resolve_api_key(&provider).is_err());
    }

    #[test]
    fn test_identity_config_deserialization() {
        // Test IdentityConfig with both fields
        let json =
            r#"{"identity": "You are a Rust developer", "guidelines": "Use idiomatic Rust"}"#;
        let config: IdentityConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.identity.as_deref(), Some("You are a Rust developer"));
        assert!(
            matches!(config.guidelines, Some(GuidelinesConfig::Single(ref s)) if s == "Use idiomatic Rust"),
            "expected Single('Use idiomatic Rust'), got {:?}",
            config.guidelines
        );

        // Test IdentityConfig with only identity field
        let json = r#"{"identity": "You are a Python developer"}"#;
        let config: IdentityConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            config.identity.as_deref(),
            Some("You are a Python developer")
        );
        assert_eq!(config.guidelines, None);

        // Test IdentityConfig with only guidelines field
        let json = r#"{"guidelines": "Follow PEP 8"}"#;
        let config: IdentityConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.identity, None);
        assert!(
            matches!(config.guidelines, Some(GuidelinesConfig::Single(ref s)) if s == "Follow PEP 8"),
            "expected Single('Follow PEP 8'), got {:?}",
            config.guidelines
        );

        // Test empty IdentityConfig
        let json = r#"{}"#;
        let config: IdentityConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.identity, None);
        assert_eq!(config.guidelines, None);
    }

    #[test]
    fn test_identity_config_default() {
        let config = IdentityConfig::default();
        assert_eq!(config.identity, None);
        assert_eq!(config.guidelines, None);
    }

    #[test]
    fn test_settings_with_identities() {
        let json = r#"{
            "active_identity": "dev",
            "identities": {
                "dev": {
                    "identity": "You are a Rust developer",
                    "guidelines": "Use idiomatic Rust"
                },
                "reviewer": {
                    "identity": "You are a code reviewer"
                }
            }
        }"#;
        let settings: Settings = serde_json::from_str(json).unwrap();

        assert_eq!(settings.active_identity.as_deref(), Some("dev"));
        assert_eq!(settings.identities.len(), 2);

        let dev = settings.identities.get("dev").unwrap();
        assert_eq!(dev.identity.as_deref(), Some("You are a Rust developer"));
        assert!(
            matches!(dev.guidelines, Some(GuidelinesConfig::Single(ref s)) if s == "Use idiomatic Rust"),
            "expected Single('Use idiomatic Rust'), got {:?}",
            dev.guidelines
        );

        let reviewer = settings.identities.get("reviewer").unwrap();
        assert_eq!(
            reviewer.identity.as_deref(),
            Some("You are a code reviewer")
        );
        assert_eq!(reviewer.guidelines, None);
    }

    #[test]
    fn test_settings_with_empty_identities() {
        let json = r#"{
            "active_identity": null,
            "identities": {}
        }"#;
        let settings: Settings = serde_json::from_str(json).unwrap();

        assert_eq!(settings.active_identity, None);
        assert!(settings.identities.is_empty());
    }

    #[test]
    fn test_guidelines_config_deserialization() {
        // GuidelinesConfig::Multi with a mix of Text and Ref entries
        let json = r#"[
            "- Always check logs first.",
            ["- Company rules:", "/path/to/company.md"]
        ]"#;
        let config: GuidelinesConfig = serde_json::from_str(json).unwrap();
        match config {
            GuidelinesConfig::Multi(ref entries) => {
                assert_eq!(entries.len(), 2);
                assert!(
                    matches!(&entries[0], GuidelineEntry::Text(s) if s == "- Always check logs first."),
                    "expected Text entry"
                );
                assert!(
                    matches!(&entries[1], GuidelineEntry::Ref((text, _)) if text == "- Company rules:"),
                    "expected Ref entry"
                );
            }
            _ => panic!("Expected Multi variant"),
        }
    }
}
// ---------------------------------------------------------------------------
// MCP
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
#[derive(Default)]
pub struct McpConfig {
    pub servers: HashMap<String, McpServerConfig>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct McpServerConfig {
    pub command: Option<Vec<String>>,
    pub url: Option<String>,
    /// Custom HTTP headers for url-based MCP servers (e.g. Authorization, API keys).
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// User-written description shown to LLM before activation (zero-cost routing).
    #[serde(default)]
    pub description: Option<String>,
    /// Optional tags for semantic routing by the LLM.
    #[serde(default)]
    pub tags: Vec<String>,
}

// ---------------------------------------------------------------------------
// Permissions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum PermissionMode {
    Allow,
    Deny,
    #[default]
    Ask,
}

impl fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Allow => write!(f, "allow"),
            Self::Deny => write!(f, "deny"),
            Self::Ask => write!(f, "ask"),
        }
    }
}

impl FromStr for PermissionMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "allow" => Ok(Self::Allow),
            "deny" => Ok(Self::Deny),
            "ask" => Ok(Self::Ask),
            _ => Err(format!("unknown permission mode: {}", s)),
        }
    }
}

// ---------------------------------------------------------------------------
// LLM
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct LlmConfig {
    /// Maximum number of retries when the LLM returns an empty response
    /// or the request times out / fails with a transient error.
    pub max_retries: u32,
    /// Fraction of the model context window (0.0–1.0) at which
    /// auto-compaction triggers.  0.5 = compact when total tokens
    /// exceed 33% of the context window.  Set to 0.0 to disable.
    pub compact_threshold: f64,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            compact_threshold: 0.5,
        }
    }
}

// ---------------------------------------------------------------------------
// Delegation
// ---------------------------------------------------------------------------

/// Configuration for the delegate_task tool (sub-agent system).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct DelegationConfig {
    /// Maximum number of sub-agents that can run concurrently.
    /// Default: 3. Minimum: 1.
    pub max_concurrent_children: u32,
    /// Maximum number of tool-calling turns for a sub-agent.
    pub max_turns: u32,
    /// Max auto-continuations for sub-agent.
    pub max_auto_continue: u32,
}

impl Default for DelegationConfig {
    fn default() -> Self {
        Self {
            max_concurrent_children: 3,
            max_turns: 200,
            max_auto_continue: 2,
        }
    }
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

/// Configuration for the query engine behavior.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct EngineConfig {
    /// Maximum auto-continue attempts per user input.
    pub max_auto_continue: u32,
    /// Per-stream-event idle timeout in seconds.
    ///
    /// If the stream produces no event within this duration, it's considered
    /// stalled and triggers a retry. Set to `0` to disable (wait indefinitely).
    pub stream_timeout_secs: u64,
    /// Context collapse char limit.
    pub collapse_char_limit: usize,
    /// Context collapse head chars.
    pub collapse_head_chars: usize,
    /// Context collapse tail chars.
    pub collapse_tail_chars: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_auto_continue: 3,
            stream_timeout_secs: 120,
            collapse_char_limit: 2400,
            collapse_head_chars: 900,
            collapse_tail_chars: 500,
        }
    }
}

// ---------------------------------------------------------------------------
// Skills (curator, background review, lifecycle)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SkillsConfig {
    /// Enable the background review fork (runs after every N turns).
    pub background_review_enabled: bool,
    /// How many turns between background review runs. 0 = disabled.
    pub review_interval_turns: u32,
    /// Enable the curator (periodic consolidation + lifecycle transitions).
    pub curator_enabled: bool,
    /// How often the curator runs (in hours).
    pub curator_interval_hours: u64,
    /// Days without activity before a skill is marked stale.
    pub stale_after_days: u64,
    /// Days without activity before a skill is archived.
    pub archive_after_days: u64,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            background_review_enabled: true,
            review_interval_turns: 10,
            curator_enabled: true,
            curator_interval_hours: 168, // 7 days
            stale_after_days: 30,
            archive_after_days: 90,
        }
    }
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// Character limit for MEMORY.md (agent notes). Default: 2200.
    pub memory_char_limit: usize,
    /// Character limit for USER.md (user profile). Default: 1375.
    pub user_char_limit: usize,
    /// Name of the active external memory provider (e.g. "mem0", "hindsight").
    /// Empty string means no external provider (built-in only).
    /// Configured via `zn.memory_provider("name", require("module"))` in init.lua.
    /// The provider table lives in the shared Lua VM's registry.
    pub provider: String,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            memory_char_limit: 2200,
            user_char_limit: 1375,
            provider: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Auxiliary
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct AuxiliaryConfig {
    pub compression: AuxiliaryTaskConfig,
    pub vision: AuxiliaryTaskConfig,
    pub web_fetch: AuxiliaryTaskConfig,
    pub title_generation: AuxiliaryTaskConfig,
    pub session_search: AuxiliaryTaskConfig,
    /// Delegation task config (for sub-agents). When provider is set
    /// (not "auto"), sub-agents use a different model/provider than the parent.
    pub delegation: AuxiliaryTaskConfig,
}

impl Default for AuxiliaryConfig {
    fn default() -> Self {
        Self {
            compression: AuxiliaryTaskConfig::default_with_timeout(30.0),
            vision: AuxiliaryTaskConfig::default_with_timeout(30.0),
            web_fetch: AuxiliaryTaskConfig::default_with_timeout(60.0),
            title_generation: AuxiliaryTaskConfig::default_with_timeout(30.0),
            session_search: AuxiliaryTaskConfig::default_with_timeout(30.0),
            delegation: AuxiliaryTaskConfig::default_with_timeout(60.0),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AuxiliaryTaskConfig {
    #[serde(default = "default_auto")]
    pub provider: String,
    #[serde(default)]
    pub model: String,
    /// Custom API endpoint URL for this task.
    /// If unset, falls back to the resolved provider's base_url.
    pub url: Option<String>,
    /// API key or environment variable name (auto-detected).
    /// Same logic as `ProviderConfig.api_key` and `WebSearchConfig.api_key`.
    pub api_key: Option<String>,
    /// Per-task extra body fields (provider-specific request parameters).
    /// E.g. `{"enable_thinking": false}` for providers that support it.
    #[serde(default)]
    pub extra_body: HashMap<String, serde_json::Value>,
    /// Maximum output tokens for this task. 0 = use default (4096).
    #[serde(default)]
    pub max_tokens: u32,
    /// Request timeout in seconds for this task. 0 = no timeout (wait indefinitely).
    #[serde(default = "default_task_timeout")]
    pub timeout: f64,
    /// Temperature override for this task. None = use task-specific default.
    pub temperature: Option<f64>,
}

fn default_auto() -> String {
    "auto".into()
}

fn default_task_timeout() -> f64 {
    30.0
}

impl Default for AuxiliaryTaskConfig {
    fn default() -> Self {
        Self {
            provider: "auto".into(),
            model: String::new(),
            url: None,
            api_key: None,
            extra_body: HashMap::new(),
            max_tokens: 0,
            timeout: default_task_timeout(),
            temperature: None,
        }
    }
}

impl AuxiliaryTaskConfig {
    pub fn default_with_timeout(timeout: f64) -> Self {
        Self {
            timeout,
            ..Self::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Load / resolve
// ---------------------------------------------------------------------------

/// Returns true if the string looks like an environment variable name:
/// UPPER_SNAKE_CASE — only ASCII uppercase letters, digits, and underscores,
/// must start with a letter or underscore, at least 2 characters.
pub fn looks_like_env_var(s: &str) -> bool {
    if s.len() < 2 {
        return false;
    }
    let mut has_letter = false;
    for c in s.chars() {
        match c {
            'A'..='Z' => has_letter = true,
            '0'..='9' => {}
            '_' => {}
            _ => return false,
        }
    }
    has_letter
}

/// Resolve an optional `api_key` value with auto-detection.
///
/// - If the value looks like an env var name (UPPER_SNAKE_CASE),
///   try reading from the environment first; fall back to using it as a literal key.
/// - Otherwise, use it as a literal key directly.
pub fn resolve_api_key_opt(value: Option<&str>) -> Option<String> {
    value.and_then(|key| {
        if key.is_empty() {
            return None;
        }
        if looks_like_env_var(key) {
            Some(std::env::var(key).unwrap_or_else(|_| key.to_string()))
        } else {
            Some(key.to_string())
        }
    })
}

/// Resolve the API key for a `ProviderConfig` with auto-detection.
pub fn resolve_api_key(provider: &ProviderConfig) -> anyhow::Result<String> {
    resolve_api_key_opt(provider.api_key.as_deref())
        .ok_or_else(|| anyhow::anyhow!("No api_key configured for provider"))
}
