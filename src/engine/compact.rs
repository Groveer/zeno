//! Conversation history auto-compaction system.
//!
//! Three levels:
//! 1. Micro-compact: cheap, no LLM call. Truncates old tool_result content.
//! 2. Context collapse: deterministic TextBlock truncation (pre-full-compact).
//! 3. Full compact: calls auxiliary LLM to summarize older messages.
//! 4. Reactive compact: triggered when API returns "prompt too long" errors.
//!    Includes PTL retry — if the compact request itself is too long,
//!    truncates the oldest prompt rounds and retries (up to 3 times).
//!
//! Full compact injects carryover working memory into the compression
//! prompt so the LLM preserves key facts (files read, artifacts modified,
//! work done) even after the conversation history is compressed.

use crate::api::types::ContentBlock;
use crate::api::types::Role;
use crate::config::settings::Settings;
use crate::engine::carryover::Carryover;
use crate::engine::messages::{ConversationEntry, ConversationHistory, find_safe_split_point};
use serde_json::Value;
// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Max retries when the compact request itself is "prompt too long".
const MAX_PTL_RETRIES: usize = 3;

/// Marker text injected when PTL retry truncates oldest rounds.
const PTL_RETRY_MARKER: &str = "[earlier conversation truncated for compaction retry]";

// Context collapse thresholds
const CONTEXT_COLLAPSE_CHAR_LIMIT: usize = 2400;
const CONTEXT_COLLAPSE_HEAD_CHARS: usize = 900;
const CONTEXT_COLLAPSE_TAIL_CHARS: usize = 500;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum CompactError {
    #[error("Compression LLM call failed: {0}")]
    LlmFailed(String),
}

#[derive(Debug)]
pub enum CompactMethod {
    Micro,
    Full,
}

#[derive(Debug)]
pub struct CompactResult {
    pub method: CompactMethod,
    pub tokens_before: usize,
    pub tokens_after: usize,
}

pub struct CompactConfig {
    /// Fraction (0.0–1.0) of the model context window that triggers compaction.
    /// E.g., 0.33 = compact when estimated tokens exceed 33% of context window.
    /// Set to 0.0 to disable auto-compact entirely.
    pub threshold_ratio: f64,
    pub keep_recent: usize,
    pub enabled: bool,
}

