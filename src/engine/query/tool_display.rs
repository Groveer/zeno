//! Tool display formatting — TUI summaries and input previews.
//!
//! Provides compact, human-readable representations of tool calls and results
//! for the terminal UI. The full content goes to the LLM — these are just
//! what the user sees.

use serde_json::Value;

use super::json_utils::parse_tool_input_or_empty;

/// Build a one-line summary for the TUI output, combining tool identity + result.
///
/// Shows compact summaries like:
///   git status → ok (5 lines)
///   rm -rf / → exit 1: Permission denied
///   src/main.rs → L1-L50 (of 300)
pub(super) fn summarize_tool_output(tool_name: &str, output: &str, _input_json: &str) -> String {
    // Error output — show first line with exit code
    if output.starts_with("[exit code:") || output.starts_with("[stderr]") {
        let first_line = output.lines().next().unwrap_or(output);
        let preview: String = first_line.chars().take(80).collect();
        if let Some(code) = output.lines().find_map(|l| {
            l.strip_prefix("[exit code: ")
                .and_then(|s| s.strip_suffix(']'))
        }) {
            return format!("exit {}: {}", code, preview);
        }
        return format!("{}", preview);
    }

    match tool_name {
        "bash" => {
            let line_count = output.lines().count();
            if output.is_empty() || output == "(no output)" {
                "(no output)".to_string()
            } else {
                format!("({} lines)", line_count)
            }
        }
        "read" | "read_file" => summarize_read_output(output),
        "glob" => {
            let first_line = output.lines().next().unwrap_or(output);
            if first_line.starts_with("Found ") {
                first_line.to_string()
            } else {
                let line_count = output.lines().count();
                format!("({} lines)", line_count)
            }
        }
        "grep" => summarize_grep_output(output),
        "web_search" => {
            let result_count = output.lines().filter(|l| !l.is_empty()).count();
            format!("{} results", result_count)
        }
        "web_fetch" => {
            let char_count = output.len();
            format!("{} chars", char_count)
        }
        "edit" => {
            if let Ok(parsed) = serde_json::from_str::<Value>(output)
                && let Some(summary) = parsed.get("summary").and_then(|s| s.as_str())
            {
                return summary.to_string();
            }
            output.to_string()
        }
        "todo" => {
            if let Some(action_line) = output.lines().next() {
                action_line.to_string()
            } else {
                output.to_string()
            }
        }
        "memory" => summarize_memory_output(output),
        "skill_view" => {
            let input = parse_tool_input_or_empty(_input_json);
            let name = input.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let line_count = output.lines().count();
            if let Some(fp) = input.get("file_path").and_then(|v| v.as_str()) {
                format!("{} ({}, {} lines)", name, fp, line_count)
            } else {
                format!("{} ({} lines)", name, line_count)
            }
        }
        "skill_list" => {
            let line_count = output.lines().count();
            let first_line = output.lines().next().unwrap_or("");
            if !first_line.is_empty() {
                first_line.to_string()
            } else {
                format!("({} lines)", line_count)
            }
        }
        "mcp_list_tools" => summarize_mcp_list_tools(output, _input_json),
        "mcp_call_tool" => summarize_mcp_call_tool(output, _input_json),
        "mcp_describe_tool" => {
            let input = parse_tool_input_or_empty(_input_json);
            let server = input
                .get("server_name")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let tool_name = input
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let line_count = output.lines().count();
            format!("{}/{} ({} lines)", server, tool_name, line_count)
        }
        "mcp_list_servers" => {
            let connected = output.lines().filter(|l| l.contains('●')).count();
            let total = output
                .lines()
                .filter(|l| l.contains('●') || l.contains('○'))
                .count();
            if total > 0 {
                format!("{}/{} servers connected", connected, total)
            } else {
                let line_count = output.lines().count();
                format!("({} lines)", line_count)
            }
        }
        "delegate_task" => summarize_delegate_task(output),
        _ => output.to_string(),
    }
}

