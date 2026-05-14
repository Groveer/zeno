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

/// Destructive shell commands that require user confirmation.
/// These are operations that can cause irreversible data loss.
const DESTRUCTIVE_PREFIXES: &[&str] = &[
    "rm ",
    "rm\t",
    "rm -", // rm with any flags (including -r, -f, -rf, etc.)
    "rmdir ",
    "mkfs.",
    "dd ",
    "fdisk ",
    "sudo ",
    "doas ",
    "su ",
    "chmod ",
    "chown ",
    "chgrp ",
    "kill ",
    "pkill ",
    "killall ",
    "shutdown",
    "reboot",
    "halt ",
    "poweroff",
    "init ",
    "systemctl stop",
    "systemctl disable",
    "systemctl mask",
    "apt remove",
    "apt purge",
    "apt autoremove",
    "yum remove",
    "yum erase",
    "dnf remove",
    "pacman -R",
    "brew uninstall",
    "brew remove",
    "pip uninstall",
    "pip remove",
    "npm uninstall",
    "cargo uninstall",
];

/// Dangerous git subcommands that can cause data loss.
const DESTRUCTIVE_GIT_PATTERNS: &[&str] = &[
    "git reset --hard",
    "git push --force",
    "git push -f ",
    "git push --delete",
    "git clean -f",
    "git checkout -- ", // discard unstaged changes
    "git restore .",    // discard all unstaged changes
    "git branch -D",    // force delete branch
    "git tag -d",       // delete tag
    "git submodule deinit",
];

/// Check if a bash command is destructive (irreversible / dangerous).
/// Returns true only for commands that can cause data loss or system damage.
/// Handles compound commands (e.g. `git status && rm file`) by splitting on
/// shell operators and checking each sub-command.
///
/// Built-in `DESTRUCTIVE_PREFIXES` are matched with `starts_with` (safe for
/// `rm`, `sudo`, etc.). Built-in `DESTRUCTIVE_GIT_PATTERNS` and user
/// `extra_commands` are matched with `contains`, giving a wildcard-like effect
/// — any substring match triggers the destructive check.
fn is_destructive_command(cmd: &str, extra_commands: &[String]) -> bool {
    let trimmed = cmd.trim();

    // Split on shell operators to handle compound commands
    // e.g. `git status && rm file.txt` → ["git status ", "rm file.txt"]
    let sub_commands = split_shell_operators(trimmed);

    for sub_cmd in &sub_commands {
        let sub = sub_cmd.trim();
        if sub.is_empty() {
            continue;
        }

        // Check built-in destructive prefixes (starts_with — precise match)
        for prefix in DESTRUCTIVE_PREFIXES.iter().copied() {
            if sub.starts_with(prefix) {
                return true;
            }
        }

        // Check built-in destructive git patterns + user extra commands
        // (contains — substring/wildcard match, so "git reset --hard"
        //  matches any git command involving a hard reset)
        for pattern in DESTRUCTIVE_GIT_PATTERNS
            .iter()
            .copied()
            .chain(extra_commands.iter().map(|s| s.as_str()))
        {
            if sub.contains(pattern) {
                return true;
            }
        }
    }

    false
}

