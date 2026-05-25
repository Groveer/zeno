//! Linux sandbox implementation using Bubblewrap (bwrap).
//!
//! Bubblewrap provides lightweight process isolation using Linux namespaces.
//! It creates a new mount namespace where the filesystem can be selectively
//! exposed (read-only or read-write).
//!
//! # Security Model
//!
//! - **WorkspaceWrite mode**: Full host filesystem is mounted read-only,
//!   working directory and /tmp are read-write. Network is available.
//! - **Strict mode**: Only explicitly allowed paths are mounted. Network disabled.
//!
//! # Fallback
//!
//! If `bwrap` is not installed, the sandbox gracefully falls back to
//! environment-variable-based restrictions (less secure but still useful).

use std::path::Path;

use super::config::{SandboxConfig, SandboxMode};
use super::{AccessCheck, Sandbox};

/// Check if bwrap is available on the system.
pub fn bwrap_available() -> bool {
    which::which("bwrap").is_ok()
}

/// Linux sandbox using Bubblewrap (bwrap).
pub struct BwrapSandbox {
    config: SandboxConfig,
}

impl BwrapSandbox {
    pub fn new(config: &SandboxConfig) -> Self {
        Self {
            config: config.clone(),
        }
    }
}

impl Sandbox for BwrapSandbox {
    fn mode(&self) -> SandboxMode {
        self.config.mode
    }

    fn wrap_command(&self, command: &str, cwd: &Path) -> Vec<String> {
        let mut args: Vec<String> = vec!["bwrap".into()];

        match self.config.mode {
            SandboxMode::None => return Vec::new(),
            SandboxMode::WorkspaceWrite => {
                // Mount root filesystem read-only
                args.extend(["--ro-bind".into(), "/".into(), "/".into()]);
                // Working directory read-write
                args.extend([
                    "--bind".into(),
                    cwd.to_string_lossy().to_string(),
                    cwd.to_string_lossy().to_string(),
                ]);
                // Extra writable paths
                for path in &self.config.writable_paths {
                    args.extend(["--bind".into(), path.clone(), path.clone()]);
                }
                // /tmp
                if self.config.tmp_access {
                    args.extend(["--tmpfs".into(), "/tmp".into()]);
                }
                // /dev
                if self.config.dev_access {
                    args.extend(["--dev".into(), "/dev".into()]);
                }
                // Network
                if !self.config.effective_network() {
                    args.extend(["--unshare-net".into()]);
                }
                // Proc for /proc/self, etc.
                args.extend(["--proc".into(), "/proc".into()]);
                // Ensure HOME, TERM, etc. are available
                args.extend(["--setenv".into(), "HOME".into(), home_dir()]);
                args.extend(["--die-with-parent".into()]);
            }
            SandboxMode::Strict => {
                // Minimal filesystem — only bind what's explicitly needed
                // Always need /usr, /lib, /lib64, /bin, /sbin for dynamic linking
                for sys_path in &["/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc"] {
                    if Path::new(sys_path).exists() {
                        args.extend([
                            "--ro-bind".into(),
                            sys_path.to_string(),
                            sys_path.to_string(),
                        ]);
                    }
                }
                // Working directory read-write
                args.extend([
                    "--bind".into(),
                    cwd.to_string_lossy().to_string(),
                    cwd.to_string_lossy().to_string(),
                ]);
                // Extra readable paths (read-only)
                for path in &self.config.readable_paths {
                    if Path::new(path).exists() {
                        args.extend(["--ro-bind".into(), path.clone(), path.clone()]);
                    }
                }
                // /tmp
                if self.config.tmp_access {
                    args.extend(["--tmpfs".into(), "/tmp".into()]);
                }
                // /dev
                if self.config.dev_access {
                    args.extend(["--dev".into(), "/dev".into()]);
                }
                // Always disable network in strict mode
                args.extend(["--unshare-net".into()]);
                args.extend(["--proc".into(), "/proc".into()]);
                args.extend(["--setenv".into(), "HOME".into(), home_dir()]);
                args.extend(["--die-with-parent".into()]);
            }
        }

        // The command itself is passed as remaining args (bash -c will handle it)
        args.extend(["--".into(), "bash".into(), "-c".into(), command.to_string()]);

        args
    }

