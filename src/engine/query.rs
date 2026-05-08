//! Core tool-aware conversation loop.
//!
//! Flow:
//! 1. Assemble messages (system + history + user)
//! 2. Auto-compact if token estimate exceeds threshold
//! 3. Call api_client.stream_messages() with tool schemas
//! 4. Consume StreamEvent:
//!    - TextDelta → push to TUI render
//!    - ToolUseStart/Delta → accumulate tool input
//!    - MessageComplete → record usage
//! 5. If stop_reason == ToolUse:
//!    a. Execute each tool
//!    b. Append tool results to messages
//!    c. goto 1
//! 6. If stop_reason == EndTurn or turn >= max_turns:
//!    - If task_focus has a pending goal, inject a continuation
//!      prompt and loop again (auto-continue).
//!    - Otherwise, done.
//!
//! Reactive compact: if the API returns a "prompt too long" error,
//! auto-compact and retry once.
//!
//! Auto-continue: when the model returns an empty response or a
//! text-only response (no tool calls) while a goal is still pending,
//! inject a continuation message and loop — up to
//! `max_auto_continue` times per user input. This prevents the
//! agent from stopping prematurely before the task is done.

use futures::StreamExt;
use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::api::retry::{RetryConfig, get_retry_delay};
use crate::api::types::{ApiError, ContentBlock, StopReason, StreamEvent, Usage};
use crate::engine::carryover::resolve_file_path;
use crate::engine::compact::{auto_compact_if_needed, is_prompt_too_long_error};
use crate::engine::query_engine::QueryEngine;
use crate::engine::tui_events::UiEvent;
use crate::hooks::executor::HookExecutor;
use crate::hooks::types::{HookContext, HookEvent};
use crate::permissions::checker;
use crate::tools::base::ToolContext;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// Maximum number of auto-continue attempts per user input.
/// Prevents infinite loops when the LLM keeps stopping without
/// making progress.
const MAX_AUTO_CONTINUE: u32 = 3;

/// A collected tool use from the stream.
#[derive(Debug, Clone)]
struct CollectedToolUse {
    id: String,
    name: String,
    input_json: String,
}

/// Parse tool input JSON, returning an error value for the LLM if parsing fails
/// instead of silently defaulting to an empty object.
fn parse_tool_input(input_json: &str) -> Value {
    if input_json.is_empty() {
        Value::Object(Default::default())
    } else {
        serde_json::from_str(input_json).unwrap_or_else(|e| {
            tracing::warn!(
                error = %e,
                raw_len = input_json.len(),
                "Failed to parse tool input JSON"
            );
            Value::Object(Default::default())
        })
    }
}

/// Truncate a string to at most `max_chars` characters (not bytes).
/// This is safe for multi-byte UTF-8 (CJK, emoji, etc.).
/// Returns the truncated string with "...(truncated)" suffix if truncated.
fn safe_truncate_str(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars).collect();
        format!("{}...(truncated)", truncated)
    }
}

/// Min chars in a tool result to consider it for summarization.
/// Results shorter than this are left intact — they're cheap enough.
const MIN_TOOL_RESULT_TO_SUMMARIZE: usize = 500;

/// Lines to keep at the head of a read result when summarizing.
const FILE_READ_HEAD_LINES: usize = 60;
/// Lines to keep at the tail of a read result when summarizing.
const FILE_READ_TAIL_LINES: usize = 15;

/// Summarize tool results before storing in history to prevent cumulative
/// token bloat across multi-turn conversations.
///
/// Strategy (inspired by hermes-agent `_prune_old_tool_results`):
/// 1. **Dedup**: identical read results → back-reference marker
/// 2. **1-liner summaries**: non-read tools → deterministic summary
///    like "[grep] pattern='foo' → 12 matches in src/"
/// 3. **Head+tail**: read results → keep first 60 + last 15 lines
///    with a navigation hint to read the omitted section
///
/// Returns the number of blocks that were summarized.
fn summarize_tool_results(results: &mut [ContentBlock], tool_uses: &[CollectedToolUse]) -> usize {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut count = 0;

    // Build tool_call_id → (name, input_json) lookup
    let tool_info: std::collections::HashMap<&str, (&str, &str)> = tool_uses
        .iter()
        .map(|tu| (tu.id.as_str(), (tu.name.as_str(), tu.input_json.as_str())))
        .collect();

    // ── Pass 1: Deduplicate identical read results ────────────
    let mut content_hashes: std::collections::HashMap<u64, ()> = std::collections::HashMap::new();
    for block in results.iter_mut() {
        let &mut ContentBlock::ToolResult {
            ref tool_use_id,
            ref mut content,
            ..
        } = block
        else {
            continue;
        };
        let tool_name = tool_info
            .get(tool_use_id.as_str())
            .map(|(n, _)| *n)
            .unwrap_or("");
        if tool_name != "read" || content.len() < 200 {
            continue;
        }
        let mut hasher = DefaultHasher::new();
        content.hash(&mut hasher);
        let h = hasher.finish();

        if content_hashes.contains_key(&h) {
            *content =
                "[Duplicate — same content as an earlier read result in this batch]".to_string();
            count += 1;
        } else {
            content_hashes.insert(h, ());
        }
    }

    // ── Pass 2: Summarize oversized tool results ──────────────────
    for block in results.iter_mut() {
        let tool_id_str: &str;
        let content_ptr: &mut String;
        match block {
            &mut ContentBlock::ToolResult {
                ref tool_use_id,
                ref mut content,
                ..
            } => {
                tool_id_str = tool_use_id.as_str();
                content_ptr = content;
            }
            _ => continue,
        };

        let (tool_name, input_json) = tool_info.get(tool_id_str).copied().unwrap_or(("", ""));
        if content_ptr.len() <= MIN_TOOL_RESULT_TO_SUMMARIZE {
            continue;
        }
        if content_ptr.starts_with("[Duplicate") {
            continue;
        }

        match tool_name {
            "read" => {
                if let Some(summarized) = summarize_read_result(content_ptr) {
                    *content_ptr = summarized;
                    count += 1;
                }
            }
            _ => {
                if let Some(summary) =
                    summarize_generic_tool_result(tool_name, input_json, content_ptr)
                {
                    *content_ptr = summary;
                    count += 1;
                }
            }
        }
    }

    count
}
/// Deterministic 1-liner summary for non-read tool results.
///
/// Inspired by hermes-agent `_summarize_tool_result()`:
///   [bash] ran `cmd` → exit 0, 47 lines output
///   [grep] search for 'foo' in src/ → 12 matches
///   [glob] pattern='**/*.rs' → 34 files
///   [web_search] query='rust async' → 5 results
///
/// Returns None if the result is too small to summarize or if the tool
/// has no meaningful summary.
fn summarize_generic_tool_result(
    tool_name: &str,
    input_json: &str,
    content: &str,
) -> Option<String> {
    let args: serde_json::Value = serde_json::from_str(input_json).ok().unwrap_or_default();
    let content_len = content.len();
    let line_count = content.lines().count();

    let summary = match tool_name {
        "bash" => {
            let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("?");
            let cmd_preview: String = if cmd.len() > 80 {
                format!("{}...", &cmd[..77])
            } else {
                cmd.to_string()
            };
            format!(
                "[bash] ran `{}` → {} lines, {} chars",
                cmd_preview, line_count, content_len,
            )
        }
        "grep" => {
            let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("?");
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            // Count matches from output: each match block starts with "path:line"
            let match_count = content.match_indices("Found ").count();
            let desc = if match_count > 0 {
                format!("found matches")
            } else {
                "no matches".into()
            };
            format!(
                "[grep] '{}' in {} → {} ({} chars)",
                pattern, path, desc, content_len,
            )
        }
        "glob" => {
            let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("?");
            let file_count = content.lines().filter(|l| !l.starts_with("Found ")).count();
            format!(
                "[glob] '{}' → {} files ({} chars)",
                pattern, file_count, content_len,
            )
        }
        "web_search" => {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("?");
            format!("[web_search] '{}' → {} chars result", query, content_len,)
        }
        "web_fetch" => {
            let urls = args.get("urls").and_then(|v| v.as_array());
            let url_desc = urls
                .and_then(|u| u.first())
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            format!("[web_fetch] {} → {} chars result", url_desc, content_len,)
        }
        "edit" | "write" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            format!("[{}] {} → {} chars result", tool_name, path, content_len,)
        }
        "skill_view" => {
            let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            format!("[skill_view] loaded '{}' → {} chars", name, content_len,)
        }
        "skill_list" => {
            let cat = args
                .get("category")
                .and_then(|v| v.as_str())
                .unwrap_or("all");
            format!("[skill_list] browsed '{}' → {} chars", cat, content_len,)
        }
        "memory" => {
            let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("?");
            format!("[memory] {} → {} chars", action, content_len,)
        }
        "ask_user" => {
            // Don't summarize — the LLM needs the user's actual answer
            return None;
        }
        _ => {
            // Unknown tool: simple truncation
            if content.chars().count() > 4000 {
                let truncated: String = content.chars().take(4000).collect();
                format!("{}...(truncated)", truncated)
            } else {
                return None;
            }
        }
    };

    Some(summary)
}

