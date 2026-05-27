//! Core tool-aware conversation loop.
//!
//! This module is split into submodules:
//! - `json_utils`: JSON repair and parsing for streaming tool arguments
//! - `tool_exec`: Tool execution, parallel safety, permissions, hooks
//! - `tool_display`: TUI formatting for tool inputs and outputs

mod json_utils;
mod tool_display;
mod tool_exec;

// Re-export for use by other engine modules (currently unused externally but available)
#[allow(unused_imports)]
pub(crate) use json_utils::{parse_tool_input, parse_tool_input_or_empty};

use futures::StreamExt;
use std::sync::{Arc, Mutex};

use crate::api::retry::{RetryConfig, get_retry_delay};
use crate::api::types::{ApiError, ContentBlock, StopReason, StreamEvent, Usage};
use crate::engine::carryover::resolve_file_path;
use crate::engine::compact::{auto_compact_if_needed, is_prompt_too_long_error};
use crate::engine::query_engine::QueryEngine;
use crate::engine::tui_events::EngineEvent;
use crate::hooks::types::HookEvent;
use crate::prompts::system_prompt::{MEMORY_GUIDANCE, session_files_block};
use crate::store::EdgeStatus;
use crate::tools::base::{SubAgentDeps, ToolContext};
use tokio_util::sync::CancellationToken;

use json_utils::{parse_tool_input_or_empty as ptoi_empty, safe_truncate_str};
use tool_display::format_tool_input_summary;
pub(crate) use tool_exec::format_permission_detail;
use tool_exec::{
    CollectedToolUse, ToolExecConfig, execute_single_tool_tui_catch, should_parallelize,
};

/// Check if cancellation was requested. If so, log, handle interrupt, and return `Ok(())`.
macro_rules! bail_if_cancelled {
    ($cancel:expr, $self:expr, $sender:expr, $phase:expr) => {
        if $cancel.is_cancelled() {
            tracing::info!(event = "cancelled", phase = $phase, "query_tui: cancelled");
            $self.handle_interrupt($sender);
            return Ok(());
        }
    };
}

