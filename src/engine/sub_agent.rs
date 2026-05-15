//! Sub-agent engine for delegated tasks.
//!
//! Spawns a child agent with isolated conversation context, restricted toolset,
//! and its own API client. Each sub-agent gets:
//! - A fresh conversation (no parent history)
//! - Its own API client (from the parent's client_factory)
//! - A restricted toolset (configurable)
//! - A focused system prompt built from the delegated goal + context
//!
//! The parent's context only sees the delegation call and the summary result,
//! never the child's intermediate tool calls or reasoning.
//!
//! Design reference: hermes-agent `tools/delegate_tool.py` (2562 lines of Python).
//! Key differences:
//! - Uses async Rust with tokio instead of ThreadPoolExecutor
//! - Sub-agents share the parent's ToolRegistry (read-only reference)
//! - Progress via mpsc channel instead of callbacks

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::api::client::SupportsStreamingMessages;
use crate::api::types::{ContentBlock, Role, StreamEvent, Usage};
use crate::engine::messages::ConversationHistory;
use crate::tools::base::{SubAgentDeps, ToolContext};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

// DEFAULT_SUBAGENT_MAX_TURNS is now configurable via delegation_config.max_turns.

// SUBAGENT_MAX_AUTO_CONTINUE is now configurable via delegation_config.max_auto_continue.

/// Built-in tools that sub-agents must never have access to.
const BUILTIN_SUBAGENT_BLOCKED_TOOLS: &[&str] = &[
    "delegate_task", // No recursive delegation
    "ask_user",      // No user interaction from sub-agents
    "memory",        // No writes to shared memory
    "skill_manage",  // No modification to skill system
];

/// Built-in default tools available to sub-agents (read-only safe set).
const BUILTIN_SUBAGENT_TOOLS: &[&str] = &["read", "glob", "grep", "web_search", "web_fetch"];

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Result from a single sub-agent execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentResult {
    /// The final summary text produced by the sub-agent.
    pub summary: String,
    /// Number of API calls (turns) made by the sub-agent.
    pub api_calls: u32,
    /// How the sub-agent finished: "completed", "max_turns", "interrupted", "timeout", "error".
    pub exit_reason: String,
    /// Token usage from the sub-agent.
    pub tokens: SubAgentTokenUsage,
    /// Tool trace: what tools were called.
    pub tool_trace: Vec<ToolTraceEntry>,
    /// Whether the sub-agent was interrupted.
    pub interrupted: bool,
    /// Error message if the sub-agent failed.
    pub error: Option<String>,
    /// Duration in seconds.
    pub duration_seconds: f64,
    /// Files that the sub-agent created or modified (for parent re-read).
    #[serde(default)]
    pub modified_files: Vec<String>,
}

/// Token usage summary from a sub-agent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubAgentTokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// A single tool call trace entry from a sub-agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolTraceEntry {
    pub tool: String,
    pub args_bytes: usize,
    pub result_bytes: usize,
    pub is_error: bool,
}

/// Events emitted by a running sub-agent for parent progress display.
#[derive(Debug, Clone)]
#[allow(
    dead_code,
    reason = "event fields are consumed by the TUI integration layer"
)]
pub enum SubAgentEvent {
    Started {
        task_index: usize,
        goal: String,
        tools: Vec<String>,
    },
    Thinking {
        task_index: usize,
        text: String,
    },
    ToolStarted {
        task_index: usize,
        tool: String,
        input_summary: String,
    },
    ToolCompleted {
        task_index: usize,
        tool: String,
        result_bytes: usize,
        is_error: bool,
    },
    Status {
        task_index: usize,
        message: String,
    },
    Completed {
        task_index: usize,
        result: SubAgentResult,
    },
}