/// Build a one-line summary of a tool's input for the TUI status display.
/// Uses Nerd Font icons (PUA codepoints) for reliable terminal rendering.
pub(super) fn format_tool_input_summary(tool_name: &str, input_json: &str) -> String {
    let input = parse_tool_input_or_empty(input_json);

    match tool_name {
        "bash" => input
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| format!("\u{f489} $ {}", s.trim()))
            .unwrap_or_else(|| "\u{f489} bash".into()),
        "read" | "read_file" => input
            .get("path")
            .or_else(|| input.get("file_path"))
            .and_then(|v| v.as_str())
            .map(|s| format!("\u{f15c} read {}", s))
            .unwrap_or_else(|| "\u{f15c} read".into()),
        "write" => input
            .get("path")
            .or_else(|| input.get("file_path"))
            .and_then(|v| v.as_str())
            .map(|s| {
                let lines = input
                    .get("content")
                    .and_then(|v| v.as_str())
                    .map(|c| c.lines().count())
                    .unwrap_or(0);
                if lines > 0 {
                    format!("\u{f040} write {} ({} lines)", s, lines)
                } else {
                    format!("\u{f040} write {}", s)
                }
            })
            .unwrap_or_else(|| "\u{f040} write".into()),
        "edit" => {
            let path = input
                .get("path")
                .or_else(|| input.get("file_path"))
                .and_then(|v| v.as_str())
                .unwrap_or("file");
            let replace_all = input
                .get("replace_all")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if replace_all {
                format!("\u{f040} edit {} (replace all)", path)
            } else {
                format!("\u{f040} edit {}", path)
            }
        }
        "grep" => {
            let pattern = input
                .get("pattern")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if path.is_empty() {
                format!("\u{f002} grep {}", pattern)
            } else {
                format!("\u{f002} grep {} in {}", pattern, path)
            }
        }
        "glob" => {
            let pattern = input
                .get("pattern")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if path.is_empty() {
                format!("\u{f07b} glob {}", pattern)
            } else {
                format!("\u{f07b} glob {} in {}", pattern, path)
            }
        }
        "web_search" => input
            .get("query")
            .and_then(|v| v.as_str())
            .map(|s| format!("\u{f0ac} web_search: {}", s))
            .unwrap_or_else(|| "\u{f0ac} web_search".into()),
        "web_fetch" => input
            .get("url")
            .and_then(|v| v.as_str())
            .map(|s| format!("\u{f0c1} web_fetch {}", s))
            .unwrap_or_else(|| "\u{f0c1} web_fetch".into()),
        "skill_list" => {
            let cat = input.get("category").and_then(|v| v.as_str()).unwrap_or("");
            let tag = input.get("tag").and_then(|v| v.as_str()).unwrap_or("");
            if !cat.is_empty() {
                format!("\u{f0ca} skill_list category: {}", cat)
            } else if !tag.is_empty() {
                format!("\u{f0ca} skill_list tag: {}", tag)
            } else {
                "\u{f0ca} skill_list".into()
            }
        }
        "skill_view" => input
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| format!("\u{f06e} skill_view {}", s))
            .unwrap_or_else(|| "\u{f06e} skill_view".into()),
        "ask_user" => "\u{f059} ask_user".into(),
        "memory" => format_memory_summary(&input),
        "todo" => {
            let action = input.get("action").and_then(|v| v.as_str()).unwrap_or("?");
            let icon = match action {
                "create" => "\u{f0ca}",
                "add" => "\u{f067}",
                "update" => "\u{f021}",
                "delete" => "\u{f1f8}",
                "list" => "\u{f15c}",
                _ => "\u{f0ca}",
            };
            format!("{} todo {}", icon, action)
        }
        "delegate_task" => {
            let goal = input.get("goal").and_then(|v| v.as_str()).unwrap_or("");
            if goal.is_empty() {
                "\u{f0c0} delegate_task".into()
            } else {
                let preview: String = goal.chars().take(40).collect();
                format!("\u{f0c0} delegate_task: {}", preview)
            }
        }
        "skill_manage" => {
            let action = input.get("action").and_then(|v| v.as_str()).unwrap_or("?");
            let name = input.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if name.is_empty() {
                format!("\u{f013} skill_manage {}", action)
            } else {
                format!("\u{f013} skill_manage {} {}", action, name)
            }
        }
        n if n.starts_with("mcp_") => {
            let server = input
                .get("server_name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if server.is_empty() {
                format!("\u{f1e6} {}", n)
            } else {
                format!("\u{f1e6} {} on {}", n, server)
            }
        }
        _ => format!("\u{f0ad} {}", tool_name),
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

