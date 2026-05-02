//! Permission checker — controls tool execution authorization.
//!
//! Supports fine-grained permission decisions based on:
//! - Tool read-only status
//! - File path context
//! - Command context (for bash)
//!
//! All decisions are logged with structured fields for audit:
//! `tool_name`, `permission_decision`, `reason`, `mode`, `file_path`, `command`

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::settings::PermissionMode;
use crate::tools::base::ToolError;

// ---------------------------------------------------------------------------
// Permission Decision
// ---------------------------------------------------------------------------

/// Permission decision returned by the evaluator.
pub struct PermissionDecision {
    pub allowed: bool,
    pub requires_confirmation: bool,
    pub reason: String,
}

// ---------------------------------------------------------------------------
// Path/command resolution
// ---------------------------------------------------------------------------

pub struct ResolvedPaths {
    pub file_path: Option<PathBuf>,
    pub command: Option<String>,
}

pub fn resolve_paths(tool_name: &str, tool_input: &Value, cwd: &Path) -> ResolvedPaths {
    let mut file_path = None;
    let mut command = None;

    if let Some(obj) = tool_input.as_object() {
        // Check common path field names
        for key in &["path", "file_path", "root"] {
            if let Some(path_str) = obj.get(*key).and_then(|v| v.as_str())
                && !path_str.is_empty()
            {
                let p = PathBuf::from(path_str);
                let resolved = if p.is_absolute() { p } else { cwd.join(p) };
                // Canonicalize to resolve symlinks and `..` components.
                // This prevents symlink-based sandbox escapes (e.g. ln -s /etc/shadow link).
                // For non-existent paths (e.g. write creating new files),
                // canonicalize as far as we can (the parent directory).
                file_path = Some(canonicalize_safe(&resolved));
                break;
            }
        }
    }

    if tool_name == "bash" {
        command = extract_command(tool_input);
    }

    ResolvedPaths { file_path, command }
}

/// Best-effort canonicalization: resolves symlinks and `..` in the path.
/// If the path itself exists, returns its canonical form.
/// If not, tries to canonicalize the parent directory and appends the file name.
/// Falls back to the original path if canonicalization fails entirely.
fn canonicalize_safe(path: &Path) -> PathBuf {
    if let Ok(canon) = std::fs::canonicalize(path) {
        return canon;
    }
    // Path doesn't exist yet (e.g. write creating new file).
    // Canonicalize as much of the path as possible: try the parent dir.
    if let Some(parent) = path.parent()
        && let Ok(canon_parent) = std::fs::canonicalize(parent)
        && let Some(file_name) = path.file_name()
    {
        return canon_parent.join(file_name);
    }
    // Fallback: return the path as-is (symlink not resolved)
    path.to_path_buf()
}

/// Truncate a string to at most `max_chars` characters (not bytes).
/// This is safe for multi-byte UTF-8 (CJK, emoji, etc.).
#[allow(dead_code)]
fn safe_truncate_str(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars).collect();
        format!("{}...(truncated)", truncated)
    }
}

fn extract_command(tool_input: &Value) -> Option<String> {
    tool_input
        .as_object()
        .and_then(|o| o.get("command").and_then(|v| v.as_str()))
        .map(|s| s.to_string())
}

// ---------------------------------------------------------------------------
// Evaluate permission
// ---------------------------------------------------------------------------

