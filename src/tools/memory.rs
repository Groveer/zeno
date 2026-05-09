//! Memory tool — persistent curated memory that survives across sessions.
//!
//! Two stores:
//!   - MEMORY.md: agent's personal notes (environment facts, project conventions)
//!   - USER.md: user profile (preferences, communication style, expectations)
//!
//! Single `memory` tool with action parameter: add, replace, remove.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::base::{Tool, ToolContext, ToolError};
use crate::memory::manager::SharedMemoryManager;
use crate::memory::store::MemoryStore;

pub struct MemoryTool {
    store: Arc<Mutex<MemoryStore>>,
    memory_manager: SharedMemoryManager,
}

impl MemoryTool {
    pub fn new(store: Arc<Mutex<MemoryStore>>, memory_manager: SharedMemoryManager) -> Self {
        Self {
            store,
            memory_manager,
        }
    }
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &str {
        "memory"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "memory",
                "description": concat!(
                    "Save durable information to persistent memory that survives across sessions. ",
                    "Memory is injected into future turns, so keep it compact and focused on facts ",
                    "that will still matter later.\n\n",
                    "WHEN TO SAVE (do this proactively, don't wait to be asked):\n",
                    "- User corrects you or says 'remember this' / 'don't do that again'\n",
                    "- User shares a preference, habit, or personal detail (name, role, timezone, coding style)\n",
                    "- You discover something about the environment (OS, installed tools, project structure)\n",
                    "- You learn a convention, API quirk, or workflow specific to this user's setup\n",
                    "- You identify a stable fact that will be useful again in future sessions\n\n",
                    "PRIORITY: User preferences and corrections > environment facts > procedural knowledge. ",
                    "The most valuable memory prevents the user from having to repeat themselves.\n\n",
                    "Do NOT save task progress, session outcomes, completed-work logs, or temporary TODO ",
                    "state to memory.\n\n",
                    "TWO TARGETS:\n",
                    "- 'user': who the user is -- name, role, preferences, communication style, pet peeves\n",
                    "- 'memory': your notes -- environment facts, project conventions, tool quirks, lessons learned\n\n",
                    "ACTIONS: add (new entry), replace (update existing -- old_text identifies it), ",
                    "remove (delete -- old_text identifies it).\n\n",
                    "SKIP: trivial/obvious info, things easily re-discovered, raw data dumps, and temporary task state."
                ),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["add", "replace", "remove"],
                            "description": "The action to perform."
                        },
                        "target": {
                            "type": "string",
                            "enum": ["memory", "user"],
                            "description": "Which memory store: 'memory' for personal notes, 'user' for user profile.",
                            "default": "memory"
                        },
                        "content": {
                            "type": "string",
                            "description": "The entry content. Required for 'add' and 'replace'."
                        },
                        "old_text": {
                            "type": "string",
                            "description": "Short unique substring identifying the entry to replace or remove."
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
            .ok_or_else(|| ToolError::InvalidArguments("missing 'action'".into()))?;

        let target = arguments["target"].as_str().unwrap_or("memory");

        if target != "memory" && target != "user" {
            return Ok(json!({
                "success": false,
                "error": format!("Invalid target '{}'. Use 'memory' or 'user'.", target)
            })
            .to_string());
        }

        let mut store = self.store.lock().await;

        let result = match action {
            "add" => {
                let content = arguments["content"].as_str().ok_or_else(|| {
                    ToolError::InvalidArguments("'content' is required for 'add'".into())
                })?;
                let result = store.add(target, content);
                // Mirror to external memory provider (if any)
                self.memory_manager
                    .lock()
                    .await
                    .on_memory_write(action, target, content)
                    .await;
                result
            }
            "replace" => {
                let old_text = arguments["old_text"].as_str().ok_or_else(|| {
                    ToolError::InvalidArguments("'old_text' is required for 'replace'".into())
                })?;
                let content = arguments["content"].as_str().ok_or_else(|| {
                    ToolError::InvalidArguments("'content' is required for 'replace'".into())
                })?;
                let result = store.replace(target, old_text, content);
                // Mirror to external memory provider (if any)
                self.memory_manager
                    .lock()
                    .await
                    .on_memory_write(action, target, content)
                    .await;
                result
            }
            "remove" => {
                let old_text = arguments["old_text"].as_str().ok_or_else(|| {
                    ToolError::InvalidArguments("'old_text' is required for 'remove'".into())
                })?;
                let result = store.remove(target, old_text);
                // Mirror to external memory provider (if any)
                self.memory_manager
                    .lock()
                    .await
                    .on_memory_write(action, target, old_text)
                    .await;
                result
            }
            _ => {
                return Ok(json!({
                    "success": false,
                    "error": format!("Unknown action '{}'. Use: add, replace, remove", action)
                })
                .to_string());
            }
        };

        Ok(serde_json::to_string(&result)
            .unwrap_or_else(|_| r#"{"success":false,"error":"serialization failed"}"#.to_string()))
    }
}
