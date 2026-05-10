//! Delegate Task tool — spawn sub-agents with isolated context.
//!
//! Allows the LLM to decompose complex tasks into independent subtasks
//! that run via sub-agents. Each sub-agent gets a fresh conversation,
//! its own API client, and a restricted toolset.
//!
//! Reference: hermes-agent `tools/delegate_tool.py`

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::base::{Tool, ToolContext, ToolError};
use crate::engine::sub_agent::{run_delegated_task, run_delegated_tasks_batch};

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
                    The sub-agent gets a fresh context and a restricted set of tools. \
                    Use this ONLY when you have MULTIPLE independent subtasks that can run \
                    in parallel (batch mode with 'tasks' array).\n\n\
                    DO NOT use for single tool calls — call tools like web_search, web_fetch, \
                    read, grep, bash directly instead. Delegating a single tool call wastes \
                    tokens and loses the original result.\n\n\
                    Two modes:\n\
                    - Single task: provide 'goal' (+ optional 'context', 'tools')\n\
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
                        },
                        "tools": {
                            "type": "array",
                            "description": "Additional tools beyond the defaults (read, glob, grep, web_search, web_fetch) that the sub-agent may use. \
                                Allowed: bash, write, edit. Blocked: delegate_task, ask_user, memory, skill_manage.",
                            "items": {
                                "type": "string",
                                "enum": ["bash", "write", "edit"]
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

    async fn execute(&self, arguments: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let deps = ctx.sub_agent_deps.clone().ok_or_else(|| {
            ToolError::Execution(
                "Sub-agent dependencies not available. The engine must be configured.".to_string(),
            )
        })?;

        let extra_tools: Vec<String> = arguments
            .get("tools")
            .and_then(|t| t.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        // Link to parent's cancellation token so Ctrl+C propagates to sub-agents
        let parent_cancel = ctx.cancel_token.clone();

        // Batch mode: tasks array
        if let Some(tasks) = arguments.get("tasks").and_then(|t| t.as_array()) {
            if tasks.is_empty() {
                return Ok(json!({"error": "tasks array must not be empty"}).to_string());
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
                    return Ok(json!({"error": format!("Task {} has empty goal", i)}).to_string());
                }
            }

            let progress_tx = deps.progress_tx.clone();
            let max_concurrent = deps.delegation_config.max_concurrent_children.max(1) as usize;
            let child_timeout = deps.delegation_config.child_timeout.max(30.0);
            let results = run_delegated_tasks_batch(
                deps,
                ctx.cwd.clone(),
                task_pairs,
                extra_tools,
                max_concurrent,
                child_timeout,
                parent_cancel.unwrap_or_else(CancellationToken::new),
                progress_tx,
            )
            .await;
            return serde_json::to_string(&results)
                .map_err(|e| ToolError::Execution(format!("Serialization error: {}", e)));
        }

        // Single task mode
        let goal = arguments
            .get("goal")
            .and_then(|g| g.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("Missing required field 'goal'".into()))?;

        if goal.trim().is_empty() {
            return Ok(json!({"error": "goal must not be empty"}).to_string());
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

        let result = run_delegated_task(
            &deps,
            ctx.cwd.clone(),
            goal.to_string(),
            context,
            extra_tools,
            child_cancel,
            deps.progress_tx.clone(),
        )
        .await;

        serde_json::to_string(&result)
            .map_err(|e| ToolError::Execution(format!("Serialization error: {}", e)))
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        false
    }
}
