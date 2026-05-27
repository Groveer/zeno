//! Delegate Task tool — spawn sub-agents with isolated context.
//!
//! Allows the LLM to decompose complex tasks into independent subtasks
//! that run via sub-agents. Each sub-agent gets a fresh conversation,
//! its own API client, and full tool access (only `delegate_task` is blocked).
//!
//! Reference: hermes-agent `tools/delegate_tool.py`

use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::base::{Tool, ToolContext, ToolError};
use crate::engine::sub_agent::{run_delegated_task, run_delegated_tasks_batch};
use crate::store::agent_graph::EdgeStatus;
use zeno_tools::{JsonToolOutput, ToolOutput};

/// Maximum goal length stored in the graph store (byte length, approximate).
///
/// Longer goals are truncated to prevent JSON file bloat. This is a safety net
/// — the display-side truncation in GraphPanel (char-based, ~50 chars) is tighter,
/// so this storage limit only affects what lands in the persisted JSON.
const STORAGE_MAX_GOAL_LEN: usize = 200;

/// Drop guard that closes a spawn edge on panic or early return.
///
/// Normal path: call `.close().await` to synchronously close the edge.
/// The `close()` method takes `graph_store`, so Drop becomes a no-op
/// (no memory leak, no redundant write).
///
/// Panic path: if dropped without `.close()` being called, the Drop impl
/// does a best-effort close via `tokio::spawn`.
struct CloseEdgeGuard {
    graph_store: Option<Arc<dyn crate::store::agent_graph::SubAgentGraphStore>>,
    child_id: String,
}

impl CloseEdgeGuard {
    /// Explicitly close the edge synchronously. After this, Drop is a no-op
    /// because `graph_store` has been taken.
    async fn close(mut self) {
        if let Some(store) = self.graph_store.take() {
            let _ = store
                .set_edge_status(&self.child_id, EdgeStatus::Closed)
                .await
                .map_err(|e| {
                    tracing::warn!(error = %e, child_id = %self.child_id, "Failed to close spawn edge");
                });
        }
    }
}

impl Drop for CloseEdgeGuard {
    fn drop(&mut self) {
        if let Some(store) = self.graph_store.take() {
            let child_id = self.child_id.clone();
            tokio::spawn(async move {
                let _ = store.set_edge_status(&child_id, EdgeStatus::Closed).await;
            });
        }
    }
}

/// Drop guard that closes multiple spawn edges on panic or early return.
///
/// Uses `Arc<Mutex<Vec<String>>>` so the guard can be installed *before*
/// the recording loop, eliminating the race window where task cancellation
/// between iterations would orphan Open edges.
///
/// Normal path: call `.close().await` for synchronous close + leak-free Drop.
/// Panic path: Drop impl does best-effort close via `tokio::spawn`.
struct CloseEdgesGuard {
    graph_store: Option<Arc<dyn crate::store::agent_graph::SubAgentGraphStore>>,
    child_ids: Arc<Mutex<Vec<String>>>,
}

impl CloseEdgesGuard {
    /// Explicitly close all edges synchronously. After this, Drop is a no-op
    /// because `graph_store` has been taken.
    async fn close(mut self) {
        if let Some(store) = self.graph_store.take() {
            let child_ids = self.child_ids.lock().unwrap().clone();
            for child_id in &child_ids {
                let _ = store
                    .set_edge_status(child_id, EdgeStatus::Closed)
                    .await
                    .map_err(|e| {
                        tracing::warn!(
                            error = %e,
                            child_id = %child_id,
                            "Failed to close spawn edge"
                        );
                    });
            }
        }
    }
}

impl Drop for CloseEdgesGuard {
    fn drop(&mut self) {
        if let Some(store) = self.graph_store.take() {
            let child_ids = self.child_ids.lock().unwrap().clone();
            tokio::spawn(async move {
                for child_id in child_ids {
                    let _ = store.set_edge_status(&child_id, EdgeStatus::Closed).await;
                }
            });
        }
    }
}