/// Errors from sub-agent operations.
#[derive(Debug, thiserror::Error)]
#[allow(
    dead_code,
    reason = "reserved for future sub-agent lifecycle management"
)]
pub enum SubAgentError {
    #[error("Client creation failed: {0}")]
    ClientCreation(String),
    #[error("No provider available")]
    NoProvider,
    #[error("Sub-agent timed out after {0}s")]
    Timeout(f64),
    #[error("Cancelled")]
    Cancelled,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Run a single delegated task using dependencies from `SubAgentDeps`.
///
/// This is the main entry point called from the `delegate_task` tool.
pub async fn run_delegated_task(
    deps: &SubAgentDeps,
    cwd: PathBuf,
    goal: String,
    context: Option<String>,
    extra_tools: Vec<String>,
    cancel: CancellationToken,
    progress_tx: tokio::sync::mpsc::UnboundedSender<SubAgentEvent>,
) -> SubAgentResult {
    let allowed_tools = build_effective_tools(
        &extra_tools,
        &deps.delegation_config.default_tools,
        &deps.delegation_config.blocked_tools,
    );
    let timeout = deps.delegation_config.child_timeout.max(30.0);

    run_single_sub_agent(
        deps,
        cwd,
        0,
        &goal,
        context.as_deref(),
        &allowed_tools,
        timeout,
        &cancel,
        &progress_tx,
    )
    .await
}

/// Run multiple delegated tasks in parallel (batch mode).
///
/// Each task runs in its own `tokio::task::spawn` with a shared `CancellationToken`.
/// Results are returned in task order.
#[allow(dead_code, reason = "used by delegate_task tool in batch mode")]
pub async fn run_delegated_tasks_batch(
    deps: SubAgentDeps,
    cwd: PathBuf,
    tasks: Vec<(String, Option<String>)>, // (goal, context) pairs
    extra_tools: Vec<String>,
    max_concurrent: usize,
    child_timeout: f64,
    cancel: CancellationToken,
    progress_tx: tokio::sync::mpsc::UnboundedSender<SubAgentEvent>,
) -> Vec<SubAgentResult> {
    let n_tasks = tasks.len();
    if n_tasks == 0 {
        return vec![];
    }

    let allowed_tools = build_effective_tools(
        &extra_tools,
        &deps.delegation_config.default_tools,
        &deps.delegation_config.blocked_tools,
    );
    let max_concurrent = max_concurrent.max(1);

    // Use a semaphore to limit concurrency
    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrent));

    // Create a child cancellation token so we can cancel all tasks at once
    let batch_cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    let _cancel_link = {
        let bc = batch_cancel.clone();
        tokio::spawn(async move {
            cancel_clone.cancelled().await;
            bc.cancel();
        })
    };

    let mut handles = Vec::with_capacity(n_tasks);

    for (i, (goal, context)) in tasks.into_iter().enumerate() {
        let permit = semaphore.clone().acquire_owned().await;
        let deps = deps.clone();
        let cwd = cwd.clone();
        let allowed_tools = allowed_tools.clone();
        let pt = progress_tx.clone();
        let bc = batch_cancel.clone();

        let handle = tokio::spawn(async move {
            let _permit = permit; // held until task finishes

            let _ = pt.send(SubAgentEvent::Started {
                task_index: i,
                goal: goal.clone(),
                tools: allowed_tools.clone(),
            });

            let result = run_single_sub_agent(
                &deps,
                cwd,
                i,
                &goal,
                context.as_deref(),
                &allowed_tools,
                child_timeout,
                &bc,
                &pt,
            )
            .await;

            let _ = pt.send(SubAgentEvent::Completed {
                task_index: i,
                result: result.clone(),
            });

            result
        });

        handles.push(handle);
    }

    // Collect results in order — cancel_link stays alive so Ctrl+C
    // propagates through the entire batch execution.
    // After cancellation, use a grace timeout to avoid blocking shutdown
    // on stuck sub-agent tasks.
    let mut results = Vec::with_capacity(n_tasks);
    for handle in handles {
        if cancel.is_cancelled() {
            match tokio::time::timeout(std::time::Duration::from_secs(5), handle).await {
                Ok(Ok(result)) => results.push(result),
                _ => results.push(SubAgentResult {
                    summary: String::new(),
                    api_calls: 0,
                    exit_reason: "interrupted".into(),
                    tokens: SubAgentTokenUsage::default(),
                    tool_trace: vec![],
                    interrupted: true,
                    error: None,
                    duration_seconds: 0.0,
                    modified_files: vec![],
                }),
            }
        } else {
            match handle.await {
                Ok(result) => results.push(result),
                Err(e) => {
                    results.push(SubAgentResult {
                        summary: String::new(),
                        api_calls: 0,
                        exit_reason: "error".into(),
                        tokens: SubAgentTokenUsage::default(),
                        tool_trace: vec![],
                        interrupted: false,
                        error: Some(format!("Sub-agent task panicked: {}", e)),
                        duration_seconds: 0.0,
                        modified_files: vec![],
                    });
                }
            }
        }
    }

    results
}