impl Default for CompactConfig {
    fn default() -> Self {
        Self {
            threshold_ratio: 0.33,
            keep_recent: 3,
            enabled: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Token estimation (CJK-aware)
// ---------------------------------------------------------------------------

/// Estimate token count with CJK and code-density awareness.
///
/// ASCII text: ~3.5 chars per token (was 4 — 3.5 is safer for code-dense text).
/// Code-heavy text (many braces, operators): ~2.5 chars per token.
/// CJK text: ~1.5 chars per token (each CJK char ≈ 1-2 tokens).
///
/// The more conservative ratios (3.5 vs 4, 2.5 vs 4) reduce the risk of
/// underestimation that leads to API "prompt too long" errors. Code is
/// particularly token-dense: `fn main() {` (12 chars) ≈ 6-7 tokens.
/// Overestimation is safer than underestimation — early compaction is
/// better than an API error that wastes a round-trip.
pub fn estimate_tokens(history: &ConversationHistory) -> usize {
    let mut ascii_chars: usize = 0;
    let mut code_chars: usize = 0;
    let mut cjk_count: usize = 0;

    for entry in history.entries_raw() {
        for block in &entry.content {
            match block {
                ContentBlock::Text { text } | ContentBlock::ToolResult { content: text, .. } => {
                    let mut lines = text.lines().peekable();
                    while let Some(line) = lines.next() {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        // Code-density heuristic: count special chars
                        // Lines with many braces/operators/semicolons are likely code.
                        let code_special = trimmed
                            .chars()
                            .filter(|&c| {
                                matches!(
                                    c,
                                    '{' | '}'
                                        | '('
                                        | ')'
                                        | ';'
                                        | '='
                                        | '>'
                                        | '<'
                                        | '|'
                                        | '&'
                                        | '*'
                                        | '/'
                                        | '['
                                        | ']'
                                )
                            })
                            .count();
                        let line_len = trimmed.len();
                        // If >20% of chars are code-special chars, treat as code
                        if line_len > 4 && code_special * 100 > line_len * 20 {
                            code_chars += line_len;
                        } else {
                            for ch in trimmed.chars() {
                                if is_cjk(ch) {
                                    cjk_count += 1;
                                } else {
                                    ascii_chars += 1;
                                }
                            }
                        }
                    }
                }
                ContentBlock::ToolUse { name, .. } => {
                    ascii_chars += name.len() + 50; // rough overhead
                }
                ContentBlock::Image { .. } => ascii_chars += 4000, // images cost ~1000 tokens
            }
        }
    }

    // ASCII: ~3.5 chars/token (was 4 — safer against prompt-too-long errors)
    // Code-heavy: ~2.5 chars/token (code is more token-dense)
    // CJK: ~1.5 chars/token
    (ascii_chars / 4) + (code_chars * 2 / 5) + (cjk_count * 2 / 3)
}

/// Check if a character is CJK (Chinese, Japanese, Korean).
fn is_cjk(ch: char) -> bool {
    matches!(
        ch,
        '\u{4E00}'..='\u{9FFF}'    // CJK Unified Ideographs
        | '\u{3400}'..='\u{4DBF}'  // CJK Unified Ideographs Extension A
        | '\u{20000}'..='\u{2A6DF}' // CJK Unified Ideographs Extension B
        | '\u{F900}'..='\u{FAFF}'  // CJK Compatibility Ideographs
        | '\u{2F800}'..='\u{2FA1F}' // CJK Compatibility Ideographs Supplement
        | '\u{3000}'..='\u{303F}'  // CJK Symbols and Punctuation
        | '\u{3040}'..='\u{309F}'  // Hiragana
        | '\u{30A0}'..='\u{30FF}'  // Katakana
        | '\u{AC00}'..='\u{D7AF}'  // Hangul Syllables
        | '\u{FF00}'..='\u{FFEF}'  // Fullwidth Forms
    )
}

// ---------------------------------------------------------------------------
// Micro-compact
// ---------------------------------------------------------------------------

/// Truncate old tool_result content. Keep recent entries intact.
/// For entries beyond `keep_recent` from the end, truncate ToolResult content to `max_chars`.
pub fn micro_compact(history: &mut ConversationHistory, keep_recent: usize) -> bool {
    history.truncate_old_tool_results(keep_recent, 200)
}

// ---------------------------------------------------------------------------
// Context collapse (deterministic, before full compact)
// ---------------------------------------------------------------------------

/// Deterministically shrink oversized TextBlocks in older messages.
///
/// For TextBlocks exceeding `CONTEXT_COLLAPSE_CHAR_LIMIT`, keep the head
/// and tail and replace the middle with a `[collapsed N chars]` marker.
/// This reduces the token count sent to the compression LLM and lowers
/// the risk of the compact request itself exceeding the context window.
///
/// Only modifies entries *before* the `keep_recent` boundary (using
/// `find_safe_split_point` to preserve tool_use/result pairs).
pub fn context_collapse(history: &mut ConversationHistory, keep_recent: usize) -> bool {
    let entries = history.entries_raw();
    let total = entries.len();
    if total <= keep_recent + 2 {
        return false;
    }

    let cutoff = find_safe_split_point(entries, keep_recent);
    if cutoff == 0 {
        return false;
    }

    let mut changed = false;
    let entries = history.entries_mut();

    for entry in entries.iter_mut().take(cutoff) {
        for block in &mut entry.content {
            if let ContentBlock::Text { text } = block
                && text.len() > CONTEXT_COLLAPSE_CHAR_LIMIT
            {
                // Use char boundaries — byte-slicing a UTF-8 string can panic
                // when the offset falls in the middle of a multi-byte char.
                let head_end = text
                    .char_indices()
                    .nth(CONTEXT_COLLAPSE_HEAD_CHARS)
                    .map_or(text.len(), |(i, _)| i);
                let tail_start = text
                    .char_indices()
                    .nth_back(CONTEXT_COLLAPSE_TAIL_CHARS)
                    .map_or(0, |(i, _)| i);
                let head = text[..head_end].trim_end();
                let tail = text[tail_start..].trim_start();
                let omitted = tail_start - head_end;
                *text = format!("{}\n...[collapsed {} chars]...\n{}", head, omitted, tail);
                changed = true;
            }
        }
    }

    changed
}

// ---------------------------------------------------------------------------
// Prompt-too-long detection
// ---------------------------------------------------------------------------

/// Check if an API error indicates the prompt was too long.
pub fn is_prompt_too_long_error(error: &crate::api::types::ApiError) -> bool {
    match error {
        crate::api::types::ApiError::Http { body, .. } => {
            let b = body.to_lowercase();
            b.contains("prompt too long")
                || b.contains("context length")
                || b.contains("too many tokens")
                || b.contains("maximum context")
                || b.contains("too large for the model")
        }
        _ => false,
    }
}

/// Check if a compact error message indicates the prompt was too long.
fn is_prompt_too_long_msg(msg: &str) -> bool {
    let b = msg.to_lowercase();
    b.contains("prompt too long")
        || b.contains("context length")
        || b.contains("too many tokens")
        || b.contains("maximum context")
        || b.contains("too large for the model")
}

// ---------------------------------------------------------------------------
// Prompt-round grouping (for PTL retry)
// ---------------------------------------------------------------------------

/// Group messages by "prompt round" — a new round starts when a user
/// message contains non-tool-result text (i.e., actual user input).
/// Tool-result-only user messages belong to the same round as the
/// preceding assistant tool_use.
fn group_messages_by_prompt_round(entries: &[ConversationEntry]) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();

    for (i, entry) in entries.iter().enumerate() {
        let starts_new_round = entry.role == Role::User
            && !entry
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
            && entry
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text } if !text.trim().is_empty()));