/// Helper: record a sub-agent spawn edge in the graph store (if available).
/// Returns the generated child_id.
///
/// Synchronous (no `tokio::spawn`) — the RwLock write is fast, and awaiting
/// it here guarantees the edge is persisted before any `close_spawn_edge`
/// call, eliminating the open→never-closed race.
async fn record_spawn_edge(
    graph_store: &Option<Arc<dyn crate::store::agent_graph::SubAgentGraphStore>>,
    parent_id: &str,
    task_index: usize,
    goal: &str,
) -> String {
    let child_id = Uuid::new_v4().to_string();
    if let Some(store) = graph_store {
        let goal = if goal.len() > STORAGE_MAX_GOAL_LEN {
            // char_indices ensures we never slice in the middle of a multi-byte char
            let safe_end = goal
                .char_indices()
                .take(STORAGE_MAX_GOAL_LEN - 1)
                .last()
                .map(|(i, c)| i + c.len_utf8())
                .unwrap_or(0);
            format!("{}…", &goal[..safe_end])
        } else {
            goal.to_string()
        };
        let _ = store
            .upsert_edge(parent_id, &child_id, EdgeStatus::Open, task_index, &goal)
            .await
            .map_err(|e| {
                tracing::warn!(
                    error = %e,
                    parent_id = %parent_id,
                    child_id = %child_id,
                    "Failed to record spawn edge"
                );
            });
    }
    child_id
}

pub struct DelegateTaskTool;

