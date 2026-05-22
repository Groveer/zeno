#![allow(dead_code)]
//! Execution policy — rule-based command authorization.
//!
//! Inspired by Codex's `execpolicy` crate: provides fine-grained control
//! over which bash commands can be auto-approved, require confirmation,
//! or are denied.
//!
//! # Rule Structure
//!
//! Each rule consists of:
//! - `pattern`: A regex or prefix pattern to match against the command
//! - `action`: Auto (allow), Ask (confirm), or Deny (block)
//! - `reason`: Human-readable explanation for the decision
//!
//! Rules are evaluated in order — first match wins.
//!
//! # Usage
//!
//! Rules can be configured in the project's `.zeno.toml`:
//!
//! ```toml
//! [[exec_policy]]
//! pattern = "^git push"
//! action = "ask"
//! reason = "Pushing to remote requires confirmation"
//!
//! [[exec_policy]]
//! pattern = "^cargo test"
//! action = "auto"
//! reason = "Running tests is safe"
//! ```

use regex::Regex;
use serde::{Deserialize, Serialize};

/// Action to take when a command matches a rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyAction {
    /// Automatically allow — no confirmation needed.
    Auto,
    /// Ask user for confirmation.
    Ask,
    /// Deny — block the command unconditionally.
    Deny,
}

/// A single execution policy rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecRule {
    /// Pattern to match against the command. Can be:
    /// - A regex pattern (anchored with ^ and/or $)
    /// - A prefix string (matched with starts_with)
    pub pattern: String,
    /// Action to take when the pattern matches.
    pub action: PolicyAction,
    /// Human-readable reason for this rule.
    #[serde(default)]
    pub reason: String,
    /// Whether to use regex matching (default: false = prefix matching).
    #[serde(default)]
    pub is_regex: bool,
}

/// Result of evaluating a command against the execution policy.
#[derive(Debug, Clone)]
pub struct PolicyDecision {
    pub action: PolicyAction,
    pub reason: String,
    /// Which rule matched (for logging).
    pub matched_rule: Option<String>,
}

/// The execution policy engine.
pub struct ExecPolicy {
    rules: Vec<CompiledRule>,
}

/// A compiled rule with pre-compiled regex (if applicable).
struct CompiledRule {
    rule: ExecRule,
    regex: Option<Regex>,
}

impl ExecPolicy {
    /// Create a new empty policy.
    pub fn new() -> Self {
        Self { rules: Vec::new() }
    }

    /// Create a policy from a list of rules.
    pub fn from_rules(rules: Vec<ExecRule>) -> Self {
        let compiled = rules
            .into_iter()
            .map(|rule| {
                let regex = if rule.is_regex {
                    Regex::new(&rule.pattern).ok()
                } else {
                    None
                };
                CompiledRule { rule, regex }
            })
            .collect();
        Self { rules: compiled }
    }

    /// Evaluate a command against the policy.
    /// Returns the first matching rule's action, or None if no rule matches.
    pub fn evaluate(&self, command: &str) -> Option<PolicyDecision> {
        let trimmed = command.trim();
        for compiled in &self.rules {
            let matched = if let Some(ref re) = compiled.regex {
                re.is_match(trimmed)
            } else {
                trimmed.starts_with(&compiled.rule.pattern)
            };

            if matched {
                return Some(PolicyDecision {
                    action: compiled.rule.action,
                    reason: if compiled.rule.reason.is_empty() {
                        format!("Matched rule: {}", compiled.rule.pattern)
                    } else {
                        compiled.rule.reason.clone()
                    },
                    matched_rule: Some(compiled.rule.pattern.clone()),
                });
            }
        }
        None
    }

    /// Add a rule to the policy.
    pub fn add_rule(&mut self, rule: ExecRule) {
        let regex = if rule.is_regex {
            Regex::new(&rule.pattern).ok()
        } else {
            None
        };
        self.rules.push(CompiledRule { rule, regex });
    }
}

impl Default for ExecPolicy {
    fn default() -> Self {
        Self::new()
    }
}

