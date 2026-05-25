//! Permission checker — controls tool execution authorization.
//!
//! Supports fine-grained permission decisions based on:
//! - Tool read-only status
//! - File path context
//! - Command context (for bash)
//!
//! **Relaxed Ask mode** (default):
//! - Auto-allow: read-only tools, file ops in CWD, safe bash in CWD, temp dirs
//! - Confirm: destructive commands (rm, sudo, dd, …), ops outside CWD/temp
//!
//! All decisions are logged with structured fields for audit:
//! `tool_name`, `permission_decision`, `reason`, `mode`, `file_path`, `command`

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::permissions::execpolicy::ExecPolicy;

use crate::config::settings::PermissionMode;

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

/// Write/edit tools: these create or overwrite file content, which is
/// irreversible unless version control is available.
const WRITE_TOOLS: &[&str] = &["write", "edit"];

/// Check if a directory is inside a git repository.
/// Walks up the directory tree looking for a `.git` entry (file or directory).
/// Handles worktrees (`.git` is a file) and bare repos.
fn is_inside_git_repo(path: &Path) -> bool {
    let check = |dir: &Path| {
        let git_path = dir.join(".git");
        git_path.exists()
    };

    // Check the path itself (if it's a directory)
    if path.is_dir() && check(path) {
        return true;
    }
    // Check parent directories
    let mut current = path.parent();
    while let Some(dir) = current {
        if check(dir) {
            return true;
        }
        current = dir.parent();
    }
    false
}
const BUILTIN_SAFE_PATH_PREFIXES: &[&str] = &[
    "/tmp/",
    "/tmp", // exact match
    "/var/tmp/",
    "/var/tmp", // exact match
];

/// Check if a path is in a system temp directory or user-configured safe paths.
fn is_in_tmp_dir(path: &Path, extra_safe_paths: &[String]) -> bool {
    BUILTIN_SAFE_PATH_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix))
        || extra_safe_paths.iter().any(|p| path.starts_with(p))
}

/// Check if a path is in the "safe zone" — CWD or a system temp directory.
fn is_in_safe_zone(path: &Path, cwd: &Path, extra_safe_paths: &[String]) -> bool {
    // CWD check (both canonical and raw)
    let canon_cwd = canonicalize_safe(cwd);
    if path.starts_with(&canon_cwd) || path.starts_with(cwd) {
        return true;
    }
    is_in_tmp_dir(path, extra_safe_paths)
}

