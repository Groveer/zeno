//! Tool: list_sub_agents — query active sub-agent spawns.
//!
//! Registered with `Exposure::Deferred` so it's not included in the initial
//! tool list. The model discovers it via `tool_search("sub_agent")` or
//! `tool_search("delegate")` at runtime.
//!
//! Returns the list of direct children spawned by the current task, along with
//! their lifecycle status and metadata. Parent agents can use this to check on
//! their sub-agents after delegation.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::base::{Tool, ToolContext, ToolError};
use zeno_tools::{JsonToolOutput, ToolOutput};

pub struct ListSubAgentsTool;

impl ListSubAgentsTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for ListSubAgentsTool {
    fn name(&self) -> &str {
        "list_sub_agents"
    }

    fn exposure(&self) -> zeno_tools::ToolExposure {
        zeno_tools::ToolExposure::Deferred
    }

    fn supports_parallel(&self) -> bool {
        true
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "list_sub_agents",
                "description": "List sub-agents (children spawned via delegate_task) of the current agent. \
                    Returns each child's id, status (open/closed), task index, goal, and timestamps. \
                    Use this to check on work delegated to sub-agents, especially when waiting for \
                    batch tasks to complete.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "parent_id": {
                            "type": "string",
                            "description": "Optional parent agent id. When omitted, uses the current session id."
                        },
                        "status_filter": {
                            "type": "string",
                            "enum": ["open", "closed"],
                            "description": "Optional filter: return only 'open' or 'closed' children. \
                                When omitted, all children are returned regardless of status."
                        }
                    }
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

        let graph_store = deps.graph_store.as_ref().ok_or_else(|| {
            ToolError::Execution("Graph store not configured in this session.".to_string())
        })?;

        let parent_id = arguments
            .get("parent_id")
            .and_then(|v| v.as_str())
            .unwrap_or(&ctx.task_id);

        let status_filter = arguments
            .get("status_filter")
            .and_then(|v| v.as_str())
            .and_then(|s| match s {
                "open" => Some(crate::store::agent_graph::EdgeStatus::Open),
                "closed" => Some(crate::store::agent_graph::EdgeStatus::Closed),
                _ => None,
            });

        // Fetch full edge records in a single query
        let records = graph_store
            .list_children_with_details(parent_id, status_filter)
            .await
            .map_err(|e| ToolError::Execution(format!("Failed to list children: {}", e)))?;

        let summary = if records.is_empty() {
            format!("No sub-agents found for parent '{}'.", parent_id)
        } else {
            let status_label = match status_filter {
                Some(crate::store::agent_graph::EdgeStatus::Open) => "open ",
                Some(crate::store::agent_graph::EdgeStatus::Closed) => "closed ",
                None => "",
            };
            format!(
                "Found {} {}sub-agent(s) for parent '{}':\n\n{}",
                records.len(),
                status_label,
                parent_id,
                serde_json::to_string_pretty(&records)
                    .map_err(|e| ToolError::Execution(format!("Serialization error: {}", e)))?
            )
        };

        Ok(Box::new(JsonToolOutput::success(summary)))
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }
}
