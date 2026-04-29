//! File write tool — create or overwrite a file.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::base::{Tool, ToolContext, ToolError};

pub struct FileWriteTool;

impl FileWriteTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for FileWriteTool {
    fn name(&self) -> &str {
        "file_write"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "file_write",
                "description": "Write content to a file, creating it if it doesn't exist or overwriting it entirely. Creates parent directories automatically.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file to write."
                        },
                        "content": {
                            "type": "string",
                            "description": "The complete content to write to the file."
                        }
                    },
                    "required": ["path", "content"]
                }
            }
        })
    }

    async fn execute(
        &self,
        arguments: Value,
        ctx: &ToolContext,
    ) -> Result<String, ToolError> {
        let path = arguments["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("missing 'path'".into()))?;
        let content = arguments["content"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("missing 'content'".into()))?;

        let resolved = ctx.resolve_path(path);

        // Create parent directories
        if let Some(parent) = resolved.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        tokio::fs::write(&resolved, content).await?;

        let lines = content.lines().count();
        Ok(format!(
            "Written {} lines to {}",
            lines,
            resolved.display()
        ))
    }
}