/// Recursively extract sub-commands from shell command substitution constructs.
///
/// Handles:
/// - `$(...)` — POSIX command substitution (nested parens handled correctly)
/// - `` `...` `` — backtick command substitution
///
/// Pure variable references (`$VAR`, `${VAR}`) are NOT extracted.
/// Recursively extracts from nested substitutions.
fn extract_subshell_commands(cmd: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut i = 0;
    let chars: Vec<char> = cmd.chars().collect();
    let len = chars.len();

    while i < len {
        // `$(...)` — dollar-paren command substitution (handles nested parens)
        if i + 1 < len && chars[i] == '$' && chars[i + 1] == '(' {
            let mut depth = 1;
            let start = i + 2;
            let mut j = start;
            while j < len && depth > 0 {
                if chars[j] == '(' {
                    depth += 1;
                } else if chars[j] == ')' {
                    depth -= 1;
                }
                j += 1;
            }
            if depth == 0 {
                let inner = chars[start..j - 1].iter().collect::<String>();
                result.push(inner.clone());
                // Recursively extract nested subshells from the inner command
                result.extend(extract_subshell_commands(&inner));
            }
            i = j;
            continue;
        }

        // `...` — backtick command substitution
        if chars[i] == '`' {
            let start = i + 1;
            let mut j = start;
            while j < len && chars[j] != '`' {
                if chars[j] == '\\' && j + 1 < len {
                    j += 2;
                } else {
                    j += 1;
                }
            }
            if j < len {
                let inner = chars[start..j].iter().collect::<String>();
                result.push(inner.clone());
                // Recursively extract nested subshells from the inner command
                result.extend(extract_subshell_commands(&inner));
                i = j + 1;
            } else {
                i = j + 1;
            }
            continue;
        }

        i += 1;
    }

    result
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
/// **Relaxed Ask mode** philosophy:
/// - Auto-allow harmless operations (read-only, file ops in CWD, safe bash)
/// - Only ask for potentially destructive or boundary-crossing operations
///
/// Logic:
/// - Denied commands are always blocked, regardless of mode.
/// - Allow mode: all other tools allowed
/// - Deny mode: only read-only tools allowed
/// - Ask mode: read-only auto-allowed; writes in CWD/temp auto-allowed;
///   ask commands and destructive commands require confirmation
pub fn evaluate_permission(
    mode: &PermissionMode,
    trusted_paths: &[String],
    tool_name: &str,
    is_read_only: bool,
    file_path: Option<&Path>,
    command: Option<&str>,
    cwd: &Path,
    safe_paths: &[String],
    exec_policy: Option<&ExecPolicy>,
) -> PermissionDecision {
    // 0. Trusted path check — bypasses all other checks
    if let Some(path) = file_path
        && trusted_paths
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
            reason: "Path is under trusted path".to_string(),
        };
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
            // --- Read-only: always auto-allow ---
            if is_read_only {
                tracing::debug!(
                    tool_name = %tool_name,
                    permission_decision = "allowed",
                    mode = "ask",
                    is_read_only = true,
                    "Read-only tool auto-allowed in Ask mode"
                );
                return PermissionDecision {
                    allowed: true,
                    requires_confirmation: false,
                    reason: "Read-only tool automatically allowed".to_string(),
                };
            }

            // --- Bash commands: security check + ExecPolicy ---
            if tool_name == "bash"
                && let Some(cmd) = command
            {
                // ── Security layer: shell expansion / path traversal ──────────
                // These are semantic analyses that simple pattern matching can't replace.
                // ExecPolicy prefix matching would miss `ls $(rm -rf /)`, so we check
                // shell expansion first, then delegate to ExecPolicy for the sub-commands.
                let has_shell_expansion = cmd.contains('$') || cmd.contains('`');
                let has_path_traversal = cmd.contains("../")
                    || cmd.contains("/..")
                    || cmd.ends_with("..")
                    || cmd.contains("..\\");

                // Path traversal: requires confirmation (can't safely resolve statically)
                if has_path_traversal {
                    tracing::info!(
                        tool_name = "bash",
                        permission_decision = "requires_confirmation",
                        mode = "ask",
                        has_path_traversal = has_path_traversal,
                        command = %cmd,
                        "Command with path traversal requires confirmation"
                    );
                    return PermissionDecision {
                        allowed: false,
                        requires_confirmation: true,
                        reason: "Command contains path traversal".to_string(),
                    };
                }

                // Shell expansion: extract and check $(...) / `...` sub-commands
                if has_shell_expansion {
                    let subcommands = extract_subshell_commands(cmd);
                    for sub in &subcommands {
                        let sub_trimmed = sub.trim();
                        if sub_trimmed.is_empty() {
                            continue;
                        }

                        // Check sub-command against ExecPolicy first
                        if let Some(policy) = exec_policy
                            && let Some(decision) = policy.evaluate(sub_trimmed)
                        {
                            match decision.action {
                                // Deny in a subshell → Ask for confirmation (can't be
                                // 100% sure the sub-command actually runs)
                                crate::permissions::execpolicy::PolicyAction::Deny
                                | crate::permissions::execpolicy::PolicyAction::Ask => {
                                    tracing::info!(
                                        tool_name = "bash",
                                        permission_decision = "requires_confirmation",
                                        reason = "destructive_in_subshell",
                                        command = %cmd,
                                        subshell_cmd = %sub_trimmed,
                                        "Subshell sub-command is destructive"
                                    );
                                    return PermissionDecision {
                                        allowed: false,
                                        requires_confirmation: true,
                                        reason: format!(
                                            "Subshell sub-command '{}' is destructive",
                                            sub_trimmed.chars().take(60).collect::<String>()
                                        ),
                                    };
                                }
                                // Auto → sub-command is safe, continue checking
                                crate::permissions::execpolicy::PolicyAction::Auto => {}
                            }
                        }

                        // Check sensitive paths in the sub-command
                        let sensitive_prefixes = [
                            "/etc/",
                            "/root/",
                            "/var/",
                            "/proc/",
                            "/sys/",
                            "/boot/",
                            "/usr/local/etc/",
                            "/opt/",
                        ];
                        let has_sensitive_path = sensitive_prefixes
                            .iter()
                            .any(|prefix| sub_trimmed.contains(prefix));
                        let home_prefix = if cfg!(target_os = "macos") {
                            "/Users"
                        } else {
                            "/home"
                        };
                        let contains_other_home = sub_trimmed.contains(home_prefix)
                            && !sub_trimmed.contains(&cwd.to_string_lossy().to_string());

                        if has_sensitive_path || contains_other_home {
                            tracing::info!(
                                tool_name = "bash",
                                permission_decision = "requires_confirmation",
                                mode = "ask",
                                reason = "sensitive_path_in_subshell",
                                command = %cmd,
                                subshell_cmd = %sub_trimmed,
                                "Subshell sub-command accesses sensitive paths"
                            );
                            return PermissionDecision {
                                allowed: false,
                                requires_confirmation: true,
                                reason: format!(
                                    "Subshell sub-command accesses sensitive paths: {}",
                                    sub_trimmed.chars().take(60).collect::<String>()
                                ),
                            };
                        }

                        // Check path traversal in the sub-command
                        if sub_trimmed.contains("../")
                            || sub_trimmed.contains("/..")
                            || sub_trimmed.ends_with("..")
                            || sub_trimmed.contains("..\\")
                        {
                            tracing::info!(
                                tool_name = "bash",
                                permission_decision = "requires_confirmation",
                                mode = "ask",
                                reason = "path_traversal_in_subshell",
                                command = %cmd,
                                subshell_cmd = %sub_trimmed,
                                "Subshell sub-command has path traversal"
                            );
                            return PermissionDecision {
                                allowed: false,
                                requires_confirmation: true,
                                reason: format!(
                                    "Subshell sub-command contains path traversal: {}",
                                    sub_trimmed.chars().take(60).collect::<String>()
                                ),
                            };
                        }
                    }

                    // All sub-commands are safe
                    tracing::debug!(
                        tool_name = "bash",
                        permission_decision = "allowed_after_subshell_check",
                        mode = "ask",
                        command = %cmd,
                        subcommands = ?subcommands,
                        "All subshell sub-commands are safe, continuing to check main command"
                    );
                }

                // Check if command accesses sensitive paths outside safe zone
                let sensitive_prefixes = [
                    "/etc/",
                    "/root/",
                    "/var/",
                    "/proc/",
                    "/sys/",
                    "/boot/",
                    "/usr/local/etc/",
                    "/opt/",
                ];
                let has_sensitive_path =
                    sensitive_prefixes.iter().any(|prefix| cmd.contains(prefix));
                let home_prefix = if cfg!(target_os = "macos") {
                    "/Users"
                } else {
                    "/home"
                };
                let contains_other_home =
                    cmd.contains(home_prefix) && !cmd.contains(&cwd.to_string_lossy().to_string());

                if has_sensitive_path || contains_other_home {
                    tracing::info!(
                        tool_name = "bash",
                        permission_decision = "requires_confirmation",
                        mode = "ask",
                        has_sensitive_path = has_sensitive_path,
                        contains_other_home = contains_other_home,
                        command = %cmd,
                        "Command accessing sensitive/external paths requires confirmation"
                    );
                    return PermissionDecision {
                        allowed: false,
                        requires_confirmation: true,
                        reason: "Command may access system-sensitive or external paths".to_string(),
                    };
                }

                // ── ExecPolicy: rule-based command authorization ──────────────
                // After security checks pass, apply ExecPolicy rules.
                // This replaces the legacy hardcoded destructive/read-only prefix arrays.
                if let Some(policy) = exec_policy
                    && let Some(decision) = policy.evaluate(cmd)
                {
                    use crate::permissions::execpolicy::PolicyAction;
                    match decision.action {
                        PolicyAction::Auto => {
                            tracing::debug!(
                                tool_name = "bash",
                                permission_decision = "allowed",
                                reason = "exec_policy_auto",
                                command = %cmd,
                                rule = %decision.reason,
                                "Command auto-allowed by exec_policy"
                            );
                            return PermissionDecision {
                                allowed: true,
                                requires_confirmation: false,
                                reason: format!(
                                    "Command auto-allowed by policy: {}",
                                    decision.reason
                                ),
                            };
                        }
                        PolicyAction::Ask => {
                            tracing::info!(
                                tool_name = "bash",
                                permission_decision = "requires_confirmation",
                                reason = "exec_policy_ask",
                                command = %cmd,
                                rule = %decision.reason,
                                "Command requires confirmation by exec_policy"
                            );
                            return PermissionDecision {
                                allowed: false,
                                requires_confirmation: true,
                                reason: format!(
                                    "Policy requires confirmation: {}",
                                    decision.reason
                                ),
                            };
                        }
                        PolicyAction::Deny => {
                            tracing::warn!(
                                tool_name = "bash",
                                permission_decision = "denied",
                                reason = "exec_policy_deny",
                                command = %cmd,
                                rule = %decision.reason,
                                "Command denied by exec_policy"
                            );
                            return PermissionDecision {
                                allowed: false,
                                requires_confirmation: false,
                                reason: format!("Command denied by policy: {}", decision.reason),
                            };
                        }
                    }
                }

                // No ExecPolicy rule matched — safe command, auto-allow
                tracing::debug!(
                    tool_name = "bash",
                    permission_decision = "allowed",
                    mode = "ask",
                    command = %cmd,
                    "Non-destructive bash command auto-allowed in relaxed mode"
                );
                return PermissionDecision {
                    allowed: true,
                    requires_confirmation: false,
                    reason: "Non-destructive command in relaxed mode".to_string(),
                };
            }

            // --- File-based tools (write, edit, glob, grep, read): ---
            if let Some(path) = file_path {
                // Temp dirs: always auto-allow (transient files, no data loss concern)
                if is_in_safe_zone(path, cwd, safe_paths) {
                    // Write/Edit tools in CWD: only auto-allow if git repo
                    // (git provides history/revertibility; non-git = irreversible)
                    if WRITE_TOOLS.contains(&tool_name) && !is_in_tmp_dir(path, safe_paths) {
                        if is_inside_git_repo(cwd) {
                            tracing::debug!(
                                tool_name = %tool_name,
                                permission_decision = "allowed",
                                mode = "ask",
                                file_path = %path.display(),
                                reason = "git_repo",
                                "Write/edit in git repo auto-allowed (reversible)"
                            );
                            return PermissionDecision {
                                allowed: true,
                                requires_confirmation: false,
                                reason: "Write in git repo (reversible via git)".to_string(),
                            };
                        } else {
                            tracing::info!(
                                tool_name = %tool_name,
                                permission_decision = "requires_confirmation",
                                mode = "ask",
                                file_path = %path.display(),
                                reason = "no_git_repo",
                                "Write/edit outside git repo requires confirmation"
                            );
                            return PermissionDecision {
                                allowed: false,
                                requires_confirmation: true,
                                reason: format!(
                                    "Write to '{}' in non-git directory (irreversible)",
                                    path.display()
                                ),
                            };
                        }
                    }
                    // Read-only or other non-write tools in safe zone: auto-allow
                    tracing::debug!(
                        tool_name = %tool_name,
                        permission_decision = "allowed",
                        mode = "ask",
                        file_path = %path.display(),
                        "File operation in safe zone auto-allowed"
                    );
                    return PermissionDecision {
                        allowed: true,
                        requires_confirmation: false,
                        reason: "File operation within safe zone (CWD/temp)".to_string(),
                    };
                } else {
                    // Outside safe zone — ask for confirmation
                    tracing::info!(
                        tool_name = %tool_name,
                        permission_decision = "requires_confirmation",
                        mode = "ask",
                        file_path = %path.display(),
                        "File operation outside safe zone requires confirmation"
                    );
                    return PermissionDecision {
                        allowed: false,
                        requires_confirmation: true,
                        reason: format!(
                            "Path '{}' is outside the current working directory",
                            path.display()
                        ),
                    };
                }
            }

            // --- Other tools (no path, no command): auto-allow ---
            // e.g. memory, ask_user, web_search, skill_view, etc.
            tracing::debug!(
                tool_name = %tool_name,
                permission_decision = "allowed",
                mode = "ask",
                "Tool auto-allowed in relaxed mode"
            );
            PermissionDecision {
                allowed: true,
                requires_confirmation: false,
                reason: "Tool auto-allowed in relaxed mode".to_string(),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::execpolicy::{PolicyAction, builtin_rules};

    const ASK: PermissionMode = PermissionMode::Ask;
    const ALLOW: PermissionMode = PermissionMode::Allow;
    const DENY: PermissionMode = PermissionMode::Deny;

    /// Create an ExecPolicy with builtin rules for tests.
    fn test_policy() -> Option<ExecPolicy> {
        Some(ExecPolicy::from_rules(builtin_rules()))
    }

    /// Helper: evaluate with no trusted paths and no file/command.
    fn eval_simple(mode: &PermissionMode, tool: &str, ro: bool) -> PermissionDecision {
        evaluate_permission(
            mode,
            &[],
            tool,
            ro,
            None,
            None,
            Path::new("/tmp/work"),
            &[],
            None,
        )
    }

    /// Helper: evaluate bash command in CWD with ExecPolicy.
    fn eval_bash(mode: &PermissionMode, cmd: &str, cwd: &str) -> PermissionDecision {
        evaluate_permission(
            mode,
            &[],
            "bash",
            false,
            None,
            Some(cmd),
            Path::new(cwd),
            &[],
            test_policy().as_ref(),
        )
    }

    /// Helper: evaluate file tool with a path in CWD.
    fn eval_file(mode: &PermissionMode, tool: &str, path: &str, cwd: &str) -> PermissionDecision {
        evaluate_permission(
            mode,
            &[],
            tool,
            false,
            Some(Path::new(path)),
            None,
            Path::new(cwd),
            &[],
            None,
        )
    }

    // -- Read-only: always allowed in all modes except Deny(non-read-only) --

    #[test]
    fn ask_read_only_is_allowed() {
        let d = eval_simple(&ASK, "read", true);
        assert!(d.allowed);
        assert!(!d.requires_confirmation);
    }

    #[test]
    fn ask_glob_is_allowed() {
        let d = eval_simple(&ASK, "glob", true);
        assert!(d.allowed);
    }

    // -- Bash: non-destructive auto-allowed --

    #[test]
    fn ask_bash_cargo_build_allowed() {
        let d = eval_bash(&ASK, "cargo build", "/home/user/proj");
        assert!(d.allowed);
        assert!(!d.requires_confirmation);
    }

    #[test]
    fn ask_bash_git_status_allowed() {
        let d = eval_bash(&ASK, "git status", "/home/user/proj");
        assert!(d.allowed);
    }

    #[test]
    fn ask_bash_git_add_allowed() {
        let d = eval_bash(&ASK, "git add .", "/home/user/proj");
        assert!(d.allowed);
    }

    #[test]
    fn ask_bash_git_commit_allowed() {
        let d = eval_bash(&ASK, "git commit -m \"test\"", "/home/user/proj");
        assert!(d.allowed);
    }

    #[test]
    fn ask_bash_mkdir_allowed() {
        let d = eval_bash(&ASK, "mkdir -p src/new_dir", "/home/user/proj");
        assert!(d.allowed);
    }

    #[test]
    fn ask_bash_touch_allowed() {
        let d = eval_bash(&ASK, "touch newfile.txt", "/home/user/proj");
        assert!(d.allowed);
    }

    // -- Bash: destructive commands require confirmation --

    #[test]
    fn ask_bash_rm_requires_confirmation() {
        let d = eval_bash(&ASK, "rm -rf target/", "/home/user/proj");
        assert!(!d.allowed);
        assert!(d.requires_confirmation);
    }

    #[test]
    fn ask_bash_rm_simple_requires_confirmation() {
        let d = eval_bash(&ASK, "rm file.txt", "/home/user/proj");
        assert!(!d.allowed);
        assert!(d.requires_confirmation);
    }

    #[test]
    fn ask_bash_sudo_requires_confirmation() {
        let d = eval_bash(&ASK, "sudo apt update", "/home/user/proj");
        assert!(!d.allowed);
        assert!(d.requires_confirmation);
    }

    #[test]
    fn ask_bash_git_push_force_requires_confirmation() {
        let d = eval_bash(&ASK, "git push --force origin main", "/home/user/proj");
        assert!(!d.allowed);
        assert!(d.requires_confirmation);
    }

    #[test]
    fn ask_bash_git_push_f_requires_confirmation() {
        let d = eval_bash(&ASK, "git push -f origin main", "/home/user/proj");
        assert!(!d.allowed);
        assert!(d.requires_confirmation);
    }

    #[test]
    fn ask_bash_git_reset_hard_requires_confirmation() {
        let d = eval_bash(&ASK, "git reset --hard HEAD~1", "/home/user/proj");
        assert!(!d.allowed);
        assert!(d.requires_confirmation);
    }

    #[test]
    fn ask_bash_dd_requires_confirmation() {
        let d = eval_bash(&ASK, "dd if=/dev/zero of=/dev/sda", "/home/user/proj");
        assert!(!d.allowed);
        assert!(d.requires_confirmation);
    }

    // -- Bash: shell expansion / path traversal --

    #[test]
    fn ask_bash_shell_expansion_safe_subcommand_allowed() {
        // echo $(whoami) — whoami is safe, should now be auto-allowed
        let d = eval_bash(&ASK, "echo $(whoami)", "/home/user/proj");
        assert!(
            d.allowed,
            "safe subshell sub-command should be auto-allowed"
        );
        assert!(!d.requires_confirmation);
    }

    #[test]
    fn ask_bash_shell_expansion_destructive_subcommand_requires_confirmation() {
        // echo $(rm -rf /tmp) — rm is destructive, should require confirmation
        let d = eval_bash(&ASK, "echo $(rm -rf /tmp)", "/home/user/proj");
        assert!(!d.allowed);
        assert!(d.requires_confirmation);
    }

    #[test]
    fn ask_bash_backtick_safe_subcommand_allowed() {
        let d = eval_bash(&ASK, "echo `whoami`", "/home/user/proj");
        assert!(
            d.allowed,
            "safe backtick sub-command should be auto-allowed"
        );
        assert!(!d.requires_confirmation);
    }

    #[test]
    fn ask_bash_backtick_destructive_subcommand_requires_confirmation() {
        let d = eval_bash(&ASK, "echo `rm -rf /tmp`", "/home/user/proj");
        assert!(!d.allowed);
        assert!(d.requires_confirmation);
    }

    #[test]
    fn ask_bash_var_ref_no_subshell_allowed() {
        // Pure variable reference $VAR — no command substitution, should be allowed
        let d = eval_bash(&ASK, "echo $HOME", "/home/user/proj");
        assert!(d.allowed, "pure variable reference should be auto-allowed");
        assert!(!d.requires_confirmation);
    }

    #[test]
    fn ask_bash_nested_subshell_destructive_requires_confirmation() {
        // echo $(echo $(rm -rf /tmp)) — nested destructive should be caught
        let d = eval_bash(&ASK, "echo $(echo $(rm -rf /tmp))", "/home/user/proj");
        assert!(!d.allowed);
        assert!(d.requires_confirmation);
    }

    #[test]
    fn ask_bash_subshell_sensitive_path_requires_confirmation() {
        // echo $(cat /etc/shadow) — sensitive path in subshell
        let d = eval_bash(&ASK, "echo $(cat /etc/shadow)", "/home/user/proj");
        assert!(!d.allowed);
        assert!(d.requires_confirmation);
    }

    #[test]
    fn ask_bash_subshell_path_traversal_requires_confirmation() {
        // echo $(cat ../../../etc/passwd) — path traversal in subshell
        let d = eval_bash(&ASK, "echo $(cat ../../../etc/passwd)", "/home/user/proj");
        assert!(!d.allowed);
        assert!(d.requires_confirmation);
    }

    #[test]
    fn ask_bash_compound_with_subshell_safe_allowed() {
        // git status && echo $(whoami) — both safe
        let d = eval_bash(&ASK, "git status && echo $(whoami)", "/home/user/proj");
        assert!(d.allowed);
        assert!(!d.requires_confirmation);
    }

    #[test]
    fn ask_bash_compound_with_subshell_destructive_requires_confirmation() {
        // git status && echo $(rm -rf /tmp) — subshell is destructive
        let d = eval_bash(&ASK, "git status && echo $(rm -rf /tmp)", "/home/user/proj");
        assert!(!d.allowed);
        assert!(d.requires_confirmation);
    }

    // -- extract_subshell_commands --
    #[test]
    fn extract_subshell_dollar_paren() {
        let cmds = extract_subshell_commands("echo $(whoami)");
        assert_eq!(cmds, vec!["whoami"]);
    }

    #[test]
    fn extract_subshell_backtick() {
        let cmds = extract_subshell_commands("echo `whoami`");
        assert_eq!(cmds, vec!["whoami"]);
    }

    #[test]
    fn extract_subshell_nested() {
        let cmds = extract_subshell_commands("echo $(echo $(whoami))");
        // Outer $(...) ) extracts $(echo $(whoami)) and recursively $(whoami)
        assert_eq!(cmds, vec!["echo $(whoami)", "whoami"]);
    }

    #[test]
    fn extract_subshell_multiple() {
        let cmds = extract_subshell_commands("echo $(whoami) && echo $(date)");
        assert_eq!(cmds, vec!["whoami", "date"]);
    }

    #[test]
    fn extract_subshell_no_match() {
        let cmds = extract_subshell_commands("echo hello");
        assert!(cmds.is_empty());
    }

    #[test]
    fn extract_subshell_var_ref_only() {
        // $VAR should NOT be extracted
        let cmds = extract_subshell_commands("echo $HOME");
        assert!(cmds.is_empty());
    }

    #[test]
    fn extract_subshell_empty() {
        let cmds = extract_subshell_commands("");
        assert!(cmds.is_empty());
    }

    #[test]
    fn ask_bash_path_traversal_requires_confirmation() {
        let d = eval_bash(&ASK, "cat ../../../etc/passwd", "/home/user/proj");
        assert!(!d.allowed);
        assert!(d.requires_confirmation);
    }

    // -- Bash: sensitive system paths --

    #[test]
    fn ask_bash_etc_access_requires_confirmation() {
        let d = eval_bash(&ASK, "cat /etc/shadow", "/home/user/proj");
        assert!(!d.allowed);
        assert!(d.requires_confirmation);
    }

    #[test]
    fn ask_bash_other_user_home_requires_confirmation() {
        let d = eval_bash(&ASK, "ls /home/otheruser/.ssh/", "/home/user/proj");
        assert!(!d.allowed);
        assert!(d.requires_confirmation);
    }

    // -- File operations: write/edit in CWD + git repo → auto-allowed --

    #[test]
    fn ask_write_in_cwd_git_repo_allowed() {
        // /home/guo/Develop/zeno is a real git repo
        let d = eval_file(
            &ASK,
            "write",
            "/home/guo/Develop/zeno/src/main.rs",
            "/home/guo/Develop/zeno",
        );
        assert!(d.allowed, "write in git repo should be auto-allowed");
        assert!(!d.requires_confirmation);
    }

    #[test]
    fn ask_edit_in_cwd_git_repo_allowed() {
        let d = eval_file(
            &ASK,
            "edit",
            "/home/guo/Develop/zeno/README.md",
            "/home/guo/Develop/zeno",
        );
        assert!(d.allowed, "edit in git repo should be auto-allowed");
        assert!(!d.requires_confirmation);
    }

    // -- File operations: write/edit in CWD but NOT a git repo → requires confirmation --

    #[test]
    fn ask_write_in_cwd_no_git_repo_requires_confirmation() {
        // /etc is not a git repo and not a temp dir
        let d = eval_file(&ASK, "write", "/etc/nginx/nginx.conf", "/etc/nginx");
        assert!(
            !d.allowed,
            "write in non-git CWD should require confirmation"
        );
        assert!(d.requires_confirmation);
    }

    #[test]
    fn ask_edit_in_cwd_no_git_repo_requires_confirmation() {
        let d = eval_file(&ASK, "edit", "/etc/hosts", "/etc");
        assert!(
            !d.allowed,
            "edit in non-git CWD should require confirmation"
        );
        assert!(d.requires_confirmation);
    }

    // -- File operations: temp dirs always auto-allowed (no git needed) --

    #[test]
    fn ask_write_in_tmp_allowed() {
        let d = eval_file(&ASK, "write", "/tmp/test.txt", "/home/user/proj");
        assert!(d.allowed, "/tmp writes should always be allowed");
        assert!(!d.requires_confirmation);
    }

    #[test]
    fn ask_edit_in_tmp_allowed() {
        let d = eval_file(&ASK, "edit", "/var/tmp/test.txt", "/home/user/proj");
        assert!(d.allowed, "/var/tmp edits should always be allowed");
    }

    // -- Trusted paths bypass everything --

    #[test]
    fn trusted_path_allows_external_access() {
        let trusted = vec!["/data/projects".to_string()];
        let d = evaluate_permission(
            &ASK,
            &trusted,
            "write",
            false,
            Some(Path::new("/data/projects/lib/main.rs")),
            None,
            Path::new("/home/user/proj"),
            &[],
            None,
        );
        assert!(d.allowed);
        assert!(!d.requires_confirmation);
    }

    // -- Allow mode: always allowed --

    #[test]
    fn allow_mode_allows_everything() {
        let d = eval_bash(&ALLOW, "rm -rf /", "/home/user/proj");
        assert!(d.allowed);
        assert!(!d.requires_confirmation);
    }

    // -- Deny mode: only read-only --

    #[test]
    fn deny_mode_blocks_writes() {
        let d = eval_simple(&DENY, "write", false);
        assert!(!d.allowed);
        assert!(!d.requires_confirmation);
    }

    #[test]
    fn deny_mode_allows_read_only() {
        let d = eval_simple(&DENY, "read", true);
        assert!(d.allowed);
    }

    // -- Other tools without path/command: auto-allowed in Ask --

    #[test]
    fn ask_memory_auto_allowed() {
        let d = eval_simple(&ASK, "memory", false);
        assert!(d.allowed);
        assert!(!d.requires_confirmation);
    }

    #[test]
    fn ask_web_search_auto_allowed() {
        let d = eval_simple(&ASK, "web_search", false);
        assert!(d.allowed);
    }

    // -- ExecPolicy destructive detection (replaces is_destructive_command) --

    #[test]
    fn destructive_rm() {
        let p = test_policy().unwrap();
        let r = p.evaluate("rm -rf /").unwrap();
        assert_eq!(r.action, PolicyAction::Deny, "rm -rf / should be Deny");
        let r = p.evaluate("rm file.txt").unwrap();
        assert_eq!(r.action, PolicyAction::Ask, "rm should be Ask");
    }

    #[test]
    fn destructive_sudo() {
        let p = test_policy().unwrap();
        let r = p.evaluate("sudo apt install vim").unwrap();
        assert_eq!(r.action, PolicyAction::Ask, "sudo should be Ask");
    }

    #[test]
    fn not_destructive_cargo() {
        let p = test_policy().unwrap();
        let r = p.evaluate("cargo build");
        assert!(r.is_none(), "cargo build should have no rule match");
    }

    #[test]
    fn not_destructive_git_add() {
        let p = test_policy().unwrap();
        let r = p.evaluate("git add .");
        assert!(r.is_none(), "git add should have no rule match");
        let r = p.evaluate("git commit -m \"msg\"");
        assert!(r.is_none(), "git commit should have no rule match");
    }

    #[test]
    fn destructive_git_push_force() {
        let p = test_policy().unwrap();
        let r = p.evaluate("git push --force origin main").unwrap();
        assert_eq!(
            r.action,
            PolicyAction::Ask,
            "git push --force should be Ask"
        );
        let r = p.evaluate("git push -f origin main").unwrap();
        assert_eq!(r.action, PolicyAction::Ask, "git push -f should be Ask");
    }

    #[test]
    fn destructive_git_reset_hard() {
        let p = test_policy().unwrap();
        let r = p.evaluate("git reset --hard HEAD~3").unwrap();
        assert_eq!(
            r.action,
            PolicyAction::Ask,
            "git reset --hard should be Ask"
        );
    }

    // -- File operations: outside CWD → always requires confirmation --

    #[test]
    fn ask_write_outside_cwd_requires_confirmation() {
        // Even though zeno is a git repo, writing to /etc should require confirmation
        let d = eval_file(&ASK, "write", "/etc/hosts", "/home/guo/Develop/zeno");
        assert!(
            !d.allowed,
            "write outside CWD should always require confirmation"
        );
        assert!(d.requires_confirmation);
    }

    #[test]
    fn ask_edit_outside_cwd_requires_confirmation() {
        let d = eval_file(
            &ASK,
            "edit",
            "/home/otheruser/.bashrc",
            "/home/guo/Develop/zeno",
        );
        assert!(
            !d.allowed,
            "edit outside CWD should always require confirmation"
        );
        assert!(d.requires_confirmation);
    }

    // -- is_in_safe_zone --

    #[test]
    fn safe_zone_cwd() {
        assert!(is_in_safe_zone(
            Path::new("/home/user/proj/src/main.rs"),
            Path::new("/home/user/proj"),
            &[],
        ));
    }

    #[test]
    fn safe_zone_tmp() {
        assert!(is_in_safe_zone(
            Path::new("/tmp/test.txt"),
            Path::new("/home/user/proj"),
            &[],
        ));
    }

    #[test]
    fn not_safe_zone_etc() {
        assert!(!is_in_safe_zone(
            Path::new("/etc/hosts"),
            Path::new("/home/user/proj"),
            &[],
        ));
    }
}
