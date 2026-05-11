//! Bash/shell command execution tool with optional rtk routing.

use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::base::{Tool, ToolContext, ToolError};

/// Commands that are known to be read-only (no side effects).
/// Used by `is_read_only` to skip unnecessary permission confirmations
/// in "ask" mode — e.g. `ls`, `cat`, `git status` don't modify anything.
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
    "rg ",
    "ag ",
    "ack ",
    "find ",
    "fd ",
    "locate ",
    "git status",
    "git diff",
    "git log",
    "git show",
    "git branch",
    "git tag",
    "git remote",
    "gh ", // GitHub CLI — read-only subcommands dominate
    "echo ",
    "printf ",
    "pwd",
    "whoami",
    "hostname",
    "uname",
    "env ",
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
    "awk ",
    "sed -n",
    "xargs -n",
];

pub struct BashTool {
    use_rtk: bool,
    timeout_secs: u64,
    /// Extra environment variables injected into every bash command execution.
    env: HashMap<String, String>,
}

impl BashTool {
    pub fn new(use_rtk: bool, env: HashMap<String, String>) -> Self {
        Self {
            use_rtk,
            timeout_secs: 120,
            env,
        }
    }

    /// Check if rtk can route this command. Uses `rtk rewrite` as the single
    /// source of truth — no hardcoded prefix lists to maintain.
    /// Returns the rewritten command and an optional cd directory to set as cwd.
    async fn maybe_rtk_route(&self, cmd: &str) -> Option<(String, Option<PathBuf>)> {
        if !self.use_rtk {
            return None;
        }
        if which::which("rtk").is_err() {
            return None;
        }
        let trimmed = cmd.trim();
        if trimmed.is_empty() {
            return None;
        }

        // Extract a leading `cd <dir> &&` or `cd <dir>;` prefix if present.
        // LLMs commonly prepend `cd /path &&` before the actual command.
        // We strip it, route the inner command through rtk, then execute with
        // the cd directory set as the process's current_dir instead of via shell.
        let (cd_dir, inner_cmd) = Self::strip_cd_prefix(trimmed);

        // Skip compound commands (pipes, chains) — rtk proxy can't handle them
        if inner_cmd.contains('|') || inner_cmd.contains("&&") || inner_cmd.contains("||") {
            return None;
        }

        // Ask rtk to rewrite — this is the authoritative check
        let Ok(output) = tokio::process::Command::new("rtk")
            .arg("rewrite")
            .args(inner_cmd.split_whitespace())
            .output()
            .await
        else {
            return None;
        };
        if output.status.code() == Some(3) {
            let rewritten = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !rewritten.is_empty() {
                return Some((rewritten, cd_dir));
            }
        }
        None
    }