        if starts_new_round && !current.is_empty() {
            groups.push(current);
            current = Vec::new();
        }
        current.push(i);
    }

    if !current.is_empty() {
        groups.push(current);
    }

    groups
}

/// Truncate the oldest prompt rounds when the compact request itself
/// is too long ("PTL retry"). Drops roughly 1/5 of the oldest rounds
/// and injects a marker message to maintain conversation coherence.
///
/// Returns `true` if truncation was performed, `false` if not possible.
pub fn truncate_head_for_ptl_retry(history: &mut ConversationHistory) -> bool {
    let entries = history.entries_raw();
    let groups = group_messages_by_prompt_round(entries);
    if groups.len() < 2 {
        return false;
    }

    // Drop at least 1 group, at most 1/5 of total groups
    let drop_count = std::cmp::max(1, groups.len() / 5);
    let drop_count = std::cmp::min(drop_count, groups.len() - 1);

    // Collect all indices to drop
    let drop_indices: std::collections::HashSet<usize> = groups[..drop_count]
        .iter()
        .flat_map(|g| g.iter().copied())
        .collect();

    let entries = history.entries_mut();
    let retained: Vec<ConversationEntry> = entries
        .iter()
        .enumerate()
        .filter(|(i, _)| !drop_indices.contains(i))
        .map(|(_, e)| e.clone())
        .collect();

    if retained.is_empty() {
        return false;
    }

    // If the first retained entry is an assistant message, prepend a
    // synthetic user marker so the API doesn't reject the sequence.
    *entries = if retained[0].role == Role::Assistant {
        let marker = ConversationEntry {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: PTL_RETRY_MARKER.to_string(),
            }],
        };
        std::iter::once(marker).chain(retained).collect()
    } else {
        retained
    };

    true
}

