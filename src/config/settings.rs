use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::paths;

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
    pub mcp: McpConfig,
    pub permissions: PermissionMode,
    pub max_turns: u32,
    pub max_tokens: u32,
    pub theme: String,
    pub plugins: PluginConfig,
    pub memory: MemoryConfig,
    pub auxiliary: AuxiliaryConfig,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            providers: HashMap::new(),
            active_provider: "anthropic".into(),
            model: "claude-sonnet-4-20250514".into(),
            tools: ToolsConfig::default(),
            mcp: McpConfig::default(),
            permissions: PermissionMode::Ask,
            max_turns: 8,
            max_tokens: 4096,
            theme: "default".into(),
            plugins: PluginConfig::default(),
            memory: MemoryConfig::default(),
            auxiliary: AuxiliaryConfig::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub api_key_env: Option<String>,
    pub api_key: Option<String>,
    #[serde(default = "default_anthropic_url")]
    pub base_url: String,
    #[serde(default)]
    pub default_model: String,
}

fn default_anthropic_url() -> String {
    "https://api.anthropic.com".into()
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ToolsConfig {
    pub bash: bool,
    pub file_read: bool,
    pub file_write: bool,
    pub file_edit: bool,
    pub glob: bool,
    pub grep: bool,
    pub web_search: bool,
    pub web_fetch: bool,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            bash: true,
            file_read: true,
            file_write: true,
            file_edit: true,
            glob: true,
            grep: true,
            web_search: true,
            web_fetch: false,
        }
    }
}

// ---------------------------------------------------------------------------
// MCP
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct McpConfig {
    pub servers: HashMap<String, McpServerConfig>,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            servers: HashMap::new(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct McpServerConfig {
    pub command: Option<Vec<String>>,
    pub url: Option<String>,
}

// ---------------------------------------------------------------------------
// Permissions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    Allow,
    Deny,
    Ask,
}

impl Default for PermissionMode {
    fn default() -> Self {
        Self::Ask
    }
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
            dir: "~/.config/rcode/plugins".into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct MemoryConfig {
    pub dir: String,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            dir: ".rcode/memory".into(),
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
}

impl Default for AuxiliaryConfig {
    fn default() -> Self {
        Self {
            compression: AuxiliaryTaskConfig::default_with_timeout(30.0),
            vision: AuxiliaryTaskConfig::default_with_timeout(30.0),
            web_extract: AuxiliaryTaskConfig::default_with_timeout(60.0),
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

/// Load settings from `~/.config/rcode/config.yaml`, or return defaults.
pub fn load() -> anyhow::Result<Settings> {
    let path = paths::config_path();
    if !path.exists() {
        tracing::info!("No config file at {}, using defaults", path.display());
        return Ok(Settings::default());
    }
    let content = std::fs::read_to_string(&path)?;
    let settings: Settings = serde_yaml::from_str(&content)?;
    Ok(settings)
}

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

/// Save settings to the config file.
pub fn save(settings: &Settings) -> anyhow::Result<()> {
    paths::ensure_config_dir()?;
    let path = paths::config_path();
    let content = serde_yaml::to_string(settings)?;
    std::fs::write(&path, content)?;
    Ok(())
}

/// Get the effective model for a given provider.
pub fn effective_model(settings: &Settings, provider_name: &str) -> Option<String> {
    settings
        .providers
        .get(provider_name)
        .map(|p| {
            if p.default_model.is_empty() {
                settings.model.clone()
            } else {
                p.default_model.clone()
            }
        })
}