    /// Strip a leading `cd <dir> &&` or `cd <dir>;` prefix from a command.
    /// Returns `(Some(dir_path), inner_command)` if a cd prefix was found,
    /// or `(None, original_command)` if not.
    ///
    /// Examples:
    /// - `"cd /home/user && cargo test"` → `(Some("/home/user"), "cargo test")`
    /// - `"cd ..; ls"` → `(Some(".."), "ls")`
    /// - `"cargo test"` → `(None, "cargo test")`
    fn strip_cd_prefix(cmd: &str) -> (Option<PathBuf>, &str) {
        let trimmed = cmd.trim();
        if let Some(rest) = trimmed.strip_prefix("cd ") {
            // Find where the directory path ends (next ` && ` or `; `)
            if let Some(sep_pos) = rest.find(" && ") {
                let dir = rest[..sep_pos].trim();
                let remaining = rest[sep_pos + 4..].trim();
                if !dir.is_empty() && !remaining.is_empty() {
                    return (Some(PathBuf::from(dir)), remaining);
                }
            }
            if let Some(sep_pos) = rest.find("; ") {
                let dir = rest[..sep_pos].trim();
                let remaining = rest[sep_pos + 2..].trim();
                if !dir.is_empty() && !remaining.is_empty() {
                    return (Some(PathBuf::from(dir)), remaining);
                }
            }
        }
        (None, trimmed)
    }
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Execute a shell command and return its output.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "The shell command to execute."
                        },
                        "timeout": {
                            "type": "integer",
                            "description": "Timeout in seconds (default: 120).",
                            "default": 120
                        }
                    },
                    "required": ["command"]
                }
            }
        })
    }

    /// Dynamically determine if a bash command is read-only based on its content.
    /// Matches the command string against a list of known read-only prefixes.
    /// This avoids unnecessary "ask" permission prompts for harmless commands
    /// like `ls`, `cat`, `git status`, etc.
    fn is_read_only(&self, input: &Value) -> bool {
        let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
        let trimmed = cmd.trim();

        // Empty commands are not read-only
        if trimmed.is_empty() {
            return false;
        }
        // Reject commands containing shell injection / dangerous constructs.
        // This prevents bypasses like `ls $(rm -rf /)` or `cat `wget evil.sh``
        // where a "read-only" prefix hides a destructive sub-command.
        for danger in &[
            "$(", // command substitution
            "${", // variable expansion (can contain commands in some forms)
            "`",  // backtick command substitution
            "|",  // pipe (can chain to destructive commands)
            "&&", // chain
            "||", // chain
            ";",  // sequential separator
            ">>", // append redirect
            ">",  // redirect (must check before readonly prefix)
        ] {
            if trimmed.contains(danger) {
                return false;
            }
        }
        // Commands that are always destructive — not read-only regardless of flags
        for destructive in &[
            "rm ",
            "mv ",
            "cp ",
            "mkdir ",
            "touch ",
            "chmod ",
            "chown ",
            "kill ",
            "pkill ",
            "killall ",
            "dd ",
            "mkfs ",
            "fdisk ",
            "mount ",
            "umount ",
            "sudo ",
            "doas ",
            "su ",
            "apt ",
            "yum ",
            "dnf ",
            "pacman ",
            "brew install",
            "brew uninstall",
            "pip install",
            "pip uninstall",
            "npm install",
            "npm uninstall",
            "cargo install",
            "cargo uninstall",
            "systemctl ",
            "service ",
        ] {
            if trimmed.contains(destructive) {
                return false;
            }
        }

        // Check if the command starts with a known read-only prefix.
        // We verify the *first token* (before any whitespace) matches a known
        // prefix, to prevent tricks like embedding dangerous constructs after
        // a safe prefix on the same line.
        READONLY_PREFIXES
            .iter()
            .any(|prefix| trimmed.starts_with(prefix))
    }

    async fn execute(&self, arguments: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let cmd = arguments["command"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("missing 'command'".into()))?;

        let timeout: u64 = arguments
            .get("timeout")
            .and_then(|t| t.as_u64())
            .unwrap_or(self.timeout_secs);

        // Try rtk routing first
        if let Some((rtk_cmd_str, cd_override)) = self.maybe_rtk_route(cmd).await {
            tracing::debug!(rtk_command = %rtk_cmd_str, "Routing through rtk");
            let parts: Vec<&str> = rtk_cmd_str.split_whitespace().collect();
            if let Some((program, args)) = parts.split_first() {
                let mut rtk_cmd = tokio::process::Command::new(program);
                // Use the cd-override directory if one was extracted from the command,
                // otherwise fall back to the context's cwd.
                rtk_cmd
                    .args(args)
                    .current_dir(cd_override.as_ref().unwrap_or(&ctx.cwd));
                for (k, v) in &self.env {
                    rtk_cmd.env(k, v);
                }
                rtk_cmd
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .kill_on_drop(true);
                let child = rtk_cmd.spawn().map_err(ToolError::Io)?;
                let output = tokio::time::timeout(
                    std::time::Duration::from_secs(timeout),
                    child.wait_with_output(),
                )
                .await
                .map_err(|_| {
                    ToolError::Timeout(format!("rtk command timed out after {}s", timeout))
                })?
                .map_err(ToolError::Io)?;

                if output.status.success() {
                    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                    let mut result = stdout;
                    if !stderr.is_empty() {
                        result.push_str("\n[stderr]\n");
                        result.push_str(&stderr);
                    }
                    if result.is_empty() {
                        result = "(no output)".into();
                    }
                    return Ok(result);
                }
                tracing::debug!(
                    event = "rtk_fallback",
                    "rtk proxy failed, falling back to raw command"
                );
            }
        }

        // Normal execution via bash
        // Use spawn() + kill_on_drop(true) so that if this future is
        // cancelled (e.g. by tokio::select! on Ctrl+C), the child process
        // is killed immediately instead of becoming an orphan.
        let mut bash_cmd = tokio::process::Command::new("bash");
        bash_cmd.arg("-c").arg(cmd).current_dir(&ctx.cwd);
        for (k, v) in &self.env {
            bash_cmd.env(k, v);
        }
        bash_cmd
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let child = bash_cmd.spawn().map_err(ToolError::Io)?;
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(timeout),
            child.wait_with_output(),
        )
        .await
        .map_err(|_| ToolError::Timeout(format!("command timed out after {}s", timeout)))?
        .map_err(ToolError::Io)?;

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        let exit_code = output.status.code().unwrap_or(-1);
        let mut result = String::new();

        if !stdout.is_empty() {
            result.push_str(&stdout);
        }
        if !stderr.is_empty() {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str("[stderr]\n");
            result.push_str(&stderr);
        }
        if !output.status.success() {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(&format!("[exit code: {}]", exit_code));
        }
        if result.is_empty() {
            result = "(no output)".into();
        }

        Ok(result)
    }
}