/// Build the effective tool list for sub-agents: defaults + extras - blocked.
/// If the user configured `default_tools` or `blocked_tools` in DelegationConfig,
/// those override the built-in defaults.
pub fn build_effective_tools(
    extra_tools: &[String],
    config_default_tools: &[String],
    config_blocked_tools: &[String],
) -> Vec<String> {
    // Use user-configured defaults if provided, otherwise built-in
    let mut tools: Vec<String> = if config_default_tools.is_empty() {
        BUILTIN_SUBAGENT_TOOLS
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        config_default_tools.to_vec()
    };

    let blocked: Vec<&str> = BUILTIN_SUBAGENT_BLOCKED_TOOLS
        .iter()
        .copied()
        .chain(config_blocked_tools.iter().map(|s| s.as_str()))
        .collect();

    for tool in extra_tools {
        if !blocked.contains(&tool.as_str()) && !tools.contains(tool) {
            tools.push(tool.clone());
        }
    }

    tools
}

// ---------------------------------------------------------------------------
// Internal
// ---------------------------------------------------------------------------

/// A collected tool use from the stream.
#[derive(Debug, Clone)]
struct CollectedToolUse {
    id: String,
    name: String,
    input_json: String,
}

// ---------------------------------------------------------------------------
// Sub-agent conversation loop
// ---------------------------------------------------------------------------

