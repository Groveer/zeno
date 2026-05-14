//! Todo tool — in-memory task list for planning and tracking progress.
//!
//! Allows the LLM to decompose complex tasks into actionable steps,
//! mark progress, and let the user see what's been done.
//!
//! Actions:
//!   - create: Start a new plan with a list of tasks.
//!   - add: Append tasks to the existing plan.
//!   - update: Change a task's status or description.
//!   - delete: Remove a task.
//!   - list: Show all tasks with their current status.
//!
//! State lives in memory (Arc<Mutex<>>) so it survives across turns
//! within a session.  The full task list is returned on every call.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::base::{Tool, ToolContext, ToolError};

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

/// A single task item.
#[derive(Debug, Clone)]
pub struct Task {
    pub id: String,
    pub description: String,
    pub status: String, // "pending" | "in_progress" | "completed"
}

/// In-memory todo list state, shared via Arc<Mutex<>>.
#[derive(Debug, Default)]
pub struct TodoState {
    pub plan: String,
    pub tasks: Vec<Task>,
    pub next_id: u32,
}

impl TodoState {
    /// Format a human-readable summary of the plan and all tasks.
    pub fn format_task_list(&self) -> String {
        if self.tasks.is_empty() {
            return "No tasks in the plan.".to_string();
        }

        let total = self.tasks.len();
        let completed = self
            .tasks
            .iter()
            .filter(|t| t.status == "completed")
            .count();

        let mut out = String::new();
        if !self.plan.is_empty() {
            out.push_str(&format!("Plan: {}\n", self.plan));
        } else {
            out.push_str(&format!(
                "{} tasks, {}/{} completed\n",
                total, completed, total
            ));
        }

        for task in &self.tasks {
            out.push_str(&format!(
                "  {} {} ({})\n",
                task.id, task.description, task.status
            ));
        }

        out.pop(); // trailing newline
        out
    }
}

// ---------------------------------------------------------------------------
// TodoTool
// ---------------------------------------------------------------------------

pub struct TodoTool {
    state: Arc<Mutex<TodoState>>,
}

