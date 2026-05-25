//! Sandbox system for secure command execution.
//!
//! Inspired by Codex's multi-platform sandbox architecture:
//! - `SandboxMode` controls the level of filesystem/network isolation
//! - `Sandbox` trait provides the execution wrapper interface
//! - Platform-specific implementations (Linux bwrap, macOS Seatbelt)
//!
//! # Design Principles
//!
//! - **Defense in depth**: Sandbox is an additional layer on top of permission checks
//! - **Graceful degradation**: If sandbox tool isn't available, fall back to env vars
//! - **Transparent to tools**: Tools don't need to know about sandboxing

pub mod config;
pub mod linux;

pub use config::SandboxConfig;
pub use config::SandboxMode;

use std::path::Path;

/// Result of a sandbox access check.
#[derive(Debug, Clone)]
#[allow(dead_code, reason = "part of Sandbox trait API, used in tests")]
pub struct AccessCheck {
    pub allowed: bool,
    pub reason: Option<String>,
}

/// The sandbox trait — platform-specific implementations wrap commands
/// with isolation primitives (bwrap, Seatbelt, etc.).
pub trait Sandbox: Send + Sync {
    /// The current sandbox mode.
    #[allow(dead_code, reason = "Sandbox trait API, used in tests")]
    fn mode(&self) -> SandboxMode;

    /// Wrap a command with sandbox isolation.
    /// Returns the modified command args that should be passed to the shell.
    ///
    /// For example, on Linux with bwrap:
    ///   `ls -la` → `bwrap --ro-bind / / --dev /dev --tmpfs /tmp ls -la`
    fn wrap_command(&self, command: &str, cwd: &Path) -> Vec<String>;

    /// Check if a filesystem path is accessible for the given operation.
    #[allow(dead_code, reason = "Sandbox trait API, used in tests")]
    fn check_path_access(&self, path: &Path, write: bool) -> AccessCheck;

    /// Check if network access is allowed.
    #[allow(dead_code, reason = "Sandbox trait API, used in tests")]
    fn is_network_allowed(&self) -> bool;

    /// Environment variables to set for sandboxed execution.
    #[allow(dead_code, reason = "Sandbox trait API, used in tests")]
    fn env_vars(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}

/// Create the best available sandbox for the current platform.
/// Returns a `Box<dyn Sandbox>` — Linux uses bwrap if available, else NoSandbox.
pub fn create_sandbox(config: &SandboxConfig) -> Box<dyn Sandbox> {
    if config.mode == SandboxMode::None {
        return Box::new(NoSandbox);
    }

    #[cfg(target_os = "linux")]
    {
        if linux::bwrap_available() {
            tracing::info!(mode = ?config.mode, "Creating bwrap sandbox");
            return Box::new(linux::BwrapSandbox::new(config));
        }
        tracing::warn!("bwrap not available, falling back to NoSandbox");
    }

    #[cfg(not(target_os = "linux"))]
    {
        tracing::warn!("No sandbox implementation for this platform, falling back to NoSandbox");
    }

    Box::new(NoSandbox)
}

/// No-op sandbox — commands run without isolation.
/// Used when no sandbox is available or mode is None.
pub struct NoSandbox;

impl Sandbox for NoSandbox {
    fn mode(&self) -> SandboxMode {
        SandboxMode::None
    }

    fn wrap_command(&self, _command: &str, _cwd: &Path) -> Vec<String> {
        Vec::new() // no wrapping
    }

    fn check_path_access(&self, _path: &Path, _write: bool) -> AccessCheck {
        AccessCheck {
            allowed: true,
            reason: None,
        }
    }

    fn is_network_allowed(&self) -> bool {
        true
    }
}
