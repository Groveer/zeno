//! Bash/shell command execution tool with optional rtk routing.

use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::base::{Tool, ToolContext, ToolError};
use crate::sandbox::Sandbox;

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
    /// Commands always allowed (merged with built-in read-only prefixes).
    allowed_commands: Vec<String>,
    /// Commands requiring confirmation (merged with built-in destructive prefixes).
    ask_commands: Vec<String>,
    /// Commands always denied (blocked unconditionally).
    denied_commands: Vec<String>,
    /// Sandbox for secure command execution (optional).
    /// When set, commands are wrapped with isolation (bwrap, nsjail, etc.).
    sandbox: Box<dyn Sandbox>,
}

impl BashTool {
    pub fn new(
        use_rtk: bool,
        env: HashMap<String, String>,
        allowed_commands: Vec<String>,
        ask_commands: Vec<String>,
        denied_commands: Vec<String>,
        sandbox: Box<dyn Sandbox>,
    ) -> Self {
        Self {
            use_rtk,
            timeout_secs: 120,
            env,
            allowed_commands,
            ask_commands,
            denied_commands,
            sandbox,
        }
    }

    /// Check if rtk can route this command. Uses `rtk rewrite` as the single
    /// source of truth — no hardcoded prefix lists to maintain.
    /// Returns the rewritten command and an optional cd directory to set as cwd.
    ///
    /// rtk natively handles compound commands (|, &&, ||) — it rewrites each
    /// segment independently and preserves shell operators. The rewritten command
    /// is then executed via `bash -c`, so all shell syntax (redirects, pipes,
    /// chains) works correctly.
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