/// Smart summarization for read results.
///
/// Preserves:
/// 1. The first `head_lines` lines (file beginning, structure)
/// 2. The last `tail_lines` lines + metadata footer (showing lines X-Y of Z)
/// 3. A clearly marked omission marker with exact line range and a ready-to-use
///    `read` call to retrieve the omitted section.
///
/// Returns None if the result is not a valid read output.
fn summarize_read_result(content: &str) -> Option<String> {
    // Verify this looks like a read result with a metadata footer
    // (the extractor below bails if no "of N total" line is found)
    {
        let has_footer = content
            .lines()
            .rev()
            .any(|line| line.contains(" of ") && line.contains(" total"));
        if !has_footer {
            return None;
        }
    }

    let lines: Vec<&str> = content.lines().collect();
    let n = lines.len();

    // Not worth summarizing if it's already small
    if n <= FILE_READ_HEAD_LINES + FILE_READ_TAIL_LINES + 5 {
        return None;
    }

    // Find the original line range being shown (first line like "     1 | ...")
    let first_line_num = lines
        .first()
        .and_then(|l| l.split('|').next())
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(1);

    let last_shown = first_line_num + n - 1; // approximate, but good enough

    // Build head section
    let mut result = String::new();
    let head_end = FILE_READ_HEAD_LINES.min(n);
    for line in &lines[..head_end] {
        result.push_str(line);
        result.push('\n');
    }

    // Omission marker with navigation hint
    let omitted_start = first_line_num + head_end;
    let omitted_end = last_shown - FILE_READ_TAIL_LINES;
    let omitted_count = omitted_end.saturating_sub(omitted_start);
    result.push_str(&format!(
        "\n... [omitted {} lines ({}–{}) — use read(path, offset={}, limit={}) to read this section] ...\n\n",
        omitted_count,
        omitted_start,
        omitted_end,
        omitted_start,
        omitted_count.min(2000),
    ));

    // Add original footer metadata: the last tail_lines + all footer lines after content
    let content_end = n.saturating_sub(FILE_READ_TAIL_LINES);
    // Also pick up the footer (starts with blank line then "(showing lines...")
    let footer_start = lines
        .iter()
        .rposition(|l| l.trim().is_empty())
        .unwrap_or(content_end);
    let tail_start = content_end.min(footer_start);

    for line in &lines[tail_start..] {
        result.push_str(line);
        result.push('\n');
    }

    Some(result)
}

// ---------------------------------------------------------------------------
// Parallel tool-call safety check
// ---------------------------------------------------------------------------

/// Tools that must never run concurrently (interactive / user-facing).
/// When any of these appear in a batch, the entire batch falls back to
/// sequential execution.
const NEVER_PARALLEL_TOOLS: &[&str] = &["ask_user"];

/// Tools that target a file and need path-scoped conflict detection.
/// Two file-scoped tools can run in parallel only when their target
/// paths do not overlap.
const PATH_SCOPED_TOOLS: &[&str] = &["read", "edit", "write"];

/// Extract the normalised absolute file path from a tool's input.
/// Returns None when the tool has no "path" field or the path is empty.
fn extract_scope_path(tool_name: &str, input: &Value, cwd: &Path) -> Option<PathBuf> {
    if !PATH_SCOPED_TOOLS.contains(&tool_name) {
        return None;
    }
    let raw = input.get("path").and_then(|v| v.as_str())?;
    if raw.is_empty() {
        return None;
    }
    // Expand ~ to home directory (mirrors ui/input.rs find_path_matches)
    let expanded = if let Some(stripped) = raw.strip_prefix("~/") {
        dirs::home_dir()
            .map(|h| h.join(stripped))
            .unwrap_or_else(|| PathBuf::from(raw))
    } else if raw == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from(raw))
    } else {
        PathBuf::from(raw)
    };
    if expanded.is_absolute() {
        Some(expanded)
    } else {
        Some(cwd.join(expanded))
    }
}

/// Return true when two paths may refer to the same subtree.
/// `left` being a prefix of `right` (or vice versa) counts as overlap.
fn paths_overlap(left: &Path, right: &Path) -> bool {
    let l = left.components().collect::<Vec<_>>();
    let r = right.components().collect::<Vec<_>>();
    let common = l.len().min(r.len());
    l[..common] == r[..common]
}

/// Decide whether a batch of tool calls is safe to execute concurrently.
///
/// Logic (mirrors hermes-agent `_should_parallelize_tool_batch`):
/// 1. ≤ 1 tool → parallel (trivially safe)
/// 2. Any `NEVER_PARALLEL_TOOLS` → sequential
/// 3. Non-JSON / non-object args → sequential (can't inspect)
/// 4. Path-scoped tools targeting overlapping paths → sequential
/// 5. Otherwise → parallel
fn should_parallelize(tool_uses: &[CollectedToolUse], cwd: &Path) -> bool {
    if tool_uses.len() <= 1 {
        return true; // trivially safe — single tool is always "parallel"
    }

    // Rule 2: interactive tools force sequential
    if tool_uses
        .iter()
        .any(|tu| NEVER_PARALLEL_TOOLS.contains(&tu.name.as_str()))
    {
        tracing::debug!(
            reason = "interactive_tool",
            "Parallel safety: falling back to sequential (interactive tool in batch)"
        );
        return false;
    }

    // Rule 3 & 4: parse args and check path overlaps
    let mut reserved_paths: Vec<PathBuf> = Vec::new();
    for tu in tool_uses {
        let input = parse_tool_input(&tu.input_json);
        if !input.is_object() {
            tracing::debug!(
                reason = "non_object_args",
                tool_name = %tu.name,
                "Parallel safety: falling back to sequential (non-object args)"
            );
            return false;
        }

        if let Some(scoped) = extract_scope_path(&tu.name, &input, cwd) {
            if reserved_paths.iter().any(|p| paths_overlap(p, &scoped)) {
                tracing::debug!(
                    reason = "path_overlap",
                    path = ?scoped,
                    "Parallel safety: falling back to sequential (path overlap)"
                );
                return false;
            }
            reserved_paths.push(scoped);
        }
    }

    true
}