/// Built-in policy rules for common commands.
///
/// These provide sensible defaults that can be overridden by user config.
/// User rules are evaluated first (added before built-in rules during construction),
/// so user configuration can override any built-in rule.
///
/// Rules are organized by action:
/// - `Auto`: read-only / safe commands — no permission prompt needed
/// - `Ask`: destructive / dangerous commands — require user confirmation
/// - `Deny`: extremely dangerous commands — blocked unconditionally
///
/// These rules replace the legacy hardcoded prefix arrays (`READONLY_PREFIXES`,
/// `BUILTIN_DESTRUCTIVE`, `DESTRUCTIVE_PREFIXES`, `DESTRUCTIVE_GIT_PATTERNS`).
/// Read-only prefixes also remain in `BashTool::is_read_only()` for Deny mode.
pub fn builtin_rules() -> Vec<ExecRule> {
    let mut rules = Vec::new();

    // ── Auto: read-only / safe commands ──────────────────────────────
    let auto_patterns: &[(&str, &str)] = &[
        // File reading
        ("ls", "Directory listing is read-only"),
        ("cat ", "Reading files is safe"),
        ("head ", "Reading file head is safe"),
        ("tail ", "Reading file tail is safe"),
        ("less ", "Paging through files is safe"),
        ("more ", "Paging through files is safe"),
        ("file ", "Checking file type is safe"),
        ("wc ", "Counting words/lines is safe"),
        // Search
        ("grep ", "Searching file contents is read-only"),
        ("rg ", "Searching with ripgrep is read-only"),
        ("ag ", "Searching with the_silver_searcher is read-only"),
        ("ack ", "Searching with ack is read-only"),
        ("find ", "Finding files is read-only"),
        ("fd ", "Finding files with fd is safe"),
        ("locate ", "Locating files is read-only"),
        // Git read-only
        ("git status", "Git status is read-only"),
        ("git diff", "Git diff is read-only"),
        ("git log", "Git log is read-only"),
        ("git show", "Git show is read-only"),
        ("git branch", "Git branch listing is read-only"),
        ("git tag", "Git tag listing is read-only"),
        ("git remote", "Git remote listing is read-only"),
        ("gh ", "GitHub CLI is read-only"),
        // System info
        ("echo ", "Echoing text is safe"),
        ("printf ", "Printing formatted text is safe"),
        ("pwd", "Printing working directory is safe"),
        ("whoami", "Printing user name is safe"),
        ("hostname", "Printing hostname is safe"),
        ("uname", "Printing system info is safe"),
        ("env ", "Printing environment vars is safe"),
        ("printenv ", "Printing environment vars is safe"),
        ("set ", "Printing shell vars is safe"),
        // Cargo read-only
        ("cargo check", "Cargo check is read-only"),
        ("cargo test", "Running tests is safe"),
        ("cargo clippy", "Linting is read-only"),
        ("cargo doc", "Building docs is read-only"),
        // Test/lint read-only
        ("pytest ", "Running tests is safe"),
        ("ruff check", "Linting is read-only"),
        ("mypy ", "Type checking is read-only"),
        ("test ", "Running tests is safe"),
        // Data processing
        ("sort ", "Sorting data is read-only"),
        ("uniq ", "Deduplicating data is read-only"),
        ("cut ", "Cutting fields is read-only"),
        ("tr ", "Translating characters is read-only"),
        ("awk ", "Text processing with awk is read-only"),
        ("sed -n", "Sed with -n flag is read-only (no side effects)"),
        ("xargs -n", "Xargs with -n flag is safer"),
        // Utility
        ("which ", "Locating executables is safe"),
        ("where ", "Locating executables is safe"),
        ("type ", "Checking command type is safe"),
        ("[ ", "Test expression is safe"),
        ("[[ ", "Test expression is safe"),
    ];
    for (pattern, reason) in auto_patterns {
        rules.push(ExecRule {
            pattern: pattern.to_string(),
            action: PolicyAction::Auto,
            reason: reason.to_string(),
            is_regex: false,
        });
    }

    // ── Deny: extremely dangerous ────────────────────────────────────
    // Must be BEFORE less specific Ask rules so `"rm -rf /"` matches Deny
    // before the general `"rm "` Ask rule. First-match-wins.
    rules.push(ExecRule {
        pattern: "rm -rf /".into(),
        action: PolicyAction::Deny,
        reason: "Recursive delete of root filesystem".into(),
        is_regex: false,
    });
    rules.push(ExecRule {
        pattern: r"^sudo\s+rm\s+".into(),
        action: PolicyAction::Deny,
        reason: "sudo rm is too dangerous".into(),
        is_regex: true,
    });

    // ── Ask: destructive / dangerous commands ────────────────────────
    let ask_patterns: &[(&str, &str)] = &[
        // File destructive
        ("rm ", "Removing files/dirs can cause data loss"),
        ("rmdir ", "Removing directories can cause data loss"),
        ("mkfs.", "Formatting a filesystem destroys all data"),
        ("dd ", "DD can overwrite disks and cause data loss"),
        ("fdisk ", "Partitioning can destroy data"),
        ("shutdown", "Shutting down the system is destructive"),
        ("reboot", "Rebooting the system is destructive"),
        ("halt ", "Halting the system is destructive"),
        ("poweroff", "Powering off is destructive"),
        ("init ", "Changing init state is destructive"),
        // Permission/ownership
        ("chmod ", "Changing permissions can break access"),
        ("chown ", "Changing ownership can break access"),
        ("chgrp ", "Changing group can break access"),
        // Process
        ("kill ", "Killing processes can cause data loss"),
        ("pkill ", "Killing processes can cause data loss"),
        ("killall ", "Killing processes can cause data loss"),
        // Privilege escalation
        ("sudo ", "Running with elevated privileges is destructive"),
        ("doas ", "Running with elevated privileges is destructive"),
        ("su ", "Switching users is destructive"),
        // Package management
        ("apt remove", "Removing packages is destructive"),
        ("apt purge", "Purging packages is destructive"),
        ("apt autoremove", "Removing packages is destructive"),
        ("yum remove", "Removing packages is destructive"),
        ("yum erase", "Removing packages is destructive"),
        ("dnf remove", "Removing packages is destructive"),
        ("pacman -R", "Removing packages is destructive"),
        // Brew
        ("brew uninstall", "Uninstalling formulae is destructive"),
        ("brew remove", "Removing formulae is destructive"),
        // Pip
        ("pip uninstall", "Uninstalling packages is destructive"),
        ("pip remove", "Removing packages is destructive"),
        // Npm
        ("npm uninstall", "Uninstalling packages is destructive"),
        // Cargo
        ("cargo uninstall", "Uninstalling crates is destructive"),
        // Systemctl
        ("systemctl stop", "Stopping system services is destructive"),
        (
            "systemctl disable",
            "Disabling system services is destructive",
        ),
        ("systemctl mask", "Masking system services is destructive"),
        // Dangerous git
        ("git push", "Pushing to remote requires confirmation"),
        (
            "git push --force",
            "Force-pushing can rewrite remote history",
        ),
        ("git push -f ", "Force-pushing can rewrite remote history"),
        ("git push --delete", "Deleting remote refs is destructive"),
        (
            "git reset --hard",
            "Hard reset discards uncommitted changes",
        ),
        (
            "git clean -f",
            "Force-cleaning untracked files is destructive",
        ),
        (
            "git checkout -- ",
            "Discarding unstaged changes is destructive",
        ),
        ("git restore .", "Restoring all files discards changes"),
        ("git branch -D", "Force-deleting a branch is destructive"),
        ("git tag -d", "Deleting a tag is destructive"),
        (
            "git submodule deinit",
            "Deinitializing submodules is destructive",
        ),
    ];
    for (pattern, reason) in ask_patterns {
        rules.push(ExecRule {
            pattern: pattern.to_string(),
            action: PolicyAction::Ask,
            reason: reason.to_string(),
            is_regex: false,
        });
    }

    rules
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prefix_match() {
        let policy = ExecPolicy::from_rules(builtin_rules());

        // Auto-allowed
        let decision = policy.evaluate("ls -la").unwrap();
        assert_eq!(decision.action, PolicyAction::Auto);

        let decision = policy.evaluate("git status").unwrap();
        assert_eq!(decision.action, PolicyAction::Auto);

        // Ask
        let decision = policy.evaluate("git push origin main").unwrap();
        assert_eq!(decision.action, PolicyAction::Ask);

        // Deny
        let decision = policy.evaluate("rm -rf /").unwrap();
        assert_eq!(decision.action, PolicyAction::Deny);
    }

    #[test]
    fn test_regex_match() {
        let mut policy = ExecPolicy::new();
        policy.add_rule(ExecRule {
            pattern: r"^docker\s+rm\s+".into(),
            action: PolicyAction::Ask,
            reason: "Removing containers requires confirmation".into(),
            is_regex: true,
        });

        let decision = policy.evaluate("docker rm my_container").unwrap();
        assert_eq!(decision.action, PolicyAction::Ask);

        assert!(policy.evaluate("docker ps").is_none());
    }

    #[test]
    fn test_no_match() {
        let policy = ExecPolicy::new();
        assert!(policy.evaluate("arbitrary command").is_none());
    }

    #[test]
    fn test_first_match_wins() {
        let mut policy = ExecPolicy::new();
        policy.add_rule(ExecRule {
            pattern: "git".into(),
            action: PolicyAction::Auto,
            reason: "Git is safe".into(),
            is_regex: false,
        });
        policy.add_rule(ExecRule {
            pattern: "git push".into(),
            action: PolicyAction::Deny,
            reason: "No pushing".into(),
            is_regex: false,
        });

        // First rule matches "git push" because "git" is a prefix of "git push"
        let decision = policy.evaluate("git push").unwrap();
        assert_eq!(decision.action, PolicyAction::Auto);
    }

    #[test]
    fn test_builtin_rules_count() {
        let rules = builtin_rules();
        assert!(
            rules.len() >= 80,
            "Expected at least 80 builtin rules, got {}",
            rules.len()
        );
    }
}
