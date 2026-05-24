//! Tool execution logic — parallel safety checks, execution wrappers, and permission routing.
//!
//! This module handles:
//! - Parallel safety analysis for tool batches
//! - Single tool execution with hooks, permissions, and caching
//! - ToolExecConfig for session-scoped execution parameters

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::api::types::ContentBlock;
use crate::engine::tui_events::EngineEvent;
use crate::hooks::executor::HookExecutor;
use crate::hooks::types::HookEvent;
use crate::permissions::checker;
use crate::permissions::execpolicy::ExecPolicy;
use crate::tools::base::{ToolContext, ToolRegistry};

use super::json_utils::{parse_tool_input, parse_tool_input_or_empty};

// ---------------------------------------------------------------------------
// Collected tool use from the stream
// ---------------------------------------------------------------------------

/// A collected tool use from the stream.
#[derive(Debug, Clone)]
pub(super) struct CollectedToolUse {
    pub id: String,
    pub name: String,
    pub input_json: String,
}

// ---------------------------------------------------------------------------
// Parallel tool-call safety check
// ---------------------------------------------------------------------------

/// Tools that must never run concurrently (interactive / user-facing).
const NEVER_PARALLEL_TOOLS: &[&str] = &["ask_user"];

/// Tools that target a file and need path-scoped conflict detection.
const PATH_SCOPED_TOOLS: &[&str] = &["read", "edit", "write"];

