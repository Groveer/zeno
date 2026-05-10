//! Edit tool — find-and-replace within a file.
//!
//! Implements a 9-strategy fuzzy matching chain (inspired by Hermes Agent)
//! to handle common LLM errors when generating old_string:
//!
//! Strategy chain (tried in order):
//! 1. Exact match
//! 2. Strip line-number prefixes (from read output)
//! 3. Normalize trailing whitespace
//! 4. Indentation shift (±8 spaces)
//! 5. Tab ↔ space normalization
//! 6. Whitespace collapse (multiple spaces → single space)
//! 7. Escape sequence normalization (\\n literal → real newline)
//! 8. Trimmed boundary (trim first/last line only)
//! 9. Block anchor (first+last line exact, middle similarity)
//!
//! Safety features:
//! - Escape-drift detection: prevents `\'` serialization artifacts from corrupting files
//! - Post-write verification: re-reads file to confirm write succeeded
//! - Did-you-mean hints: SequenceMatcher-based closest match suggestions

use async_trait::async_trait;
use serde_json::{Value, json};

use super::base::{Tool, ToolContext, ToolError};

pub struct EditTool;

impl EditTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "edit",
                "description": "Find-and-replace within a file. Uses fuzzy matching to handle common LLM errors with indentation and whitespace. old_string must be unique in the file. Use this for targeted edits instead of rewriting entire files.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path to edit."
                        },
                        "old_string": {
                            "type": "string",
                            "description": "Text to find and replace. Must be unique in the file. Fuzzy matching handles minor differences."
                        },
                        "new_string": {
                            "type": "string",
                            "description": "Replacement text. Use empty string to delete."
                        },
                        "replace_all": {
                            "type": "boolean",
                            "description": "Replace all occurrences instead of just the first (default: false).",
                            "default": false
                        }
                    },
                    "required": ["path", "old_string", "new_string"]
                }
            }
        })
    }

    async fn execute(&self, arguments: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let path = arguments["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("missing 'path'".into()))?;
        let old_string = arguments["old_string"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("missing 'old_string'".into()))?;
        let new_string = arguments["new_string"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("missing 'new_string'".into()))?;
        let replace_all = arguments
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let resolved = ctx.resolve_path(path);

        if !resolved.exists() {
            return Err(ToolError::NotFound(format!(
                "File not found: {}",
                resolved.display()
            )));
        }

        let content = tokio::fs::read_to_string(&resolved).await?;

        if old_string.is_empty() {
            return Err(ToolError::InvalidArguments(
                "old_string cannot be empty".into(),
            ));
        }

        if old_string == new_string {
            return Err(ToolError::InvalidArguments(
                "old_string and new_string are identical — no change needed".into(),
            ));
        }

        // Run the full strategy chain
        let (new_content, match_count, strategy, error) =
            fuzzy_find_and_replace(&content, old_string, new_string, replace_all)?;

        if let Some(err) = error {
            // Generate did-you-mean hint for no-match errors
            let hint = if match_count == 0 && err.starts_with("Could not find") {
                find_closest_lines(old_string, &content)
            } else {
                String::new()
            };
            return Err(ToolError::Execution(format!("{}{}", err, hint)));
        }

        // Write back
        tokio::fs::write(&resolved, &new_content).await?;

        // Post-write verification: re-read and confirm
        match tokio::fs::read_to_string(&resolved).await {
            Ok(verified) if verified == new_content => {}
            Ok(verified) => {
                return Err(ToolError::Execution(format!(
                    "Post-write verification failed for {}: on-disk content differs from intended write (wrote {} chars, read back {}). The patch did not persist. Re-read the file and try again.",
                    resolved.display(),
                    new_content.len(),
                    verified.len()
                )));
            }
            Err(e) => {
                return Err(ToolError::Execution(format!(
                    "Post-write verification failed: could not re-read {}: {}",
                    resolved.display(),
                    e
                )));
            }
        }

        let strategy_info = if strategy != "exact" {
            format!(" [fuzzy: {}]", strategy)
        } else {
            String::new()
        };
        Ok(format!(
            "Replaced {} occurrence(s) in {}{}",
            match_count,
            resolved.display(),
            strategy_info
        ))
    }
}

// ===========================================================================
// Core: fuzzy_find_and_replace — the 9-strategy chain
// ===========================================================================