impl DelegateTaskTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for DelegateTaskTool {
    fn name(&self) -> &str {
        "delegate_task"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "delegate_task",
                "description": "Spawn a sub-agent to handle a delegated task independently. \
                    The sub-agent gets a fresh context and full tool access. \
                    Use this ONLY when you have MULTIPLE independent subtasks that can run \
                    in parallel (batch mode with 'tasks' array).\n\n\
                    DO NOT use for single tool calls — call tools like web_search, web_fetch, \
                    read, grep, bash directly instead. Delegating a single tool call wastes \
                    tokens and loses the original result.\n\n\
                    Two modes:\n\
                    - Single task: provide 'goal' (+ optional 'context')\n\
                    - Batch: provide 'tasks' array for parallel execution\n\n\
                    Each result includes 'summary' (the sub-agent's final report), 'api_calls', \
                    'exit_reason', and 'tool_trace'.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "goal": {
                            "type": "string",
                            "description": "The task goal for the sub-agent. Required if 'tasks' is not provided."
                        },
                        "context": {
                            "type": "string",
                            "description": "Optional context to pass to the sub-agent (e.g. file contents, relevant findings)."
                        },
                        "tasks": {
                            "type": "array",
                            "description": "Array of task objects for parallel execution. Each task has 'goal' and optional 'context'.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "goal": {
                                        "type": "string",
                                        "description": "The task goal for this sub-agent."
                                    },
                                    "context": {
                                        "type": "string",
                                        "description": "Optional context for this sub-agent."
                                    }
                                },
                                "required": ["goal"]
                            }
                        }
                    },
                    "anyOf": [
                        {"required": ["goal"]},
                        {"required": ["tasks"]}
                    ]
                }
            }
        })
    }

    async fn execute(
        &self,
        arguments: Value,
        ctx: &ToolContext,
    ) -> Result<Box<dyn ToolOutput>, ToolError> {
        let deps = ctx.sub_agent_deps.clone().ok_or_else(|| {
            ToolError::Execution(
                "Sub-agent dependencies not available. The engine must be configured.".to_string(),
            )
        })?;

        // Link to parent's cancellation token so Ctrl+C propagates to sub-agents
        let parent_cancel = ctx.cancel_token.clone();

        // Batch mode: tasks array
        if let Some(tasks) = arguments.get("tasks").and_then(|t| t.as_array()) {
            if tasks.is_empty() {
                return Ok(Box::new(JsonToolOutput::success(
                    json!({"error": "tasks array must not be empty"}).to_string(),
                )));
            }

            let task_pairs: Vec<(String, Option<String>)> = tasks
                .iter()
                .map(|t| {
                    let goal = t
                        .get("goal")
                        .and_then(|g| g.as_str())
                        .unwrap_or("")
                        .to_string();
                    let context = t.get("context").and_then(|c| c.as_str()).map(String::from);
                    (goal, context)
                })
                .collect();

            // Validate all goals are non-empty
            for (i, (goal, _)) in task_pairs.iter().enumerate() {
                if goal.trim().is_empty() {
                    return Ok(Box::new(JsonToolOutput::success(
                        json!({"error": format!("Task {} has empty goal", i)}).to_string(),
                    )));
                }
            }

            // Install panic guard BEFORE recording edges so task cancellation
            // between iterations cannot orphan Open edges.
            let recorded_ids: Arc<Mutex<Vec<String>>> =
                Arc::new(Mutex::new(Vec::with_capacity(task_pairs.len())));
            let guard = CloseEdgesGuard {
                graph_store: deps.graph_store.clone(),
                child_ids: Arc::clone(&recorded_ids),
            };

            // Record spawn edges in graph store.
            for (i, (goal, _)) in task_pairs.iter().enumerate() {
                let id = record_spawn_edge(&deps.graph_store, &ctx.task_id, i, goal).await;
                recorded_ids.lock().unwrap().push(id);
            }
            // Defensive: ensure every task has a corresponding child_id before
            // passing to run_delegated_tasks_batch, which indexes by position.
            debug_assert_eq!(
                recorded_ids.lock().unwrap().len(),
                task_pairs.len(),
                "child_ids length must match task_pairs length"
            );
            // Snapshot child_ids for run_delegated_tasks_batch (guard holds its
            // own Arc<Mutex<...>>, so the snapshot is independent).
            let child_ids = recorded_ids.lock().unwrap().clone();

            // Save references for on_delegation notification after the move
            let mm_ref = deps.memory_manager.clone();
            let task_goals: Vec<String> = task_pairs.iter().map(|(g, _)| g.clone()).collect();
            let child_ids_for_notify = child_ids.clone();

            let progress_tx = deps.progress_tx.clone();
            let max_concurrent = deps.delegation_config.max_concurrent_children.max(1) as usize;
            let results = run_delegated_tasks_batch(
                deps,
                ctx.get_cwd(),
                child_ids,
                task_pairs,
                max_concurrent,
                parent_cancel.unwrap_or_default(),
                progress_tx,
            )
            .await;

            // Mark all edges as closed. guard.close() takes graph_store,
            // making the eventual Drop a no-op (no leak, no redundant write).
            guard.close().await;

            // Notify memory provider of batch delegation outcomes
            if let Some(mm) = mm_ref {
                let mm = mm.lock().await;
                for (i, result) in results.iter().enumerate() {
                    let goal = task_goals.get(i).map(|s| s.as_str()).unwrap_or("");
                    let cid = child_ids_for_notify
                        .get(i)
                        .map(|s| s.as_str())
                        .unwrap_or("");
                    mm.on_delegation(goal, &result.summary, cid);
                }
            }

            return Ok(Box::new(JsonToolOutput::success(
                serde_json::to_string(&results)
                    .map_err(|e| ToolError::Execution(format!("Serialization error: {}", e)))?,
            )));
        }

        // Single task mode
        let goal = arguments
            .get("goal")
            .and_then(|g| g.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("Missing required field 'goal'".into()))?;

        if goal.trim().is_empty() {
            return Ok(Box::new(JsonToolOutput::success(
                json!({"error": "goal must not be empty"}).to_string(),
            )));
        }

        let context = arguments
            .get("context")
            .and_then(|c| c.as_str())
            .map(String::from);

        // For single task, create a child token linked to parent
        let child_cancel = if let Some(pc) = ctx.cancel_token.clone() {
            let child = CancellationToken::new();
            let child_clone = child.clone();
            tokio::spawn(async move {
                pc.cancelled().await;
                child_clone.cancel();
            });
            child
        } else {
            CancellationToken::new()
        };

        // Record spawn edge in graph store
        let child_id = record_spawn_edge(&deps.graph_store, &ctx.task_id, 0, goal).await;
        let guard = CloseEdgeGuard {
            graph_store: deps.graph_store.clone(),
            child_id: child_id.clone(),
        };

        let result = run_delegated_task(
            &deps,
            ctx.get_cwd(),
            &child_id,
            goal.to_string(),
            context,
            child_cancel,
            deps.progress_tx.clone(),
        )
        .await;

        // Mark edge as closed. guard.close() takes graph_store, making the
        // eventual Drop a no-op (no leak, no redundant write).
        // If we panicked before this line, Drop handles it best-effort.
        guard.close().await;

        // Notify memory provider of delegation outcome
        if let Some(ref mm) = deps.memory_manager {
            mm.lock()
                .await
                .on_delegation(goal, &result.summary, &child_id);
        }

        Ok(Box::new(JsonToolOutput::success(
            serde_json::to_string(&result)
                .map_err(|e| ToolError::Execution(format!("Serialization error: {}", e)))?,
        )))
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        false
    }
}