impl QueryEngine {
    /// Run a user query with TUI event streaming.
    /// `cancel`: a CancellationToken that the caller can cancel (e.g. on Ctrl+C).
    /// When cancelled, the loop exits gracefully at the next check point.
    pub async fn query_tui(
        &mut self,
        user_input: &str,
        sender: &tokio::sync::mpsc::UnboundedSender<UiEvent>,
        cancel: CancellationToken,
    ) -> Result<(), ApiError> {
        self.history.push_user(user_input);

        // Record user goal in carryover for context preservation
        self.carryover.remember_user_goal(user_input);

        let mut turn = 0;
        let mut auto_continue_count: u32 = 0;
        let tool_schemas = self.tools.schemas();

        loop {
            // Check cancellation at the top of each turn
            if cancel.is_cancelled() {
                tracing::info!(
                    turn = turn,
                    event = "cancelled",
                    "query_tui: cancelled at top of turn"
                );
                self.handle_interrupt(sender);
                return Ok(());
            }
            turn += 1;
            if turn > self.max_turns {
                let _ = sender.send(UiEvent::Status(format!(
                "max turns ({}) reached — task may not be complete. Type '继续' or 'continue' to resume.",
                self.max_turns
            )));
                // NOTE: do NOT clear the goal here — the user may want to
                // continue the task by typing "继续" in the next input.
                // But clear steer — it was for this run.
                self.clear_steer();
                let _ = sender.send(UiEvent::QueryDone {
                    text: String::new(),
                    tool_calls: 0,
                    tokens: self.cost_tracker.last_prompt_tokens
                        + self.cost_tracker.last_output_tokens,
                });
                break;
            }

            // Auto-compact: check if conversation is too long and compress.
            // Use tokio::select! so Ctrl+C can abort a lengthy compact operation.
            let context_window = self.effective_context_window();
            if let Ok(Some(result)) = tokio::select! {
                r = auto_compact_if_needed(
                    &self.settings,
                    &mut self.history,
                    &self.compact_config,
                    &self.carryover,
                    context_window,
                ) => r,
                _ = cancel.cancelled() => {
                    tracing::info!(event = "cancelled", phase = "auto_compact", "query_tui: cancelled during auto-compact");
                    self.handle_interrupt(sender);
                    return Ok(());
                }
            } {
                let _ = sender.send(UiEvent::CompactProgress {
                    method: match result.method {
                        crate::engine::compact::CompactMethod::Micro => "micro".into(),
                        crate::engine::compact::CompactMethod::Full => "full".into(),
                    },
                    tokens_before: result.tokens_before,
                    tokens_after: result.tokens_after,
                });
            }

            self.history.sanitize();

            // ── Inner retry loop ──────────────────────────────────────
            // Retries up to `settings.llm.max_retries` times on:
            //   - API call errors (non-prompt-too-long)
            //   - Stream consumption errors
            //   - Empty responses (no text, no tool calls)
            // On each retry, emits a Status event (not Error) so the TUI
            // shows a soft notification. Only after exhausting all retries
            // do we propagate the error or fall through to the existing
            // empty-response handling below.
            let mut assistant_text = String::new();
            let mut tool_uses: Vec<CollectedToolUse> = Vec::new();
            let mut last_stop_reason: Option<StopReason> = None;
            let mut last_error: Option<ApiError> = None;
            let max_retries = self.settings.llm.max_retries;
            let retry_config = RetryConfig {
                max_retries,
                ..Default::default()
            };

            'retry: for retry_attempt in 0..=max_retries {
                // Reset per-attempt state
                assistant_text.clear();
                tool_uses.clear();
                let mut current_tool: Option<CollectedToolUse> = None;
                let mut pending_usage: Option<Usage> = None;
                last_stop_reason = None;

                if retry_attempt > 0 {
                    tracing::info!(
                        turn = turn,
                        retry_attempt = retry_attempt,
                        max_retries = max_retries,
                        "LLM retry attempt after empty or failed response"
                    );
                    let _ = sender.send(UiEvent::Status(format!(
                        "LLM response empty or failed, retrying ({}/{})...",
                        retry_attempt, max_retries
                    )));
                    // Exponential backoff with jitter (via shared retry module)
                    let last_status = match &last_error {
                        Some(ApiError::Http { status, .. }) => Some(*status),
                        _ => None,
                    };
                    let retry_after = match &last_error {
                        Some(ApiError::Http { headers, .. }) => headers
                            .as_ref()
                            .and_then(|h| h.get("retry-after").or_else(|| h.get("Retry-After")))
                            .and_then(|v| v.parse::<f64>().ok()),
                        _ => None,
                    };
                    let delay =
                        get_retry_delay(retry_attempt, &retry_config, last_status, retry_after);
                    tracing::info!(delay_secs = delay, "Retry backoff");
                    tokio::time::sleep(tokio::time::Duration::from_secs_f64(delay)).await;
                }

                // Re-sanitize and rebuild messages each attempt (history may
                // have been mutated by reactive compact on a prior attempt).
                self.history.sanitize();
                let messages = self.history.to_api_messages();

                // ── Acquire stream (with reactive compact on prompt-too-long) ──
                let stream = match tokio::select! {
                    result = self
                        .client
                        .stream_messages(
                            &self.model,
                            &self.system_prompt,
                            &messages,
                            &tool_schemas,
                            self.effective_max_tokens(),
                        ) => {
                        match result {
                            Ok(s) => Ok(s),
                            Err(e) => {
                                if is_prompt_too_long_error(&e) {
                                    tracing::warn!(
                    compact_trigger = "reactive",
                    "Prompt too long, triggering reactive compact"
                );
                                    let _ =
                                        sender.send(UiEvent::Status("Prompt too long, compressing...".into()));
                                    let cw = self.effective_context_window();
                                    match tokio::select! {
                                        r = auto_compact_if_needed(
                                            &self.settings,
                                            &mut self.history,
                                            &self.compact_config,
                                            &self.carryover,
                                            cw,
                                        ) => r,
                                        _ = cancel.cancelled() => {
                                            tracing::info!(event = "cancelled", phase = "reactive_compact", "query_tui: cancelled during reactive compact");
                                            self.handle_interrupt(sender);
                                            return Ok(());
                                        }
                                    } {
                                        Ok(Some(result)) => {
                                            let _ = sender.send(UiEvent::Status(format!(
                                                "reactive-compact: {} → {} tokens",
                                                result.tokens_before, result.tokens_after
                                            )));
                                            self.history.sanitize();
                                            let retry_messages = self.history.to_api_messages();
                                            tokio::select! {
                                                r = self.client
                                                    .stream_messages(
                                                        &self.model,
                                                        &self.system_prompt,
                                                        &retry_messages,
                                                        &tool_schemas,
                                                        self.effective_max_tokens(),
                                                    ) => r.map_err(Some),
                                                _ = cancel.cancelled() => {
                                                    tracing::info!(event = "cancelled", phase = "retry_stream", "query_tui: cancelled during retry stream");
                                                    self.handle_interrupt(sender);
                                                    return Ok(());
                                                }
                                            }
                                        }
                                        Ok(None) => Err(Some(e)),
                                        Err(_) => Err(Some(e)),
                                    }
                                } else {
                                    Err(Some(e))
                                }
                            }
                        }
                    }
                    _ = cancel.cancelled() => {
                        tracing::info!(event = "cancelled", phase = "initial_stream", "query_tui: cancelled during initial stream connect");
                        self.handle_interrupt(sender);
                        return Ok(());
                    }
                } {
                    Ok(s) => s,
                    Err(e) => {
                        last_error = e;
                        continue 'retry;
                    }
                };

                // ── Consume the stream ─────────────────────────────────
                tokio::pin!(stream);
                let mut stream_failed = false;

                loop {
                    let event = tokio::select! {
                        event = stream.next() => {
                            match event {
                                Some(e) => e,
                                None => break, // stream ended
                            }
                        }
                        _ = cancel.cancelled() => {
                            tracing::info!(event = "cancelled", phase = "stream_consumption", "query_tui: cancelled during stream consumption");
                            if !assistant_text.trim().is_empty() {
                                let blocks = vec![ContentBlock::Text {
                                    text: assistant_text.clone(),
                                }];
                                self.history.push_assistant_blocks(blocks);
                            }
                            self.handle_interrupt(sender);
                            return Ok(());
                        }
                    };

                    match event {
                        Ok(StreamEvent::TextDelta(delta)) => {
                            let _ = sender.send(UiEvent::TextDelta(delta.clone()));
                            assistant_text.push_str(&delta);
                        }
                        Ok(StreamEvent::ToolUseStart {
                            id,
                            name,
                            input_json,
                        }) => {
                            if let Some(tool) = current_tool.take() {
                                tool_uses.push(tool);
                            }
                            current_tool = Some(CollectedToolUse {
                                id,
                                name,
                                input_json: input_json.unwrap_or_default(),
                            });
                        }
                        Ok(StreamEvent::ToolUseDelta { id, delta_json }) => {
                            if let Some(ref mut tool) = current_tool
                                && tool.id == id
                            {
                                tool.input_json.push_str(&delta_json);
                            }
                        }
                        Ok(StreamEvent::UsageUpdate(usage_update)) => {
                            pending_usage = Some(usage_update);
                        }
                        Ok(StreamEvent::MessageComplete { stop_reason, usage }) => {
                            let final_usage = if let Some(pending) = pending_usage.take() {
                                // Anthropic: message_start had input+output+cache, message_delta has
                                // only output_tokens.  Merge: take input+ cache from pending, output
                                // from usage (message_delta has the final output count).
                                Usage {
                                    input_tokens: pending.input_tokens,
                                    output_tokens: usage.output_tokens,
                                    cache_read_input_tokens: pending.cache_read_input_tokens,
                                    cache_creation_input_tokens: pending
                                        .cache_creation_input_tokens,
                                    reasoning_tokens: pending.reasoning_tokens,
                                }
                            } else {
                                usage
                            };
                            self.cost_tracker.record(&self.model, &final_usage);
                            let _ = sender.send(UiEvent::TokenUpdate {
                                total_tokens: self.cost_tracker.last_prompt_tokens
                                    + self.cost_tracker.last_output_tokens,
                                turn_count: self.cost_tracker.turn_count,
                            });
                            last_stop_reason = Some(stop_reason);
                        }
                        Ok(StreamEvent::Error(e)) => {
                            tracing::warn!(error = %e, "Stream event error");
                            last_error = Some(ApiError::Stream(e));
                            stream_failed = true;
                            break;
                        }
                        Err(e) => {
                            tracing::warn!(error = ?e, "Stream error during consumption");
                            last_error = Some(e);
                            stream_failed = true;
                            break;
                        }
                    }
                }

                if let Some(tool) = current_tool.take() {
                    tool_uses.push(tool);
                }

                // Stream-level error — retry
                if stream_failed {
                    continue 'retry;
                }

                // Empty response — retry (unless this is the last attempt,
                // in which case fall through to existing empty-handling below)
                if assistant_text.trim().is_empty() && tool_uses.is_empty() {
                    tracing::warn!(
                        retry_attempt = retry_attempt + 1,
                        max_attempts = max_retries + 1,
                        "LLM returned empty response"
                    );
                    if retry_attempt < max_retries {
                        continue 'retry;
                    }
                    // Last attempt also empty — break out of retry loop and
                    // let the existing empty-response handler deal with it.
                    break 'retry;
                }

                // Got a non-empty response — done retrying
                break 'retry;
            } // end 'retry loop

            // If we exhausted retries with an API/stream error (and never
            // got any text or tools), provide a helpful message instead of
            // aborting the entire query loop. This allows the LLM to decide
            // whether to retry on its own.
            if let (true, true, Some(err_msg)) = (
                assistant_text.trim().is_empty(),
                tool_uses.is_empty(),
                &last_error,
            ) {
                let user_msg = format!(
                    "[System] The API stream was interrupted after all retry attempts: {}. \
                     You may retry your request or try a different approach.",
                    err_msg
                );
                self.history.push_user(&user_msg);
                let _ = sender.send(UiEvent::Status(
                    "Stream interrupted — retry context passed to LLM".into(),
                ));
                turn += 1;
                continue; // next turn: LLM sees the error and can decide to retry
            }

            // --- Output-side [DONE] sentinel detection ---
            // If the LLM included "[DONE]" in its text, it explicitly signals
            // the task is complete. Strip it from the text (user never sees it)
            // and set a flag to prevent auto-continue.
            let done_signaled = assistant_text.contains("[DONE]");
            if done_signaled {
                assistant_text = assistant_text.replace("[DONE]", "");
                tracing::info!(
                    event = "done_signaled",
                    "LLM signaled [DONE] — task complete"
                );
            }

            let mut assistant_blocks: Vec<ContentBlock> = Vec::new();
            if !assistant_text.is_empty() {
                assistant_blocks.push(ContentBlock::Text {
                    text: assistant_text.clone(),
                });
            }
            for tu in &tool_uses {
                let input = parse_tool_input(&tu.input_json);
                assistant_blocks.push(ContentBlock::ToolUse {
                    id: tu.id.clone(),
                    name: tu.name.clone(),
                    input,
                });
            }

            // Guard: drop empty assistant messages before pushing to history.
            // Empty messages corrupt the conversation and cause API failures.
            //
            // If we got here, all retry attempts returned empty. Now check
            // if we have a pending goal and can auto-continue.
            if assistant_text.trim().is_empty() && tool_uses.is_empty() {
                tracing::warn!(
                    max_retries = max_retries,
                    "dropping empty assistant message from provider response"
                );
                let _ = sender.send(UiEvent::Status(format!(
                    "Model returned empty response after {} attempts.",
                    max_retries + 1
                )));

                if auto_continue_count < MAX_AUTO_CONTINUE && self.carryover.has_pending_goal() {
                    auto_continue_count += 1;
                    tracing::info!(
                        auto_continue = auto_continue_count,
                        max_auto_continue = MAX_AUTO_CONTINUE,
                        "auto-continue: injecting continuation for empty response"
                    );
                    let _ = sender.send(UiEvent::Status(
                        "Model returned empty — retrying with goal reminder...".into(),
                    ));
                    if let Some(prompt) = self.carryover.build_continuation_prompt() {
                        self.history.push_user(&prompt);
                    }
                    continue;
                }

                let _ = sender.send(UiEvent::QueryDone {
                    text: String::new(),
                    tool_calls: 0,
                    tokens: self.cost_tracker.last_prompt_tokens
                        + self.cost_tracker.last_output_tokens,
                });
                break;
            }

            self.history.push_assistant_blocks(assistant_blocks);

            // No tool calls — the model gave a text-only response.
            //
            // If the stop_reason was MaxTokens, the output was truncated.
            // Auto-continue by injecting a "continue" message so the model
            // resumes from where it left off (up to MAX_AUTO_CONTINUE times).
            //
            // If the LLM signaled [DONE] (detected above), skip auto-continue.
            // Otherwise, check if there's a pending goal and loop again.
            if tool_uses.is_empty() {
                let is_max_tokens = matches!(last_stop_reason, Some(StopReason::MaxTokens));

                if is_max_tokens
                    && auto_continue_count < MAX_AUTO_CONTINUE
                    && !assistant_text.trim().is_empty()
                {
                    auto_continue_count += 1;
                    tracing::info!(
                        auto_continue = auto_continue_count,
                        max_auto_continue = MAX_AUTO_CONTINUE,
                        reason = "max_tokens",
                        "auto-continue: output truncated, requesting continuation"
                    );
                    let _ = sender.send(UiEvent::Status(
                        "Output truncated (max tokens reached) — continuing...".into(),
                    ));
                    // Tell the model its previous output was cut off and to continue
                    self.history.push_user(
                "Your previous response was cut off because it reached the maximum output token limit. Please continue from exactly where you left off — do not repeat what you already said.",
            );
                    continue;
                }

                if !done_signaled
                    && auto_continue_count < MAX_AUTO_CONTINUE
                    && self.carryover.has_pending_goal()
                {
                    auto_continue_count += 1;
                    tracing::info!(
                        auto_continue = auto_continue_count,
                        max_auto_continue = MAX_AUTO_CONTINUE,
                        reason = "pending_goal",
                        "auto-continue: model stopped without tool use, goal still pending"
                    );
                    let _ =
                        sender.send(UiEvent::Status("Task not complete — continuing...".into()));
                    if let Some(prompt) = self.carryover.build_continuation_prompt() {
                        self.history.push_user(&prompt);
                    }
                    continue;
                }

                // Goal completed or no goal — genuinely done
                self.carryover.clear_goal();
                // Clear any pending steer — it was meant for this run, not the next.
                self.clear_steer();
                let _ = sender.send(UiEvent::QueryDone {
                    text: String::new(),
                    tool_calls: 0,
                    tokens: self.cost_tracker.last_prompt_tokens
                        + self.cost_tracker.last_output_tokens,
                });
                break;
            }

            // Check cancellation before executing tools
            if cancel.is_cancelled() {
                tracing::info!(
                    event = "cancelled",
                    phase = "before_tool_execution",
                    "query_tui: cancelled before tool execution"
                );
                let mut pre_tool_blocks: Vec<ContentBlock> = Vec::new();
                if !assistant_text.trim().is_empty() {
                    pre_tool_blocks.push(ContentBlock::Text {
                        text: assistant_text.clone(),
                    });
                }
                for tu in &tool_uses {
                    let tu_input = parse_tool_input(&tu.input_json);
                    pre_tool_blocks.push(ContentBlock::ToolUse {
                        id: tu.id.clone(),
                        name: tu.name.clone(),
                        input: tu_input,
                    });
                }
                if !pre_tool_blocks.is_empty() {
                    self.history.push_assistant_blocks(pre_tool_blocks);
                }
                self.handle_interrupt(sender);
                return Ok(());
            }

            let ctx = ToolContext::with_ask_sender(self.cwd.clone(), sender.clone());
            let mut tool_results: Vec<ContentBlock> = Vec::new();

            for tu in &tool_uses {
                let summary = format_tool_input_summary(&tu.name, &tu.input_json);
                let _ = sender.send(UiEvent::ToolStart {
                    name: tu.name.clone(),
                    input_summary: summary,
                });
            }

            // ── Parallel safety check ──────────────────────────────────
            // Decide whether this batch can run concurrently or must be
            // sequential.  Read-only tools and independent-path file tools
            // are safe in parallel; interactive tools (ask_user) and
            // overlapping-path writes fall back to sequential.
            let parallel = should_parallelize(&tool_uses, &self.cwd);
            if !parallel {
                tracing::info!(
                    tool_count = tool_uses.len(),
                    execution_mode = "sequential",
                    "Tool batch not parallel-safe, executing sequentially"
                );
            }

            if parallel {
                // ── Concurrent execution ────────────────────────────────
                // Error-tolerant parallel execution: each tool failure is
                // captured as a ToolResult { is_error: true } so no orphaned
                // ToolUse blocks remain — matching return_exceptions=True.
                let futures: Vec<_> = tool_uses
                    .iter()
                    .map(|tu| {
                        execute_single_tool_tui_catch(
                            &self.permission_mode,
                            &self.settings.trusted_paths,
                            &self.permission_allow_all,
                            &self.tools,
                            tu,
                            &ctx,
                            sender,
                            self.hook_executor.as_ref(),
                            &cancel,
                        )
                    })
                    .collect();

                // Use tokio::select! so Ctrl+C cancels immediately instead of
                // waiting for all tool executions to finish.
                let results = tokio::select! {
                    r = futures::future::join_all(futures) => r,
                    _ = cancel.cancelled() => {
                        tracing::info!(event = "cancelled", phase = "parallel_tool_execution", "query_tui: cancelled during tool execution");
                        // Tool futures are dropped here — tokio::process children
                        // are killed when the Child handle is dropped (kill_on_drop).
                        self.handle_interrupt(sender);
                        return Ok(());
                    }
                };

                // Check cancellation after tool execution (still useful for
                // the race where cancel fires right after join_all returns)
                if cancel.is_cancelled() {
                    tracing::info!(
                        event = "cancelled",
                        phase = "after_tool_execution",
                        "query_tui: cancelled after tool execution"
                    );
                    for res in results {
                        tool_results.push(res);
                    }
                    self.history.push_tool_results(tool_results);
                    self.handle_interrupt(sender);
                    return Ok(());
                }

                for res in results {
                    tool_results.push(res);
                }
            } else {
                // ── Sequential execution (fallback) ─────────────────────
                for tu in &tool_uses {
                    if cancel.is_cancelled() {
                        tracing::info!(
                            event = "cancelled",
                            phase = "sequential_tool_execution",
                            "query_tui: cancelled during sequential tool execution"
                        );
                        self.handle_interrupt(sender);
                        return Ok(());
                    }
                    let result = execute_single_tool_tui_catch(
                        &self.permission_mode,
                        &self.settings.trusted_paths,
                        &self.permission_allow_all,
                        &self.tools,
                        tu,
                        &ctx,
                        sender,
                        self.hook_executor.as_ref(),
                        &cancel,
                    )
                    .await;
                    tool_results.push(result);
                }
            }

            // Summarize oversized tool results before storing in history
            // to prevent cumulative token bloat across multi-turn conversations.
            // read gets smart summarization (head+tail+nav hint),
            // other tools get truncated.
            let summarized = summarize_tool_results(&mut tool_results, &tool_uses);
            if summarized > 0 {
                tracing::debug!(
                    summarized_blocks = summarized,
                    total_blocks = tool_results.len(),
                    "Summarized oversized tool results"
                );
            }

            self.history.push_tool_results(tool_results);

            // ── Drain pending steer ────────────────────────────────
            // If the user typed input while the agent was running, inject
            // it into the last tool result's content so the model sees it
            // on the next turn. This preserves role alternation — we don't
            // insert a new user message (which would create consecutive
            // user-role entries), we append with a clear marker instead.
            // Pattern from hermes-agent `_apply_pending_steer_to_tool_results`.
            if let Some(steer_text) = self.drain_steer() {
                tracing::info!(
                    steer_len = steer_text.len(),
                    "Draining pending steer into conversation"
                );
                let _ = sender.send(UiEvent::Status(format!(
                    "⇢ Steered: {}",
                    safe_truncate_str(&steer_text, 60)
                )));
                if !self.history.append_steer_to_last_tool_result(&steer_text) {
                    // Fallback: no tool result to append to (shouldn't happen
                    // since we just pushed tool_results, but handle gracefully).
                    // Inject as a user message instead.
                    tracing::warn!(
                        event = "steer_fallback",
                        "No tool result to append steer to, injecting as user message"
                    );
                    self.history.push_user(&steer_text);
                }
            }

            // Record tool results in carryover (same pattern as query())
            for tu in &tool_uses {
                let input = parse_tool_input(&tu.input_json);
                let resolved_path = resolve_file_path(&input);
                let result_content = self.history.entries_raw().last().and_then(|entry| {
                    entry.content.iter().find_map(|b| match b {
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } if tool_use_id == &tu.id => {
                            Some((content.clone(), is_error.unwrap_or(false)))
                        }
                        _ => None,
                    })
                });
                if let Some((output, is_error)) = result_content {
                    self.carryover.record_tool_result(
                        &tu.name,
                        &input,
                        &output,
                        is_error,
                        resolved_path.as_deref(),
                    );
                }
            }