    fn check_path_access(&self, path: &Path, write: bool) -> AccessCheck {
        match self.config.mode {
            SandboxMode::None => AccessCheck {
                allowed: true,
                reason: None,
            },
            SandboxMode::WorkspaceWrite => {
                if !write {
                    // Everything is readable in WorkspaceWrite mode
                    return AccessCheck {
                        allowed: true,
                        reason: None,
                    };
                }
                // Write access: cwd, writable_paths, /tmp
                if is_path_under(path, &self.config.writable_paths) {
                    return AccessCheck {
                        allowed: true,
                        reason: None,
                    };
                }
                if self.config.tmp_access && path.starts_with("/tmp") {
                    return AccessCheck {
                        allowed: true,
                        reason: None,
                    };
                }
                AccessCheck {
                    allowed: false,
                    reason: Some(format!(
                        "Path {} is read-only in WorkspaceWrite sandbox",
                        path.display()
                    )),
                }
            }
            SandboxMode::Strict => {
                // Read access: system paths + readable_paths
                let readable = self
                    .config
                    .readable_paths
                    .iter()
                    .chain(self.config.writable_paths.iter());
                if !write && is_path_under(path, &readable.cloned().collect::<Vec<_>>()) {
                    return AccessCheck {
                        allowed: true,
                        reason: None,
                    };
                }
                if write && is_path_under(path, &self.config.writable_paths) {
                    return AccessCheck {
                        allowed: true,
                        reason: None,
                    };
                }
                AccessCheck {
                    allowed: false,
                    reason: Some(format!(
                        "Path {} not allowed in Strict sandbox",
                        path.display()
                    )),
                }
            }
        }
    }

    fn is_network_allowed(&self) -> bool {
        self.config.effective_network()
    }

    fn env_vars(&self) -> Vec<(String, String)> {
        vec![
            ("ZENO_SANDBOX".into(), "bwrap".into()),
            (
                "ZENO_SANDBOX_MODE".into(),
                format!("{:?}", self.config.mode).to_lowercase(),
            ),
        ]
    }
}

/// Check if `path` is under any of the given `roots`.
#[allow(dead_code, reason = "used by check_path_access which is trait API")]
fn is_path_under(path: &Path, roots: &[String]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

/// Get the HOME directory, defaulting to "/" if not set.
fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bwrap_available() {
        // Just verify it doesn't panic
        let _ = bwrap_available();
    }

    #[test]
    fn test_workspace_write_check() {
        let config = SandboxConfig {
            mode: SandboxMode::WorkspaceWrite,
            writable_paths: vec!["/data".into()],
            ..Default::default()
        };
        let sandbox = BwrapSandbox::new(&config);

        // Read access is always allowed
        assert!(
            sandbox
                .check_path_access(Path::new("/etc/passwd"), false)
                .allowed
        );

        // Write to /tmp is allowed
        assert!(
            sandbox
                .check_path_access(Path::new("/tmp/test"), true)
                .allowed
        );

        // Write to /data is allowed
        assert!(
            sandbox
                .check_path_access(Path::new("/data/test"), true)
                .allowed
        );

        // Write to /etc is denied
        assert!(
            !sandbox
                .check_path_access(Path::new("/etc/passwd"), true)
                .allowed
        );
    }

    #[test]
    fn test_strict_check() {
        let config = SandboxConfig {
            mode: SandboxMode::Strict,
            writable_paths: vec!["/workspace".into()],
            readable_paths: vec!["/workspace".into(), "/data".into()],
            ..Default::default()
        };
        let sandbox = BwrapSandbox::new(&config);

        // Read /data is allowed (in readable_paths)
        assert!(
            sandbox
                .check_path_access(Path::new("/data/file"), false)
                .allowed
        );

        // Read /etc is denied (not in readable_paths)
        assert!(
            !sandbox
                .check_path_access(Path::new("/etc/passwd"), false)
                .allowed
        );

        // Write /workspace is allowed
        assert!(
            sandbox
                .check_path_access(Path::new("/workspace/file"), true)
                .allowed
        );

        // Network is disabled
        assert!(!sandbox.is_network_allowed());
    }

    #[test]
    fn test_wrap_command_workspace() {
        let config = SandboxConfig {
            mode: SandboxMode::WorkspaceWrite,
            ..Default::default()
        };
        let sandbox = BwrapSandbox::new(&config);
        let args = sandbox.wrap_command("ls -la", Path::new("/home/user/project"));

        assert_eq!(args[0], "bwrap");
        assert!(args.contains(&"--ro-bind".to_string()));
        assert!(args.contains(&"ls -la".to_string()));
        assert!(args.contains(&"--die-with-parent".to_string()));
        // Network should be allowed (no --unshare-net)
        assert!(!args.contains(&"--unshare-net".to_string()));
    }

    #[test]
    fn test_wrap_command_strict() {
        let config = SandboxConfig {
            mode: SandboxMode::Strict,
            ..Default::default()
        };
        let sandbox = BwrapSandbox::new(&config);
        let args = sandbox.wrap_command("echo hello", Path::new("/workspace"));

        assert!(args.contains(&"--unshare-net".to_string()));
        assert!(args.contains(&"echo hello".to_string()));
    }

    #[test]
    fn test_no_sandbox() {
        let sandbox = super::super::NoSandbox;
        assert_eq!(sandbox.mode(), SandboxMode::None);
        assert!(sandbox.is_network_allowed());
        assert!(
            sandbox
                .check_path_access(Path::new("/etc/passwd"), true)
                .allowed
        );
    }
}