/// Evaluate whether a tool execution is permitted.
///
/// Logic:
/// - Allow mode: all tools allowed
/// - Deny mode: only read-only tools allowed
/// - Ask mode: read-only auto-allowed, others need confirmation
pub fn evaluate_permission(
    mode: &PermissionMode,
    trusted_paths: &[String],
    tool_name: &str,
    is_read_only: bool,
    file_path: Option<&Path>,
    command: Option<&str>,
    cwd: &Path,
) -> PermissionDecision {
    // 0. Trusted path check — bypasses all other checks
    // If the file path falls under any configured trusted path, allow it
    // unconditionally. This lets users declare directories like
    // "/home/user/Develop/" where all file operations are trusted.
    if let Some(path) = file_path {
        if trusted_paths
            .iter()
            .any(|trusted| path.starts_with(trusted))
        {
            tracing::debug!(
                tool_name = %tool_name,
                permission_decision = "allowed",
                reason = "trusted_path",
                file_path = %path.display(),
                "Permission allowed by trusted path"
            );
            return PermissionDecision {
                allowed: true,
                requires_confirmation: false,
                reason: format!("Path is under trusted path"),
            };
        }
    }

    // 1. Path-based boundary check (Sandbox)
    // Protects against files outside CWD in Ask and Deny modes.
    // In Allow mode, the user has explicitly opted into full trust, so we skip
    // this check — it would interfere with legitimate cross-project access
    // within a trusted directory tree (e.g. /home/user/Develop/).
    // Canonicalize cwd for accurate comparison (paths already canonicalized in resolve_paths).
    // Dual check: also compare against the non-canonicalized cwd to handle the
    // case where cwd itself is a symlink (e.g. /home/user/proj → /data/proj).
    // Without the fallback, a user-visible path like /home/user/proj/src/main.rs
    // would fail the starts_with check against the canonical /data/proj.
    if *mode != PermissionMode::Allow {
        if let Some(path) = file_path {
            let canon_cwd = canonicalize_safe(cwd);
            if !path.starts_with(&canon_cwd) && !path.starts_with(cwd) {
                let decision = PermissionDecision {
                    allowed: false,
                    requires_confirmation: true,
                    reason: format!(
                        "Path '{}' is outside the current working directory ({})",
                        path.display(),
                        canon_cwd.display()
                    ),
                };
                tracing::warn!(
                    tool_name = %tool_name,
                    permission_decision = "denied",
                    reason = %decision.reason,
                    mode = %format!("{:?}", mode),
                    file_path = %path.display(),
                    "Permission denied: path outside CWD"
                );
                return decision;
            }
        }
    }

    // 2. Command-based heuristic for bash
    if tool_name == "bash"
        && let Some(cmd) = command
    {
        // Check for path traversal patterns that could escape the CWD.
        // Use patterns with path separators to avoid false positives like
        // `git log HEAD~5..HEAD` or `diff a..b`.
        let has_path_traversal = cmd.contains("../")
            || cmd.contains("/..")
            || cmd.ends_with("..")
            || cmd.contains("..\\"); // backslash-escaped traversal

        // Check for shell variable/command expansion that could bypass
        // literal traversal checks (e.g. $HOME/../etc, $(cat /etc/passwd)).
        let has_shell_expansion = cmd.contains('$') || cmd.contains('`');

        // Check for absolute paths targeting sensitive system areas.
        // More comprehensive than just /home — covers /etc, /root, /var, /proc, etc.
        let sensitive_prefixes = [
            "/etc/",
            "/root/",
            "/var/",
            "/proc/",
            "/sys/",
            "/boot/",
            "/usr/local/etc/",
            "/tmp/",
            "/opt/",
        ];
        let has_sensitive_path = sensitive_prefixes.iter().any(|prefix| cmd.contains(prefix));

        // Check for home directories of other users
        let home_prefix = if cfg!(target_os = "macos") {
            "/Users"
        } else {
            "/home"
        };
        let contains_other_home =
            cmd.contains(home_prefix) && !cmd.contains(&cwd.to_string_lossy().to_string());

        // Combine all external-access indicators.
        // If any of these are true, the command may access files outside the
        // current working directory and should not be auto-allowed.
        let has_external_access =
            has_path_traversal || has_shell_expansion || has_sensitive_path || contains_other_home;

        // Trusted path check for bash: if CWD is under a trusted path AND the
        // command shows no signs of accessing files outside the trusted zone,
        // bypass the Ask-mode confirmation.
        // This allows running commands like `cargo build` in trusted directories
        // without being prompted every time, while still catching commands like
        // `ls /home/guo/.config/` or `cat /etc/passwd`.
        let cwd_is_trusted = trusted_paths.iter().any(|trusted| cwd.starts_with(trusted));

        if cwd_is_trusted && !has_external_access {
            tracing::debug!(
                tool_name = %tool_name,
                permission_decision = "allowed",
                reason = "trusted_path_cwd",
                command = %cmd,
                cwd = %cwd.display(),
                "Bash command allowed by trusted CWD"
            );
            return PermissionDecision {
                allowed: true,
                requires_confirmation: false,
                reason: "CWD is under trusted path".to_string(),
            };
        }

        if has_external_access {
            let decision = PermissionDecision {
                allowed: false,
                requires_confirmation: true,
                reason: "Bash command may access paths outside the current working directory"
                    .to_string(),
            };
            tracing::warn!(
                tool_name = "bash",
                permission_decision = "denied",
                reason = %decision.reason,
                mode = %format!("{:?}", mode),
                command = %cmd,
                has_path_traversal = has_path_traversal,
                has_shell_expansion = has_shell_expansion,
                has_sensitive_path = has_sensitive_path,
                "Permission denied: bash command safety check failed"
            );
            return decision;
        }
    }

    match mode {
        PermissionMode::Allow => {
            tracing::debug!(
                tool_name = %tool_name,
                permission_decision = "allowed",
                mode = "allow",
                is_read_only = is_read_only,
                "Permission allowed by mode"
            );
            PermissionDecision {
                allowed: true,
                requires_confirmation: false,
                reason: "Mode set to Allow".to_string(),
            }
        }
        PermissionMode::Deny => {
            if is_read_only {
                tracing::debug!(
                    tool_name = %tool_name,
                    permission_decision = "allowed",
                    mode = "deny",
                    is_read_only = true,
                    "Read-only tool allowed in Deny mode"
                );
                PermissionDecision {
                    allowed: true,
                    requires_confirmation: false,
                    reason: "Read-only tool allowed in Deny mode".to_string(),
                }
            } else {
                tracing::warn!(
                    tool_name = %tool_name,
                    permission_decision = "denied",
                    mode = "deny",
                    is_read_only = false,
                    "Tool blocked in Deny mode"
                );
                PermissionDecision {
                    allowed: false,
                    requires_confirmation: false,
                    reason: format!("Tool '{}' is blocked in Deny mode", tool_name),
                }
            }
        }
        PermissionMode::Ask => {
            if is_read_only {
                tracing::debug!(
                    tool_name = %tool_name,
                    permission_decision = "allowed",
                    mode = "ask",
                    is_read_only = true,
                    "Read-only tool auto-allowed in Ask mode"
                );
                PermissionDecision {
                    allowed: true,
                    requires_confirmation: false,
                    reason: "Read-only tool automatically allowed".to_string(),
                }
            } else {
                let mut reason = format!("Confirmation required for tool '{}'", tool_name);
                if let Some(p) = file_path {
                    reason.push_str(&format!(" on path: {}", p.display()));
                }
                if let Some(c) = command {
                    reason.push_str(&format!(" executing command: {}", c));
                }
                tracing::info!(
                    tool_name = %tool_name,
                    permission_decision = "requires_confirmation",
                    mode = "ask",
                    is_read_only = false,
                    file_path = file_path.map(|p| p.display().to_string()).unwrap_or_default(),
                    command = command.unwrap_or(""),
                    "Permission requires user confirmation"
                );
                PermissionDecision {
                    allowed: false,
                    requires_confirmation: true,
                    reason,
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Fine-grained check (uses evaluate_permission)
// ---------------------------------------------------------------------------

/// Fine-grained permission check with tool input analysis.
#[allow(dead_code)]
pub fn check_permission_fine(
    mode: &PermissionMode,
    tool_name: &str,
    is_read_only: bool,
    tool_input: &Value,
    cwd: &Path,
) -> Result<bool, ToolError> {
    let paths = resolve_paths(tool_name, tool_input, cwd);
    let decision = evaluate_permission(
        mode,
        &[], // no trusted_paths in this legacy path
        tool_name,
        is_read_only,
        paths.file_path.as_deref(),
        paths.command.as_deref(),
        cwd,
    );

    if decision.allowed {
        return Ok(true);
    }
    if !decision.requires_confirmation {
        return Err(ToolError::PermissionDenied(decision.reason));
    }

    // Need user confirmation
    prompt_user(tool_name, &decision.reason, &tool_input.to_string())
}

// ---------------------------------------------------------------------------
// Legacy check_permission (backward compatible)
// ---------------------------------------------------------------------------

/// Legacy permission check: simple mode-based check.
/// For Ask mode, always prompts the user.
#[allow(dead_code)]
pub fn check_permission(
    mode: &PermissionMode,
    tool_name: &str,
    description: &str,
    tool_input: &str,
) -> Result<bool, ToolError> {
    match mode {
        PermissionMode::Allow => Ok(true),
        PermissionMode::Deny => {
            if is_safe_tool(tool_name) {
                Ok(true)
            } else {
                Err(ToolError::PermissionDenied(format!(
                    "Tool '{}' is blocked in deny mode",
                    tool_name
                )))
            }
        }
        PermissionMode::Ask => prompt_user(tool_name, description, tool_input),
    }
}

/// Tools that are always safe to execute (read-only, no side effects).
#[allow(dead_code)]
fn is_safe_tool(name: &str) -> bool {
    matches!(
        name,
        "read" | "glob" | "grep" | "config" | "ask_user" | "web_search" | "web_fetch"
    )
}

// ---------------------------------------------------------------------------
// User prompt
// ---------------------------------------------------------------------------

/// Prompt the user for permission in interactive mode.
#[allow(dead_code)]
pub(crate) fn prompt_user(
    tool_name: &str,
    description: &str,
    tool_input: &str,
) -> Result<bool, ToolError> {
    eprintln!();
    eprintln!("[permission] Tool: {}", tool_name);
    eprintln!("[permission] {}", description);

    // Truncate input display — use character-level slicing to avoid panicking
    // on multi-byte UTF-8 characters (e.g. Chinese, emoji).
    let display_input = safe_truncate_str(tool_input, 200);
    eprintln!("[permission] Input: {}", display_input);
    eprint!("[permission] Allow? (y/n/a = yes to all): ");
    io::stderr().flush()?;

    let mut response = String::new();
    io::stdin()
        .read_line(&mut response)
        .map_err(ToolError::Io)?;
    let response = response.trim().to_lowercase();

    match response.as_str() {
        "y" | "yes" => Ok(true),
        "n" | "no" => Ok(false),
        "a" | "all" | "always" => Ok(true),
        _ => Ok(false),
    }
}