            // Do NOT send QueryDone here — the loop continues to the next LLM call.
            // QueryDone is only sent when the loop exits (no more tool_calls, max turns, or error).
        }

        Ok(())
    }

    /// Called when the query is interrupted (Ctrl+C).
    /// Sanitizes history and sends the appropriate UI events.
    fn handle_interrupt(&mut self, sender: &tokio::sync::mpsc::UnboundedSender<UiEvent>) {
        tracing::info!(event = "interrupted", "Query interrupted by user");
        // A hard interrupt supersedes any pending steer — the steer was
        // meant for the agent's next tool-call iteration, which will no
        // longer happen. Drop it instead of surprising the user.
        self.clear_steer();
        self.history.sanitize();
        let _ = sender.send(UiEvent::Interrupted);
        let _ = sender.send(UiEvent::QueryDone {
            text: String::new(),
            tool_calls: 0,
            tokens: self.cost_tracker.last_prompt_tokens + self.cost_tracker.last_output_tokens,
        });
    }
}

/// Error-tolerant wrapper for TUI tool execution.
/// Always returns a ContentBlock, converting None to an error ToolResult.
#[allow(clippy::too_many_arguments)]
async fn execute_single_tool_tui_catch(
    permission_mode: &crate::config::settings::PermissionMode,
    trusted_paths: &[String],
    permission_allow_all: &Arc<Mutex<bool>>,
    tools: &crate::tools::base::ToolRegistry,
    tu: &CollectedToolUse,
    ctx: &ToolContext,
    sender: &tokio::sync::mpsc::UnboundedSender<crate::engine::tui_events::UiEvent>,
    hook_executor: Option<&HookExecutor>,
    cancel: &CancellationToken,
) -> ContentBlock {
    match execute_single_tool_tui(
        permission_mode,
        trusted_paths,
        permission_allow_all,
        tools,
        tu,
        ctx,
        sender,
        hook_executor,
        cancel,
    )
    .await
    {
        Some(block) => block,
        None => {
            let _ = sender.send(UiEvent::ToolError {
                name: tu.name.clone(),
                error: "Internal error: tool returned no result".into(),
            });
            ContentBlock::ToolResult {
                tool_use_id: tu.id.clone(),
                content: format!("Tool {} returned no result (internal error)", tu.name),
                is_error: Some(true),
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_single_tool_tui(
    permission_mode: &crate::config::settings::PermissionMode,
    trusted_paths: &[String],
    permission_allow_all: &Arc<Mutex<bool>>,
    tools: &crate::tools::base::ToolRegistry,
    tu: &CollectedToolUse,
    ctx: &ToolContext,
    sender: &tokio::sync::mpsc::UnboundedSender<crate::engine::tui_events::UiEvent>,
    hook_executor: Option<&HookExecutor>,
    cancel: &CancellationToken,
) -> Option<ContentBlock> {
    let input = parse_tool_input(&tu.input_json);

    // --- Pre-tool-use hook ---
    if let Some(he) = hook_executor {
        let mut hook_ctx: HookContext = serde_json::Map::new();
        hook_ctx.insert(
            "tool_name".into(),
            serde_json::Value::String(tu.name.clone()),
        );
        hook_ctx.insert("tool_input".into(), input.clone());
        let result = he.execute(HookEvent::PreToolUse, &mut hook_ctx).await;
        if result.blocked {
            let reason = result
                .reason
                .unwrap_or_else(|| format!("Hook blocked tool '{}'", tu.name));
            let _ = sender.send(UiEvent::ToolError {
                name: tu.name.clone(),
                error: reason.clone(),
            });
            return Some(ContentBlock::ToolResult {
                tool_use_id: tu.id.clone(),
                content: reason,
                is_error: Some(true),
            });
        }
    }

    // --- Fine-grained permission check ---
    // Resolve the tool to check is_read_only status
    let is_read_only = tools
        .get(&tu.name)
        .map(|t| t.as_ref().is_read_only(&input))
        .unwrap_or(false);

    // Validate input schema before permission check
    if let Some(t) = tools.get(&tu.name) {
        let validation: Result<(), crate::tools::base::ToolError> =
            t.as_ref().validate_input(&input);
        if let Err(e) = validation {
            let _ = sender.send(UiEvent::ToolError {
                name: tu.name.clone(),
                error: e.to_string(),
            });
            return Some(ContentBlock::ToolResult {
                tool_use_id: tu.id.clone(),
                content: format!("Validation error: {}", e),
                is_error: Some(true),
            });
        }
    }

    // --- Fine-grained permission check (TUI-aware) ---
    // Instead of calling check_permission_fine() which uses eprintln/stdin,
    // we do the evaluation ourselves and route confirmation through the
    // TUI event channel so it renders properly in the terminal UI.

    // Check if user has already approved "allow all" for this session
    let already_allowed = {
        let guard = permission_allow_all.lock().unwrap();
        *guard
    };

    let permitted = if already_allowed {
        tracing::debug!(
            tool_name = %tu.name,
            permission_decision = "allowed",
            mode = "session_allow_all",
            tool_id = %tu.id,
            "Tool permitted by session-wide allow-all"
        );
        true
    } else {
        let resolved = checker::resolve_paths(&tu.name, &input, &ctx.cwd);
        let decision = checker::evaluate_permission(
            permission_mode,
            trusted_paths,
            &tu.name,
            is_read_only,
            resolved.file_path.as_deref(),
            resolved.command.as_deref(),
            &ctx.cwd,
        );

        if decision.allowed {
            tracing::info!(
                tool_name = %tu.name,
                permission_decision = "allowed",
                mode = %format!("{:?}", permission_mode),
                is_read_only = is_read_only,
                tool_id = %tu.id,
                "Tool execution permitted"
            );
            true
        } else if decision.requires_confirmation {
            // Send permission request through TUI channel
            let display_input = safe_truncate_str(&tu.input_json, 200);
            let (tx, rx) = tokio::sync::oneshot::channel();
            let response_tx = Arc::new(Mutex::new(Some(tx)));
            let _ = sender.send(UiEvent::PermissionAsk {
                tool_name: tu.name.clone(),
                reason: decision.reason.clone(),
                input: display_input,
                response_tx,
            });
            match tokio::select! {
                result = rx => result,
                _ = cancel.cancelled() => {
                    // User pressed Ctrl+C while waiting for permission
                    return None;
                }
            } {
                Ok(response) => {
                    let r = response.trim().to_lowercase();
                    let allowed = r == "y" || r == "yes" || r == "a" || r == "all" || r == "always";
                    tracing::info!(
                        tool_name = %tu.name,
                        permission_decision = if allowed { "user_allowed" } else { "user_denied" },
                        mode = "ask",
                        user_response = %r,
                        tool_id = %tu.id,
                        "User responded to permission prompt"
                    );
                    // If user said "allow all", set the session-wide flag
                    if allowed && matches!(r.as_str(), "a" | "all" | "always") {
                        let mut guard = permission_allow_all.lock().unwrap();
                        *guard = true;
                        let _ = sender.send(UiEvent::Status(
                            "All permissions granted for this session.".into(),
                        ));
                    }
                    allowed
                }
                Err(_) => false,
            }
        } else {
            tracing::warn!(
                tool_name = %tu.name,
                permission_decision = "denied",
                mode = %format!("{:?}", permission_mode),
                tool_id = %tu.id,
                "Tool execution denied"
            );
            false
        }
    };

    if !permitted {
        tracing::warn!(
            tool_name = %tu.name,
            permission_decision = "denied",
            tool_id = %tu.id,
            "Tool execution blocked by permission check"
        );
        let _ = sender.send(UiEvent::ToolError {
            name: tu.name.clone(),
            error: "Permission denied".into(),
        });
        return Some(ContentBlock::ToolResult {
            tool_use_id: tu.id.clone(),
            content: "Permission denied by user.".into(),
            is_error: Some(true),
        });
    }

    match tools.execute(&tu.name, input.clone(), ctx).await {
        Ok(result) => {
            // Show a compact one-line summary in the TUI; full content goes to LLM.
            let display = summarize_tool_output(&tu.name, &result, &tu.input_json);
            let _ = sender.send(UiEvent::ToolOutput {
                name: tu.name.clone(),
                output: display,
            });
            tracing::info!(
                tool_name = %tu.name,
                tool_id = %tu.id,
                tool_result = "success",
                result_len = result.len(),
                "Tool executed successfully"
            );

            // --- Post-tool-use hook ---
            if let Some(he) = hook_executor {
                let mut hook_ctx: HookContext = serde_json::Map::new();
                hook_ctx.insert(
                    "tool_name".into(),
                    serde_json::Value::String(tu.name.clone()),
                );
                hook_ctx.insert("tool_input".into(), input);
                hook_ctx.insert(
                    "tool_output".into(),
                    serde_json::Value::String(result.clone()),
                );
                hook_ctx.insert("tool_is_error".into(), serde_json::Value::Bool(false));
                he.execute(HookEvent::PostToolUse, &mut hook_ctx).await;
            }

            Some(ContentBlock::ToolResult {
                tool_use_id: tu.id.clone(),
                content: result,
                is_error: None,
            })
        }
        Err(e) => {
            tracing::warn!(
                tool_name = %tu.name,
                tool_id = %tu.id,
                tool_result = "error",
                error = %e,
                "Tool execution failed"
            );
            let _ = sender.send(UiEvent::ToolError {
                name: tu.name.clone(),
                error: e.to_string(),
            });

            // --- Post-tool-use hook (error case) ---
            if let Some(he) = hook_executor {
                let mut hook_ctx: HookContext = serde_json::Map::new();
                hook_ctx.insert(
                    "tool_name".into(),
                    serde_json::Value::String(tu.name.clone()),
                );
                hook_ctx.insert("tool_input".into(), input);
                hook_ctx.insert(
                    "tool_output".into(),
                    serde_json::Value::String(e.to_string()),
                );
                hook_ctx.insert("tool_is_error".into(), serde_json::Value::Bool(true));
                he.execute(HookEvent::PostToolUse, &mut hook_ctx).await;
            }

            Some(ContentBlock::ToolResult {
                tool_use_id: tu.id.clone(),
                content: format!("Error: {}", e),
                is_error: Some(true),
            })
        }
    }
}

/// Produce a compact TUI preview of a tool result.
///
/// Shows at most `max_lines` lines of the output, followed by an
/// ellipsis hint when there is more content.  The full content is
/// still sent to the LLM — this only affects what the user sees
/// in the terminal.
/// Build a one-line summary for the TUI output, combining tool identity + result.
///
/// Instead of showing raw output, produce a compact summary like:
/// ⚡ git status → ok (5 lines)
/// ⚡ rm -rf / → exit 1: Permission denied
/// 📖 src/main.rs → L1-L50 (of 300)
/// ✏️ Editing src/main.rs → -old_line / +new_line
///
/// Note: No icon prefix here — the icon is already in ToolStart's input_summary.
fn summarize_tool_output(tool_name: &str, output: &str, input_json: &str) -> String {
    // Error output — show first line with exit code
    if output.starts_with("[exit code:") || output.starts_with("[stderr]") {
        let first_line = output.lines().next().unwrap_or(output);
        let preview: String = first_line.chars().take(80).collect();
        // Extract exit code for bash
        if let Some(code) = output.lines().find_map(|l| {
            l.strip_prefix("[exit code: ")
                .and_then(|s| s.strip_suffix(']'))
        }) {
            return format!("exit {}: {}", code, preview);
        }
        return format!("⚠ {}", preview);
    }

    match tool_name {
        "bash" => {
            let line_count = output.lines().count();
            if output.contains("[exit code:") {
                // Already handled above, but just in case
                format!("failed ({} lines)", line_count)
            } else if output.trim() == "(no output)" {
                "ok (no output)".into()
            } else {
                format!("ok ({} lines)", line_count)
            }
        }
        "read" | "read_file" => {
            // Parse line number range from output like "   1 | ..." and footer "(showing lines X-Y of Z total)"
            let mut first_line_num: Option<usize> = None;
            let mut last_line_num: Option<usize> = None;
            let mut total_lines: Option<usize> = None;

            for line in output.lines() {
                // Parse "   1 | content" format
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
                // Parse "(showing lines X-Y of Z total)"
                if let Some(rest) = line.strip_prefix("(showing lines ")
                    && let Some(end_part) = rest.strip_suffix(" total)")
                {
                    // Format: "X-Y of Z"
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
        "write" => "written".into(),
        "edit" => {
            // Generate multi-line diff from input_json's old_string / new_string.
            // Each line prefixed with "-" (deleted) or "+" (added), separated by \n.
            let input = parse_tool_input(input_json);

            let old_str = input
                .get("old_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let new_str = input
                .get("new_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let replace_all = input
                .get("replace_all")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if old_str.is_empty() {
                return "patched".into();
            }

            let old_lines: Vec<&str> = old_str.lines().collect();
            let new_lines: Vec<&str> = new_str.lines().collect();

            if old_lines.is_empty() && new_lines.is_empty() {
                return "patched".into();
            }

            let mut diff_lines: Vec<String> = Vec::new();
            const MAX_DIFF_LINES: usize = 20;
            const MAX_LINE_WIDTH: usize = 80;

            for line in &old_lines {
                let truncated: String = line.chars().take(MAX_LINE_WIDTH).collect();
                diff_lines.push(format!("-{}", truncated));
            }
            for line in &new_lines {
                let truncated: String = line.chars().take(MAX_LINE_WIDTH).collect();
                diff_lines.push(format!("+{}", truncated));
            }

            if replace_all {
                diff_lines.push("(replace all)".into());
            }

            // Truncate if too many lines
            if diff_lines.len() > MAX_DIFF_LINES {
                let omitted = diff_lines.len() - MAX_DIFF_LINES;
                diff_lines.truncate(MAX_DIFF_LINES);
                diff_lines.push(format!("... ({} more lines omitted)", omitted));
            }

            diff_lines.join("\n")
        }
        "grep" => {
            let match_count = output.lines().filter(|l| !l.is_empty()).count();
            format!("{} matches", match_count)
        }
        "glob" => {
            let file_count = output
                .lines()
                .filter(|l| !l.starts_with("Found") && !l.is_empty())
                .count();
            format!("{} files", file_count)
        }
        "web_search" => {
            let result_count = output.lines().filter(|l| !l.is_empty()).count();
            format!("{} results", result_count)
        }
        "web_fetch" => {
            let char_count = output.len();
            format!("{} chars", char_count)
        }
        "skill_list" | "skill_view" => {
            let line_count = output.lines().count();
            format!("{} lines", line_count)
        }
        "memory" => {
            // Parse JSON output for meaningful display
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(output) {
                let success = val
                    .get("success")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if success {
                    let target = val
                        .get("target")
                        .and_then(|v| v.as_str())
                        .unwrap_or("memory");
                    let count = val.get("entry_count").and_then(|v| v.as_u64()).unwrap_or(0);
                    let usage = val.get("usage").and_then(|v| v.as_str()).unwrap_or("");
                    let msg = val.get("message").and_then(|v| v.as_str()).unwrap_or("");
                    let icon = if target == "user" {
                        "\u{f007}"
                    } else {
                        "\u{f0e0}"
                    }; // user icon / memory icon
                    format!(
                        "{} {} [{} entries, {}] — {}",
                        icon, target, count, usage, msg
                    )
                } else {
                    let error = val
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");
                    format!("failed: {}", error)
                }
            } else {
                let line_count = output.lines().count();
                format!("{} lines", line_count)
            }
        }
        _ => {
            let line_count = output.lines().count();
            format!("{} lines", line_count)
        }
    }
}

/// Build a one-line summary of a tool's input for the TUI status display.
fn format_tool_input_summary(tool_name: &str, input_json: &str) -> String {
    let input = parse_tool_input(input_json);

    match tool_name {
        "bash" => input
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| {
                let s = s.trim();
                if s.chars().count() > 80 {
                    let truncated: String = s.chars().take(77).collect();
                    format!("\u{f120} $ {}…", truncated)
                } else {
                    format!("\u{f120} $ {}", s)
                }
            })
            .unwrap_or_else(|| "\u{f120} bash".into()),
        "read" | "read_file" => input
            .get("path")
            .or_else(|| input.get("file_path"))
            .and_then(|v| v.as_str())
            .map(|s| format!("\u{f15c} Reading {}", s))
            .unwrap_or_else(|| "\u{f15c} Reading file".into()),
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
                    format!("\u{f0c7} Writing {} ({} lines)", s, lines)
                } else {
                    format!("\u{f0c7} Writing {}", s)
                }
            })
            .unwrap_or_else(|| "\u{f0c7} Writing file".into()),
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
                format!("\u{f044} Editing {} (replace all)", path)
            } else {
                format!("\u{f044} Editing {}", path)
            }
        }
        "grep" => {
            let pattern = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("?");
            let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if path.is_empty() {
                format!("\u{f002} grep {}", pattern)
            } else {
                format!("\u{f002} grep {} in {}", pattern, path)
            }
        }
        "glob" => {
            let pattern = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("?");
            let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if path.is_empty() {
                format!("\u{f024b} glob {}", pattern)
            } else {
                format!("\u{f024b} glob {} in {}", pattern, path)
            }
        }
        "web_search" => input
            .get("query")
            .and_then(|v| v.as_str())
            .map(|s| format!("\u{f0ac} Searching: {}", s))
            .unwrap_or_else(|| "\u{f0ac} Web search".into()),
        "web_fetch" => input
            .get("url")
            .and_then(|v| v.as_str())
            .map(|s| format!("\u{f0c1} Fetching {}", s))
            .unwrap_or_else(|| "\u{f0c1} Web fetch".into()),
        "skill_list" => {
            let cat = input.get("category").and_then(|v| v.as_str()).unwrap_or("");
            let tag = input.get("tag").and_then(|v| v.as_str()).unwrap_or("");
            if !cat.is_empty() {
                format!("\u{f03a} Skills by category: {}", cat)
            } else if !tag.is_empty() {
                format!("\u{f03a} Skills by tag: {}", tag)
            } else {
                "\u{f03a} Listing skills".into()
            }
        }
        "skill_view" => input
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| format!("\u{f06e} Viewing skill: {}", s))
            .unwrap_or_else(|| "\u{f06e} Viewing skill".into()),
        "ask_user" => input
            .get("question")
            .and_then(|v| v.as_str())
            .map(|q| {
                let preview: String = q.chars().take(60).collect();
                if q.chars().count() > 60 {
                    format!("\u{f059} Asking: {}…", preview)
                } else {
                    format!("\u{f059} Asking: {}", q)
                }
            })
            .unwrap_or_else(|| "\u{f059} Asking user".into()),
        "config" => input
            .get("action")
            .and_then(|v| v.as_str())
            .map(|s| format!("\u{f013} Config: {}", s))
            .unwrap_or_else(|| "\u{f013} Config".into()),
        "memory" => {
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
                        format!("\u{f0e0} Memorizing {}: {}…", target, preview)
                    } else {
                        format!("\u{f0e0} Memorizing {}: {}", target, preview)
                    }
                }
                "replace" => {
                    let old = input.get("old_text").and_then(|v| v.as_str()).unwrap_or("");
                    let content = input.get("content").and_then(|v| v.as_str()).unwrap_or("");
                    let old_preview: String = old.chars().take(30).collect();
                    let new_preview: String = content.chars().take(30).collect();
                    format!(
                        "\u{f044} Updating {}: {} → {}",
                        target, old_preview, new_preview
                    )
                }
                "remove" => {
                    let old = input.get("old_text").and_then(|v| v.as_str()).unwrap_or("");
                    let preview: String = old.chars().take(40).collect();
                    format!("\u{f014} Forgetting {}: {}", target, preview)
                }
                _ => format!("\u{f0e0} Memory {}", action),
            }
        }
        _ => format!("\u{f05a} {}", tool_name),
    }
}