// ---------------------------------------------------------------------------
// Full compact (LLM-based)
// ---------------------------------------------------------------------------

/// Full compact: uses auxiliary LLM to summarize older messages.
///
/// Injects carryover working memory into the compression prompt so
/// the LLM preserves key facts even after the history is compressed.
/// Also asks the external memory provider to extract insights from
/// the messages about to be discarded (via on_pre_compress).
/// Keeps the last `keep_recent` messages intact, compresses everything before.
pub async fn full_compact(
    settings: &Settings,
    history: &mut ConversationHistory,
    keep_recent: usize,
    carryover: &Carryover,
    memory_manager: Option<&crate::memory::manager::SharedMemoryManager>,
) -> Result<(), CompactError> {
    let carryover_ctx = if carryover.has_data() {
        Some(carryover.to_context_text())
    } else {
        None
    };

    let carryover_ref = carryover_ctx.as_deref();

    // Ask external memory provider to extract insights from messages
    // about to be compressed, so they can be preserved in the summary.
    let memory_insight = if let Some(mm) = memory_manager {
        let entries = history.entries_raw();
        let cutoff = crate::engine::messages::find_safe_split_point(entries, keep_recent);
        if cutoff > 0 {
            let messages_json: Vec<Value> = entries[..cutoff]
                .iter()
                .filter_map(|e| serde_json::to_value(e).ok())
                .collect();
            let mm = mm.lock().await;
            mm.on_pre_compress(&messages_json)
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    // Combine carryover context with memory provider insight
    let combined_context = match (carryover_ref, memory_insight.as_str()) {
        (Some(carry), _) if !memory_insight.is_empty() => Some(format!(
            "{}\n\n## External memory insights\n\n{}",
            carry, memory_insight
        )),
        (Some(carry), _) => Some(carry.to_string()),
        (None, _) if !memory_insight.is_empty() => {
            Some(format!("## External memory insights\n\n{}", memory_insight))
        }
        (None, _) => None,
    };
    let combined_ref = combined_context.as_deref();

    let summary = crate::auxiliary::compressor::compress_history(settings, history, combined_ref)
        .await
        .map_err(|e| CompactError::LlmFailed(e.to_string()))?;

    history.replace_with_summary(summary, keep_recent);
    Ok(())
}

// ---------------------------------------------------------------------------
// Auto-compact trigger
// ---------------------------------------------------------------------------

/// Estimate token count for extra context that `estimate_tokens(history)` misses:
/// the system prompt and tool schemas.
///
/// Tool schemas can be substantial (20-30K tokens with many tools + MCP servers).
/// Without including them, auto-compact may never trigger even when the real
/// API prompt already exceeds the threshold.
///
/// Uses the same CJK-aware estimation as `estimate_tokens` for consistency.
fn estimate_extra_tokens(system_prompt: &str, tool_schemas: &[serde_json::Value]) -> usize {
    // System prompt: CJK-aware estimation
    let mut ascii_bytes: usize = 0;
    let mut cjk_count: usize = 0;
    for ch in system_prompt.chars() {
        if is_cjk(ch) {
            cjk_count += 1;
        } else {
            ascii_bytes += ch.len_utf8();
        }
    }

    // Tool schemas: JSON serialised length / 4 (schemas are almost always ASCII)
    let schema_tokens: usize = tool_schemas
        .iter()
        .map(|v| {
            let json_str = serde_json::to_string(v).unwrap_or_default();
            json_str.len() / 4
        })
        .sum();

    (ascii_bytes / 4) + (cjk_count * 2 / 3) + schema_tokens
}

/// Check if auto-compact is needed and perform it.
/// 1. Try micro-compact first
/// 2. If still over threshold, context-collapse then try full LLM compact
/// 3. If full compact itself gets "prompt too long", PTL retry (up to 3x)
///
/// `system_prompt` and `tool_schemas` are needed for accurate token estimation —
/// they consume context window space but are not part of the message history.
pub async fn auto_compact_if_needed(
    settings: &Settings,
    history: &mut ConversationHistory,
    config: &CompactConfig,
    carryover: &Carryover,
    context_window: u32,
    memory_manager: Option<&crate::memory::manager::SharedMemoryManager>,
    system_prompt: &str,
    tool_schemas: &[serde_json::Value],
) -> Result<Option<CompactResult>, CompactError> {
    if !config.enabled || config.threshold_ratio <= 0.0 {
        return Ok(None);
    }

    let threshold_tokens = ((context_window as f64) * config.threshold_ratio) as usize;
    let extra_tokens = estimate_extra_tokens(system_prompt, tool_schemas);
    let tokens = estimate_tokens(history) + extra_tokens;
    if tokens < threshold_tokens {
        return Ok(None);
    }

    // Step 1: micro-compact (cheap, no LLM call)
    let _was_micro = micro_compact(history, config.keep_recent);
    let new_tokens = estimate_tokens(history) + extra_tokens;

    if new_tokens < threshold_tokens {
        tracing::info!(
            compact_method = "micro",
            compact_trigger = "auto",
            tokens_before = tokens,
            tokens_after = new_tokens,
            threshold = threshold_tokens,
            keep_recent = config.keep_recent,
            "Auto-compact completed (micro)"
        );
        return Ok(Some(CompactResult {
            method: CompactMethod::Micro,
            tokens_before: tokens,
            tokens_after: new_tokens,
        }));
    }

    // Step 2: context collapse (deterministic, reduces LLM input size)
    context_collapse(history, config.keep_recent);

    // Step 3: full compact with PTL retry
    let mut attempt = 0;
    loop {
        match full_compact(
            settings,
            history,
            config.keep_recent,
            carryover,
            memory_manager,
        )
        .await
        {
            Ok(()) => {
                let final_tokens = estimate_tokens(history) + extra_tokens;
                tracing::info!(
                    compact_method = "full",
                    compact_trigger = "auto",
                    tokens_before = tokens,
                    tokens_after = final_tokens,
                    threshold = threshold_tokens,
                    keep_recent = config.keep_recent,
                    "Auto-compact completed (full LLM)"
                );
                return Ok(Some(CompactResult {
                    method: CompactMethod::Full,
                    tokens_before: tokens,
                    tokens_after: final_tokens,
                }));
            }
            Err(CompactError::LlmFailed(ref msg)) if is_prompt_too_long_msg(msg) => {
                attempt += 1;
                if attempt > MAX_PTL_RETRIES {
                    tracing::error!(
                        compact_method = "full",
                        compact_trigger = "auto",
                        ptl_retry_attempt = attempt,
                        max_ptl_retries = MAX_PTL_RETRIES,
                        tokens_before = tokens,
                        "PTL retry exhausted after {} attempts, giving up",
                        attempt
                    );
                    return Err(CompactError::LlmFailed(msg.clone()));
                }

                let truncated = truncate_head_for_ptl_retry(history);
                if !truncated {
                    tracing::warn!(
                        compact_method = "full",
                        compact_trigger = "auto",
                        ptl_retry_attempt = attempt,
                        "PTL retry: cannot truncate further, giving up"
                    );
                    return Err(CompactError::LlmFailed(msg.clone()));
                }

                tracing::warn!(
                    compact_method = "full",
                    compact_trigger = "auto",
                    ptl_retry_attempt = attempt,
                    max_ptl_retries = MAX_PTL_RETRIES,
                    "Compact request was too long, PTL retry #{} (truncated oldest rounds)",
                    attempt
                );
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cjk_token_estimation() {
        // Pure ASCII
        let mut h = ConversationHistory::new();
        h.push_user("Hello world test"); // 17 chars → ~4 tokens
        let tokens = estimate_tokens(&h);
        assert!(tokens > 0);

        // With CJK — should estimate higher token count per char
        let mut h2 = ConversationHistory::new();
        h2.push_user("你好世界"); // 4 CJK chars → ~6 tokens
        let tokens2 = estimate_tokens(&h2);
        assert!(tokens2 > 0);
        // CJK should have more tokens per char than ASCII
        let ratio_ascii = tokens as f64 / 17.0;
        let ratio_cjk = tokens2 as f64 / 4.0;
        assert!(
            ratio_cjk > ratio_ascii,
            "CJK should have higher token/char ratio"
        );
    }

    #[test]
    fn test_is_cjk() {
        assert!(is_cjk('你'));
        assert!(is_cjk('の'));
        assert!(is_cjk('한'));
        assert!(!is_cjk('a'));
        assert!(!is_cjk('1'));
    }

    #[test]
    fn test_context_collapse_short_text_unchanged() {
        let mut h = ConversationHistory::new();
        h.push_user("short text");
        h.push_assistant_blocks(vec![ContentBlock::Text { text: "ok".into() }]);
        h.push_user("another prompt");

        // All text is short — collapse should not modify anything
        let changed = context_collapse(&mut h, 1);
        assert!(!changed);
    }

    #[test]
    fn test_context_collapse_long_text_truncated() {
        let mut h = ConversationHistory::new();
        // Create a long text entry (> 2400 chars)
        let long_text = "x".repeat(3000);
        h.push_user(&long_text);
        h.push_assistant_blocks(vec![ContentBlock::Text {
            text: "response".into(),
        }]);
        h.push_user("prompt 2");
        h.push_assistant_blocks(vec![ContentBlock::Text {
            text: "response 2".into(),
        }]);
        h.push_user("keep this");

        let changed = context_collapse(&mut h, 1);
        assert!(changed);

        // The first entry's text should now contain "[collapsed"
        let entries = h.entries_raw();
        let first_text = entries[0]
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert!(first_text.contains("[collapsed"));
    }

    #[test]
    fn test_group_messages_by_prompt_round() {
        let mut h = ConversationHistory::new();
        h.push_user("prompt 1");
        h.push_assistant_blocks(vec![ContentBlock::Text {
            text: "answer 1".into(),
        }]);
        h.push_user("prompt 2");
        h.push_assistant_blocks(vec![ContentBlock::Text {
            text: "answer 2".into(),
        }]);

        let groups = group_messages_by_prompt_round(h.entries_raw());
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].len(), 2); // [user, assistant]
        assert_eq!(groups[1].len(), 2);
    }

    #[test]
    fn test_truncate_head_for_ptl_retry() {
        let mut h = ConversationHistory::new();
        h.push_user("prompt 1");
        h.push_assistant_blocks(vec![ContentBlock::Text {
            text: "answer 1".into(),
        }]);
        h.push_user("prompt 2");
        h.push_assistant_blocks(vec![ContentBlock::Text {
            text: "answer 2".into(),
        }]);
        h.push_user("prompt 3");
        h.push_assistant_blocks(vec![ContentBlock::Text {
            text: "answer 3".into(),
        }]);

        let original_len = h.len();
        let truncated = truncate_head_for_ptl_retry(&mut h);
        assert!(truncated);
        assert!(h.len() < original_len);
    }

    #[test]
    fn test_truncate_head_single_round_no_op() {
        let mut h = ConversationHistory::new();
        h.push_user("only prompt");
        h.push_assistant_blocks(vec![ContentBlock::Text {
            text: "only answer".into(),
        }]);

        let truncated = truncate_head_for_ptl_retry(&mut h);
        assert!(!truncated); // Only 1 round, can't truncate
    }

    #[test]
    fn test_is_prompt_too_long_msg() {
        assert!(is_prompt_too_long_msg("Prompt too long for model"));
        assert!(is_prompt_too_long_msg("context length exceeded"));
        assert!(!is_prompt_too_long_msg("network timeout"));
    }
}
