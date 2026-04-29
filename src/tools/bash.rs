//! Bash/shell command execution tool with optional rtk routing.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::base::{Tool, ToolContext, ToolError};

/// rtk supported command prefixes for auto-routing.
const RTK_SUPPORTED_PREFIXES: &[&str] = &[
    "git ", "gh ", "cargo ", "npm ", "pnpm ", "npx ",
    "pytest ", "ruff ", "mypy ", "jest ", "vitest ",
    "tsc ", "next ", "ls ", "tree ", "cat ", "grep ",
    "docker ", "kubectl ", "aws ", "go ", "gcc ", "g++",
    "rustc ", "make ", "cmake ",
];

pub struct BashTool {
    use_rtk: bool,
    timeout_secs: u64,
}

impl BashTool {
    pub fn new(use_rtk: bool) -> Self {
        Self {
            use_rtk,
            timeout_secs: 120,
        }
    }

    /// Check if rtk is available and the command is in the supported list.
    fn maybe_rtk_route(&self, cmd: &str) -> Option<Vec<String>> {
        if !self.use_rtk {
            return None;
        }
        if which::which("rtk").is_err() {
            return None;
        }
        let trimmed = cmd.trim();
        RTK_SUPPORTED_PREFIXES
            .iter()
            .find(|prefix| trimmed.starts_with(*prefix))
            .map(|_| trimmed.split_whitespace().map(String::from).collect())
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
                "description": "Execute a shell command and return its output. Use for running builds, tests, git operations, and any CLI tool.",
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

    async fn execute(
        &self,
        arguments: Value,
        ctx: &ToolContext,
    ) -> Result<String, ToolError> {
        let cmd = arguments["command"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("missing 'command'".into()))?;

        let timeout: u64 = arguments
            .get("timeout")
            .and_then(|t| t.as_u64())
            .unwrap_or(self.timeout_secs);

        // Try rtk routing first
        if let Some(rtk_parts) = self.maybe_rtk_route(cmd) {
            tracing::debug!("Routing through rtk: {:?}", rtk_parts);
            let output = tokio::time::timeout(
                std::time::Duration::from_secs(timeout),
                tokio::process::Command::new("rtk")
                    .args(&rtk_parts)
                    .current_dir(&ctx.cwd)
                    .output(),
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
            tracing::debug!("rtk proxy failed, falling back to raw command");
        }

        // Normal execution via bash
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(timeout),
            tokio::process::Command::new("bash")
                .arg("-c")
                .arg(cmd)
                .current_dir(&ctx.cwd)
                .output(),
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