/// Split a shell command on common operators: &&, ||, ;, |
/// Returns the sub-command strings (preserving original spacing).
fn split_shell_operators(cmd: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = cmd.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        if i + 1 < len {
            let two = format!("{}{}", chars[i], chars[i + 1]);
            if two == "&&" || two == "||" {
                parts.push(current.clone());
                current.clear();
                i += 2;
                continue;
            }
        }
        match chars[i] {
            ';' | '|' => {
                parts.push(current.clone());
                current.clear();
            }
            _ => {
                current.push(chars[i]);
            }
        }
        i += 1;
    }
    if !current.trim().is_empty() {
        parts.push(current);
    }

    parts
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
    extra_destructive_commands: &[String],
    safe_paths: &[String],
    denied_commands: &[String],
) -> PermissionDecision {
    // 0. Trusted path check — bypasses all other checks
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
                reason: "Path is under trusted path".to_string(),
            };
        }
    }

    // Denied commands: blocked unconditionally, before mode check
    if tool_name == "bash" {
        if let Some(cmd) = command {
            let trimmed = cmd.trim();
            for denied in denied_commands {
                if trimmed.contains(denied) {
                    tracing::warn!(
                        tool_name = "bash",
                        permission_decision = "denied",
                        reason = "denied_command",
                        command = %cmd,
                        denied_pattern = %denied,
                        "Command blocked by denied_commands"
                    );
                    return PermissionDecision {
                        allowed: false,
                        requires_confirmation: false,
                        reason: format!(
                            "Command '{}' is blocked by policy",
                            cmd.chars().take(80).collect::<String>()
                        ),
                    };
                }
            }
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

            // --- Bash commands: relaxed check ---
            if tool_name == "bash" {
                if let Some(cmd) = command {
                    // Check for shell injection / expansion that could bypass checks
                    let has_shell_expansion = cmd.contains('$') || cmd.contains('`');
                    let has_path_traversal = cmd.contains("../")
                        || cmd.contains("/..")
                        || cmd.ends_with("..")
                        || cmd.contains("..\\");

                    // Destructive commands always require confirmation
                    if is_destructive_command(cmd, extra_destructive_commands) {
                        tracing::info!(
                            tool_name = "bash",
                            permission_decision = "requires_confirmation",
                            mode = "ask",
                            reason = "destructive_command",
                            command = %cmd,
                            "Destructive bash command requires confirmation"
                        );
                        return PermissionDecision {
                            allowed: false,
                            requires_confirmation: true,
                            reason: format!(
                                "Destructive command requires confirmation: {}",
                                cmd.chars().take(80).collect::<String>()
                            ),
                        };
                    }

                    // Shell expansion or path traversal: requires confirmation
                    if has_shell_expansion || has_path_traversal {
                        tracing::info!(
                            tool_name = "bash",
                            permission_decision = "requires_confirmation",
                            mode = "ask",
                            has_shell_expansion = has_shell_expansion,
                            has_path_traversal = has_path_traversal,
                            command = %cmd,
                            "Command with expansion/traversal requires confirmation"
                        );
                        return PermissionDecision {
                            allowed: false,
                            requires_confirmation: true,
                            reason: "Command contains shell expansion or path traversal"
                                .to_string(),
                        };
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
                    let contains_other_home = cmd.contains(home_prefix)
                        && !cmd.contains(&cwd.to_string_lossy().to_string());

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
                            reason: "Command may access system-sensitive or external paths"
                                .to_string(),
                        };
                    }

                    // Non-destructive command, no expansion, no sensitive paths → auto-allow
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

    const ASK: PermissionMode = PermissionMode::Ask;
    const ALLOW: PermissionMode = PermissionMode::Allow;
    const DENY: PermissionMode = PermissionMode::Deny;

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
            &[],
            &[],
        )
    }

    /// Helper: evaluate bash command in CWD.
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
            &[],
            &[],
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
            &[],
            &[],
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
    fn ask_bash_shell_expansion_requires_confirmation() {
        let d = eval_bash(&ASK, "echo $(whoami)", "/home/user/proj");
        assert!(!d.allowed);
        assert!(d.requires_confirmation);
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
            &[],
            &[],
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

    // -- is_destructive_command --

    #[test]
    fn destructive_rm() {
        assert!(is_destructive_command("rm -rf /", &[]));
        assert!(is_destructive_command("rm file.txt", &[]));
    }

    #[test]
    fn destructive_sudo() {
        assert!(is_destructive_command("sudo apt install vim", &[]));
    }

    #[test]
    fn not_destructive_cargo() {
        assert!(!is_destructive_command("cargo build", &[]));
        assert!(!is_destructive_command("cargo test", &[]));
    }

    #[test]
    fn not_destructive_git_add() {
        assert!(!is_destructive_command("git add .", &[]));
        assert!(!is_destructive_command("git commit -m \"msg\"", &[]));
    }

    #[test]
    fn destructive_git_push_force() {
        assert!(is_destructive_command("git push --force origin main", &[]));
        assert!(is_destructive_command("git push -f origin main", &[]));
    }

    #[test]
    fn destructive_git_reset_hard() {
        assert!(is_destructive_command("git reset --hard HEAD~3", &[]));
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
