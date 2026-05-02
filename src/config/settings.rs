use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

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
    pub plugins: PluginConfig,
    pub memory: MemoryConfig,
    pub auxiliary: AuxiliaryConfig,
    pub llm: LlmConfig,
    pub log_retention_days: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            providers: HashMap::new(),
            active_provider: "anthropic".into(),
            model: "claude-sonnet-4-20250514".into(),
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
            plugins: PluginConfig::default(),
            memory: MemoryConfig::default(),
            auxiliary: AuxiliaryConfig::default(),
            llm: LlmConfig::default(),
            log_retention_days: 3,
        }
    }
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ProviderConfig {
    #[serde(default)]
    pub api_key_env: Option<String>,
    pub api_key: Option<String>,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub default_model: String,
    /// Optional: max output tokens per request.
    /// If set, overrides the top-level `max_tokens` setting.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
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
    pub read: bool,
    pub write: bool,
    pub edit: bool,
    pub glob: bool,
    pub grep: bool,
    pub web_search: bool,
    pub web_fetch: bool,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            bash: true,
            use_rtk: true,
            bash_env: HashMap::new(),
            read: true,
            write: true,
            edit: true,
            glob: true,
            grep: true,
            web_search: true,
            web_fetch: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Role (identity/persona)
// ---------------------------------------------------------------------------

/// Customizable role sections for the system prompt.
/// All fields are optional — `None` means use the built-in default text.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct RoleConfig {
    /// Core identity and role declaration (replaces the default "You are zeno..." block).
    pub identity: Option<String>,
    /// Guidelines section (replaces the default guidelines block).
    pub guidelines: Option<String>,
}

impl Default for RoleConfig {
    fn default() -> Self {
        Self {
            identity: None,
            guidelines: None,
        }
    }
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
    /// Environment variable name containing the API key.
    pub api_key_env: Option<String>,
    /// Direct API key (not recommended, prefer api_key_env).
    pub api_key: Option<String>,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            provider: "searxng".into(),
            url: String::new(), // empty = use provider default
            api_key_env: None,
            api_key: None,
        }
    }
}

impl WebSearchConfig {
    /// Resolve the API key: prefer explicit key, then env var.
    pub fn resolve_api_key(&self) -> Option<String> {
        if let Some(ref key) = self.api_key {
            if !key.is_empty() {
                return Some(key.clone());
            }
        }
        if let Some(ref env) = self.api_key_env {
            return std::env::var(env).ok();
        }
        None
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
// Plugins
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct PluginConfig {
    pub dir: String,
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            dir: "~/.config/zeno/plugins".into(),
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
    /// auto-compaction triggers.  0.33 = compact when total tokens
    /// exceed 33% of the context window.  Set to 0.0 to disable.
    pub compact_threshold: f64,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            compact_threshold: 0.33,
        }
    }
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

/// Config for a single Lua memory provider entry.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
#[derive(Default)]
pub struct MemoryProviderEntry {
    /// Path to the Lua script (relative to config dir), or inline script source.
    pub script: String,
    /// Whether the script is inline (true) or a file path (false).
    #[serde(default)]
    pub inline: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// Character limit for MEMORY.md (agent notes). Default: 4000.
    pub memory_char_limit: usize,
    /// Character limit for USER.md (user profile). Default: 2500.
    pub user_char_limit: usize,
    /// Name of the active external memory provider (e.g. "mem0", "honcho").
    /// Empty string means no external provider (built-in only).
    /// Configured via `zn.memory_provider("name", {...})` in init.lua.
    pub provider: String,
    /// Registered memory provider configs (name → config).
    /// Populated by `zn.memory_provider()` calls in init.lua.
    #[serde(default)]
    pub providers: HashMap<String, MemoryProviderEntry>,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            memory_char_limit: 4000,
            user_char_limit: 2500,
            provider: String::new(),
            providers: HashMap::new(),
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
    pub web_extract: AuxiliaryTaskConfig,
    pub title_generation: AuxiliaryTaskConfig,
    pub session_search: AuxiliaryTaskConfig,
}

impl Default for AuxiliaryConfig {
    fn default() -> Self {
        Self {
            compression: AuxiliaryTaskConfig::default_with_timeout(30.0),
            vision: AuxiliaryTaskConfig::default_with_timeout(30.0),
            web_extract: AuxiliaryTaskConfig::default_with_timeout(60.0),
            title_generation: AuxiliaryTaskConfig::default_with_timeout(30.0),
            session_search: AuxiliaryTaskConfig::default_with_timeout(30.0),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AuxiliaryTaskConfig {
    #[serde(default = "default_auto")]
    pub provider: String,
    #[serde(default)]
    pub model: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub timeout: f64,
    /// Per-task extra body fields (provider-specific request parameters).
    /// E.g. `{"enable_thinking": false}` for providers that support it.
    #[serde(default)]
    pub extra_body: HashMap<String, serde_json::Value>,
    /// Maximum output tokens for this task. 0 = use default (4096).
    #[serde(default)]
    pub max_tokens: u32,
    /// Temperature override for this task. None = use task-specific default.
    pub temperature: Option<f64>,
}

fn default_auto() -> String {
    "auto".into()
}

impl Default for AuxiliaryTaskConfig {
    fn default() -> Self {
        Self {
            provider: "auto".into(),
            model: String::new(),
            base_url: None,
            api_key: None,
            timeout: 30.0,
            extra_body: HashMap::new(),
            max_tokens: 0,
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

/// Resolve the API key for a provider: prefer explicit `api_key`, then env var.
pub fn resolve_api_key(provider: &ProviderConfig) -> anyhow::Result<String> {
    if let Some(ref key) = provider.api_key {
        return Ok(key.clone());
    }
    if let Some(ref env_var) = provider.api_key_env {
        std::env::var(env_var)
            .map_err(|_| anyhow::anyhow!("Environment variable {} not set", env_var))
    } else {
        anyhow::bail!("No api_key or api_key_env configured for provider")
    }
}

/// Get the effective model for a given provider.
#[allow(dead_code, reason = "reserved for future provider-switching UI")]
pub fn effective_model(settings: &Settings, provider_name: &str) -> Option<String> {
    settings.providers.get(provider_name).map(|p| {
        if p.default_model.is_empty() {
            settings.model.clone()
        } else {
            p.default_model.clone()
        }
    })
}