/// Extract the normalised absolute file path from a tool's input.
fn extract_scope_path(tool_name: &str, input: &Value, cwd: &Path) -> Option<PathBuf> {
    if !PATH_SCOPED_TOOLS.contains(&tool_name) {
        return None;
    }
    let raw = input.get("path").and_then(|v| v.as_str())?;
    if raw.is_empty() {
        return None;
    }
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
fn paths_overlap(left: &Path, right: &Path) -> bool {
    let l = left.components().collect::<Vec<_>>();
    let r = right.components().collect::<Vec<_>>();
    let common = l.len().min(r.len());
    l[..common] == r[..common]
}

/// Decide whether a batch of tool calls is safe to execute concurrently.
///
/// Logic:
/// 1. ≤ 1 tool → parallel (trivially safe)
/// 2. Any `NEVER_PARALLEL_TOOLS` → sequential
/// 3. Non-JSON / non-object args → sequential (can't inspect)
/// 4. Path-scoped tools targeting overlapping paths → sequential
/// 5. Otherwise → parallel
pub(super) fn should_parallelize(tool_uses: &[CollectedToolUse], cwd: &Path) -> bool {
    if tool_uses.len() <= 1 {
        return true;
    }

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

// ---------------------------------------------------------------------------
// Tool execution configuration
// ---------------------------------------------------------------------------

/// Session-level configuration for tool execution.
/// Groups fields from `QueryEngine` that are constant across a single `query_tui` call,
/// reducing parameter lists from 15 to 6.
pub(super) struct ToolExecConfig<'a> {
    pub permission_mode: &'a crate::config::settings::PermissionMode,
    pub trusted_paths: &'a [String],
    pub permission_allow_all: &'a Arc<AtomicBool>,
    pub tools: &'a ToolRegistry,
    pub hook_executor: Option<&'a HookExecutor>,
    pub tool_cache: Option<&'a std::sync::Mutex<crate::tools::cache::ToolCache>>,
    pub denied_commands: &'a [String],
    pub safe_paths: &'a [String],
    pub exec_policy: Option<&'a ExecPolicy>,
}

// ---------------------------------------------------------------------------
// Tool execution functions
// ---------------------------------------------------------------------------

/// Error-tolerant wrapper for TUI tool execution.
/// Always returns a ContentBlock, converting None to an error ToolResult.
pub(super) async fn execute_single_tool_tui_catch(
    config: &ToolExecConfig<'_>,
    tu: &CollectedToolUse,
    ctx: &ToolContext,
    sender: &tokio::sync::mpsc::UnboundedSender<EngineEvent>,
    cancel: &CancellationToken,
    last_failed_tool_input: &Arc<Mutex<Option<String>>>,
) -> ContentBlock {
    match execute_single_tool_tui(config, tu, ctx, sender, cancel, last_failed_tool_input).await {
        Some(block) => block,
        None => {
            let _ = sender.send(EngineEvent::ToolError {
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

/// Execute a single tool with hooks, permissions, and caching.
pub(super) async fn execute_single_tool_tui(
    config: &ToolExecConfig<'_>,
    tu: &CollectedToolUse,
    ctx: &ToolContext,
    sender: &tokio::sync::mpsc::UnboundedSender<EngineEvent>,
    cancel: &CancellationToken,
    last_failed_tool_input: &Arc<Mutex<Option<String>>>,
) -> Option<ContentBlock> {
    let input = match parse_tool_input(&tu.input_json) {
        Ok(v) => v,
        Err(e) => {
            if let Ok(mut guard) = last_failed_tool_input.lock() {
                *guard = Some(tu.input_json.clone());
            }
            let _ = sender.send(EngineEvent::ToolError {
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
    if let Some(he) = config.hook_executor
        && he.has_hooks_for(HookEvent::PreToolUse)
        && let Ok(hook_ctx) = he.build_context()
    {
        let _ = hook_ctx.set("tool_name", tu.name.as_str());
        let _ = hook_ctx.set(
            "tool_input",
            crate::hooks::executor::json_to_lua_value(&he.lua(), &input),
        );
        let _ = hook_ctx.set("cwd", ctx.get_cwd().to_string_lossy().to_string());
        if let Some(block_reason) = he
            .execute_first_block(HookEvent::PreToolUse, &hook_ctx)
            .await
        {
            let _ = sender.send(EngineEvent::ToolError {
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

    // --- Input validation ---
    let is_read_only = config
        .tools
        .get(&tu.name)
        .map(|t| t.as_ref().is_read_only(&input))
        .unwrap_or(false);

    if let Some(t) = config.tools.get(&tu.name) {
        let validation: Result<(), crate::tools::base::ToolError> =
            t.as_ref().validate_input(&input);
        if let Err(e) = validation {
            let _ = sender.send(EngineEvent::ToolError {
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

    // --- Permission check ---
    let already_allowed = config
        .permission_allow_all
        .load(std::sync::atomic::Ordering::Relaxed);

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
            config.permission_mode,
            config.trusted_paths,
            &tu.name,
            is_read_only,
            resolved.file_path.as_deref(),
            resolved.command.as_deref(),
            &cwd,
            config.safe_paths,
            config.denied_commands,
            config.exec_policy,
        );

        if decision.allowed {
            tracing::info!(
                tool_name = %tu.name,
                permission_decision = "allowed",
                mode = %format!("{:?}", config.permission_mode),
                is_read_only = is_read_only,
                tool_id = %tu.id,
                "Tool execution permitted"
            );
            true
        } else if decision.requires_confirmation {
            let display_detail = format_permission_detail(&tu.name, &tu.input_json);
            let (tx, rx) = tokio::sync::oneshot::channel();
            let response_tx = Arc::new(Mutex::new(Some(tx)));
            let _ = sender.send(EngineEvent::PermissionAsk {
                tool_name: tu.name.clone(),
                reason: decision.reason.clone(),
                input: display_detail,
                response_tx,
            });
            match tokio::select! {
                result = rx => result,
                _ = cancel.cancelled() => {
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
                    if allowed && matches!(r.as_str(), "a" | "all" | "always") {
                        config
                            .permission_allow_all
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                        let _ = sender.send(EngineEvent::Status(
                            "All permissions granted for this session.".into(),
                        ));
                    }
                    allowed
                }
                Err(_) => false,
            }
        } else {
            let denied_reason = decision.reason.clone();
            tracing::warn!(
                tool_name = %tu.name,
                permission_decision = "denied",
                mode = %format!("{:?}", config.permission_mode),
                reason = %denied_reason,
                tool_id = %tu.id,
                "Tool execution denied by policy"
            );
            let _ = sender.send(EngineEvent::ToolError {
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

    if !permitted {
        tracing::warn!(
            tool_name = %tu.name,
            permission_decision = "denied_by_user",
            tool_id = %tu.id,
            "Tool execution denied by user"
        );
        let _ = sender.send(EngineEvent::ToolError {
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
    if cacheable_tools.contains(&tu.name.as_str())
        && let Some(cache) = config.tool_cache
        && let Ok(mut cache) = cache.lock()
    {
        let normalized = crate::tools::cache::normalize_for_cache(&tu.name, &input);
        if let Some(cached) = cache.get(&tu.name, &normalized) {
            tracing::debug!(
                tool_name = %tu.name,
                "Tool result cache hit"
            );
            let _ = sender.send(EngineEvent::ToolOutput {
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

    // --- Execute ---
    let result = config.tools.execute(&tu.name, input.clone(), ctx).await;

    match result {
        Ok(result) => {
            // Cache result for read-only tools
            if cacheable_tools.contains(&tu.name.as_str())
                && let Some(cache) = config.tool_cache
                && let Ok(mut cache) = cache.lock()
            {
                let normalized = crate::tools::cache::normalize_for_cache(&tu.name, &input);
                cache.insert(&tu.name, &normalized, result.clone());
            }

            // Invalidate cache on write/edit
            if (tu.name == "write" || tu.name == "edit")
                && let Some(path) = input.get("path").and_then(|v| v.as_str())
            {
                if let Some(cache) = config.tool_cache
                    && let Ok(mut cache) = cache.lock()
                {
                    cache.invalidate_path(std::path::Path::new(path));
                }
                if let Some(ref pool_arc) = ctx.file_content_pool {
                    let resolved = ctx.resolve_path(path);
                    let resolved_str = resolved.to_string_lossy().to_string();
                    let mut pool = pool_arc.lock().await;
                    pool.remove(&resolved_str);
                }
            }

            let display =
                super::tool_display::summarize_tool_output(&tu.name, &result, &tu.input_json);
            let _ = sender.send(EngineEvent::ToolOutput {
                name: tu.name.clone(),
                output: display,
            });

            // For edit tool, extract diff from structured JSON result
            if tu.name == "edit"
                && let Ok(parsed) = serde_json::from_str::<Value>(&result)
                && let Some(diff_str) = parsed.get("diff").and_then(|d| d.as_str())
                && !diff_str.is_empty()
            {
                let _ = sender.send(EngineEvent::ToolDiff {
                    name: tu.name.clone(),
                    diff: diff_str.to_string(),
                });
            }
            tracing::info!(
                tool_name = %tu.name,
                tool_id = %tu.id,
                tool_result = "success",
                result_len = result.len(),
                "Tool executed successfully"
            );

            // --- Post-tool-use hook ---
            if let Some(he) = config.hook_executor
                && he.has_hooks_for(HookEvent::PostToolUse)
                && let Ok(hook_ctx) = he.build_context()
            {
                let _ = hook_ctx.set("tool_name", tu.name.as_str());
                let _ = hook_ctx.set(
                    "tool_input",
                    crate::hooks::executor::json_to_lua_value(&he.lua(), &input),
                );
                let _ = hook_ctx.set("tool_output", result.clone());
                let _ = hook_ctx.set("tool_is_error", false);
                let _ = hook_ctx.set("cwd", ctx.get_cwd().to_string_lossy().to_string());
                he.execute(HookEvent::PostToolUse, &hook_ctx).await;
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
            let _ = sender.send(EngineEvent::ToolError {
                name: tu.name.clone(),
                error: e.to_string(),
            });

            // --- Post-tool-use hook (error case) ---
            if let Some(he) = config.hook_executor
                && he.has_hooks_for(HookEvent::PostToolUse)
                && let Ok(hook_ctx) = he.build_context()
            {
                let _ = hook_ctx.set("tool_name", tu.name.as_str());
                let _ = hook_ctx.set(
                    "tool_input",
                    crate::hooks::executor::json_to_lua_value(&he.lua(), &input),
                );
                let _ = hook_ctx.set("tool_output", e.to_string());
                let _ = hook_ctx.set("tool_is_error", true);
                let _ = hook_ctx.set("cwd", ctx.get_cwd().to_string_lossy().to_string());
                he.execute(HookEvent::PostToolUse, &hook_ctx).await;
            }

            Some(ContentBlock::ToolResult {
                tool_use_id: tu.id.clone(),
                content: format!("Error: {}", e),
                is_error: Some(true),
            })
        }
    }
}

/// Build a human-readable detail line for permission prompts.
pub(crate) fn format_permission_detail(tool_name: &str, input_json: &str) -> String {
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