        // Ask rtk to rewrite — this is the authoritative check.
        // rtk handles redirects (2>&1), pipes (|), and chains (&&, ||) natively
        // by rewriting each segment independently and preserving operators.
        let Ok(output) = tokio::process::Command::new("rtk")
            .arg("rewrite")
            .args(inner_cmd.split_whitespace())
            .output()
            .await
        else {
            return None;
        };
        // Exit 0 = auto-allowed, exit 3 = ask (rtk signals the caller to prompt,
        // but we auto-allow since zeno's own permission system handles confirmation).
        // Both produce rewritten output we can use.
        let exit_code = output.status.code();
        if exit_code == Some(0) || exit_code == Some(3) {
            let rewritten = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !rewritten.is_empty() {
                return Some((rewritten, cd_dir));
            }
        }
        None
    }

    /// Build a `tokio::process::Command` for executing the given shell command,
    /// optionally wrapping it with sandbox isolation.
    ///
    /// If the sandbox is active (not NoSandbox), the command is wrapped with
    /// bwrap/nsjail args. Otherwise, it runs as `bash -c <cmd>`.
    fn build_command<'a>(
        &'a self,
        cmd: &'a str,
        cwd: &'a std::path::Path,
    ) -> tokio::process::Command {
        let sandbox_args = self.sandbox.wrap_command(cmd, cwd);
        let mut cmd_obj = if sandbox_args.is_empty() {
            // No sandbox — run directly
            let mut c = tokio::process::Command::new("bash");
            c.arg("-c").arg(cmd);
            c
        } else {
            // Sandboxed — first arg is program, rest are args
            let mut c = tokio::process::Command::new(&sandbox_args[0]);
            c.args(&sandbox_args[1..]);
            c
        };
        cmd_obj.current_dir(cwd);
        for (k, v) in &self.env {
            cmd_obj.env(k, v);
        }
        cmd_obj
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        cmd_obj
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
        const BUILTIN_DESTRUCTIVE: &[&str] = &[
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
        ];
        // Check denied commands first — blocked unconditionally
        for denied in &self.denied_commands {
            if trimmed.contains(denied) {
                return false;
            }
        }
        // Check built-in destructive + user ask_commands
        for destructive in BUILTIN_DESTRUCTIVE
            .iter()
            .copied()
            .chain(self.ask_commands.iter().map(|s| s.as_str()))
        {
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
            || self
                .allowed_commands
                .iter()
                .any(|cmd| trimmed.starts_with(cmd))
    }

    async fn execute(&self, arguments: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        // Rate limit check: prevent runaway bash execution
        if let Some(ref limiter) = ctx.rate_limiter
            && let Ok(mut limiter) = limiter.lock()
        {
            limiter.check_and_record().map_err(ToolError::Timeout)?;
        }

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
            // rtk rewritten command may contain shell syntax (|, &&, ||, redirects),
            // so execute via bash -c (or sandbox wrapper) with the cd directory as working directory
            let cwd = cd_override.clone().unwrap_or_else(|| ctx.get_cwd());
            let mut bash_cmd = self.build_command(&rtk_cmd_str, &cwd);
            let child = bash_cmd.spawn().map_err(ToolError::Io)?;
            let output = tokio::time::timeout(
                std::time::Duration::from_secs(timeout),
                child.wait_with_output(),
            )
            .await
            .map_err(|_| ToolError::Timeout(format!("rtk command timed out after {}s", timeout)))?
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

        // Normal execution via bash (or sandbox wrapper)
        // Use spawn() + kill_on_drop(true) so that if this future is
        // cancelled (e.g. by tokio::select! on Ctrl+C), the child process
        // is killed immediately instead of becoming an orphan.
        let cwd = ctx.get_cwd();
        let mut bash_cmd = self.build_command(cmd, &cwd);
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

        // CWD tracking: detect standalone `cd <dir>` commands and update the
        // shared context so subsequent tool calls use the new directory.
        let trimmed_cmd = cmd.trim();
        if let Some(dir_str) = trimmed_cmd.strip_prefix("cd ") {
            // Only update for bare `cd <dir>` (no `&&`, `;`, `|` chaining).
            let is_bare_cd = !dir_str.contains("&&")
                && !dir_str.contains(';')
                && !dir_str.contains('|')
                && !dir_str.contains('>')
                && !dir_str.contains('<');
            if is_bare_cd && output.status.success() {
                let dir = dir_str.trim().trim_matches(&['"', '\''][..]);
                let new_cwd = if dir.starts_with('/') {
                    PathBuf::from(dir)
                } else if dir == ".." {
                    let mut parent = ctx.get_cwd();
                    parent.pop();
                    parent
                } else if dir == "." {
                    ctx.get_cwd()
                } else {
                    ctx.get_cwd().join(dir)
                };
                // Canonicalize to resolve any `..` / `.` components
                if let Ok(canonical) = std::fs::canonicalize(&new_cwd) {
                    ctx.set_cwd(canonical);
                } else if new_cwd.is_absolute() {
                    ctx.set_cwd(new_cwd);
                }
            }
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

    #[tokio::test]
    async fn test_rtk_route_with_redirect_stripped() {
        let nosb = Box::new(crate::sandbox::NoSandbox);
        let tool = BashTool::new(true, HashMap::new(), vec![], vec![], vec![], nosb);
        let result = tool.maybe_rtk_route("git status 2>&1").await;
        assert!(result.is_some(), "rtk should route despite 2>&1 redirect");
    }

    #[tokio::test]
    async fn test_rtk_route_with_pipe_and_redirect() {
        let nosb = Box::new(crate::sandbox::NoSandbox);
        let tool = BashTool::new(true, HashMap::new(), vec![], vec![], vec![], nosb);
        // rtk natively handles pipes — it rewrites the left side and preserves
        // shell operators, so compound commands should route through rtk.
        let result = tool.maybe_rtk_route("cargo test 2>&1 | grep error").await;
        assert!(
            result.is_some(),
            "rtk should route pipe commands with redirects"
        );
        let (rewritten, _) = result.unwrap();
        assert!(
            rewritten.starts_with("rtk"),
            "rewritten should start with rtk prefix"
        );
    }

    #[tokio::test]
    async fn test_rtk_route_unsupported_returns_none() {
        let nosb = Box::new(crate::sandbox::NoSandbox);
        let tool = BashTool::new(true, HashMap::new(), vec![], vec![], vec![], nosb);
        let result = tool.maybe_rtk_route("foobar xyz").await;
        assert!(
            result.is_none(),
            "unsupported commands should not route through rtk"
        );
    }
}