#[cfg(test)]
mod rtk_tests {
    use super::*;

    #[test]
    fn test_strip_cd_prefix_none() {
        let (dir, cmd) = BashTool::strip_cd_prefix("cargo test");
        assert!(dir.is_none());
        assert_eq!(cmd, "cargo test");
        let (dir, cmd) = BashTool::strip_cd_prefix("ls -la");
        assert!(dir.is_none());
        assert_eq!(cmd, "ls -la");
        let (dir, cmd) = BashTool::strip_cd_prefix("");
        assert!(dir.is_none());
        assert_eq!(cmd, "");
    }

    #[test]
    fn test_strip_cd_prefix_with_and() {
        let (dir, inner) = BashTool::strip_cd_prefix("cd /home/user && cargo test");
        assert_eq!(dir, Some(PathBuf::from("/home/user")));
        assert_eq!(inner, "cargo test");
    }

    #[test]
    fn test_strip_cd_prefix_with_semicolon() {
        let (dir, inner) = BashTool::strip_cd_prefix("cd ..; ls");
        assert_eq!(dir, Some(PathBuf::from("..")));
        assert_eq!(inner, "ls");
    }

    #[test]
    fn test_strip_cd_prefix_nested_path() {
        let (dir, inner) =
            BashTool::strip_cd_prefix("cd /home/guo/Develop/zeno && cargo test 2>&1");
        assert_eq!(dir, Some(PathBuf::from("/home/guo/Develop/zeno")));
        assert_eq!(inner, "cargo test 2>&1");
    }

    #[tokio::test]
    async fn test_rtk_route_with_cd_prefix() {
        let tool = BashTool::new(true, HashMap::new());
        let result = tool.maybe_rtk_route("cd /tmp && ls").await;
        assert!(
            result.is_some(),
            "rtk should route 'ls' even with cd prefix"
        );
        let (rewritten, cd_dir) = result.unwrap();
        assert_eq!(rewritten, "rtk ls");
        assert_eq!(cd_dir, Some(PathBuf::from("/tmp")));
    }

    #[tokio::test]
    async fn test_rtk_route_disabled_with_cd() {
        let tool = BashTool::new(false, HashMap::new());
        let result = tool.maybe_rtk_route("cd /tmp && ls").await;
        assert!(result.is_none(), "should not route when disabled");
    }

    #[tokio::test]
    async fn test_rtk_route_simple() {
        let tool = BashTool::new(true, HashMap::new());
        let result = tool.maybe_rtk_route("ls").await;
        assert!(result.is_some());
        let (rewritten, cd_dir) = result.unwrap();
        assert_eq!(rewritten, "rtk ls");
        assert!(cd_dir.is_none());
    }

    #[tokio::test]
    async fn test_rtk_skip_real_compound() {
        let tool = BashTool::new(true, HashMap::new());
        // Real compound commands with pipes should still be skipped
        assert!(tool.maybe_rtk_route("ls | head").await.is_none());
        assert!(
            tool.maybe_rtk_route("cargo test && cargo clippy")
                .await
                .is_none()
        );
    }
}
