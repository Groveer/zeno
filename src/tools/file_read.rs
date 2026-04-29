//! File read tool — read file contents with optional offset/limit.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::base::{Tool, ToolContext, ToolError};

pub struct FileReadTool;

impl FileReadTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for FileReadTool {
    fn name(&self) -> &str {
        "file_read"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "file_read",
                "description": "Read the contents of a file. Returns file content with line numbers. Use offset and limit for large files.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file to read (relative to cwd or absolute)."
                        },
                        "offset": {
                            "type": "integer",
                            "description": "Line number to start reading from (1-indexed, default: 1).",
                            "default": 1
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of lines to read (default: 500, max: 2000).",
                            "default": 500
                        }
                    },
                    "required": ["path"]
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

        let resolved = ctx.resolve_path(path);

        if !resolved.exists() {
            return Err(ToolError::NotFound(format!("File not found: {}", resolved.display())));
        }

        let offset = arguments
            .get("offset")
            .and_then(|v| v.as_u64())
            .unwrap_or(1) as usize;
        let limit = arguments
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(500)
            .min(2000) as usize;

        let content = tokio::fs::read_to_string(&resolved).await?;
        let lines: Vec<&str> = content.lines().collect();

        let start = offset.saturating_sub(1);
        let end = (start + limit).min(lines.len());

        if start >= lines.len() {
            return Ok(format!(
                "(file has {} lines, offset {} is past end)",
                lines.len(),
                offset
            ));
        }

        let mut result = String::new();
        for (i, line) in lines[start..end].iter().enumerate() {
            let line_num = start + i + 1;
            result.push_str(&format!("{:>6} | {}\n", line_num, line));
        }

        if end < lines.len() {
            result.push_str(&format!(
                "\n(showing lines {}-{} of {} total)\n",
                start + 1,
                end,
                lines.len()
            ));
        }

        Ok(result)
    }
}
