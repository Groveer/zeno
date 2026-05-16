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
use crate::hooks::types::HookEvent;
use crate::permissions::checker;
use crate::tools::base::{SubAgentDeps, ToolContext};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

// MAX_AUTO_CONTINUE, STREAM_EVENT_TIMEOUT_SECS, TOOL_EXECUTION_TIMEOUT_SECS
// are now configurable via settings.engine and loaded from init.lua.

/// A collected tool use from the stream.
#[derive(Debug, Clone)]
struct CollectedToolUse {
    id: String,
    name: String,
    input_json: String,
}

/// Try to repair common JSON issues from streaming tool argument concatenation.
///
/// Streaming tool calls can produce malformed JSON when chunk boundaries split
/// inside JSON syntax. This function attempts lightweight repairs:
/// 1. Missing closing `}` — append `}`
/// 2. Trailing comma before `}` — remove the trailing `,`
/// 3. Unclosed string value at end — close with `"`
/// 4. Multiple closing braces — remove extras
fn try_repair_json(raw: &str) -> Option<String> {
    let trimmed = raw.trim();

    // Rule 1: extra closing braces — find the right balance point
    let opens = trimmed.chars().filter(|&c| c == '{').count();
    let closes = trimmed.chars().filter(|&c| c == '}').count();
    if closes > opens {
        // Remove extra closing braces from the end until balanced
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

    // Rule 3: trailing comma before end — remove it
    if trimmed.ends_with(',') {
        let candidate = trimmed.trim_end_matches(',');
        if serde_json::from_str::<Value>(candidate).is_ok() {
            return Some(candidate.to_string());
        }
        // Also try with closing brace
        let candidate = format!("{}}}", candidate);
        if serde_json::from_str::<Value>(&candidate).is_ok() {
            return Some(candidate);
        }
    }

    // Rule 4: unclosed string at end — find last unclosed string and close it
    // Count unescaped quotes to detect unclosed strings
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
        // Try closing the string and the object
        let candidate = format!("{}\"}}", trimmed);
        if serde_json::from_str::<Value>(&candidate).is_ok() {
            return Some(candidate);
        }
        // Also try just closing the string (object might already be closed)
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
fn parse_tool_input(input_json: &str) -> Result<Value, String> {
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
fn parse_tool_input_or_empty(input_json: &str) -> Value {
    parse_tool_input(input_json).unwrap_or_default()
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
        let input = parse_tool_input_or_empty(&tu.input_json);

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
        image_blocks: Vec<(String, String)>, // (media_type, base64_data)
        sender: &tokio::sync::mpsc::UnboundedSender<UiEvent>,
        cancel: CancellationToken,
    ) -> Result<(), ApiError> {
        // --- UserMessage hook: may transform the input ---
        let effective_input = if let Some(he) = &self.hook_executor {
            if he.has_hooks_for(HookEvent::UserMessage) {
                if let Ok(ctx) = he.build_context() {
                    let _ = ctx.set("input", user_input);
                    let _ = ctx.set("cwd", self.cwd.to_string_lossy().to_string());
                    match he.execute_user_message(&ctx).await {
                        Some(modified) => {
                            tracing::info!(
                                original_len = user_input.len(),
                                modified_len = modified.len(),
                                "UserMessage hook modified input"
                            );
                            modified
                        }
                        None => user_input.to_string(),
                    }
                } else {
                    user_input.to_string()
                }
            } else {
                user_input.to_string()
            }
        } else {
            user_input.to_string()
        };

        if image_blocks.is_empty() {
            self.history.push_user(&effective_input);
        } else {
            let mut blocks: Vec<crate::api::types::ContentBlock> =
                vec![crate::api::types::ContentBlock::Text {
                    text: effective_input.clone(),
                }];
            for (media_type, data) in &image_blocks {
                blocks.push(crate::api::types::ContentBlock::Image {
                    media_type: media_type.clone(),
                    data: data.clone(),
                    source_path: String::new(),
                });
            }
            self.history.push_user_blocks(blocks);
        }

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

            // Notify external memory provider of new turn
            if let Some(ref mm) = self.memory_manager {
                let mm = mm.lock().await;
                mm.on_turn_start(turn, &effective_input);
            }

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
                    self.memory_manager.as_ref(),
                    &self.system_prompt,
                    &tool_schemas,
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

            // Inner retry loop
            // Retries up to `settings.llm.max_retries` times on:
            //   - API call errors (non-prompt-too-long)
            //   - Stream consumption errors
            //   - Empty responses (no text, no tool calls)
            // On each retry, emits a Status event (not Error) so the TUI
            // shows a soft notification. Only after exhausting all retries
            // do we propagate the error or fall through to the existing
            // empty-response handling below.
            let mut assistant_text = String::new();
            let mut reasoning_text = String::new();
            let mut tool_uses: Vec<CollectedToolUse> = Vec::new();
            let mut last_stop_reason: Option<StopReason> = None;
            let mut last_error: Option<ApiError> = None;
            // Shared state to track the last tool input JSON that failed parsing,
            // so the empty-response handler can log the raw data for debugging.
            let last_failed_tool_input: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
            let max_retries = self.settings.llm.max_retries;
            let retry_config = RetryConfig::default();

            'retry: for retry_attempt in 0..=max_retries {
                // Reset per-attempt state
                assistant_text.clear();
                reasoning_text.clear();
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
                    let error_detail = match &last_error {
                        Some(e) => format!(": {}", e),
                        None => String::new(),
                    };
                    let label = if last_error.is_some() {
                        "LLM response failed"
                    } else {
                        "LLM response empty"
                    };
                    let _ = sender.send(UiEvent::Status(format!(
                        "{}{} — retrying ({}/{})...",
                        label, error_detail, retry_attempt, max_retries
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

                // --- Vision preprocessing: analyze images via auxiliary model ---
                // For providers that don't support vision (non-Anthropic), send images to the
                // auxiliary vision model. The description is injected into the system prompt so
                // the model treats it as context, not user input.
                let vision_context = self.preprocess_images(sender).await;

                let messages = self.history.to_api_messages();

                // --- PreLlmCall hook: may inject extra context ---
                let mut effective_system_prompt = self.system_prompt.clone();
                // Append vision analysis to system prompt (before hooks so hooks see it)
                if !vision_context.is_empty() {
                    effective_system_prompt.push_str(&vision_context);
                }
                if let Some(he) = &self.hook_executor {
                    if he.has_hooks_for(HookEvent::PreLlmCall) {
                        if let Ok(ctx) = he.build_context() {
                            let _ = ctx.set("model", self.model.as_str());
                            let _ = ctx.set("turn", turn as i64);
                            let _ = ctx.set("cwd", self.cwd.to_string_lossy().to_string());
                            let _ = ctx.set("message_count", messages.len() as i64);
                            let injected = he.execute_pre_llm(&ctx).await;
                            for text in injected {
                                effective_system_prompt.push_str("\n\n");
                                effective_system_prompt.push_str(&text);
                            }
                        }
                    }
                }

                // --- Memory provider prefetch: inject recall context ---
                if let Some(ref mm) = self.memory_manager {
                    let mut mm = mm.lock().await;
                    let prefetch_text = mm.prefetch(&effective_input).await;
                    if !prefetch_text.is_empty() {
                        effective_system_prompt.push_str("\n\n## Relevant Memory\n\n");
                        effective_system_prompt.push_str(&prefetch_text);
                    }
                }

                // Acquire stream (with reactive compact on prompt-too-long)
                let stream = match tokio::select! {
                    result = self
                        .client
                        .stream_messages(
                            &self.model,
                            &effective_system_prompt,
                            &messages,
                            &tool_schemas,
                            self.effective_max_tokens(),
                            self.settings.response_format.as_ref(),
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
                                            self.memory_manager.as_ref(),
                                            &effective_system_prompt,
                                            &tool_schemas,
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
                                                        self.settings.response_format.as_ref(),
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

                // Consume the stream
                tokio::pin!(stream);
                let mut stream_failed = false;

                loop {
                    let event = if self.settings.engine.stream_timeout_secs == 0 {
                        // No timeout — wait indefinitely for the next stream event
                        tokio::select! {
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
                                    self.history.push_assistant_with_reasoning(
                                        blocks,
                                        if reasoning_text.is_empty() {
                                            None
                                        } else {
                                            Some(reasoning_text.clone())
                                        },
                                    );
                                }
                                self.handle_interrupt(sender);
                                return Ok(());
                            }
                        }
                    } else {
                        tokio::select! {
                            event = tokio::time::timeout(
                                std::time::Duration::from_secs(self.settings.engine.stream_timeout_secs),
                                stream.next(),
                            ) => {
                                match event {
                                    Ok(Some(e)) => e,
                                    Ok(None) => break, // stream ended
                                    Err(_elapsed) => {
                                        // Stream stalled — no event for stream_timeout_secs
                                        tracing::warn!(
                                            timeout_secs = self.settings.engine.stream_timeout_secs,
                                            "Stream event timeout — no token from LLM for too long"
                                        );
                                        let _ = sender.send(UiEvent::Error(
                                            format!("Stream timed out after {}s of inactivity. The LLM may be stalled or the network may be down.", self.settings.engine.stream_timeout_secs)
                                        ));
                                        stream_failed = true;
                                        break;
                                    }
                                }
                            }
                            _ = cancel.cancelled() => {
                                tracing::info!(event = "cancelled", phase = "stream_consumption", "query_tui: cancelled during stream consumption");
                                if !assistant_text.trim().is_empty() {
                                    let blocks = vec![ContentBlock::Text {
                                        text: assistant_text.clone(),
                                    }];
                                    self.history.push_assistant_with_reasoning(
                                        blocks,
                                        if reasoning_text.is_empty() {
                                            None
                                        } else {
                                            Some(reasoning_text.clone())
                                        },
                                    );
                                }
                                self.handle_interrupt(sender);
                                return Ok(());
                            }
                        }
                    };

                    match event {
                        Ok(StreamEvent::TextDelta(delta)) => {
                            let _ = sender.send(UiEvent::TextDelta(delta.clone()));
                            assistant_text.push_str(&delta);
                        }
                        Ok(StreamEvent::ReasoningDelta(delta)) => {
                            reasoning_text.push_str(&delta);
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

                            // --- PostLlmCall hook ---
                            if let Some(he) = &self.hook_executor {
                                if he.has_hooks_for(HookEvent::PostLlmCall) {
                                    if let Ok(ctx) = he.build_context() {
                                        let _ = ctx.set("model", self.model.as_str());
                                        let _ = ctx.set("turn", turn as i64);
                                        let _ = ctx
                                            .set("input_tokens", final_usage.input_tokens as i64);
                                        let _ = ctx
                                            .set("output_tokens", final_usage.output_tokens as i64);
                                        let _ = ctx.set(
                                            "total_tokens",
                                            (final_usage.input_tokens + final_usage.output_tokens)
                                                as i64,
                                        );
                                        let _ = ctx
                                            .set("stop_reason", format!("{:?}", last_stop_reason));
                                        let _ =
                                            ctx.set("cwd", self.cwd.to_string_lossy().to_string());
                                        he.execute_post_llm(&ctx).await;
                                    }
                                }
                            }
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
                let input = parse_tool_input_or_empty(&tu.input_json);
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
                // Check if the previous turn had a tool call that failed JSON parsing.
                // If so, the LLM may have given up after receiving the parse error.
                // Log the raw input for offline debugging.
                let failed_raw = last_failed_tool_input.lock().ok().and_then(|g| g.clone());
                if let Some(ref raw) = failed_raw {
                    tracing::error!(
                        event = "llm_empty_after_tool_error",
                        raw_len = raw.len(),
                        raw_preview = %raw.chars().take(200).collect::<String>(),
                        "LLM returned empty after tool input parse failure — raw JSON captured for debugging"
                    );
                }
                tracing::warn!(
                    max_retries = max_retries,
                    has_tool_parse_error = failed_raw.is_some(),
                    "dropping empty assistant message from provider response"
                );
                let error_detail = match &last_error {
                    Some(e) => format!(". Last error: {}", e),
                    None => String::new(),
                };
                let msg = if failed_raw.is_some() {
                    format!(
                        "Model returned empty response after {} attempts (tool input parse error occurred){}",
                        max_retries + 1,
                        error_detail
                    )
                } else {
                    format!(
                        "Model returned empty response after {} attempts{}",
                        max_retries + 1,
                        error_detail
                    )
                };
                let _ = sender.send(UiEvent::Status(msg));

                if auto_continue_count < self.settings.engine.max_auto_continue
                    && self.carryover.has_pending_goal()
                {
                    auto_continue_count += 1;
                    tracing::info!(
                        auto_continue = auto_continue_count,
                        max_auto_continue = self.settings.engine.max_auto_continue,
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

            self.history.push_assistant_with_reasoning(
                assistant_blocks,
                if reasoning_text.is_empty() {
                    None
                } else {
                    Some(reasoning_text.clone())
                },
            );

            // No tool calls — the model gave a text-only response.
            //
            // If the stop_reason was MaxTokens, the output was truncated.
            // Auto-continue by injecting a "continue" message so the model
            // resumes from where it left off (up to max_auto_continue times).
            //
            // If the LLM signaled [DONE] (detected above), skip auto-continue.
            // Otherwise, check if there's a pending goal and loop again.
            if tool_uses.is_empty() {
                let is_max_tokens = matches!(last_stop_reason, Some(StopReason::MaxTokens));

                if is_max_tokens
                    && auto_continue_count < self.settings.engine.max_auto_continue
                    && !assistant_text.trim().is_empty()
                {
                    auto_continue_count += 1;
                    tracing::info!(
                        auto_continue = auto_continue_count,
                        max_auto_continue = self.settings.engine.max_auto_continue,
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
                    && auto_continue_count < self.settings.engine.max_auto_continue
                    && self.carryover.has_pending_goal()
                {
                    auto_continue_count += 1;
                    tracing::info!(
                        auto_continue = auto_continue_count,
                        max_auto_continue = self.settings.engine.max_auto_continue,
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
                    let tu_input = parse_tool_input_or_empty(&tu.input_json);
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

            let mut ctx = ToolContext::with_ask_sender(
                self.cwd.clone(),
                sender.clone(),
                self.mcp_manager.clone(),
            )
            .with_cancel_token(cancel.clone())
            .with_rate_limiter(self.rate_limiter.clone())
            .with_tool_stats(self.tool_stats.clone());

            // Attach sub-agent dependencies if available
            if let Some(ref factory) = self.client_factory {
                let progress_tx = self.sub_agent_tx.clone().unwrap_or_else(|| {
                    let (tx, _) = tokio::sync::mpsc::unbounded_channel();
                    tx
                });
                let deps = SubAgentDeps::new(
                    factory.clone(),
                    self.tools.clone(),
                    self.settings.clone(),
                    progress_tx,
                    self.settings.delegation.clone(),
                    self.sub_agent_cost_tracker.clone(),
                );
                ctx = ctx.with_sub_agent_deps(deps);
            }
            let mut tool_results: Vec<ContentBlock> = Vec::new();

            for tu in &tool_uses {
                let summary = format_tool_input_summary(&tu.name, &tu.input_json);
                let _ = sender.send(UiEvent::ToolStart {
                    name: tu.name.clone(),
                    input_summary: summary,
                });
            }

            // Parallel safety check
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
                // Concurrent execution
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
                            self.tools.as_ref(),
                            tu,
                            &ctx,
                            sender,
                            self.hook_executor.as_ref(),
                            &cancel,
                            Some(&*self.tool_cache),
                            &self.settings.tools.ask_commands,
                            &self.settings.tools.denied_commands,
                            self.settings.engine.tool_timeout_secs,
                            &self.settings.safe_paths,
                            &last_failed_tool_input,
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
                // Sequential execution (fallback)
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
                        self.tools.as_ref(),
                        tu,
                        &ctx,
                        sender,
                        self.hook_executor.as_ref(),
                        &cancel,
                        Some(&*self.tool_cache),
                        &self.settings.tools.ask_commands,
                        &self.settings.tools.denied_commands,
                        self.settings.engine.tool_timeout_secs,
                        &self.settings.safe_paths,
                        &last_failed_tool_input,
                    )
                    .await;
                    tool_results.push(result);
                }
            }

            self.history.push_tool_results(tool_results);

            // Compress edit tool inputs
            // After successful edits, strip common prefix/suffix context
            // lines from the ToolUse.input to save tokens in future API calls.
            {
                let last_entry = self.history.entries_raw().last();
                if let Some(entry) = last_entry {
                    let mut successful_edit_ids = std::collections::HashSet::new();
                    for block in &entry.content {
                        if let ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } = block
                        {
                            if (is_error.is_none() || *is_error == Some(false))
                                && content.starts_with("Replaced")
                            {
                                // Find the matching tool_use name
                                if tool_uses
                                    .iter()
                                    .any(|tu| tu.id == *tool_use_id && tu.name == "edit")
                                {
                                    successful_edit_ids.insert(tool_use_id.clone());
                                }
                            }
                        }
                    }
                    self.history.compress_edit_inputs(&successful_edit_ids);
                }
            }

            // Drain pending steer
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
                let input = parse_tool_input_or_empty(&tu.input_json);
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

            // --- Sync turn to external memory provider ---
            if let Some(ref mm) = self.memory_manager {
                // Build a compact summary of this turn for the provider
                let user_summary = effective_input.clone();
                let assistant_summary = if !assistant_text.trim().is_empty() {
                    assistant_text.clone()
                } else {
                    tool_uses
                        .iter()
                        .map(|tu| format!("[{}]", tu.name))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let mm = mm.lock().await;
                mm.sync_turn(&user_summary, &assistant_summary).await;
                // Queue background prefetch for the next turn
                mm.queue_prefetch(&user_summary);
            }
        }

        // Background skill review
        // After the conversation turn completes, check if we should spawn
        // a background review to capture learnings into the skill library.
        self.turns_since_skill_review += 1;
        if crate::engine::review::should_run_review(
            self.turns_since_skill_review,
            &self.settings.skills,
        ) {
            // Build a compact conversation summary for the review agent
            let conversation_summary = self.build_conversation_summary();

            // Check if we have sub-agent deps available
            if let Some(ref factory) = self.client_factory {
                let deps = SubAgentDeps::new(
                    factory.clone(),
                    self.tools.clone(),
                    self.settings.clone(),
                    self.sub_agent_tx.clone().unwrap_or_else(|| {
                        let (tx, _) = tokio::sync::mpsc::unbounded_channel();
                        tx
                    }),
                    self.settings.delegation.clone(),
                    self.sub_agent_cost_tracker.clone(),
                )
                .with_write_origin(crate::skills::provenance::BACKGROUND_REVIEW);

                crate::engine::review::spawn_background_review(
                    deps,
                    self.cwd.clone(),
                    conversation_summary,
                    self.background_cancel.clone(),
                );

                // Reset the counter so the next review triggers after
                // another `review_interval_turns` turns.
                self.turns_since_skill_review = 0;

                tracing::info!(
                    turn = self.turns_since_skill_review,
                    "Background skill review spawned"
                );
            }
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

    /// Preprocess Image blocks in conversation history via the auxiliary vision model.
    ///
    /// Anthropic natively supports Image blocks, so preprocessing is skipped for it.
    /// For all other providers (OpenAI-compatible), Image blocks are sent to the
    /// auxiliary vision model. Returns a description string to inject into the
    /// system prompt, so the model treats it as context rather than user input.
    async fn preprocess_images(
        &mut self,
        sender: &tokio::sync::mpsc::UnboundedSender<UiEvent>,
    ) -> String {
        // Anthropic handles images natively — no preprocessing needed
        let is_anthropic = self
            .settings
            .providers
            .get(&self.settings.active_provider)
            .map(|p| p.api_type == crate::config::settings::ApiType::Anthropic)
            .unwrap_or(false);
        if is_anthropic {
            return String::new();
        }

        // Check if there are any Image blocks to process
        let has_images = self.history.entries_raw().iter().any(|e| {
            e.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Image { .. }))
        });

        if !has_images {
            return String::new();
        }

        let mut descriptions: Vec<String> = Vec::new();

        // Process each entry that contains Image blocks
        for entry in self.history.entries_mut() {
            let mut new_blocks: Vec<ContentBlock> = Vec::new();
            let mut modified = false;

            for block in &entry.content {
                if let ContentBlock::Image {
                    media_type,
                    data,
                    source_path,
                } = block
                {
                    // Send image to auxiliary vision model
                    let _ = sender.send(UiEvent::Status(
                        "Analyzing image via auxiliary model...".into(),
                    ));

                    match crate::auxiliary::vision::analyze_image(
                        &self.settings,
                        data,
                        media_type,
                        source_path,
                    )
                    .await
                    {
                        Ok(result) => {
                            let label = if source_path.is_empty() {
                                "image (clipboard)".to_string()
                            } else {
                                format!("image ({})", source_path)
                            };
                            descriptions
                                .push(format!("[Image Analysis — {}]:\n{}", label, result.content));
                            modified = true;
                            // Do NOT add a Text block — the description goes to system prompt
                        }
                        Err(e) => {
                            tracing::warn!(
                                event = "vision_fallback",
                                error = %e,
                                "Auxiliary vision model failed, removing image block"
                            );
                            // Remove the image block and add a fallback note
                            descriptions.push(format!(
                                "[Image ({}): analysis failed — {}]",
                                if source_path.is_empty() {
                                    "clipboard"
                                } else {
                                    source_path
                                },
                                e
                            ));
                            modified = true;
                        }
                    }
                } else {
                    new_blocks.push(block.clone());
                }
            }

            if modified {
                entry.content = new_blocks;
            }
        }

        if descriptions.is_empty() {
            String::new()
        } else {
            format!(
                "\n\n## Attached Image Descriptions\n\nThe following images were attached by the user. \
                 Their content has been analyzed and described below. Treat these as context, not as user input:\n\n{}",
                descriptions.join("\n\n---\n\n")
            )
        }
    }

    /// Build a compact summary of the conversation for the background review agent.
    /// Extracts the key user requests and assistant responses from recent turns.
    fn build_conversation_summary(&self) -> String {
        let entries = self.history.entries_raw();
        let total = entries.len();
        // Take the last 10 entries for the summary
        let start = if total > 10 { total - 10 } else { 0 };
        let mut lines = Vec::new();
        for entry in &entries[start..] {
            for block in &entry.content {
                match block {
                    crate::api::types::ContentBlock::Text { text } => {
                        let role = if entry.role == crate::api::types::Role::User {
                            "User"
                        } else {
                            "Assistant"
                        };
                        let preview: String = text.chars().take(2000).collect();
                        if !preview.trim().is_empty() {
                            lines.push(format!("{}: {}", role, preview));
                        }
                    }
                    crate::api::types::ContentBlock::ToolUse { name, input, .. } => {
                        let input_str = serde_json::to_string(input).unwrap_or_default();
                        let preview: String = input_str.chars().take(500).collect();
                        lines.push(format!("Tool call: {} ({})", name, preview));
                    }
                    crate::api::types::ContentBlock::ToolResult {
                        content, is_error, ..
                    } => {
                        let label = if is_error.unwrap_or(false) {
                            "Tool error"
                        } else {
                            "Tool result"
                        };
                        let preview: String = content.chars().take(500).collect();
                        lines.push(format!("{}: {}", label, preview));
                    }
                    _ => {}
                }
            }
        }
        lines.join("\n")
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
    tool_cache: Option<&std::sync::Mutex<crate::tools::cache::ToolCache>>,
    ask_commands: &[String],
    denied_commands: &[String],
    tool_timeout_secs: u64,
    safe_paths: &[String],
    last_failed_tool_input: &Arc<Mutex<Option<String>>>,
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
        tool_cache,
        ask_commands,
        denied_commands,
        tool_timeout_secs,
        safe_paths,
        last_failed_tool_input,
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
    tool_cache: Option<&std::sync::Mutex<crate::tools::cache::ToolCache>>,
    ask_commands: &[String],
    denied_commands: &[String],
    tool_timeout_secs: u64,
    safe_paths: &[String],
    last_failed_tool_input: &Arc<Mutex<Option<String>>>,
) -> Option<ContentBlock> {
    let input = match parse_tool_input(&tu.input_json) {
        Ok(v) => v,
        Err(e) => {
            // Store the raw input_json so the empty-response handler
            // can log it for debugging if the LLM gives up afterwards.
            if let Ok(mut guard) = last_failed_tool_input.lock() {
                *guard = Some(tu.input_json.clone());
            }
            let _ = sender.send(UiEvent::ToolError {
                name: tu.name.clone(),
                error: e.clone(),
            });
            return Some(ContentBlock::ToolResult {
                tool_use_id: tu.id.clone(),
                content: format!("Error: {}", e),
                is_error: Some(true),
            });
        }
    };

    // --- Pre-tool-use hook ---
    if let Some(he) = hook_executor {
        if he.has_hooks_for(HookEvent::PreToolUse) {
            if let Ok(hook_ctx) = he.build_context() {
                let _ = hook_ctx.set("tool_name", tu.name.as_str());
                let _ = hook_ctx.set(
                    "tool_input",
                    crate::hooks::executor::json_to_lua_value(&*he.lua(), &input),
                );
                let _ = hook_ctx.set("cwd", ctx.get_cwd().to_string_lossy().to_string());
                if let Some(block_reason) = he
                    .execute_first_block(HookEvent::PreToolUse, &hook_ctx)
                    .await
                {
                    let _ = sender.send(UiEvent::ToolError {
                        name: tu.name.clone(),
                        error: block_reason.clone(),
                    });
                    return Some(ContentBlock::ToolResult {
                        tool_use_id: tu.id.clone(),
                        content: block_reason,
                        is_error: Some(true),
                    });
                }
            }
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
        let cwd = ctx.get_cwd();
        let resolved = checker::resolve_paths(&tu.name, &input, &cwd);
        let decision = checker::evaluate_permission(
            permission_mode,
            trusted_paths,
            &tu.name,
            is_read_only,
            resolved.file_path.as_deref(),
            resolved.command.as_deref(),
            &cwd,
            ask_commands,
            safe_paths,
            denied_commands,
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
            let display_detail = format_permission_detail(&tu.name, &tu.input_json);
            let (tx, rx) = tokio::sync::oneshot::channel();
            let response_tx = Arc::new(Mutex::new(Some(tx)));
            let _ = sender.send(UiEvent::PermissionAsk {
                tool_name: tu.name.clone(),
                reason: decision.reason.clone(),
                input: display_detail,
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
            // Auto-denied (denied_commands, deny mode, or other policy rejection)
            let denied_reason = decision.reason.clone();
            tracing::warn!(
                tool_name = %tu.name,
                permission_decision = "denied",
                mode = %format!("{:?}", permission_mode),
                reason = %denied_reason,
                tool_id = %tu.id,
                "Tool execution denied by policy"
            );
            let _ = sender.send(UiEvent::ToolError {
                name: tu.name.clone(),
                error: denied_reason.clone(),
            });
            return Some(ContentBlock::ToolResult {
                tool_use_id: tu.id.clone(),
                content: denied_reason,
                is_error: Some(true),
            });
        }
    };

    // User-denied (interactively said "n" to a permission prompt)
    if !permitted {
        tracing::warn!(
            tool_name = %tu.name,
            permission_decision = "denied_by_user",
            tool_id = %tu.id,
            "Tool execution denied by user"
        );
        let _ = sender.send(UiEvent::ToolError {
            name: tu.name.clone(),
            error: "Permission denied by user.".into(),
        });
        return Some(ContentBlock::ToolResult {
            tool_use_id: tu.id.clone(),
            content: "Permission denied by user.".into(),
            is_error: Some(true),
        });
    }

    // --- Tool result cache lookup (read-only tools only) ---
    let cacheable_tools = ["read", "glob", "grep"];
    if cacheable_tools.contains(&tu.name.as_str()) {
        if let Some(cache) = tool_cache {
            if let Ok(mut cache) = cache.lock() {
                if let Some(cached) = cache.get(&tu.name, &input) {
                    tracing::debug!(
                        tool_name = %tu.name,
                        "Tool result cache hit"
                    );
                    let _ = sender.send(UiEvent::ToolOutput {
                        name: tu.name.clone(),
                        output: format!("(cached) {} chars", cached.len()),
                    });
                    return Some(ContentBlock::ToolResult {
                        tool_use_id: tu.id.clone(),
                        content: cached.to_string(),
                        is_error: None,
                    });
                }
            }
        }
    }

    match tokio::time::timeout(
        std::time::Duration::from_secs(tool_timeout_secs),
        tools.execute(&tu.name, input.clone(), ctx),
    )
    .await
    {
        Ok(Ok(result)) => {
            // Cache result for read-only tools
            // Cache result for read-only tools
            if cacheable_tools.contains(&tu.name.as_str()) {
                if let Some(cache) = tool_cache {
                    if let Ok(mut cache) = cache.lock() {
                        cache.insert(&tu.name, &input, result.clone());
                    }
                }
            }

            // Invalidate cache on write/edit
            if tu.name == "write" || tu.name == "edit" {
                if let Some(path) = input.get("path").and_then(|v| v.as_str()) {
                    if let Some(cache) = tool_cache {
                        if let Ok(mut cache) = cache.lock() {
                            cache.invalidate_path(std::path::Path::new(path));
                        }
                    }
                }
            }
            // Show a compact one-line summary in the TUI; full content goes to LLM.
            let display = summarize_tool_output(&tu.name, &result, &tu.input_json);
            let _ = sender.send(UiEvent::ToolOutput {
                name: tu.name.clone(),
                output: display,
            });

            // For edit tool, extract diff from structured JSON result
            if tu.name == "edit" {
                if let Ok(parsed) = serde_json::from_str::<Value>(&result) {
                    if let Some(diff_str) = parsed.get("diff").and_then(|d| d.as_str()) {
                        if !diff_str.is_empty() {
                            let _ = sender.send(UiEvent::ToolDiff {
                                name: tu.name.clone(),
                                diff: diff_str.to_string(),
                            });
                        }
                    }
                }
            }
            tracing::info!(
                tool_name = %tu.name,
                tool_id = %tu.id,
                tool_result = "success",
                result_len = result.len(),
                "Tool executed successfully"
            );

            // --- Post-tool-use hook ---
            if let Some(he) = hook_executor {
                if he.has_hooks_for(HookEvent::PostToolUse) {
                    if let Ok(hook_ctx) = he.build_context() {
                        let _ = hook_ctx.set("tool_name", tu.name.as_str());
                        let _ = hook_ctx.set(
                            "tool_input",
                            crate::hooks::executor::json_to_lua_value(&*he.lua(), &input),
                        );
                        let _ = hook_ctx.set("tool_output", result.clone());
                        let _ = hook_ctx.set("tool_is_error", false);
                        let _ = hook_ctx.set("cwd", ctx.get_cwd().to_string_lossy().to_string());
                        he.execute(HookEvent::PostToolUse, &hook_ctx).await;
                    }
                }
            }

            Some(ContentBlock::ToolResult {
                tool_use_id: tu.id.clone(),
                content: result,
                is_error: None,
            })
        }
        Ok(Err(e)) => {
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
                if he.has_hooks_for(HookEvent::PostToolUse) {
                    if let Ok(hook_ctx) = he.build_context() {
                        let _ = hook_ctx.set("tool_name", tu.name.as_str());
                        let _ = hook_ctx.set(
                            "tool_input",
                            crate::hooks::executor::json_to_lua_value(&*he.lua(), &input),
                        );
                        let _ = hook_ctx.set("tool_output", e.to_string());
                        let _ = hook_ctx.set("tool_is_error", true);
                        let _ = hook_ctx.set("cwd", ctx.get_cwd().to_string_lossy().to_string());
                        he.execute(HookEvent::PostToolUse, &hook_ctx).await;
                    }
                }
            }

            Some(ContentBlock::ToolResult {
                tool_use_id: tu.id.clone(),
                content: format!("Error: {}", e),
                is_error: Some(true),
            })
        }
        Err(_elapsed) => {
            let timeout_msg = format!(
                "Tool '{}' timed out after {}s. The command may be stuck or the output may be too large.",
                tu.name, tool_timeout_secs,
            );
            tracing::warn!(
                tool_name = %tu.name,
                tool_id = %tu.id,
                timeout_secs = tool_timeout_secs,
                "Tool execution timed out"
            );
            let _ = sender.send(UiEvent::ToolError {
                name: tu.name.clone(),
                error: timeout_msg.clone(),
            });

            if let Some(he) = hook_executor {
                if he.has_hooks_for(HookEvent::PostToolUse) {
                    if let Ok(hook_ctx) = he.build_context() {
                        let _ = hook_ctx.set("tool_name", tu.name.as_str());
                        let _ = hook_ctx.set("tool_is_error", true);
                        let _ = hook_ctx.set("cwd", ctx.get_cwd().to_string_lossy().to_string());
                        he.execute(HookEvent::PostToolUse, &hook_ctx).await;
                    }
                }
            }

            Some(ContentBlock::ToolResult {
                tool_use_id: tu.id.clone(),
                content: timeout_msg,
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
///  git status → ok (5 lines)
///  rm -rf / → exit 1: Permission denied
///  src/main.rs → L1-L50 (of 300)
///  Editing src/main.rs → -old_line / +new_line
///
/// Note: No icon prefix here — the icon is already in ToolStart's input_summary.
fn summarize_tool_output(tool_name: &str, output: &str, _input_json: &str) -> String {
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
        return format!(" {}", preview);
    }

    match tool_name {
        // bash: only show command in TUI, result is for LLM only
        "bash" => {
            // Just indicate completion; the command itself is shown in ToolStart
            let line_count = output.lines().count();
            if output.is_empty() || output == "(no output)" {
                "(no output)".to_string()
            } else {
                format!("({} lines)", line_count)
            }
        }
        // read: for LLM consumption — user just needs to know what was read
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
        // glob: just show the count
        "glob" => {
            let first_line = output.lines().next().unwrap_or(output);
            if first_line.starts_with("Found ") {
                first_line.to_string()
            } else {
                let line_count = output.lines().count();
                format!("({} lines)", line_count)
            }
        }
        // grep: show match count + file count
        "grep" => {
            let first_line = output.lines().next().unwrap_or(output);
            if first_line.starts_with("Found ") {
                // Parse file count from results
                let file_count: usize = output
                    .lines()
                    .skip(1) // skip "Found N match(es):"
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
        // web_search / web_fetch: LLM digests the content — user just needs summary
        "web_search" => {
            let result_count = output.lines().filter(|l| !l.is_empty()).count();
            format!("{} results", result_count)
        }
        "web_fetch" => {
            let char_count = output.len();
            format!("{} chars", char_count)
        }
        // edit: compute diff from input for color-coded display
        "edit" => {
            // Structured JSON result from the edit tool
            if let Ok(parsed) = serde_json::from_str::<Value>(output) {
                if let Some(summary) = parsed.get("summary").and_then(|s| s.as_str()) {
                    return summary.to_string();
                }
            }
            // Fallback: plain text result
            return output.to_string();
        }
        // todo: only show the action result line (e.g. " Updated T1 → in_progress").
        // The right-side UI panel already renders the full task list with progress,
        // so showing plan/task details here would be redundant and noisy.
        "todo" => {
            if let Some(action_line) = output.lines().next() {
                action_line.to_string()
            } else {
                output.to_string()
            }
        }
        // memory: parse JSON response into a readable summary
        "memory" => {
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
                        return format!(" memory {} error: {}", target, err);
                    }
                    return format!(" memory {} failed", target);
                }

                let mut result = if let Some(msg) = message {
                    format!(" memory {}: {}", target, msg)
                } else {
                    format!(" memory {} ok", target)
                };

                if !usage.is_empty() {
                    result.push_str(&format!(" ({})", usage));
                }

                // Show entries as a compact list
                if let Some(entries) = val.get("entries").and_then(|v| v.as_array()) {
                    if !entries.is_empty() {
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
                }

                result
            } else {
                // Not JSON — show as-is
                output.to_string()
            }
        }
        // skill_view: LLM reads the content — user just needs to know what was loaded
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
        // skill_list: compact listing — user just needs category + count
        "skill_list" => {
            let line_count = output.lines().count();
            let first_line = output.lines().next().unwrap_or("");
            // First line usually has the count, e.g. "Skills in 'foo' (5):" or "Skill categories (12 total skills):"
            if !first_line.is_empty() {
                first_line.to_string()
            } else {
                format!("({} lines)", line_count)
            }
        }
        // mcp_list_tools: full schemas are for LLM — user just needs the count
        "mcp_list_tools" => {
            let input = parse_tool_input_or_empty(_input_json);
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
        // mcp_call_tool: result is for LLM — user only needs to know it completed
        "mcp_call_tool" => {
            let input = parse_tool_input_or_empty(_input_json);
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
        // mcp_describe_tool: single tool schema — for LLM
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
        // mcp_list_servers: status summary
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
        // delegate_task: JSON result — extract summary fields
        "delegate_task" => {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(output) {
                if let Some(error) = val.get("error").and_then(|v| v.as_str()) {
                    return format!(" delegate_task: {}", error);
                }
                // Batch mode: array of results
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
                // Single task mode: object with summary
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
            // Fallback: show line count
            let line_count = output.lines().count();
            format!("delegate_task ({} lines)", line_count)
        }
        _ => output.to_string(),
    }
}

/// Build a human-readable detail line for permission prompts.
/// Unlike `format_tool_input_summary` (which is a compact one-liner),
/// this shows the key parameters so the user can make an informed decision.
/// Commands are NOT truncated — the user must see the full command to judge safety.
fn format_permission_detail(tool_name: &str, input_json: &str) -> String {
    let input = parse_tool_input_or_empty(input_json);

    match tool_name {
        "bash" => input
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| format!("$ {}", s.trim()))
            .unwrap_or_else(|| "bash command".into()),
        "read" | "read_file" => input
            .get("path")
            .or_else(|| input.get("file_path"))
            .and_then(|v| v.as_str())
            .map(|s| format!("read {}", s))
            .unwrap_or_else(|| "read".into()),
        "write" => {
            let path = input
                .get("path")
                .or_else(|| input.get("file_path"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let lines = input
                .get("content")
                .and_then(|v| v.as_str())
                .map(|c| c.lines().count())
                .unwrap_or(0);
            format!("write {} ({} lines)", path, lines)
        }
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
            let mut detail = if replace_all {
                format!("edit {} (replace all)", path)
            } else {
                format!("edit {}", path)
            };
            // Show what will be replaced so the user can judge the edit
            if let Some(old_str) = input.get("old_string").and_then(|v| v.as_str()) {
                let old_preview: String = old_str.lines().take(3).collect::<Vec<_>>().join("\n");
                let old_truncated = old_str.lines().count() > 3;
                detail.push_str(&format!(
                    "\n  - {}\n  + ",
                    if old_truncated {
                        format!("{}…", old_preview)
                    } else {
                        old_preview
                    }
                ));
                if let Some(new_str) = input.get("new_string").and_then(|v| v.as_str()) {
                    let new_preview: String =
                        new_str.lines().take(3).collect::<Vec<_>>().join("\n");
                    let new_truncated = new_str.lines().count() > 3;
                    let display = if new_truncated {
                        format!("{}…", new_preview)
                    } else {
                        new_preview
                    };
                    detail.push_str(&display);
                }
            }
            detail
        }
        "grep" => {
            let pattern = input
                .get("pattern")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("cwd");
            format!("grep {} in {}", pattern, path)
        }
        "glob" => {
            let pattern = input
                .get("pattern")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("cwd");
            format!("glob {} in {}", pattern, path)
        }
        "web_search" => input
            .get("query")
            .and_then(|v| v.as_str())
            .map(|s| format!("web_search: {}", s))
            .unwrap_or_else(|| "web_search".into()),
        "web_fetch" => input
            .get("url")
            .and_then(|v| v.as_str())
            .map(|s| format!("web_fetch {}", s))
            .unwrap_or_else(|| "web_fetch".into()),
        _ => input_json.to_string(),
    }
}

/// Build a one-line summary of a tool's input for the TUI status display.
/// Uses Nerd Font icons (PUA codepoints) instead of emoji for reliable
/// terminal rendering via ratatui.
fn format_tool_input_summary(tool_name: &str, input_json: &str) -> String {
    let input = parse_tool_input_or_empty(input_json);

    match tool_name {
        "bash" => input
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| {
                let s = s.trim();
                format!("\u{f489} $ {}", s)
            })
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
        "todo" => {
            let action = input.get("action").and_then(|v| v.as_str()).unwrap_or("?");
            let icon = match action {
                "create" => "\u{f0ca}", // list
                "add" => "\u{f067}",    // plus
                "update" => "\u{f021}", // refresh
                "delete" => "\u{f1f8}", // trash
                "list" => "\u{f15c}",   // file text
                _ => "\u{f0ca}",        // list default
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
