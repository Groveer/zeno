//! Sandbox configuration types.

use serde::{Deserialize, Serialize};

/// Sandbox isolation mode.
///
/// Controls the level of filesystem and network isolation applied
/// to command execution. Higher modes are more restrictive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    /// No sandbox — commands run with full host access.
    None,
    /// Workspace write — commands can write to the working directory
    /// and temp dirs, but the rest of the filesystem is read-only.
    /// Network access is allowed.
    WorkspaceWrite,
    /// Strict — commands can only access explicitly allowed paths.
    /// Network is disabled. Most restrictive mode.
    Strict,
}

impl Default for SandboxMode {
    fn default() -> Self {
        Self::None
    }
}

/// Configuration for the sandbox system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    /// The sandbox mode to use.
    pub mode: SandboxMode,
    /// Extra paths that should be writable (in addition to cwd in WorkspaceWrite mode).
    #[serde(default)]
    pub writable_paths: Vec<String>,
    /// Extra paths that should be readable (in Strict mode, only these + system paths).
    #[serde(default)]
    pub readable_paths: Vec<String>,
    /// Whether to allow network access in sandboxed mode.
    /// Default: true for WorkspaceWrite, false for Strict.
    pub network: Option<bool>,
    /// Whether to allow /tmp access. Default: true.
    #[serde(default = "default_true")]
    pub tmp_access: bool,
    /// Whether to allow /dev access (for /dev/null, /dev/urandom, etc.).
    #[serde(default = "default_true")]
    pub dev_access: bool,
}

fn default_true() -> bool {
    true
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            mode: SandboxMode::None,
            writable_paths: Vec::new(),
            readable_paths: Vec::new(),
            network: None,
            tmp_access: true,
            dev_access: true,
        }
    }
}

impl SandboxConfig {
    /// Check if network should be allowed for the current mode.
    pub fn effective_network(&self) -> bool {
        self.network.unwrap_or(match self.mode {
            SandboxMode::None => true,
            SandboxMode::WorkspaceWrite => true,
            SandboxMode::Strict => false,
        })
    }
}