/// Result of the fuzzy matching chain.
/// On success: (new_content, match_count, strategy_name, None)
/// On failure: (original_content, 0, None, error_message)
pub(crate) type FuzzyResult = (String, usize, &'static str, Option<String>);

pub(crate) fn fuzzy_find_and_replace(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<FuzzyResult, ToolError> {
    // Strategy 1: Exact match
    if let Some(result) = try_exact_replace(content, old_string, new_string, replace_all)? {
        return Ok((result.new_content, result.count, "exact", None));
    }

    // Strategy 2: Strip line-number prefixes
    if let Some(cleaned) = strip_line_number_prefixes(old_string)
        && let Some(result) = try_exact_replace(content, &cleaned, new_string, replace_all)?
    {
        return Ok((result.new_content, result.count, "strip-line-numbers", None));
    }

    // Strategy 3: Normalize trailing whitespace
    {
        let norm_old = normalize_trailing_whitespace(old_string);
        let norm_content = normalize_trailing_whitespace(content);
        if (norm_old != old_string || norm_content != content)
            && let Some(result) =
                try_exact_replace(&norm_content, &norm_old, new_string, replace_all)?
        {
            // Map back: line structure is identical, so we can find
            // the norm_old in content via try_apply_on_original.
            if let Some(result2) = try_apply_on_original(
                content,
                &norm_old,
                new_string,
                replace_all,
                "strip-trailing-ws",
            )? {
                return Ok((
                    result2.new_content,
                    result2.count,
                    "strip-trailing-ws",
                    None,
                ));
            }
            // Fallback: use normalized content result
            return Ok((result.new_content, result.count, "strip-trailing-ws", None));
        }
    }

    // Strategy 4: Indentation shift (±8 spaces)
    if let Some(result) = try_indent_fuzzy(content, old_string, new_string, replace_all)? {
        return Ok((result.new_content, result.count, result.method, None));
    }

    // Strategy 5: Tab ↔ space normalization
    if let Some(result) = try_tab_space_normalize(content, old_string, new_string, replace_all)? {
        return Ok((result.new_content, result.count, result.method, None));
    }

    // Strategy 6: Whitespace collapse (multiple spaces/tabs → single space)
    {
        let norm_old = collapse_whitespace(old_string);
        let norm_content = collapse_whitespace(content);
        if norm_old != old_string || norm_content != content {
            let norm_matches = find_all_exact(&norm_content, &norm_old);
            if !norm_matches.is_empty() {
                if let Some(err) = check_escape_drift(
                    content,
                    &norm_matches,
                    old_string,
                    new_string,
                    "whitespace-normalized",
                ) {
                    return Ok((content.to_string(), 0, "whitespace-normalized", Some(err)));
                }
                if !replace_all && norm_matches.len() > 1 {
                    return Ok((
                        content.to_string(),
                        0,
                        "whitespace-normalized",
                        Some(format!(
                            "Found {} matches for old_string (whitespace-normalized). Provide more context or use replace_all=True.",
                            norm_matches.len()
                        )),
                    ));
                }
                // Try to map back to original positions
                if let Some(result) = map_normalized_to_original(
                    content,
                    &norm_content,
                    &norm_matches,
                    new_string,
                    replace_all,
                    "whitespace-normalized",
                )? {
                    return Ok((
                        result.new_content,
                        result.count,
                        "whitespace-normalized",
                        None,
                    ));
                }
                // Fallback: apply on normalized content directly
                let new_c = apply_byte_matches(&norm_content, &norm_matches, new_string);
                return Ok((new_c, norm_matches.len(), "whitespace-normalized", None));
            }
        }
    }

    // Strategy 7: Escape sequence normalization (\\n → real newline, etc.)
    {
        let unescaped = unescape_common(old_string);
        if unescaped != old_string
            && let Some(result) = try_exact_replace(content, &unescaped, new_string, replace_all)?
        {
            return Ok((result.new_content, result.count, "escape-normalized", None));
        }
    }

    // Strategy 8: Trimmed boundary (trim first/last lines only)
    {
        let old_lines: Vec<&str> = old_string.lines().collect();
        if old_lines.len() >= 2 {
            let content_lines: Vec<&str> = content.lines().collect();
            if let Some(matches) = find_trimmed_boundary_matches(&content_lines, &old_lines) {
                if !replace_all && matches.len() > 1 {
                    return Ok((
                        content.to_string(),
                        0,
                        "trimmed-boundary",
                        Some(format!(
                            "Found {} matches (trimmed-boundary). Provide more context or use replace_all=True.",
                            matches.len()
                        )),
                    ));
                }
                let new_c =
                    apply_line_matches(content, &content_lines, &matches, new_string, old_string);
                return Ok((new_c, matches.len(), "trimmed-boundary", None));
            }
        }
    }

    // Strategy 9: Block anchor (first+last line exact, middle similarity)
    {
        let old_lines: Vec<&str> = old_string.lines().collect();
        if old_lines.len() >= 3 {
            let content_lines: Vec<&str> = content.lines().collect();
            if let Some(matches) =
                find_block_anchor_matches(&content_lines, &old_lines, replace_all)
            {
                let new_c =
                    apply_line_matches(content, &content_lines, &matches, new_string, old_string);
                return Ok((new_c, matches.len(), "block-anchor", None));
            }
        }
    }

    Ok((
        content.to_string(),
        0,
        "none",
        Some("Could not find a match for old_string in the file".into()),
    ))
}

// ===========================================================================
// Exact matching
// ===========================================================================

struct ReplaceResult {
    new_content: String,
    count: usize,
    method: &'static str,
}

fn try_exact_replace(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<Option<ReplaceResult>, ToolError> {
    if replace_all {
        let matches = content.matches(old_string).count();
        if matches == 0 {
            return Ok(None);
        }
        let new_content = content.replace(old_string, new_string);
        Ok(Some(ReplaceResult {
            new_content,
            count: matches,
            method: "exact",
        }))
    } else {
        match content.find(old_string) {
            None => Ok(None),
            Some(idx) => {
                if content[idx + old_string.len()..].contains(old_string) {
                    return Err(ToolError::Execution(
                        "old_string is not unique in the file. Use replace_all=true to replace all occurrences.".into(),
                    ));
                }
                let mut new_content =
                    String::with_capacity(content.len() - old_string.len() + new_string.len());
                new_content.push_str(&content[..idx]);
                new_content.push_str(new_string);
                new_content.push_str(&content[idx + old_string.len()..]);
                Ok(Some(ReplaceResult {
                    new_content,
                    count: 1,
                    method: "exact",
                }))
            }
        }
    }
}

/// Find all exact (non-overlapping) matches of pattern in content.
/// Returns list of (start_byte, end_byte).
fn find_all_exact(content: &str, pattern: &str) -> Vec<(usize, usize)> {
    let mut matches = Vec::new();
    let mut start = 0usize;
    while let Some(pos) = content[start..].find(pattern) {
        let abs = start + pos;
        matches.push((abs, abs + pattern.len()));
        // Advance past the entire match to avoid overlapping matches
        start = abs + pattern.len();
    }
    matches
}

/// Apply replacements at byte ranges (start, end), from end to start to preserve positions.
fn apply_byte_matches(content: &str, matches: &[(usize, usize)], new_string: &str) -> String {
    let mut result = content.to_string();
    // Sort by start position descending
    let mut sorted: Vec<&(usize, usize)> = matches.iter().collect();
    sorted.sort_by_key(|m| std::cmp::Reverse(m.0));
    for &(start, end) in sorted {
        if end > result.len() || start > end {
            continue;
        }
        result = format!("{}{}{}", &result[..start], new_string, &result[end..]);
    }
    result
}

// ===========================================================================
// Strategy helpers: line number stripping, trailing ws, indent, tab/space
// ===========================================================================

/// Strip ` 123 | ` style line-number prefixes from read output.
fn strip_line_number_prefixes(s: &str) -> Option<String> {
    let re = regex_if_available();
    if let Some(re) = re {
        let cleaned = re.replace_all(s, "").to_string();
        if cleaned != s && !cleaned.trim().is_empty() {
            return Some(cleaned);
        }
        return None;
    }
    strip_line_numbers_manual(s)
}

fn regex_if_available() -> Option<regex::Regex> {
    regex::Regex::new(r"(?m)^\s*\d+\s*\|\s?").ok()
}

fn strip_line_numbers_manual(s: &str) -> Option<String> {
    let mut changed = false;
    let mut result = String::with_capacity(s.len());
    for line in s.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix(|c: char| c.is_ascii_digit()) {
            let digits_end = rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len());
            let after_digits = &rest[digits_end..];
            if after_digits.trim_start().starts_with('|') {
                let pipe_content = &after_digits.trim_start()[1..];
                let content = pipe_content.strip_prefix(' ').unwrap_or(pipe_content);
                result.push_str(content);
                changed = true;
                continue;
            }
        }
        result.push_str(line);
    }
    if s.ends_with('\n') && !result.ends_with('\n') {
        result.push('\n');
    }
    if changed && !result.trim().is_empty() {
        Some(result)
    } else {
        None
    }
}

/// Remove trailing whitespace from each line.
fn normalize_trailing_whitespace(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for line in s.lines() {
        result.push_str(line.trim_end());
        result.push('\n');
    }
    if !s.ends_with('\n') && result.ends_with('\n') {
        result.pop();
    }
    result
}

/// Collapse multiple spaces/tabs to single space (preserve newlines).
fn collapse_whitespace(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut prev_was_space = false;
    for ch in s.chars() {
        match ch {
            ' ' | '\t' => {
                if !prev_was_space {
                    result.push(' ');
                    prev_was_space = true;
                }
            }
            '\n' => {
                result.push('\n');
                prev_was_space = false;
            }
            _ => {
                result.push(ch);
                prev_was_space = false;
            }
        }
    }
    result
}

/// Unescape common escape sequences: \\n → newline, \\t → tab, \\r → CR.
fn unescape_common(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if i + 1 < chars.len() && chars[i] == '\\' {
            match chars[i + 1] {
                'n' => {
                    result.push('\n');
                    i += 2;
                    continue;
                }
                't' => {
                    result.push('\t');
                    i += 2;
                    continue;
                }
                'r' => {
                    result.push('\r');
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }
        result.push(chars[i]);
        i += 1;
    }
    result
}

/// Try adjusting indentation of old_string by adding/removing leading spaces.
/// Scans indent_delta from -8 to +8. Also tries on tab→space normalized content.
fn try_indent_fuzzy(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<Option<ReplaceResult>, ToolError> {
    let old_lines: Vec<&str> = old_string.lines().collect();
    if old_lines.is_empty() {
        return Ok(None);
    }

    let min_indent = old_lines
        .iter()
        .filter(|l| !l.is_empty())
        .map(|l| l.chars().take_while(|c| *c == ' ' || *c == '\t').count())
        .min()
        .unwrap_or(0);

    for delta in -8i32..=8 {
        if delta == 0 {
            continue;
        }
        if (min_indent as i32 + delta) < 0 {
            continue;
        }

        let shifted = shift_indent(old_string, delta);
        // When old_string is shifted by delta to match the file, new_string
        // must also be shifted by the same delta to preserve the file's indentation.
        let shifted_new = shift_indent(new_string, delta);
        if let Some(result) =
            try_apply_on_original(content, &shifted, &shifted_new, replace_all, "indent-shift")?
        {
            return Ok(Some(result));
        }
    }

    // If content has tabs but old_string uses spaces, try on normalized content
    if content.contains('\t') && !old_string.contains('\t') {
        let norm_content = content.replace('\t', "    ");
        for delta in -8i32..=8 {
            if delta == 0 {
                continue;
            }
            if (min_indent as i32 + delta) < 0 {
                continue;
            }

            let shifted = shift_indent(old_string, delta);
            // When old_string is shifted by delta to match the file, new_string
            // must also be shifted by the same delta to preserve the file's indentation.
            let shifted_new = shift_indent(new_string, delta);
            if let Some(idx) = norm_content.find(&shifted) {
                if !replace_all && norm_content[idx + shifted.len()..].contains(&shifted) {
                    continue;
                }
                let new_content = if replace_all {
                    norm_content.replace(&shifted, &shifted_new)
                } else {
                    norm_content.replacen(&shifted, &shifted_new, 1)
                };
                let count = if replace_all {
                    norm_content.matches(&shifted).count()
                } else {
                    1
                };
                return Ok(Some(ReplaceResult {
                    new_content,
                    count,
                    method: "indent-shift+tab→space",
                }));
            }
        }
    }

    Ok(None)
}

fn shift_indent(s: &str, delta: i32) -> String {
    let extra = if delta > 0 {
        (delta as usize).max(10) * 10
    } else {
        10
    };
    let mut result = String::with_capacity(s.len() + extra);
    for line in s.lines() {
        if line.is_empty() {
            result.push('\n');
            continue;
        }
        let current_indent = line.chars().take_while(|c| *c == ' ' || *c == '\t').count();
        let new_indent = (current_indent as i32 + delta).max(0) as usize;
        let content_start = line
            .char_indices()
            .nth(current_indent)
            .map(|(i, _)| i)
            .unwrap_or(line.len());
        for _ in 0..new_indent {
            result.push(' ');
        }
        result.push_str(&line[content_start..]);
        result.push('\n');
    }
    if !s.ends_with('\n') && result.ends_with('\n') {
        result.pop();
    }
    result
}

fn try_tab_space_normalize(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<Option<ReplaceResult>, ToolError> {
    let content_has_tabs = content.contains('\t');
    let old_has_tabs = old_string.contains('\t');
    if content_has_tabs == old_has_tabs {
        return Ok(None);
    }

    let norm_content = content.replace('\t', "    ");
    let norm_old = old_string.replace('\t', "    ");

    if try_exact_replace(&norm_content, &norm_old, new_string, replace_all)?.is_some() {
        if let Some(result) = try_apply_on_original(
            content,
            &norm_old,
            new_string,
            replace_all,
            "tab-space-norm",
        )? {
            return Ok(Some(result));
        }

        let new_content = if replace_all {
            norm_content.replace(&norm_old, new_string)
        } else {
            norm_content.replacen(&norm_old, new_string, 1)
        };
        let count = if replace_all {
            norm_content.matches(&norm_old).count()
        } else {
            1
        };
        return Ok(Some(ReplaceResult {
            new_content,
            count,
            method: "tab→space-normalize",
        }));
    }

    Ok(None)
}

fn try_apply_on_original(
    content: &str,
    fuzzy_old: &str,
    new_string: &str,
    replace_all: bool,
    method: &'static str,
) -> Result<Option<ReplaceResult>, ToolError> {
    if replace_all {
        let matches = content.matches(fuzzy_old).count();
        if matches == 0 {
            return Ok(None);
        }
        if matches > 1 {
            return Err(ToolError::Execution(
                "old_string is not unique in the file (fuzzy match). Use replace_all=true to replace all occurrences.".into(),
            ));
        }
        let new_content = content.replace(fuzzy_old, new_string);
        Ok(Some(ReplaceResult {
            new_content,
            count: matches,
            method,
        }))
    } else {
        match content.find(fuzzy_old) {
            None => Ok(None),
            Some(idx) => {
                if content[idx + fuzzy_old.len()..].contains(fuzzy_old) {
                    return Err(ToolError::Execution(
                        "old_string is not unique in the file (fuzzy match). Use replace_all=true to replace all occurrences.".into(),
                    ));
                }
                let mut new_content =
                    String::with_capacity(content.len() - fuzzy_old.len() + new_string.len());
                new_content.push_str(&content[..idx]);
                new_content.push_str(new_string);
                new_content.push_str(&content[idx + fuzzy_old.len()..]);
                Ok(Some(ReplaceResult {
                    new_content,
                    count: 1,
                    method,
                }))
            }
        }
    }
}

// ===========================================================================
// Strategy 6: Whitespace collapse — position mapping
// ===========================================================================

/// Try to map normalized-content matches back to original content positions.
fn map_normalized_to_original(
    content: &str,
    norm_content: &str,
    norm_matches: &[(usize, usize)],
    new_string: &str,
    replace_all: bool,
    _method: &str,
) -> Result<Option<ReplaceResult>, ToolError> {
    // Build a mapping: for each char in norm_content, which range in original it came from.
    // Since collapse_whitespace only collapses spaces/tabs, we can build a char-offset map.
    let mut norm_to_orig: Vec<usize> = Vec::with_capacity(norm_content.len() + 1);
    let mut orig_idx = 0usize;
    let mut norm_idx = 0usize;
    let orig_chars: Vec<char> = content.chars().collect();
    let norm_chars: Vec<char> = norm_content.chars().collect();

    while norm_idx < norm_chars.len() && orig_idx < orig_chars.len() {
        norm_to_orig.push(orig_idx);
        if orig_chars[orig_idx] == norm_chars[norm_idx] {
            orig_idx += 1;
            norm_idx += 1;
        } else if orig_chars[orig_idx] == ' ' || orig_chars[orig_idx] == '\t' {
            // Original has extra whitespace that was collapsed
            orig_idx += 1;
            // Don't advance norm_idx — more whitespace might collapse to same space
            if orig_idx < orig_chars.len()
                && orig_chars[orig_idx] != ' '
                && orig_chars[orig_idx] != '\t'
            {
                norm_idx += 1;
            }
        } else if norm_chars[norm_idx] == ' '
            && (orig_chars[orig_idx] != ' ' && orig_chars[orig_idx] != '\t')
        {
            // Normalized added a space where original didn't have one — shouldn't happen
            norm_idx += 1;
        } else {
            orig_idx += 1;
            norm_idx += 1;
        }
    }
    // Fill remaining
    while norm_to_orig.len() <= norm_chars.len() {
        norm_to_orig.push(orig_idx);
    }

    // Convert normalized match positions to original byte positions
    let mut orig_matches: Vec<(usize, usize)> = Vec::new();
    for (norm_start, norm_end) in norm_matches {
        let orig_start = if *norm_start < norm_to_orig.len() {
            norm_to_orig[*norm_start]
        } else {
            continue;
        };
        let orig_end = if *norm_end < norm_to_orig.len() {
            norm_to_orig[*norm_end]
        } else if *norm_end > 0 && *norm_end - 1 < norm_to_orig.len() {
            norm_to_orig[*norm_end - 1] + 1
        } else {
            continue;
        };

        // Expand orig_end to include trailing whitespace that was collapsed
        let mut expanded_end = orig_end;
        while expanded_end < orig_chars.len()
            && (orig_chars[expanded_end] == ' ' || orig_chars[expanded_end] == '\t')
        {
            expanded_end += 1;
        }

        orig_matches.push((
            char_offset_to_byte(content, orig_start),
            char_offset_to_byte(content, expanded_end),
        ));
    }

    if orig_matches.is_empty() {
        return Ok(None);
    }

    if !replace_all && orig_matches.len() > 1 {
        return Err(ToolError::Execution(
            "old_string matches multiple locations (whitespace-normalized). Provide more context or use replace_all=true.".into(),
        ));
    }

    // Apply replacements from end to start to preserve positions
    let mut result = content.to_string();
    orig_matches.sort_by_key(|m| m.0);
    for (start, end) in orig_matches.iter().rev() {
        if *end > result.len() || *start > *end {
            continue;
        }
        result = format!("{}{}{}", &result[..*start], new_string, &result[*end..]);
    }

    Ok(Some(ReplaceResult {
        new_content: result,
        count: orig_matches.len(),
        method: "whitespace-normalized",
    }))
}

/// Convert a char index to a byte offset in a string.
fn char_offset_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

// ===========================================================================
// Strategy 8: Trimmed boundary matching
// ===========================================================================

/// Find matches where first and last lines match after trimming, middle lines match exactly.
fn find_trimmed_boundary_matches(
    content_lines: &[&str],
    old_lines: &[&str],
) -> Option<Vec<(usize, usize)>> {
    let n = old_lines.len();
    if content_lines.len() < n || n < 2 {
        return None;
    }

    // Build the pattern: first and last lines trimmed, middle lines exact
    let first_trimmed = old_lines[0].trim();
    let last_trimmed = old_lines[n - 1].trim();

    let mut matches = Vec::new();
    for i in 0..=content_lines.len() - n {
        // Check first line (trimmed)
        if content_lines[i].trim() != first_trimmed {
            continue;
        }
        // Check last line (trimmed)
        if content_lines[i + n - 1].trim() != last_trimmed {
            continue;
        }
        // Check middle lines (exact)
        let mut middle_ok = true;
        for j in 1..n - 1 {
            if content_lines[i + j] != old_lines[j] {
                middle_ok = false;
                break;
            }
        }
        if middle_ok {
            matches.push((i, i + n));
        }
    }

    if matches.is_empty() {
        None
    } else {
        Some(matches)
    }
}

// ===========================================================================
// Strategy 9: Block anchor matching
// ===========================================================================

/// Find matches by anchoring on first and last lines (trimmed exact),
/// then checking middle section similarity using a simple ratio.
fn find_block_anchor_matches(
    content_lines: &[&str],
    old_lines: &[&str],
    replace_all: bool,
) -> Option<Vec<(usize, usize)>> {
    let n = old_lines.len();
    if content_lines.len() < n || n < 3 {
        return None;
    }

    let first_trimmed = old_lines[0].trim();
    let last_trimmed = old_lines[n - 1].trim();

    // Collect candidate positions
    let mut candidates = Vec::new();
    for i in 0..=content_lines.len() - n {
        if content_lines[i].trim() == first_trimmed
            && content_lines[i + n - 1].trim() == last_trimmed
        {
            candidates.push(i);
        }
    }

    if candidates.is_empty() {
        return None;
    }

    // Threshold: stricter when multiple candidates
    let threshold = if candidates.len() == 1 { 0.50 } else { 0.70 };

    let mut matches = Vec::new();
    for i in candidates {
        if n <= 2 {
            matches.push((i, i + n));
            continue;
        }
        // Compute similarity of middle section using simple word overlap ratio
        let similarity =
            line_block_similarity(&content_lines[i + 1..i + n - 1], &old_lines[1..n - 1]);
        if similarity >= threshold {
            matches.push((i, i + n));
        }
    }

    if matches.is_empty() {
        return None;
    }

    if !replace_all && matches.len() > 1 {
        // Return None to let the error path handle ambiguity
        return None;
    }

    Some(matches)
}

/// Simple line-block similarity: fraction of lines where trimmed content matches.
fn line_block_similarity(content_block: &[&str], pattern_block: &[&str]) -> f64 {
    if pattern_block.is_empty() {
        return 1.0;
    }
    let mut matches = 0usize;
    for (c_line, p_line) in content_block.iter().zip(pattern_block.iter()) {
        let c_t = c_line.trim();
        let p_t = p_line.trim();
        // Use word-level overlap as similarity
        if c_t == p_t {
            matches += 1;
        } else {
            let sim = word_similarity(c_t, p_t);
            if sim >= 0.6 {
                matches += 1;
            }
        }
    }
    matches as f64 / pattern_block.len() as f64
}

/// Word-level Jaccard similarity between two strings.
fn word_similarity(a: &str, b: &str) -> f64 {
    let words_a: std::collections::HashSet<&str> = a.split_whitespace().collect();
    let words_b: std::collections::HashSet<&str> = b.split_whitespace().collect();
    if words_a.is_empty() && words_b.is_empty() {
        return 1.0;
    }
    if words_a.is_empty() || words_b.is_empty() {
        return 0.0;
    }
    let intersection = words_a.intersection(&words_b).count();
    let union = words_a.union(&words_b).count();
    intersection as f64 / union as f64
}

// ===========================================================================
// Shared helpers: apply line-based matches, escape-drift detection
// ===========================================================================

/// Apply replacements at line ranges. Each match is (start_line, end_line) in 0-based line indices.
///
/// Detects indentation differences between the file's matched region and old_string,
/// then re-indents new_string to match the file's original indentation. This prevents
/// fuzzy strategies (trimmed-boundary, block-anchor) from destroying the file's indentation.
fn apply_line_matches(
    content: &str,
    content_lines: &[&str],
    matches: &[(usize, usize)],
    new_string: &str,
    old_string: &str,
) -> String {
    let mut result = String::with_capacity(content.len());
    let mut last_end = 0usize;

    for &(start_line, end_line) in matches {
        // Calculate byte offset for start_line
        let start_byte = line_byte_offset(content, content_lines, start_line);
        // Calculate byte offset for end_line (start of the line AFTER the match)
        let end_byte = line_end_byte_offset(content, content_lines, end_line);

        result.push_str(&content[last_end..start_byte]);

        // Re-indent new_string to match the file's indentation at the match site
        let adjusted_new_string =
            reindent_for_match(content_lines, start_line, new_string, old_string);
        result.push_str(&adjusted_new_string);

        last_end = end_byte;
    }
    result.push_str(&content[last_end..]);
    result
}

/// Detect the indentation delta between the file's matched region and old_string,
/// then apply that delta to every line of new_string.
///
/// For example, if the file's matched region starts with 8 spaces but old_string
/// starts with 4 spaces, new_string needs +4 spaces on each line.
fn reindent_for_match(
    content_lines: &[&str],
    start_line: usize,
    new_string: &str,
    old_string: &str,
) -> String {
    // Get the indent of the first non-empty content line in the matched region
    let file_indent = content_lines[start_line..]
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.chars().take_while(|c| *c == ' ' || *c == '\t').count())
        .next()
        .unwrap_or(0);

    // Get the indent of the first non-empty line in old_string
    let old_lines: Vec<&str> = old_string.lines().collect();
    let old_indent = old_lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.chars().take_while(|c| *c == ' ' || *c == '\t').count())
        .next()
        .unwrap_or(0);

    let delta = file_indent as i32 - old_indent as i32;

    if delta == 0 {
        return new_string.to_string();
    }

    // Apply delta to every line of new_string
    shift_indent(new_string, delta)
}

/// Byte offset of the start of line `line_idx` (0-based).
///
/// Uses the actual content bytes to determine offsets, correctly handling
/// files that don't end with a trailing newline.
fn line_byte_offset(content: &str, lines: &[&str], line_idx: usize) -> usize {
    let mut offset = 0;
    for (i, line) in lines.iter().enumerate() {
        if i == line_idx {
            return offset;
        }
        offset += line.len();
        // Only count the newline if it actually exists in the content
        if offset < content.len() && content.as_bytes()[offset] == b'\n' {
            offset += 1;
        }
    }
    offset
}

/// Byte offset for the end of line range — start of line `end_line_idx`.
/// If end_line_idx == lines.len(), returns content.len().
fn line_end_byte_offset(content: &str, lines: &[&str], end_line_idx: usize) -> usize {
    if end_line_idx >= lines.len() {
        return content.len();
    }
    line_byte_offset(content, lines, end_line_idx)
}

/// Detect escape-drift artifacts in new_string.
///
/// When the match was found via a non-exact strategy, check if new_string
/// contains `\'` or `\"` sequences that exist in old_string but NOT in the
/// matched region of the file. This indicates tool-call serialization drift
/// where the transport layer added spurious backslashes.
fn check_escape_drift(
    content: &str,
    matches: &[(usize, usize)],
    old_string: &str,
    new_string: &str,
    strategy: &str,
) -> Option<String> {
    if strategy == "exact" {
        return None;
    }

    // Quick check: only run if new_string has suspect sequences
    if !new_string.contains("\\'") && !new_string.contains("\\\"") {
        return None;
    }

    // Collect matched regions from the file
    let matched_regions: String = matches
        .iter()
        .map(|&(start, end)| {
            if end <= content.len() {
                &content[start..end]
            } else {
                ""
            }
        })
        .collect();

    for suspect in &["\\'", "\\\""] {
        if new_string.contains(suspect)
            && old_string.contains(suspect)
            && !matched_regions.contains(suspect)
        {
            let plain = &suspect[1..]; // "'" or "\""
            return Some(format!(
                "Escape-drift detected: old_string and new_string contain the literal sequence '{}' but the matched region of the file does not. This is usually a tool-call serialization artifact where an apostrophe or quote got prefixed with a spurious backslash. Re-read the file with read_file and pass old_string/new_string without backslash-escaping '{}' characters.",
                suspect, plain
            ));
        }
    }

    None
}

// ===========================================================================
// Did-you-mean hint generation (SequenceMatcher-inspired)
// ===========================================================================

/// Find the lines in content most similar to old_string and return a formatted hint.
fn find_closest_lines(old_string: &str, content: &str) -> String {
    let old_lines: Vec<&str> = old_string.lines().collect();
    let content_lines: Vec<&str> = content.lines().collect();

    if old_lines.is_empty() || content_lines.is_empty() {
        return String::new();
    }

    // Use first non-empty trimmed line as anchor
    let anchor = old_lines
        .iter()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("");

    if anchor.is_empty() {
        return String::new();
    }

    // Score each content line by word similarity to anchor
    let mut scored: Vec<(f64, usize)> = Vec::new();
    for (i, line) in content_lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let sim = word_similarity(trimmed, anchor);
        if sim > 0.3 {
            scored.push((sim, i));
        }
    }

    if scored.is_empty() {
        return String::new();
    }

    // Sort by similarity descending, take top 3
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let top: Vec<(f64, usize)> = scored.into_iter().take(3).collect();

    let context = 2; // Lines of context around match
    let mut parts = Vec::new();
    let mut seen_ranges = std::collections::HashSet::new();

    for (_, line_idx) in top {
        let start = line_idx.saturating_sub(context);
        let end = (line_idx + old_lines.len() + context).min(content_lines.len());
        let key = (start, end);
        if seen_ranges.contains(&key) {
            continue;
        }
        seen_ranges.insert(key);

        let mut snippet = String::new();
        #[allow(clippy::needless_range_loop)]
        for j in start..end {
            snippet.push_str(&format!("{:4}| {}\n", j + 1, content_lines[j]));
        }
        parts.push(snippet);
    }

    if parts.is_empty() {
        return String::new();
    }

    format!(
        "\n\nDid you mean one of these sections?\n{}",
        parts.join("---\n")
    )
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- Strategy 6: Whitespace collapse ---

    #[test]
    fn test_collapse_whitespace() {
        assert_eq!(collapse_whitespace("fn  foo()"), "fn foo()");
        assert_eq!(collapse_whitespace("fn\tfoo()"), "fn foo()");
        assert_eq!(collapse_whitespace("fn  \t  foo()"), "fn foo()");
        assert_eq!(collapse_whitespace("fn\n  foo"), "fn\n foo");
    }

    #[test]
    fn test_whitespace_collapse_match() {
        // Content has double spaces between fn and foo, old_string uses single spaces
        let content = "fn  foo() {\n    return  1;\n}";
        let old_string = "fn foo() {\n    return 1;\n}";
        let new_string = "fn bar() {\n    return 1;\n}";
        let (new_content, count, strategy, error) =
            fuzzy_find_and_replace(content, old_string, new_string, false).unwrap();
        assert!(error.is_none(), "Expected no error, got: {:?}", error);
        assert_eq!(strategy, "whitespace-normalized");
        assert_eq!(count, 1);
        assert!(
            new_content.contains("bar"),
            "new_content should contain bar: {}",
            new_content
        );
    }

    // --- Strategy 7: Escape normalization ---

    #[test]
    fn test_unescape_common() {
        assert_eq!(unescape_common("hello\\nworld"), "hello\nworld");
        assert_eq!(unescape_common("tab\\there"), "tab\there");
        assert_eq!(unescape_common("no escape"), "no escape");
    }

    #[test]
    fn test_escape_normalization_match() {
        let content = "hello\nworld";
        let old_string = "hello\\nworld";
        let new_string = "goodbye\nworld";
        let (new_content, count, strategy, error) =
            fuzzy_find_and_replace(content, old_string, new_string, false).unwrap();
        assert!(error.is_none(), "Expected no error, got: {:?}", error);
        assert_eq!(strategy, "escape-normalized");
        assert_eq!(count, 1);
        assert_eq!(new_content, "goodbye\nworld");
    }

    // --- Strategy 8: Trimmed boundary ---

    #[test]
    fn test_trimmed_boundary_match() {
        let content = "  fn foo() {\n    return 1;\n  }";
        let old_string = "fn foo() {\n    return 1;\n}";
        let new_string = "fn bar() {\n    return 1;\n}";
        let (new_content, count, strategy, error) =
            fuzzy_find_and_replace(content, old_string, new_string, false).unwrap();
        assert!(error.is_none(), "Expected no error, got: {:?}", error);
        assert_eq!(strategy, "trimmed-boundary");
        assert_eq!(count, 1);
        assert!(new_content.contains("fn bar()"));
    }

    // --- Strategy 9: Block anchor ---

    #[test]
    fn test_block_anchor_match() {
        let content = "fn foo() {\n    let x = 1;\n    return x;\n}";
        // Old string has slightly different middle lines
        let old_string = "fn foo() {\n    let y = 2;\n    return x;\n}";
        let new_string = "fn bar() {\n    let y = 2;\n    return x;\n}";
        let (new_content, count, strategy, error) =
            fuzzy_find_and_replace(content, old_string, new_string, false).unwrap();
        assert!(error.is_none(), "Expected no error, got: {:?}", error);
        assert_eq!(strategy, "block-anchor");
        assert_eq!(count, 1);
        assert!(new_content.contains("fn bar()"));
    }

    // --- Escape-drift detection ---

    #[test]
    fn test_escape_drift_detection() {
        let content = "let s = 'hello';";
        let matches = vec![(0, content.len())];
        // old/new have \' but file doesn't
        let old_string = "let s = \\'hello\\';";
        let new_string = "let s = \\'world\\';";
        let result = check_escape_drift(
            content,
            &matches,
            old_string,
            new_string,
            "whitespace-normalized",
        );
        assert!(result.is_some(), "Should detect escape drift");
        assert!(result.unwrap().contains("Escape-drift"));
    }

    #[test]
    fn test_no_escape_drift_for_exact() {
        let result = check_escape_drift("x", &[(0, 1)], "x", "y", "exact");
        assert!(result.is_none());
    }

    // --- Did-you-mean hints ---

    #[test]
    fn test_find_closest_lines() {
        let content =
            "fn foo() {\n    let x = 1;\n    return x;\n}\n\nfn bar() {\n    let y = 2;\n}";
        let old_string = "fn foo() {\n    let z = 3;\n    return z;\n}";
        let hint = find_closest_lines(old_string, content);
        assert!(!hint.is_empty(), "Should find a hint");
        assert!(
            hint.contains("Did you mean"),
            "Should contain did-you-mean prefix"
        );
        assert!(
            hint.contains("fn foo()"),
            "Should show the closest matching function"
        );
    }

    // --- Exact match still works ---

    #[test]
    fn test_exact_match_still_works() {
        let content = "hello world";
        let old_string = "hello";
        let new_string = "goodbye";
        let (new_content, count, strategy, error) =
            fuzzy_find_and_replace(content, old_string, new_string, false).unwrap();
        assert!(error.is_none());
        assert_eq!(strategy, "exact");
        assert_eq!(count, 1);
        assert_eq!(new_content, "goodbye world");
    }

    #[test]
    fn test_no_match_gives_error() {
        let content = "hello world";
        let old_string = "something completely different";
        let new_string = "replacement";
        let (_, count, _, error) =
            fuzzy_find_and_replace(content, old_string, new_string, false).unwrap();
        assert_eq!(count, 0);
        assert!(error.is_some());
        assert!(error.unwrap().starts_with("Could not find"));
    }

    // --- Word similarity ---

    #[test]
    fn test_word_similarity_identical() {
        assert_eq!(word_similarity("fn foo()", "fn foo()"), 1.0);
    }

    #[test]
    fn test_word_similarity_partial() {
        let sim = word_similarity("fn foo() {", "fn foo()");
        assert!(sim > 0.5 && sim < 1.0);
    }

    #[test]
    fn test_word_similarity_no_overlap() {
        assert_eq!(word_similarity("abc", "xyz"), 0.0);
    }

    // --- Indentation preservation ---

    #[test]
    fn test_trimmed_boundary_preserves_indentation() {
        // File has 4-space indent, LLM's old_string has no indent
        let content = "    fn foo() {\n        return 1;\n    }";
        let old_string = "fn foo() {\n        return 1;\n}";
        let new_string = "fn bar() {\n        return 1;\n}";
        let (new_content, count, _strategy, error) =
            fuzzy_find_and_replace(content, old_string, new_string, false).unwrap();
        assert!(error.is_none(), "Expected no error, got: {:?}", error);
        assert_eq!(count, 1);
        // The file's 4-space indentation should be preserved on the first and last lines
        assert!(
            new_content.contains("    fn bar()"),
            "First line should preserve 4-space indent: {:?}",
            new_content
        );
        assert!(
            new_content.contains("    }"),
            "Closing brace should preserve 4-space indent: {:?}",
            new_content
        );
    }

    #[test]
    fn test_line_byte_offset_no_trailing_newline() {
        // Verify line_byte_offset works correctly for files without trailing newline
        let content = "line1\nline2\nline3";
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(line_byte_offset(content, &lines, 0), 0);
        assert_eq!(line_byte_offset(content, &lines, 1), 6); // "line1" + '\n'
        assert_eq!(line_byte_offset(content, &lines, 2), 12); // "line1\n" + "line2" + '\n'
    }

    #[test]
    fn test_line_byte_offset_with_trailing_newline() {
        let content = "line1\nline2\nline3\n";
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(line_byte_offset(content, &lines, 0), 0);
        assert_eq!(line_byte_offset(content, &lines, 1), 6);
        assert_eq!(line_byte_offset(content, &lines, 2), 12);
    }

    // --- find_all_exact non-overlapping ---

    #[test]
    fn test_find_all_exact_non_overlapping() {
        // "aaa" contains "aa" — non-overlapping: positions 0..2 only (not 1..3)
        let matches = find_all_exact("aaa", "aa");
        assert_eq!(
            matches.len(),
            1,
            "Should find 1 non-overlapping match in 'aaa'"
        );
        assert_eq!(matches[0], (0, 2));
    }

    #[test]
    fn test_find_all_exact_multiple() {
        let matches = find_all_exact("ababab", "ab");
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0], (0, 2));
        assert_eq!(matches[1], (2, 4));
        assert_eq!(matches[2], (4, 6));
    }

    // --- Strategy 1: Exact with replace_all ---

    #[test]
    fn test_exact_replace_all() {
        let content = "foo bar foo baz foo";
        let (new_content, count, strategy, error) =
            fuzzy_find_and_replace(content, "foo", "qux", true).unwrap();
        assert!(error.is_none());
        assert_eq!(strategy, "exact");
        assert_eq!(count, 3);
        assert_eq!(new_content, "qux bar qux baz qux");
    }

    #[test]
    fn test_exact_not_unique_without_replace_all() {
        let content = "foo bar foo";
        let result = fuzzy_find_and_replace(content, "foo", "qux", false);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("not unique") || err_msg.contains("2 match"),
            "Error should mention non-uniqueness: {}",
            err_msg
        );
    }

    // --- Strategy 2: Line number stripping ---

    #[test]
    fn test_strip_line_number_prefixes() {
        let input = "   1 | fn main() {\n   2 |     println!(\"hi\");\n   3 | }";
        let result = strip_line_number_prefixes(input).unwrap();
        assert_eq!(result, "fn main() {\n    println!(\"hi\");\n}");
    }

    // --- Strategy 3: Trailing whitespace ---

    #[test]
    fn test_trailing_whitespace_match() {
        let content = "fn foo() {\n    return 1;  \n}\n";
        let old_string = "fn foo() {\n    return 1;\n}";
        let new_string = "fn bar() {\n    return 1;\n}";
        let (new_content, count, _, error) =
            fuzzy_find_and_replace(content, old_string, new_string, false).unwrap();
        assert!(error.is_none(), "Expected no error, got: {:?}", error);
        assert_eq!(count, 1);
        assert!(new_content.contains("fn bar()"));
    }
}