fn summarize_read_output(output: &str) -> String {
    let mut first_line_num: Option<usize> = None;
    let mut last_line_num: Option<usize> = None;
    let mut total_lines: Option<usize> = None;

    for line in output.lines() {
        let trimmed = line.trim_start();
        if let Some(pipe_pos) = trimmed.find('|') {
            let num_part = trimmed[..pipe_pos].trim();
            if let Ok(num) = num_part.parse::<usize>() {
                if first_line_num.is_none() {
                    first_line_num = Some(num);
                }
                last_line_num = Some(num);
            }
        }
        if let Some(rest) = line.strip_prefix("(showing lines ")
            && let Some(end_part) = rest.strip_suffix(" total)")
        {
            let parts: Vec<&str> = end_part.splitn(2, " of ").collect();
            if parts.len() == 2 {
                total_lines = parts[1].trim().parse().ok();
            }
        }
    }

    match (first_line_num, last_line_num, total_lines) {
        (Some(first), Some(last), Some(total)) => {
            if first == 1 && last == total {
                format!("({} lines, full)", total)
            } else {
                format!("L{}-L{} (of {})", first, last, total)
            }
        }
        (Some(first), Some(last), None) => format!("L{}-L{}", first, last),
        _ => {
            let line_count = output.lines().count();
            format!("({} lines)", line_count)
        }
    }
}

fn summarize_grep_output(output: &str) -> String {
    let first_line = output.lines().next().unwrap_or(output);
    if first_line.starts_with("Found ") {
        let file_count: usize = output
            .lines()
            .skip(1)
            .filter(|l| !l.is_empty())
            .filter_map(|l| l.split(':').next())
            .filter(|file| !file.is_empty())
            .collect::<std::collections::HashSet<&str>>()
            .len();
        if file_count > 0 {
            format!("{} ({} file(s))", first_line, file_count)
        } else {
            first_line.to_string()
        }
    } else {
        let line_count = output.lines().count();
        format!("({} lines)", line_count)
    }
}

fn summarize_memory_output(output: &str) -> String {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(output) {
        let success = val
            .get("success")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let target = val
            .get("target")
            .and_then(|v| v.as_str())
            .unwrap_or("memory");
        let entry_count = val.get("entry_count").and_then(|v| v.as_u64()).unwrap_or(0);
        let usage = val.get("usage").and_then(|v| v.as_str()).unwrap_or("");
        let message = val.get("message").and_then(|v| v.as_str());
        let error = val.get("error").and_then(|v| v.as_str());

        if !success {
            if let Some(err) = error {
                return format!("memory {} error: {}", target, err);
            }
            return format!("memory {} failed", target);
        }

        let mut result = if let Some(msg) = message {
            format!("memory {}: {}", target, msg)
        } else {
            format!("memory {} ok", target)
        };

        if !usage.is_empty() {
            result.push_str(&format!(" ({})", usage));
        }

        if let Some(entries) = val.get("entries").and_then(|v| v.as_array())
            && !entries.is_empty()
        {
            result.push_str(&format!("\n  {} entries:", entry_count));
            for entry in entries.iter().take(5) {
                if let Some(text) = entry.as_str() {
                    let preview: String = text.chars().take(80).collect();
                    if text.len() > preview.len() {
                        result.push_str(&format!("\n  · {}…", preview));
                    } else {
                        result.push_str(&format!("\n  · {}", preview));
                    }
                }
            }
            if entries.len() > 5 {
                result.push_str(&format!("\n  … and {} more", entries.len() - 5));
            }
        }

        result
    } else {
        output.to_string()
    }
}

fn summarize_mcp_list_tools(output: &str, input_json: &str) -> String {
    let input = parse_tool_input_or_empty(input_json);
    let server = input
        .get("server_name")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let tool_count = output.lines().filter(|l| l.starts_with('-')).count();
    if tool_count > 0 {
        format!("{}: {} tool(s)", server, tool_count)
    } else {
        let line_count = output.lines().count();
        format!("{}: ({} lines)", server, line_count)
    }
}

