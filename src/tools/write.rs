//! Write tool — create or overwrite a file.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::base::{Tool, ToolContext, ToolError};

/// Maximum content size in bytes that write will accept (10 MB).
/// Prevents LLM from generating extremely large files that could cause OOM.
const MAX_CONTENT_SIZE_BYTES: usize = 10 * 1024 * 1024;

pub struct WriteTool;

impl WriteTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "write",
                "description": "Write content to a file, creating it or overwriting entirely. Creates parent directories automatically.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path to write."
                        },
                        "content": {
                            "type": "string",
                            "description": "Complete content to write."
                        }
                    },
                    "required": ["path", "content"]
                }
            }
        })
    }

    async fn execute(&self, arguments: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let path = arguments["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("missing 'path'".into()))?;
        let content = arguments["content"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("missing 'content'".into()))?;

        // Check content size before writing to prevent OOM
        if content.len() > MAX_CONTENT_SIZE_BYTES {
            return Err(ToolError::Execution(format!(
                "Content too large ({} bytes, max {} bytes). Write smaller chunks.",
                content.len(),
                MAX_CONTENT_SIZE_BYTES
            )));
        }

        let resolved = ctx.resolve_path(path);

        // Create parent directories
        if let Some(parent) = resolved.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        tokio::fs::write(&resolved, content).await?;

        // Post-write verification: re-read and confirm content matches
        match tokio::fs::read_to_string(&resolved).await {
            Ok(verified) if verified == content => {}
            Ok(verified) => {
                return Err(ToolError::Execution(format!(
                    "Post-write verification failed for {}: written {} chars but read back {} chars. \
                     Possible disk full or encoding issue.",
                    resolved.display(),
                    content.len(),
                    verified.len()
                )));
            }
            Err(e) => {
                return Err(ToolError::Execution(format!(
                    "Post-write verification failed: could not re-read {}: {}",
                    resolved.display(),
                    e
                )));
            }
        }

        let lines = content.lines().count();
        Ok(format!("Written {} lines to {}", lines, resolved.display()))
    }
}