async fn run_single_sub_agent(
    deps: &SubAgentDeps,
    cwd: PathBuf,
    task_index: usize,
    goal: &str,
    context: Option<&str>,
    allowed_tools: &[String],
    timeout_secs: f64,
    cancel: &CancellationToken,
    progress_tx: &tokio::sync::mpsc::UnboundedSender<SubAgentEvent>,
) -> SubAgentResult {
    let start = std::time::Instant::now();

    let _ = progress_tx.send(SubAgentEvent::Status {
        task_index,
        message: "Starting...".into(),
    });

    // Build sub-agent system prompt
    let system_prompt = build_subagent_system_prompt(goal, context, allowed_tools);

    // Create a fresh conversation history
    let mut history = ConversationHistory::new();
    history.push_user(goal);

    // Build tool context (no ask_user channel for sub-agents)
    // Pass sub_agent_deps through so skill_manage can read write_origin provenance.
    let ctx = ToolContext {
        cwd,
        ask_sender: None,
        mcp_manager: None,
        sub_agent_deps: Some(deps.clone()),
        cancel_token: None,
        rate_limiter: None,
        tool_stats: None,
    };

    // Create sub-agent's API client
    let (client, model) = match create_subagent_client(deps).await {
        Ok((c, m)) => (c, m),
        Err(e) => {
            return SubAgentResult {
                summary: String::new(),
                api_calls: 0,
                exit_reason: "error".into(),
                tokens: SubAgentTokenUsage::default(),
                tool_trace: vec![],
                interrupted: false,
                error: Some(format!("Failed to create sub-agent client: {}", e)),
                duration_seconds: start.elapsed().as_secs_f64(),
                modified_files: vec![],
            };
        }
    };

    let tool_schemas = deps.tool_registry.schemas_for(allowed_tools);

    let mut turn = 0u32;
    let mut auto_continue_count = 0u32;
    let mut total_input_tokens = 0u64;
    let mut total_output_tokens = 0u64;
    let mut tool_trace: Vec<ToolTraceEntry> = Vec::new();
    let mut modified_files: Vec<String> = Vec::new();
    let mut has_executed_tools = false; // Track whether any tools were executed

    loop {
        // Check cancellation
        if cancel.is_cancelled() {
            return SubAgentResult {
                summary: String::new(),
                api_calls: turn,
                exit_reason: "interrupted".into(),
                tokens: SubAgentTokenUsage {
                    input_tokens: total_input_tokens,
                    output_tokens: total_output_tokens,
                },
                tool_trace,
                interrupted: true,
                error: None,
                duration_seconds: start.elapsed().as_secs_f64(),
                modified_files: vec![],
            };
        }

        // Check timeout
        if start.elapsed().as_secs_f64() > timeout_secs {
            return SubAgentResult {
                summary: String::new(),
                api_calls: turn,
                exit_reason: "timeout".into(),
                tokens: SubAgentTokenUsage {
                    input_tokens: total_input_tokens,
                    output_tokens: total_output_tokens,
                },
                tool_trace,
                interrupted: false,
                error: Some(format!("Sub-agent timed out after {}s", timeout_secs)),
                duration_seconds: start.elapsed().as_secs_f64(),
                modified_files: vec![],
            };
        }

        turn += 1;
        if turn > deps.delegation_config.max_turns {
            break;
        }

        history.sanitize();
        let messages = history.to_api_messages();

        // Stream API response
        let stream = match client
            .stream_messages(&model, &system_prompt, &messages, &tool_schemas, None, None)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                let _ = progress_tx.send(SubAgentEvent::Status {
                    task_index,
                    message: format!("API error: {}", e),
                });
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                continue;
            }
        };

        tokio::pin!(stream);

        let mut assistant_text = String::new();
        let mut reasoning_text = String::new();
        let mut collected_tool_uses: Vec<CollectedToolUse> = Vec::new();
        let mut current_tool: Option<CollectedToolUse> = None;
        let mut stream_failed = false;
        let mut pending_usage: Option<Usage> = None;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!(event = "cancelled", phase = "sub_agent_stream", "Sub-agent stream cancelled");
                    return SubAgentResult {
                        summary: String::new(),
                        api_calls: turn,
                        exit_reason: "interrupted".into(),
                        tokens: SubAgentTokenUsage {
                            input_tokens: total_input_tokens,
                            output_tokens: total_output_tokens,
                        },
                        tool_trace,
                        interrupted: true,
                        error: None,
                        duration_seconds: start.elapsed().as_secs_f64(),
                        modified_files: vec![],
                    };
                }
                event = stream.next() => {
                    match event {
                        Some(Ok(StreamEvent::TextDelta(delta))) => {
                            assistant_text.push_str(&delta);
                        }
                        Some(Ok(StreamEvent::ReasoningDelta(delta))) => {
                            reasoning_text.push_str(&delta);
                        }
                        Some(Ok(StreamEvent::ToolUseStart {
                            id,
                            name,
                            input_json,
                        })) => {
                            if let Some(tool) = current_tool.take() {
                                collected_tool_uses.push(tool);
                            }
                            current_tool = Some(CollectedToolUse {
                                id,
                                name,
                                input_json: input_json.unwrap_or_default(),
                            });
                        }
                        Some(Ok(StreamEvent::ToolUseDelta { id, delta_json })) => {
                            if let Some(ref mut tool) = current_tool
                                && tool.id == id
                            {
                                tool.input_json.push_str(&delta_json);
                            }
                        }
                        Some(Ok(StreamEvent::UsageUpdate(usage))) => {
                            pending_usage = Some(usage);
                        }
                        Some(Ok(StreamEvent::MessageComplete {
                            stop_reason: _,
                            usage,
                        })) => {
                            let final_usage = if let Some(pending) = pending_usage.take() {
                                // Anthropic: message_start had input+output+cache, message_delta has
                                // only output_tokens.  Merge: take input+cache from pending, output
                                // from usage (message_delta has the final output count).
                                Usage {
                                    input_tokens: pending.input_tokens,
                                    output_tokens: usage.output_tokens,
                                    cache_read_input_tokens: pending.cache_read_input_tokens,
                                    cache_creation_input_tokens: pending.cache_creation_input_tokens,
                                    reasoning_tokens: pending.reasoning_tokens,
                                }
                            } else {
                                usage
                            };
                            total_input_tokens += final_usage.input_tokens;
                            total_output_tokens += final_usage.output_tokens;
                        }
                        Some(Err(e)) => {
                            tracing::warn!(error = ?e, "Sub-agent stream error");
                            stream_failed = true;
                            break;
                        }
                        None => break,
                    }
                }
            }
        }

        if let Some(tool) = current_tool.take() {
            collected_tool_uses.push(tool);
        }

        if stream_failed {
            let _ = progress_tx.send(SubAgentEvent::Status {
                task_index,
                message: "Stream error, retrying...".into(),
            });
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            continue;
        }

        // Empty response
        if assistant_text.trim().is_empty() && collected_tool_uses.is_empty() {
            if auto_continue_count < deps.delegation_config.max_auto_continue {
                auto_continue_count += 1;
                let _ = progress_tx.send(SubAgentEvent::Status {
                    task_index,
                    message: "Empty response, continuing...".into(),
                });
                history.push_user("Please continue with your task.");
                continue;
            }
            break;
        }

        // Build assistant blocks
        let mut assistant_blocks: Vec<ContentBlock> = Vec::new();
        if !assistant_text.trim().is_empty() {
            assistant_blocks.push(ContentBlock::Text {
                text: assistant_text.clone(),
            });
        }
        for tu in &collected_tool_uses {
            let input =
                serde_json::from_str(&tu.input_json).unwrap_or(Value::Object(Default::default()));
            assistant_blocks.push(ContentBlock::ToolUse {
                id: tu.id.clone(),
                name: tu.name.clone(),
                input,
            });
        }

        if assistant_blocks.is_empty() {
            break;
        }

        history.push_assistant_with_reasoning(
            assistant_blocks,
            if reasoning_text.is_empty() {
                None
            } else {
                Some(reasoning_text.clone())
            },
        );

        // No tool calls — text-only response
        if collected_tool_uses.is_empty() {
            // If this sub-agent has already executed tools, treat text as final summary.
            // Only auto-continue if no tools have been executed yet (premature text-only response).
            if !has_executed_tools && auto_continue_count < deps.delegation_config.max_auto_continue
            {
                auto_continue_count += 1;
                let _ = progress_tx.send(SubAgentEvent::Status {
                    task_index,
                    message: "Continuing...".into(),
                });
                history
                    .push_user("Please complete your task and provide a summary of what you did.");
                continue;
            }
            break;
        }

        // Track that tools were executed (for auto-continue gating above)
        has_executed_tools = true;

        // Notify progress about tool calls
        for tu in &collected_tool_uses {
            let summary = format_tool_input_summary(&tu.name, &tu.input_json);
            let _ = progress_tx.send(SubAgentEvent::ToolStarted {
                task_index,
                tool: tu.name.clone(),
                input_summary: summary,
            });
        }

        // Execute tools (sequential for simplicity)
        let mut tool_results: Vec<ContentBlock> = Vec::new();
        for tu in &collected_tool_uses {
            if cancel.is_cancelled() {
                return SubAgentResult {
                    summary: String::new(),
                    api_calls: turn,
                    exit_reason: "interrupted".into(),
                    tokens: SubAgentTokenUsage {
                        input_tokens: total_input_tokens,
                        output_tokens: total_output_tokens,
                    },
                    tool_trace,
                    interrupted: true,
                    error: None,
                    duration_seconds: start.elapsed().as_secs_f64(),
                    modified_files: vec![],
                };
            }

            // Check if tool is allowed
            if !allowed_tools.contains(&tu.name) {
                tool_results.push(ContentBlock::ToolResult {
                    tool_use_id: tu.id.clone(),
                    content: format!(
                        "Tool '{}' is not available to sub-agents. Allowed tools: {}",
                        tu.name,
                        allowed_tools.join(", ")
                    ),
                    is_error: Some(true),
                });
                tool_trace.push(ToolTraceEntry {
                    tool: tu.name.clone(),
                    args_bytes: tu.input_json.len(),
                    result_bytes: 0,
                    is_error: true,
                });
                let _ = progress_tx.send(SubAgentEvent::ToolCompleted {
                    task_index,
                    tool: tu.name.clone(),
                    result_bytes: 0,
                    is_error: true,
                });
                continue;
            }

            // Emit thinking event
            if !assistant_text.trim().is_empty() {
                let _ = progress_tx.send(SubAgentEvent::Thinking {
                    task_index,
                    text: assistant_text.clone(),
                });
                assistant_text.clear();
            }

            let input = serde_json::from_str::<Value>(&tu.input_json)
                .unwrap_or(Value::Object(Default::default()));
            let result = match deps.tool_registry.execute(&tu.name, input, &ctx).await {
                Ok(output) => ContentBlock::ToolResult {
                    tool_use_id: tu.id.clone(),
                    content: output,
                    is_error: None,
                },
                Err(e) => ContentBlock::ToolResult {
                    tool_use_id: tu.id.clone(),
                    content: format!("Tool execution error: {}", e),
                    is_error: Some(true),
                },
            };

            let result_bytes = match &result {
                ContentBlock::ToolResult { content, .. } => content.len(),
                _ => 0,
            };
            let is_error = match &result {
                ContentBlock::ToolResult { is_error, .. } => is_error.unwrap_or(false),
                _ => false,
            };

            tool_trace.push(ToolTraceEntry {
                tool: tu.name.clone(),
                args_bytes: tu.input_json.len(),
                result_bytes,
                is_error,
            });

            // Track modified files for parent re-read consistency
            if !is_error && (tu.name == "write" || tu.name == "edit") {
                if let Ok(val) = serde_json::from_str::<Value>(&tu.input_json) {
                    if let Some(path) = val.get("path").and_then(|p| p.as_str()) {
                        modified_files.push(path.to_string());
                    }
                }
            }

            let _ = progress_tx.send(SubAgentEvent::ToolCompleted {
                task_index,
                tool: tu.name.clone(),
                result_bytes,
                is_error,
            });

            tool_results.push(result);
        }

        history.push_tool_results(tool_results);
    }

    // Extract summary from the final assistant text
    let summary = extract_subagent_summary(&history);
    let duration = start.elapsed().as_secs_f64();

    let _ = progress_tx.send(SubAgentEvent::Status {
        task_index,
        message: format!("Completed in {:.1}s, {} API calls", duration, turn),
    });

    // Record sub-agent token usage into shared cost tracker
    if total_input_tokens > 0 || total_output_tokens > 0 {
        if let Ok(mut ct) = deps.cost_tracker.lock() {
            ct.absorb_subagent(
                &deps.settings.model,
                total_input_tokens,
                total_output_tokens,
            );
        }
    }

    // Emit file-consistency warning if sub-agent wrote files
    let file_warning = if !modified_files.is_empty() {
        let paths = modified_files.join(", ");
        format!(
            "\n\n[NOTE: sub-agent modified files: {}. Re-read them before editing.]",
            paths
        )
    } else {
        String::new()
    };

    SubAgentResult {
        summary: summary + &file_warning,
        api_calls: turn,
        exit_reason: if turn >= deps.delegation_config.max_turns {
            "max_turns"
        } else {
            "completed"
        }
        .into(),
        tokens: SubAgentTokenUsage {
            input_tokens: total_input_tokens,
            output_tokens: total_output_tokens,
        },
        tool_trace,
        interrupted: false,
        error: None,
        duration_seconds: duration,
        modified_files,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a focused system prompt for a sub-agent.
fn build_subagent_system_prompt(
    goal: &str,
    context: Option<&str>,
    allowed_tools: &[String],
) -> String {
    let mut parts = vec![
        "You are a focused sub-agent working on a specific delegated task.".into(),
        String::new(),
        format!("YOUR TASK:\n{}", goal),
    ];

    if let Some(ctx) = context {
        if !ctx.trim().is_empty() {
            parts.push(format!("\nCONTEXT:\n{}", ctx));
        }
    }

    // List available tools so the LLM knows what it can use
    if !allowed_tools.is_empty() {
        parts.push(format!(
            "\n## Available Tools\n\n{}",
            allowed_tools.join(", ")
        ));
    }

    parts.push(
        "\nComplete this task using the tools available to you. \
         When finished, provide a clear, concise summary of:\n\
         - What you did\n\
         - What you found or accomplished\n\
         - Any files you created or modified\n\
         - Any issues encountered\n\n\
         Be thorough but concise — your response is returned to the \
         parent agent as a summary."
            .into(),
    );

    parts.join("\n")
}

/// Format a short summary of tool input for progress display.
fn format_tool_input_summary(name: &str, input_json: &str) -> String {
    match name {
        "read" | "write" | "edit" => {
            if let Ok(val) = serde_json::from_str::<Value>(input_json) {
                val.get("path")
                    .and_then(|p| p.as_str())
                    .map(|p| p.to_string())
                    .unwrap_or_default()
            } else {
                String::new()
            }
        }
        "bash" => {
            if let Ok(val) = serde_json::from_str::<Value>(input_json) {
                val.get("command")
                    .and_then(|c| c.as_str())
                    .map(|c| c.to_string())
                    .unwrap_or_default()
            } else {
                String::new()
            }
        }
        "web_search" => {
            if let Ok(val) = serde_json::from_str::<Value>(input_json) {
                val.get("query")
                    .and_then(|q| q.as_str())
                    .map(|q| q.to_string())
                    .unwrap_or_default()
            } else {
                String::new()
            }
        }
        "web_fetch" => {
            if let Ok(val) = serde_json::from_str::<Value>(input_json) {
                val.get("url")
                    .and_then(|u| u.as_str())
                    .map(|u| u.to_string())
                    .unwrap_or_default()
            } else {
                String::new()
            }
        }
        "glob" => {
            if let Ok(val) = serde_json::from_str::<Value>(input_json) {
                val.get("pattern")
                    .and_then(|p| p.as_str())
                    .map(|p| p.to_string())
                    .unwrap_or_default()
            } else {
                String::new()
            }
        }
        "grep" => {
            if let Ok(val) = serde_json::from_str::<Value>(input_json) {
                val.get("pattern")
                    .and_then(|p| p.as_str())
                    .map(|p| {
                        if p.len() > 60 {
                            format!("{}...", &p[..57])
                        } else {
                            p.to_string()
                        }
                    })
                    .unwrap_or_default()
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

/// Extract a summary from the sub-agent's conversation history.
fn extract_subagent_summary(history: &ConversationHistory) -> String {
    for entry in history.entries_raw().iter().rev() {
        if entry.role == Role::Assistant {
            for block in &entry.content {
                if let ContentBlock::Text { text } = block {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        if trimmed.len() > 2000 {
                            let end = trimmed
                                .char_indices()
                                .nth(2000)
                                .map_or(trimmed.len(), |(i, _)| i);
                            return format!("{}...", &trimmed[..end]);
                        }
                        return trimmed.to_string();
                    }
                }
            }
        }
    }
    String::new()
}

/// Create an API client for a sub-agent.
///
/// Follows the same resolution pattern as other auxiliary tasks (compression,
/// vision, etc.) — see `auxiliary::router::resolve_explicit` + `build_resolved`:
///
/// 1. `provider` field selects which provider to use:
///    - "auto" → use parent's active provider (with fallback chain)
///    - explicit name → use that provider directly
/// 2. `url` overrides the resolved provider's base_url (when set)
/// 3. `api_key` overrides the resolved provider's api_key (when set)
/// 4. `model` overrides the resolved provider's default_model (when set)
async fn create_subagent_client(
    deps: &SubAgentDeps,
) -> Result<(Box<dyn SupportsStreamingMessages>, String), SubAgentError> {
    let delegation_cfg = &deps.settings.auxiliary.delegation;
    let is_auto = delegation_cfg.provider == "auto" || delegation_cfg.provider.is_empty();

    // Step 1: resolve the base provider
    let (provider_name, provider_config) = if is_auto {
        // Inherit parent's active provider
        let name = &deps.settings.active_provider;
        let config = deps
            .settings
            .providers
            .get(name)
            .ok_or(SubAgentError::NoProvider)?;
        (name.clone(), config.clone())
    } else {
        // Use explicitly configured provider
        let config = deps
            .settings
            .providers
            .get(&delegation_cfg.provider)
            .ok_or_else(|| {
                SubAgentError::ClientCreation(format!(
                    "Provider '{}' not found in config for sub-agent delegation",
                    delegation_cfg.provider
                ))
            })?;
        (delegation_cfg.provider.clone(), config.clone())
    };

    // Step 2: apply task-level overrides (independent of provider selection)
    let base_url = delegation_cfg
        .url
        .clone()
        .unwrap_or(provider_config.base_url);

    let api_key = if let Some(ref key) = delegation_cfg.api_key {
        crate::config::settings::resolve_api_key_opt(Some(key.as_str()))
            .unwrap_or_else(|| key.clone())
    } else if let Some(ref key) = provider_config.api_key {
        crate::config::settings::resolve_api_key_opt(Some(key.as_str()))
            .unwrap_or_else(|| key.clone())
    } else {
        return Err(SubAgentError::ClientCreation(format!(
            "No API key for provider '{}'",
            provider_name
        )));
    };

    let model = if !delegation_cfg.model.is_empty() {
        delegation_cfg.model.clone()
    } else if !deps.settings.model.is_empty() {
        deps.settings.model.clone()
    } else {
        provider_config.default_model.clone()
    };

    // Step 3: build client
    let effective_config = crate::config::settings::ProviderConfig {
        api_key: Some(api_key),
        base_url,
        default_model: model.clone(),
        max_output_tokens: provider_config.max_output_tokens,
        api_type: provider_config.api_type,
    };

    let client = (deps.client_factory)(&provider_name, &effective_config);
    Ok((client, model))
}