fn summarize_mcp_call_tool(output: &str, input_json: &str) -> String {
    let input = parse_tool_input_or_empty(input_json);
    let server = input
        .get("server_name")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let tool = input
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let line_count = output.lines().count();
    let char_count = output.len();
    if char_count > 200 {
        format!(
            "{}/{} ✓ ({} lines, {} chars)",
            server, tool, line_count, char_count
        )
    } else {
        format!("{}/{} ✓ ({} lines)", server, tool, line_count)
    }
}

fn summarize_delegate_task(output: &str) -> String {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(output) {
        if let Some(error) = val.get("error").and_then(|v| v.as_str()) {
            return format!("delegate_task: {}", error);
        }
        if let Some(results) = val.as_array() {
            let total = results.len();
            let success_count = results
                .iter()
                .filter(|r| {
                    r.get("exit_reason")
                        .and_then(|v| v.as_str())
                        .map(|s| s == "success" || s == "completed")
                        .unwrap_or(false)
                })
                .count();
            return format!("{}/{} tasks completed", success_count, total);
        }
        if let Some(summary) = val.get("summary").and_then(|v| v.as_str()) {
            let preview: String = summary.chars().take(80).collect();
            if summary.len() > preview.len() {
                return format!("delegate_task: {}…", preview);
            }
            return format!("delegate_task: {}", preview);
        }
        if let Some(reason) = val.get("exit_reason").and_then(|v| v.as_str()) {
            return format!("delegate_task: exit_reason={}", reason);
        }
    }
    let line_count = output.lines().count();
    format!("delegate_task ({} lines)", line_count)
}

fn format_memory_summary(input: &Value) -> String {
    let action = input.get("action").and_then(|v| v.as_str()).unwrap_or("?");
    let target = input
        .get("target")
        .and_then(|v| v.as_str())
        .unwrap_or("memory");
    match action {
        "add" => {
            let content = input.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let preview: String = content.chars().take(60).collect();
            if content.len() > preview.len() {
                format!("\u{f1c0} memory {} add: {}…", target, preview)
            } else {
                format!("\u{f1c0} memory {} add: {}", target, preview)
            }
        }
        "replace" => {
            let old = input.get("old_text").and_then(|v| v.as_str()).unwrap_or("");
            let content = input.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let old_preview: String = old.chars().take(30).collect();
            let new_preview: String = content.chars().take(30).collect();
            format!(
                "\u{f040} memory {} replace: {} → {}",
                target, old_preview, new_preview
            )
        }
        "remove" => {
            let old = input.get("old_text").and_then(|v| v.as_str()).unwrap_or("");
            let preview: String = old.chars().take(40).collect();
            format!("\u{f1f8} memory {} remove: {}", target, preview)
        }
        _ => format!("\u{f1c0} memory {}", action),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_summarize_mcp_call_tool_short() {
        let result = summarize_tool_output(
            "mcp_call_tool",
            "hello world",
            r#"{"server_name":"playwright","tool_name":"screenshot"}"#,
        );
        assert_eq!(result, "playwright/screenshot ✓ (1 lines)");
    }

    #[test]
    fn test_summarize_mcp_call_tool_long() {
        let long = "x".repeat(250);
        let result = summarize_tool_output(
            "mcp_call_tool",
            &long,
            r#"{"server_name":"filesystem","tool_name":"read"}"#,
        );
        assert_eq!(result, "filesystem/read ✓ (1 lines, 250 chars)");
    }

    #[test]
    fn test_summarize_mcp_call_tool_empty() {
        let result = summarize_tool_output(
            "mcp_call_tool",
            "",
            r#"{"server_name":"playwright","tool_name":"screenshot"}"#,
        );
        assert_eq!(result, "playwright/screenshot ✓ (0 lines)");
    }

    #[test]
    fn test_summarize_mcp_call_tool_missing_input() {
        let result = summarize_tool_output("mcp_call_tool", "some output", "");
        assert_eq!(result, "?/? ✓ (1 lines)");
    }

    #[test]
    fn test_summarize_mcp_call_tool_multi_line() {
        let output = "line1\nline2\nline3\nline4\nline5";
        let result = summarize_tool_output(
            "mcp_call_tool",
            output,
            r#"{"server_name":"git","tool_name":"status"}"#,
        );
        assert_eq!(result, "git/status ✓ (5 lines)");
    }
}