impl TodoTool {
    /// Create a new TodoTool (used in tests).
    #[cfg(test)]
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(TodoState::default())),
        }
    }

    /// Create a TodoTool that shares state with an existing Arc.
    /// Used by the TUI to render the same state in the side panel.
    pub fn from_state(state: Arc<Mutex<TodoState>>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &str {
        "todo"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "todo",
                "description": "Manage a task list for planning and tracking your progress. \
                    Create a plan with tasks, mark tasks as in_progress or completed, \
                    add new tasks, or delete tasks. The tool always returns the current \
                    state of the task list so you can see what's done and what remains.\n\n\
                    USAGE PATTERN:\n\
                    1. When given a complex request, first call `todo` with `action=create` \
                       to break it down into steps.\n\
                    2. Before each major phase, call `todo` with `action=update` to mark \
                       the current task as `in_progress`.\n\
                    3. After completing a task, mark it `completed`.\n\
                    4. If new work emerges, use `action=add` to extend the plan.\n\n\
                    ACTIONS:\n\
                    - create: Start a new plan. Provide `plan` (overall description) and `tasks` (list of {description}).\n\
                    - add: Append one or more tasks to the existing plan.\n\
                    - update: Change a task's status (pending → in_progress → completed) or its description.\n\
                    - delete: Remove a task from the plan.\n\
                    - list: Show all tasks and their current status.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["create", "add", "update", "delete", "list"],
                            "description": "The operation to perform."
                        },
                        "plan": {
                            "type": "string",
                            "description": "Overall plan description. Required for 'create'."
                        },
                        "tasks": {
                            "type": "array",
                            "description": "List of task objects. Required for 'create' and 'add'. \
                                Each object must have a 'description' field.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "description": {
                                        "type": "string",
                                        "description": "Description of the task step."
                                    }
                                },
                                "required": ["description"]
                            }
                        },
                        "task_id": {
                            "type": "string",
                            "description": "Task ID (e.g. 'T1', 'T2'). Required for 'update' and 'delete'."
                        },
                        "status": {
                            "type": "string",
                            "enum": ["pending", "in_progress", "completed"],
                            "description": "New status for the task. Used with 'update'."
                        }
                    },
                    "required": ["action"]
                }
            }
        })
    }

    async fn execute(&self, arguments: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        let action = arguments["action"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("missing 'action' field".into()))?;

        let mut state = self.state.lock().await;

        match action {
            "create" => {
                let plan = arguments["plan"].as_str().unwrap_or("").to_string();
                let tasks_raw = arguments["tasks"].as_array().ok_or_else(|| {
                    ToolError::InvalidArguments("'tasks' array is required for 'create'".into())
                })?;

                let mut tasks = Vec::new();
                for (i, task) in tasks_raw.iter().enumerate() {
                    let desc = task["description"].as_str().ok_or_else(|| {
                        ToolError::InvalidArguments(format!(
                            "task[{}] missing 'description' field",
                            i
                        ))
                    })?;
                    tasks.push(Task {
                        id: format!("T{}", i + 1),
                        description: desc.to_string(),
                        status: "pending".into(),
                    });
                }

                state.plan = plan;
                state.tasks = tasks;
                state.next_id = (state.tasks.len() + 1) as u32;

                let count = state.tasks.len();
                Ok(format!(
                    "Created plan with {} tasks.\n{}",
                    count,
                    state.format_task_list()
                ))
            }

            "add" => {
                let tasks_raw = arguments["tasks"].as_array().ok_or_else(|| {
                    ToolError::InvalidArguments("'tasks' array is required for 'add'".into())
                })?;

                for task in tasks_raw {
                    let desc = task["description"].as_str().ok_or_else(|| {
                        ToolError::InvalidArguments(
                            "each task must have a 'description' field".into(),
                        )
                    })?;
                    let id = state.next_id;
                    state.next_id += 1;
                    state.tasks.push(Task {
                        id: format!("T{}", id),
                        description: desc.to_string(),
                        status: "pending".into(),
                    });
                }

                Ok(format!(
                    "Added {} task(s).\n{}",
                    tasks_raw.len(),
                    state.format_task_list()
                ))
            }

            "update" => {
                let task_id = arguments["task_id"].as_str().ok_or_else(|| {
                    ToolError::InvalidArguments("'task_id' is required for 'update'".into())
                })?;

                let task_id_str;
                let status_str;
                {
                    let task = state
                        .tasks
                        .iter_mut()
                        .find(|t| t.id == task_id)
                        .ok_or_else(|| {
                            ToolError::InvalidArguments(format!("Task '{}' not found", task_id))
                        })?;

                    if let Some(status) = arguments["status"].as_str() {
                        if !["pending", "in_progress", "completed"].contains(&status) {
                            return Err(ToolError::InvalidArguments(format!(
                                "Invalid status '{}'. Use: pending, in_progress, completed",
                                status
                            )));
                        }
                        task.status = status.to_string();
                    }

                    if let Some(desc) = arguments["description"].as_str() {
                        task.description = desc.to_string();
                    }

                    task_id_str = task.id.clone();
                    status_str = task.status.clone();
                } // mutable borrow on state ends here

                Ok(format!(
                    "Updated {} → {}.\n{}",
                    task_id_str,
                    status_str,
                    state.format_task_list()
                ))
            }

            "delete" => {
                let task_id = arguments["task_id"].as_str().ok_or_else(|| {
                    ToolError::InvalidArguments("'task_id' is required for 'delete'".into())
                })?;

                let initial_len = state.tasks.len();
                state.tasks.retain(|t| t.id != task_id);

                if state.tasks.len() == initial_len {
                    return Err(ToolError::InvalidArguments(format!(
                        "Task '{}' not found",
                        task_id
                    )));
                }

                Ok(format!(
                    "Deleted {}.\n{}",
                    task_id,
                    state.format_task_list()
                ))
            }

            "list" => Ok(state.format_task_list()),

            _ => Err(ToolError::InvalidArguments(format!(
                "Unknown action '{}'. Use: create, add, update, delete, list",
                action
            ))),
        }
    }

    fn is_read_only(&self, input: &Value) -> bool {
        input.get("action").and_then(|a| a.as_str()) == Some("list")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn new_tool() -> TodoTool {
        TodoTool::new()
    }

    fn empty_ctx() -> ToolContext {
        ToolContext {
            cwd: std::path::PathBuf::from("/tmp"),
            ask_sender: None,
            mcp_manager: None,
            sub_agent_deps: None,
            cancel_token: None,
            rate_limiter: None,
            tool_stats: None,
        }
    }

    #[tokio::test]
    async fn test_create_plan() {
        let tool = new_tool();
        let ctx = empty_ctx();

        let result = tool
            .execute(
                json!({
                    "action": "create",
                    "plan": "Fix the login bug",
                    "tasks": [
                        {"description": "Investigate error logs"},
                        {"description": "Reproduce locally"},
                        {"description": "Implement the fix"},
                        {"description": "Write tests"}
                    ]
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(result.contains("Created plan with 4 tasks"));
    }

    #[tokio::test]
    async fn test_create_missing_tasks() {
        let tool = new_tool();
        let ctx = empty_ctx();

        let result = tool
            .execute(json!({"action": "create", "plan": "test"}), &ctx)
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_update_status() {
        let tool = new_tool();
        let ctx = empty_ctx();

        // Create first
        tool.execute(
            json!({
                "action": "create",
                "plan": "Test",
                "tasks": [{"description": "Step 1"}, {"description": "Step 2"}]
            }),
            &ctx,
        )
        .await
        .unwrap();

        // Update T1 to in_progress
        let result = tool
            .execute(
                json!({"action": "update", "task_id": "T1", "status": "in_progress"}),
                &ctx,
            )
            .await
            .unwrap();

        assert!(result.contains("Updated T1 → in_progress"));

        // Update T1 to completed
        let result = tool
            .execute(
                json!({"action": "update", "task_id": "T1", "status": "completed"}),
                &ctx,
            )
            .await
            .unwrap();

        assert!(result.contains("Updated T1 → completed"));
    }

    #[tokio::test]
    async fn test_add_tasks() {
        let tool = new_tool();
        let ctx = empty_ctx();

        // Create initial
        tool.execute(
            json!({
                "action": "create",
                "plan": "Build feature",
                "tasks": [{"description": "Setup"}, {"description": "Implement"}]
            }),
            &ctx,
        )
        .await
        .unwrap();

        // Add more
        let result = tool
            .execute(
                json!({
                    "action": "add",
                    "tasks": [{"description": "Test"}, {"description": "Deploy"}]
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(result.contains("Added 2 task(s)"));
    }

    #[tokio::test]
    async fn test_delete_task() {
        let tool = new_tool();
        let ctx = empty_ctx();

        tool.execute(
            json!({
                "action": "create",
                "plan": "Test",
                "tasks": [{"description": "A"}, {"description": "B"}, {"description": "C"}]
            }),
            &ctx,
        )
        .await
        .unwrap();

        let result = tool
            .execute(json!({"action": "delete", "task_id": "T2"}), &ctx)
            .await
            .unwrap();

        assert!(result.contains("Deleted T2"));
    }

    #[tokio::test]
    async fn test_list_empty() {
        let tool = new_tool();
        let ctx = empty_ctx();

        let result = tool.execute(json!({"action": "list"}), &ctx).await.unwrap();

        assert!(result.contains("No tasks"));
    }

    #[tokio::test]
    async fn test_list_with_tasks() {
        let tool = new_tool();
        let ctx = empty_ctx();

        tool.execute(
            json!({
                "action": "create",
                "plan": "Test",
                "tasks": [{"description": "Do something"}]
            }),
            &ctx,
        )
        .await
        .unwrap();

        let result = tool.execute(json!({"action": "list"}), &ctx).await.unwrap();

        assert!(result.contains("T1 Do something"));
        assert!(result.contains("pending"));
    }

    #[tokio::test]
    async fn test_unknown_action() {
        let tool = new_tool();
        let ctx = empty_ctx();

        let result = tool.execute(json!({"action": "invalid"}), &ctx).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_is_read_only_list() {
        let tool = new_tool();
        assert!(tool.is_read_only(&json!({"action": "list"})));
        assert!(!tool.is_read_only(&json!({"action": "create"})));
        assert!(!tool.is_read_only(&json!({"action": "update"})));
    }

    #[tokio::test]
    async fn test_progress_counter() {
        let tool = new_tool();
        let ctx = empty_ctx();

        tool.execute(
            json!({
                "action": "create",
                "plan": "Test",
                "tasks": [
                    {"description": "A"},
                    {"description": "B"},
                    {"description": "C"}
                ]
            }),
            &ctx,
        )
        .await
        .unwrap();

        // Complete 2 out of 3
        tool.execute(
            json!({"action": "update", "task_id": "T1", "status": "completed"}),
            &ctx,
        )
        .await
        .unwrap();
        tool.execute(
            json!({"action": "update", "task_id": "T2", "status": "completed"}),
            &ctx,
        )
        .await
        .unwrap();

        let result = tool.execute(json!({"action": "list"}), &ctx).await.unwrap();

        assert!(result.contains("Plan: Test"));
        assert!(result.contains("T1 A (completed)"));
        assert!(result.contains("T2 B (completed)"));
        assert!(result.contains("T3 C (pending)"));
    }

    #[tokio::test]
    async fn test_update_nonexistent_task() {
        let tool = new_tool();
        let ctx = empty_ctx();

        tool.execute(
            json!({
                "action": "create",
                "plan": "Test",
                "tasks": [{"description": "Only one"}]
            }),
            &ctx,
        )
        .await
        .unwrap();

        let result = tool
            .execute(
                json!({"action": "update", "task_id": "T99", "status": "completed"}),
                &ctx,
            )
            .await;

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Task 'T99' not found")
        );
    }
}