impl QueryEngine {
    /// Run a user query with TUI event streaming.
    /// `cancel`: a CancellationToken that the caller can cancel (e.g. on Ctrl+C).
    /// When cancelled, the loop exits gracefully at the next check point.
    pub async fn query_tui(
        &mut self,
        user_input: &str,
        image_blocks: Vec<(String, String)>, // (media_type, base64_data)
        sender: &tokio::sync::mpsc::UnboundedSender<EngineEvent>,
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
            let mut blocks: Vec<ContentBlock> = vec![ContentBlock::Text {
                text: effective_input.clone(),
            }];
            for (media_type, data) in &image_blocks {
                blocks.push(ContentBlock::Image {
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
                let _ = sender.send(EngineEvent::Status(format!(
                "max turns ({}) reached — task may not be complete. Type '继续' or 'continue' to resume.",
                self.max_turns
            )));
                self.clear_steer();
                let _ = sender.send(EngineEvent::QueryDone {
                    text: String::new(),
                    tool_calls: 0,
                    tokens: self.cost_tracker.last_prompt_tokens
                        + self.cost_tracker.last_output_tokens,
                });
                break;
            }

            // Auto-compact
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
                let _ = sender.send(EngineEvent::CompactProgress {
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
            let mut assistant_text = String::new();
            let mut reasoning_text = String::new();
            let mut tool_uses: Vec<CollectedToolUse> = Vec::new();
            let mut last_stop_reason: Option<StopReason> = None;
            let mut last_error: Option<ApiError> = None;
            let last_failed_tool_input: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
            let max_retries = self.settings.llm.max_retries;
            let retry_config = RetryConfig::default();

            'retry: for retry_attempt in 0..=max_retries {
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
                    let _ = sender.send(EngineEvent::Status(format!(
                        "{}{} — retrying ({}/{})...",
                        label, error_detail, retry_attempt, max_retries
                    )));
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

                self.history.sanitize();

                // --- Vision preprocessing ---
                let vision_context = self.preprocess_images(sender).await;

                let messages = self.history.to_api_messages();

                // --- PreLlmCall hook ---
                let mut effective_system_prompt = self.system_prompt.clone();
                if !vision_context.is_empty() {
                    effective_system_prompt.push_str(&vision_context);
                }

                // --- Inject session files already read context ---
                // Tells the LLM which files it has already seen, reducing redundant reads.
                {
                    let pool = self.file_content_pool.lock().await;
                    if let Some(summary) = pool.read_files_summary() {
                        let block = session_files_block(&summary);
                        effective_system_prompt.push_str("\n\n");
                        effective_system_prompt.push_str(&block);
                    }
                }

                // --- Inject sub-agent context ---
                // Tells the LLM how many open sub-agents it has and how to query them.
                // Uses `self.task_id` so sub-agents see their own children in multi-level delegation.
                {
                    if let Some(ref store) = self.graph_store {
                        // Single query: get all children with details, then derive
                        // counts in-memory instead of two sequential list_children calls.
                        if let Ok(records) =
                            store.list_children_with_details(&self.task_id, None).await
                        {
                            let open = records
                                .iter()
                                .filter(|r| r.status == EdgeStatus::Open)
                                .count();
                            if open > 0 {
                                let block = crate::prompts::system_prompt::sub_agent_block(
                                    open,
                                    records.len(),
                                );
                                effective_system_prompt.push_str("\n\n");
                                effective_system_prompt.push_str(&block);
                            }
                        }
                    }
                }
                if let Some(he) = &self.hook_executor
                    && he.has_hooks_for(HookEvent::PreLlmCall)
                    && let Ok(ctx) = he.build_context()
                {
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

                // --- Memory provider prefetch ---
                if let Some(ref mm) = self.memory_manager {
                    let mut mm = mm.lock().await;
                    let prefetch_text = mm.prefetch(&effective_input).await;
                    if !prefetch_text.is_empty() {
                        effective_system_prompt.push_str("\n\n## Relevant Memory\n\n");
                        effective_system_prompt.push_str(&prefetch_text);
                    }
                }

                // --- Live built-in memory ---
                if let Some(ref mm) = self.memory_manager {
                    let mm = mm.lock().await;
                    let live_block = mm.build_system_prompt();
                    if !live_block.is_empty() {
                        effective_system_prompt.push_str("\n\n");
                        effective_system_prompt.push_str(MEMORY_GUIDANCE);
                        effective_system_prompt.push_str("\n\n");
                        effective_system_prompt.push_str(&live_block);
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
                                    tracing::warn!(compact_trigger = "reactive", "Prompt too long, triggering reactive compact");
                                    let _ = sender.send(EngineEvent::Status("Prompt too long, compressing...".into()));
                                    let cw = self.effective_context_window();
                                    match tokio::select! {
                                        r = auto_compact_if_needed(
                                            &self.settings, &mut self.history, &self.compact_config,
                                            &self.carryover, cw, self.memory_manager.as_ref(),
                                            &effective_system_prompt, &tool_schemas,
                                        ) => r,
                                        _ = cancel.cancelled() => {
                                            self.handle_interrupt(sender);
                                            return Ok(());
                                        }
                                    } {
                                        Ok(Some(result)) => {
                                            let _ = sender.send(EngineEvent::Status(format!(
                                                "reactive-compact: {} → {} tokens",
                                                result.tokens_before, result.tokens_after
                                            )));
                                            self.history.sanitize();
                                            let retry_messages = self.history.to_api_messages();
                                            tokio::select! {
                                                r = self.client.stream_messages(
                                                    &self.model, &self.system_prompt, &retry_messages,
                                                    &tool_schemas, self.effective_max_tokens(),
                                                    self.settings.response_format.as_ref(),
                                                ) => r.map_err(Some),
                                                _ = cancel.cancelled() => {
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
                        tokio::select! {
                            event = stream.next() => {
                                match event {
                                    Some(e) => e,
                                    None => break,
                                }
                            }
                            _ = cancel.cancelled() => {
                                if !assistant_text.trim().is_empty() {
                                    let blocks = vec![ContentBlock::Text { text: assistant_text.clone() }];
                                    self.history.push_assistant_with_reasoning(blocks,
                                        if reasoning_text.is_empty() { None } else { Some(reasoning_text.clone()) },
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
                                    Ok(None) => break,
                                    Err(_elapsed) => {
                                        tracing::warn!(
                                            timeout_secs = self.settings.engine.stream_timeout_secs,
                                            "Stream event timeout — no token from LLM for too long"
                                        );
                                        let _ = sender.send(EngineEvent::Error(
                                            format!("Stream timed out after {}s of inactivity.", self.settings.engine.stream_timeout_secs)
                                        ));
                                        stream_failed = true;
                                        break;
                                    }
                                }
                            }
                            _ = cancel.cancelled() => {
                                if !assistant_text.trim().is_empty() {
                                    let blocks = vec![ContentBlock::Text { text: assistant_text.clone() }];
                                    self.history.push_assistant_with_reasoning(blocks,
                                        if reasoning_text.is_empty() { None } else { Some(reasoning_text.clone()) },
                                    );
                                }
                                self.handle_interrupt(sender);
                                return Ok(());
                            }
                        }
                    };

                    match event {
                        Ok(StreamEvent::TextDelta(delta)) => {
                            let _ = sender.send(EngineEvent::TextDelta(delta.clone()));
                            assistant_text.push_str(&delta);
                        }
                        Ok(StreamEvent::ReasoningDelta(delta)) => {
                            let _ = sender.send(EngineEvent::ReasoningDelta(delta.clone()));
                            reasoning_text.push_str(&delta);
                        }
                        Ok(StreamEvent::ToolUseStart {
                            id,
                            name,
                            input_json,
                        }) => {
                            tracing::debug!(
                                event = "tool_use_start",
                                name = name,
                                id = id,
                                "Received ToolUseStart from API"
                            );
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
                            let _ = sender.send(EngineEvent::TokenUpdate {
                                total_tokens: self.cost_tracker.last_prompt_tokens
                                    + self.cost_tracker.last_output_tokens,
                                turn_count: self.cost_tracker.turn_count,
                            });
                            last_stop_reason = Some(stop_reason);

                            // --- PostLlmCall hook ---
                            if let Some(he) = &self.hook_executor
                                && he.has_hooks_for(HookEvent::PostLlmCall)
                                && let Ok(ctx) = he.build_context()
                            {
                                let _ = ctx.set("model", self.model.as_str());
                                let _ = ctx.set("turn", turn as i64);
                                let _ = ctx.set("input_tokens", final_usage.input_tokens as i64);
                                let _ = ctx.set("output_tokens", final_usage.output_tokens as i64);
                                let _ = ctx.set(
                                    "total_tokens",
                                    (final_usage.input_tokens + final_usage.output_tokens) as i64,
                                );
                                let _ = ctx.set("stop_reason", format!("{:?}", last_stop_reason));
                                let _ = ctx.set("cwd", self.cwd.to_string_lossy().to_string());
                                he.execute_post_llm(&ctx).await;
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

                if stream_failed {
                    continue 'retry;
                }

                // Empty response — retry
                if assistant_text.trim().is_empty()
                    && reasoning_text.trim().is_empty()
                    && tool_uses.is_empty()
                {
                    tracing::warn!(
                        retry_attempt = retry_attempt + 1,
                        max_attempts = max_retries + 1,
                        "LLM returned empty response"
                    );
                    if retry_attempt < max_retries {
                        continue 'retry;
                    }
                    break 'retry;
                }

                break 'retry;
            } // end 'retry loop

            // --- [DONE] sentinel detection ---
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
                let input = ptoi_empty(&tu.input_json);
                assistant_blocks.push(ContentBlock::ToolUse {
                    id: tu.id.clone(),
                    name: tu.name.clone(),
                    input,
                });
            }

            // Guard: drop empty assistant messages
            if assistant_text.trim().is_empty()
                && reasoning_text.trim().is_empty()
                && tool_uses.is_empty()
            {
                let failed_raw = last_failed_tool_input.lock().ok().and_then(|g| g.clone());
                if let Some(ref raw) = failed_raw {
                    tracing::error!(event = "llm_empty_after_tool_error", raw_len = raw.len(),
                        raw_preview = %raw.chars().take(200).collect::<String>(),
                        "LLM returned empty after tool input parse failure");
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
                let _ = sender.send(EngineEvent::Status(msg));

                if auto_continue_count < self.settings.engine.max_auto_continue
                    && self.carryover.has_pending_goal()
                {
                    auto_continue_count += 1;
                    tracing::info!(
                        auto_continue = auto_continue_count,
                        "auto-continue: injecting continuation for empty response"
                    );
                    let _ = sender.send(EngineEvent::Status(
                        "Model returned empty — retrying with goal reminder...".into(),
                    ));
                    if let Some(prompt) = self.carryover.build_continuation_prompt() {
                        self.history.push_user(&prompt);
                    }
                    continue;
                }

                let _ = sender.send(EngineEvent::QueryDone {
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

            // No tool calls — text-only response handling
            if tool_uses.is_empty() {
                let is_max_tokens = matches!(last_stop_reason, Some(StopReason::MaxTokens));
                let has_content =
                    !assistant_text.trim().is_empty() || !reasoning_text.trim().is_empty();

                if is_max_tokens
                    && auto_continue_count < self.settings.engine.max_auto_continue
                    && has_content
                {
                    auto_continue_count += 1;
                    tracing::info!(
                        auto_continue = auto_continue_count,
                        reason = "max_tokens",
                        "auto-continue: output truncated"
                    );
                    let _ = sender.send(EngineEvent::Status(
                        "Output truncated (max tokens reached) — continuing...".into(),
                    ));
                    self.history.push_user("Your previous response was cut off because it reached the maximum output token limit. Please continue from exactly where you left off — do not repeat what you already said.");
                    continue;
                }

                if !reasoning_text.trim().is_empty()
                    && assistant_text.trim().is_empty()
                    && auto_continue_count < self.settings.engine.max_auto_continue
                {
                    auto_continue_count += 1;
                    tracing::info!(
                        auto_continue = auto_continue_count,
                        reason = "reasoning_only",
                        "auto-continue: reasoning only"
                    );
                    let _ = sender.send(EngineEvent::Status("Model is thinking...".into()));
                    if let Some(prompt) = self.carryover.build_continuation_prompt() {
                        self.history.push_user(&prompt);
                    } else {
                        self.history.push_user("Please provide your response or tool calls based on your analysis above. Do not repeat your reasoning.");
                    }
                    continue;
                }

                if !done_signaled
                    && auto_continue_count < self.settings.engine.max_auto_continue
                    && self.carryover.has_pending_goal()
                {
                    auto_continue_count += 1;
                    tracing::info!(
                        auto_continue = auto_continue_count,
                        reason = "pending_goal",
                        "auto-continue: goal pending"
                    );
                    let _ = sender.send(EngineEvent::Status(
                        "Task not complete — enforcing tool use...".into(),
                    ));
                    if let Some(prompt) = self.carryover.build_continuation_prompt() {
                        self.history.push_user(&prompt);
                    }
                    continue;
                }

                self.carryover.clear_goal();
                self.clear_steer();

                // Background skill review — also trigger on text-only exit
                self.maybe_spawn_background_review(sender);

                let _ = sender.send(EngineEvent::QueryDone {
                    text: String::new(),
                    tool_calls: 0,
                    tokens: self.cost_tracker.last_prompt_tokens
                        + self.cost_tracker.last_output_tokens,
                });
                break;
            }

            // Check cancellation before executing tools
            if cancel.is_cancelled() {
                let mut pre_tool_blocks: Vec<ContentBlock> = Vec::new();
                if !assistant_text.trim().is_empty() {
                    pre_tool_blocks.push(ContentBlock::Text {
                        text: assistant_text.clone(),
                    });
                }
                for tu in &tool_uses {
                    let tu_input = ptoi_empty(&tu.input_json);
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
                self.task_id.clone(),
            )
            .with_cancel_token(cancel.clone())
            .with_rate_limiter(self.rate_limiter.clone())
            .with_tool_stats(self.tool_stats.clone())
            .with_file_content_pool(self.file_content_pool.clone())
            .with_tool_registry(self.tools.clone());

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
                )
                .with_tui_event_sender(sender.clone())
                .with_permission_allow_all(self.permission_allow_all.clone())
                .with_exec_policy(self.exec_policy.clone())
                .with_graph_store_opt(self.graph_store.clone());
                ctx = ctx.with_sub_agent_deps(deps);
            }
            let mut tool_results: Vec<ContentBlock> = Vec::new();

            for tu in &tool_uses {
                let summary = format_tool_input_summary(&tu.name, &tu.input_json);
                let _ = sender.send(EngineEvent::ToolStart {
                    name: tu.name.clone(),
                    input_summary: summary,
                });
            }

            let tool_config = ToolExecConfig {
                permission_mode: &self.permission_mode,
                trusted_paths: &self.settings.trusted_paths,
                permission_allow_all: &self.permission_allow_all,
                tools: self.tools.as_ref(),
                hook_executor: self.hook_executor.as_ref(),
                tool_cache: Some(&*self.tool_cache),
                safe_paths: &self.settings.safe_paths,
                exec_policy: Some(&self.exec_policy),
            };

            let parallel = should_parallelize(&tool_uses, &self.cwd, self.tools.as_ref());
            if !parallel {
                tracing::info!(
                    tool_count = tool_uses.len(),
                    execution_mode = "sequential",
                    "Tool batch not parallel-safe, executing sequentially"
                );
            }

            if parallel {
                let futures: Vec<_> = tool_uses
                    .iter()
                    .map(|tu| {
                        execute_single_tool_tui_catch(
                            &tool_config,
                            tu,
                            &ctx,
                            sender,
                            &cancel,
                            &last_failed_tool_input,
                        )
                    })
                    .collect();

                let results = tokio::select! {
                    r = futures::future::join_all(futures) => r,
                    _ = cancel.cancelled() => {
                        self.handle_interrupt(sender);
                        return Ok(());
                    }
                };

                if cancel.is_cancelled() {
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
                for tu in &tool_uses {
                    bail_if_cancelled!(cancel, self, sender, "sequential_tool_execution");
                    let result = execute_single_tool_tui_catch(
                        &tool_config,
                        tu,
                        &ctx,
                        sender,
                        &cancel,
                        &last_failed_tool_input,
                    )
                    .await;
                    tool_results.push(result);
                }
            }

            self.history.push_tool_results(tool_results);

            // Compress edit tool inputs
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
                            && (is_error.is_none() || *is_error == Some(false))
                            && content.starts_with("Replaced")
                            && tool_uses
                                .iter()
                                .any(|tu| tu.id == *tool_use_id && tu.name == "edit")
                        {
                            successful_edit_ids.insert(tool_use_id.clone());
                        }
                    }
                    self.history.compress_edit_inputs(&successful_edit_ids);
                }
            }

            // Drain pending steer
            if let Some(steer_text) = self.drain_steer() {
                tracing::info!(
                    steer_len = steer_text.len(),
                    "Draining pending steer into conversation"
                );
                let _ = sender.send(EngineEvent::Status(format!(
                    "⇢ Steered: {}",
                    safe_truncate_str(&steer_text, 60)
                )));
                let _ = sender.send(EngineEvent::SteerHandled);
                if !self.history.append_steer_to_last_tool_result(&steer_text) {
                    tracing::warn!(
                        event = "steer_fallback",
                        "No tool result to append steer to, injecting as user message"
                    );
                    self.history.push_user(&steer_text);
                }
            }

            self.record_tool_carryover(&tool_uses);
            self.sync_memory_turn(&effective_input, &assistant_text, &tool_uses)
                .await;
        }

        // Background skill review
        self.maybe_spawn_background_review(sender);

        Ok(())
    }

    /// Record tool results in carryover working memory for context preservation.
    fn record_tool_carryover(&mut self, tool_uses: &[CollectedToolUse]) {
        for tu in tool_uses {
            let input = ptoi_empty(&tu.input_json);
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
    }

    /// Sync the current turn to the external memory provider (if configured).
    async fn sync_memory_turn(
        &self,
        user_input: &str,
        assistant_text: &str,
        tool_uses: &[CollectedToolUse],
    ) {
        if let Some(ref mm) = self.memory_manager {
            let assistant_summary = if !assistant_text.trim().is_empty() {
                assistant_text.to_string()
            } else {
                tool_uses
                    .iter()
                    .map(|tu| format!("[{}]", tu.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let mm = mm.lock().await;
            mm.sync_turn(user_input, &assistant_summary).await;
            mm.queue_prefetch(user_input);
        }
    }

    /// Check if a background skill review should run, and spawn it if so.
    fn maybe_spawn_background_review(
        &mut self,
        sender: &tokio::sync::mpsc::UnboundedSender<EngineEvent>,
    ) {
        self.turns_since_skill_review += 1;
        if !crate::engine::review::should_run_review(
            self.turns_since_skill_review,
            &self.settings.skills,
        ) {
            return;
        }
        let Some(ref factory) = self.client_factory else {
            return;
        };
        let conversation_summary = self.build_conversation_summary();
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
        .with_write_origin(crate::skills::provenance::BACKGROUND_REVIEW)
        .with_graph_store_opt(self.graph_store.clone())
        .with_tui_event_sender(sender.clone())
        .with_permission_allow_all(self.permission_allow_all.clone());

        crate::engine::review::spawn_background_review(
            deps,
            self.cwd.clone(),
            conversation_summary,
            self.background_cancel.clone(),
        );
        self.turns_since_skill_review = 0;
        tracing::info!("Background skill review spawned");
    }

    /// Called when the query is interrupted (Ctrl+C).
    fn handle_interrupt(&mut self, sender: &tokio::sync::mpsc::UnboundedSender<EngineEvent>) {
        tracing::info!(event = "interrupted", "Query interrupted by user");
        self.clear_steer();
        self.history.sanitize();
        let _ = sender.send(EngineEvent::Interrupted);
        let _ = sender.send(EngineEvent::QueryDone {
            text: String::new(),
            tool_calls: 0,
            tokens: self.cost_tracker.last_prompt_tokens + self.cost_tracker.last_output_tokens,
        });
    }

    /// Preprocess Image blocks via the auxiliary vision model.
    /// Anthropic natively supports images — skip for it.
    async fn preprocess_images(
        &mut self,
        sender: &tokio::sync::mpsc::UnboundedSender<EngineEvent>,
    ) -> String {
        let is_anthropic = self
            .settings
            .providers
            .get(&self.settings.active_provider)
            .map(|p| p.api_type == crate::config::settings::ApiType::Anthropic)
            .unwrap_or(false);
        if is_anthropic {
            return String::new();
        }

        let has_images = self.history.entries_raw().iter().any(|e| {
            e.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Image { .. }))
        });
        if !has_images {
            return String::new();
        }

        let mut descriptions: Vec<String> = Vec::new();
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
                    let _ = sender.send(EngineEvent::Status(
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
                        }
                        Err(e) => {
                            tracing::warn!(event = "vision_fallback", error = %e, "Auxiliary vision model failed, removing image block");
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
    fn build_conversation_summary(&self) -> String {
        let entries = self.history.entries_raw();
        let total = entries.len();
        let start = total.saturating_sub(10);
        let mut lines = Vec::new();
        for entry in &entries[start..] {
            for block in &entry.content {
                match block {
                    ContentBlock::Text { text } => {
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
                    ContentBlock::ToolUse { name, input, .. } => {
                        let input_str = serde_json::to_string(input).unwrap_or_default();
                        let preview: String = input_str.chars().take(500).collect();
                        lines.push(format!("Tool call: {} ({})", name, preview));
                    }
                    ContentBlock::ToolResult {
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
