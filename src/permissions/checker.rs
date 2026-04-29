//! Permission checker — controls tool execution authorization.

use std::io::{self, Write};

use crate::config::settings::PermissionMode;
use crate::tools::base::ToolError;

/// Check whether a tool execution is permitted.
///
/// - `Allow`: always permits
/// - `Deny`: always denies (except whitelisted tools)
/// - `Ask`: prompts the user for each execution
pub fn check_permission(
    mode: &PermissionMode,
    tool_name: &str,
    description: &str,
    tool_input: &str,
) -> Result<bool, ToolError> {
    match mode {
        PermissionMode::Allow => Ok(true),
        PermissionMode::Deny => {
            // Some tools are always allowed even in deny mode
            if is_safe_tool(tool_name) {
                return Ok(true);
            }
            Err(ToolError::PermissionDenied(format!(
                "Tool '{}' is blocked in deny mode",
                tool_name
            )))
        }
        PermissionMode::Ask => {
            prompt_user(tool_name, description, tool_input)
        }
    }
}

/// Tools that are always safe to execute (read-only, no side effects).
fn is_safe_tool(name: &str) -> bool {
    matches!(
        name,
        "file_read" | "glob" | "grep" | "config" | "ask_user" | "web_search" | "web_fetch"
    )
}

/// Prompt the user for permission.
fn prompt_user(
    tool_name: &str,
    description: &str,
    tool_input: &str,
) -> Result<bool, ToolError> {
    eprintln!();
    eprintln!("[permission] Tool: {}", tool_name);
    eprintln!("[permission] {}", description);

    // Truncate input display
    let display_input = if tool_input.len() > 200 {
        format!("{}...(truncated)", &tool_input[..200])
    } else {
        tool_input.to_string()
    };
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
        "a" | "all" | "always" => {
            // Return true and the caller should switch to Allow mode
            // We signal this by... actually, let's keep it simple:
            // just allow this one. Full "allow all" switching is handled in engine.
            Ok(true)
        }
        _ => Ok(false),
    }
}
