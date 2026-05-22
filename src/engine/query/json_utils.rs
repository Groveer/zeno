//! JSON repair and parsing utilities for streaming tool arguments.
//!
//! Streaming tool calls can produce malformed JSON when chunk boundaries
//! split inside JSON syntax. This module provides lightweight repair
//! strategies and fallback parsing.

use serde_json::Value;

/// Try to repair common JSON issues from streaming tool argument concatenation.
///
/// Streaming tool calls can produce malformed JSON when chunk boundaries split
/// inside JSON syntax. This function attempts lightweight repairs:
/// 1. Missing closing `}` — append `}`
/// 2. Trailing comma before `}` — remove the trailing `,`
/// 3. Unclosed string value at end — close with `"`
/// 4. Multiple closing braces — remove extras
pub(super) fn try_repair_json(raw: &str) -> Option<String> {
    let trimmed = raw.trim();

    // Rule 1: extra closing braces — find the right balance point
    let opens = trimmed.chars().filter(|&c| c == '{').count();
    let closes = trimmed.chars().filter(|&c| c == '}').count();
    if closes > opens {
        let mut balance = 0i32;
        let mut last_good = 0;
        for (i, c) in trimmed.char_indices() {
            match c {
                '{' => balance += 1,
                '}' => balance -= 1,
                _ => {}
            }
            if balance == 0 {
                last_good = i + c.len_utf8();
            } else if balance < 0 {
                break;
            }
        }
        if last_good > 0 {
            let candidate = &trimmed[..last_good];
            if serde_json::from_str::<Value>(candidate).is_ok() {
                return Some(candidate.to_string());
            }
        }
    }

    // Rule 2: missing closing brace — append `}`
    if opens > closes {
        let candidate = format!("{}}}", trimmed);
        if serde_json::from_str::<Value>(&candidate).is_ok() {
            return Some(candidate);
        }
    }

    // Rule 3: trailing comma before closing brace or at end
    // Handle both `{"key": "value",}` and `{"key": "value",`
    {
        // Try stripping trailing comma (may be followed by `}`)
        let without_comma = if trimmed.ends_with('}') {
            // Check for comma before the closing brace
            let inner = &trimmed[..trimmed.len() - 1];
            if inner.ends_with(',') {
                format!("{}}}", &inner[..inner.len() - 1])
            } else {
                String::new()
            }
        } else if trimmed.ends_with(',') {
            trimmed[..trimmed.len() - 1].to_string()
        } else {
            String::new()
        };
        if !without_comma.is_empty() {
            if serde_json::from_str::<Value>(&without_comma).is_ok() {
                return Some(without_comma);
            }
        }
    }

    // Also try just removing trailing comma at end
    if trimmed.ends_with(',') {
        let candidate = trimmed.trim_end_matches(',');
        if serde_json::from_str::<Value>(candidate).is_ok() {
            return Some(candidate.to_string());
        }
        let candidate = format!("{}}}", candidate);
        if serde_json::from_str::<Value>(&candidate).is_ok() {
            return Some(candidate);
        }
    }

    // Rule 4: unclosed string at end — find last unclosed string and close it
    let mut in_string = false;
    let mut escape = false;
    for ch in trimmed.chars() {
        if escape {
            escape = false;
            continue;
        }
        match ch {
            '\\' => escape = true,
            '"' => in_string = !in_string,
            _ => {}
        }
    }
    if in_string {
        let candidate = format!("{}\"}}", trimmed);
        if serde_json::from_str::<Value>(&candidate).is_ok() {
            return Some(candidate);
        }
        let candidate = format!("{}\"", trimmed);
        if serde_json::from_str::<Value>(&candidate).is_ok() {
            return Some(candidate);
        }
    }

    None
}

/// Parse tool input JSON with automatic repair on failure.
///
/// Returns `Ok(Value)` on success, or `Err(error_message)` on parse failure.
/// When initial parsing fails, attempts lightweight JSON repair (truncation,
/// trailing comma, unclosed string). On repair success, logs the repair at
/// WARN level with both raw and repaired versions so the root cause can be
/// investigated offline. On repair failure, returns a detailed error including
/// the raw input for debugging.
///
/// Callers in the execution path should return the error directly as a
/// ToolResult; callers in display/history paths should fall back to an
/// empty object.
pub(crate) fn parse_tool_input(input_json: &str) -> Result<Value, String> {
    if input_json.is_empty() {
        return Ok(Value::Object(Default::default()));
    }

    // Fast path: try direct parse first
    if let Ok(v) = serde_json::from_str::<Value>(input_json) {
        return Ok(v);
    }

    // Repair path: try to fix common streaming truncation issues
    if let Some(repaired) = try_repair_json(input_json) {
        tracing::warn!(
            event = "tool_input_repaired",
            raw_len = input_json.len(),
            repaired_len = repaired.len(),
            raw_preview = %input_json.chars().take(120).collect::<String>(),
            repaired_preview = %repaired.chars().take(120).collect::<String>(),
            "Repaired malformed tool input JSON"
        );
        if let Ok(v) = serde_json::from_str::<Value>(&repaired) {
            return Ok(v);
        }
    }

    // Both direct and repair failed — log full details for debugging
    let first_err = serde_json::from_str::<Value>(input_json).unwrap_err();
    tracing::error!(
        event = "tool_input_parse_failed",
        error = %first_err,
        raw_len = input_json.len(),
        raw_first_120 = %input_json.chars().take(120).collect::<String>(),
        "Failed to parse tool input JSON (repair also failed)"
    );
    Err(format!(
        "JSON parse error in tool arguments: {}. \
         Raw input (first 200 chars): {} \
         Check for unescaped characters, unclosed brackets, \
         or string values that should be numbers.",
        first_err,
        input_json.chars().take(200).collect::<String>()
    ))
}

/// Fallback: parse tool input, returning an empty object on failure.
/// Use only in non-critical paths (display, history, parallel safety).
pub(crate) fn parse_tool_input_or_empty(input_json: &str) -> Value {
    parse_tool_input(input_json).unwrap_or_default()
}

/// Truncate a string to at most `max_chars` characters (not bytes).
/// This is safe for multi-byte UTF-8 (CJK, emoji, etc.).
/// Returns the truncated string with "...(truncated)" suffix if truncated.
pub(super) fn safe_truncate_str(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars).collect();
        format!("{}...(truncated)", truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_repair_missing_closing_brace() {
        let result = try_repair_json(r#"{"key": "value""#);
        assert!(result.is_some());
        let v: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(v["key"], "value");
    }

    #[test]
    fn test_repair_trailing_comma() {
        let result = try_repair_json(r#"{"key": "value",}"#);
        assert!(result.is_some());
    }

    #[test]
    fn test_repair_unclosed_string() {
        let result = try_repair_json(r#"{"key": "val"#);
        assert!(result.is_some());
    }

    #[test]
    fn test_parse_empty_input() {
        let result = parse_tool_input("");
        assert!(result.is_ok());
        assert!(result.unwrap().as_object().unwrap().is_empty());
    }

    #[test]
    fn test_parse_valid_json() {
        let result = parse_tool_input(r#"{"command": "ls"}"#);
        assert!(result.is_ok());
    }

    #[test]
    fn test_safe_truncate_str_ascii() {
        assert_eq!(safe_truncate_str("hello", 10), "hello");
        assert_eq!(safe_truncate_str("hello world", 5), "hello...(truncated)");
    }

    #[test]
    fn test_safe_truncate_str_cjk() {
        assert_eq!(safe_truncate_str("你好世界", 2), "你好...(truncated)");
    }
}
