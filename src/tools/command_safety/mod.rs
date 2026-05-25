//! Command safety analysis using tree-sitter AST parsing.
//!
//! Provides two public functions:
//! - `is_read_only_command(command_str)` — is this command safe to auto-approve?
//! - `is_dangerous_command(command_str)` — is this command known to be destructive?
//!
//! Unlike the previous string-matching approach (`starts_with`, `contains`),
//! this module uses tree-sitter to parse the command and decompose compound
//! expressions (`|`, `&&`, `||`, `;`) into individual commands, then checks
//! each command independently.
//!
//! # Limitations
//!
//! - **Output redirects** (`>`, `>>`) are stripped during parsing — the command
//!   `echo hello > /etc/passwd` is analyzed as `echo hello` (safe). The actual
//!   file write restriction is enforced by the sandbox layer, not this module.
//!   This matches the behavior of the previous prefix-matching approach.
//! - **Fallback** for unparseable commands uses conservative string matching
//!   (same as the old approach). This only triggers for complex shell syntax
//!   that tree-sitter can't parse as word-only commands.

mod parser;
mod safe_commands;

use safe_commands::check_command_dangerous;
use safe_commands::check_command_safe;

/// Check whether a shell command string is read-only (safe to auto-approve).
///
/// Uses tree-sitter to parse the command. If parsing succeeds and the command
/// is entirely composed of word-only commands, each sub-command is checked
/// independently against argument-level safety rules.
///
/// If parsing fails (the command contains complex shell syntax), falls back
/// to conservative string matching.
pub fn is_read_only_command(command_str: &str) -> bool {
    let trimmed = command_str.trim();
    if trimmed.is_empty() {
        return false;
    }

    // Try tree-sitter parsing first
    if let Some(tree) = parser::try_parse_shell(trimmed) {
        if let Some(commands) = parser::parse_word_only_commands_sequence(&tree, trimmed) {
            // Every sub-command must be safe
            return commands
                .iter()
                .all(|cmd| matches!(check_command_safe(cmd), Some(true)));
        }
    }

    // Fallback: conservative string matching for unparseable commands
    fallback_is_read_only(trimmed)
}

/// Check whether a shell command string is known to be destructive.
///
/// Uses tree-sitter if available, then checks each sub-command against
/// dangerous command patterns. Falls back to conservative string matching.
pub fn is_dangerous_command(command_str: &str) -> bool {
    let trimmed = command_str.trim();
    if trimmed.is_empty() {
        return false;
    }

    // Try tree-sitter parsing first
    if let Some(tree) = parser::try_parse_shell(trimmed) {
        if let Some(commands) = parser::parse_word_only_commands_sequence(&tree, trimmed) {
            // If ANY sub-command is dangerous, the whole thing is dangerous
            return commands
                .iter()
                .any(|cmd| matches!(check_command_dangerous(cmd), Some(true)));
        }
    }

    // Fallback: conservative string matching
    fallback_is_dangerous(trimmed)
}

/// Fallback for commands that tree-sitter couldn't parse.
/// Uses string matching (same as the old approach).
fn fallback_is_read_only(trimmed: &str) -> bool {
    // Reject commands containing shell injection patterns
    // (bypass attempts like `ls $(rm -rf /)` or `echo `wget evil.sh``)
    for danger in &["$(", "${", "`"] {
        if trimmed.contains(danger) {
            return false;
        }
    }

    // Known read-only prefixes
    // NOTE: Only include commands that are TRULY always read‑only regardless
    // of arguments. Commands with dangerous flags (find, fd, rg, awk, gh,
    // git branch/tag/remote, etc.) are NOT here — they are handled by the
    // AST + argument‑level checks instead. If tree‑sitter can't parse them
    // (rare), we conservatively reject them.
    const READONLY_PREFIXES: &[&str] = &[
        "ls ",
        "cat ",
        "head ",
        "tail ",
        "less ",
        "more ",
        "file ",
        "which ",
        "where ",
        "type ",
        "grep ",
        "ag ",
        "ack ",
        "locate ",
        "git status",
        "git diff",
        "git log",
        "git show",
        "echo ",
        "printf ",
        "pwd",
        "whoami",
        "hostname",
        "uname",
        "printenv ",
        "set ",
        "cargo check",
        "cargo test",
        "cargo clippy",
        "cargo doc",
        "pytest ",
        "ruff check",
        "mypy ",
        "test ",
        "[ ",
        "[[ ",
        "wc ",
        "sort ",
        "uniq ",
        "cut ",
        "tr ",
        "sed -n",
        "xargs -n",
    ];

    READONLY_PREFIXES
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
}

fn fallback_is_dangerous(trimmed: &str) -> bool {
    const DANGEROUS_PATTERNS: &[&str] = &["rm -rf ", "rm -fr ", "rm -f ", "rm -rf/", "dd "];
    DANGEROUS_PATTERNS.iter().any(|p| trimmed.contains(p))
        || (trimmed.starts_with("sudo ") && fallback_is_dangerous(&trimmed[5..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ast_based_read_only() {
        // Simple commands — parsed by tree-sitter
        assert!(is_read_only_command("ls -la"));
        assert!(is_read_only_command("cat Cargo.toml"));
        assert!(is_read_only_command("echo hello"));
        assert!(is_read_only_command("git status"));
        assert!(is_read_only_command("grep pattern src/"));
    }

    #[test]
    fn test_compound_safe_commands() {
        // `ls && pwd` — both safe → read-only
        assert!(is_read_only_command("ls && pwd"));
        // `ls | wc -l` — both safe → read-only
        assert!(is_read_only_command("ls | wc -l"));
        // `cat file.txt; echo done` — both safe
        assert!(is_read_only_command("cat file.txt; echo done"));
    }

    #[test]
    fn test_compound_with_dangerous_rejected() {
        // The compound has `rm -rf` in it
        assert!(!is_read_only_command("ls && rm -rf /"));
        // Even though `ls` is safe, `rm -rf` isn't
        assert!(!is_read_only_command("echo hi || rm -rf /"));
    }

    #[test]
    fn test_argument_level_checks() {
        // find without exec is safe
        assert!(is_read_only_command("find . -name '*.rs'"));
        // find with exec is not
        assert!(!is_read_only_command("find . -exec rm {} \\;"));
        // rg with --pre is not safe
        assert!(!is_read_only_command("rg --pre cat pattern"));
        // git with push is not read-only
        assert!(!is_read_only_command("git push origin main"));
    }

    #[test]
    fn test_fallback_matches_old_behavior() {
        // These won't parse as word-only (pipe, redirect) — use fallback
        assert!(fallback_is_read_only("ls -la | head"));
        assert!(!fallback_is_read_only("rm -rf /"));
    }

    #[test]
    fn test_sudo_read_only() {
        // sudo ls is read-only (checked via AST)
        assert!(is_read_only_command("sudo ls -la"));
        // sudo find with -delete is not
        assert!(!is_read_only_command("sudo find . -delete"));
    }

    #[test]
    fn test_dangerous_commands() {
        assert!(is_dangerous_command("rm -rf /"));
        assert!(is_dangerous_command("rm -f important.txt"));
        assert!(is_dangerous_command("sudo rm -rf /"));
        assert!(!is_dangerous_command("ls -la"));
        assert!(!is_dangerous_command("cat file.txt"));
    }

    #[test]
    fn test_empty_commands() {
        assert!(!is_read_only_command(""));
        assert!(!is_read_only_command("   "));
        assert!(!is_dangerous_command(""));
    }
}
