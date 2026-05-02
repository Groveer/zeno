//! Read tool — read file contents with optional offset/limit/context.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::base::{Tool, ToolContext, ToolError};

/// Default number of lines to return when no offset is specified and the file
/// is large. A generous default avoids excessive LLM round-trips for partial reads.
const DEFAULT_PREVIEW_LINES: usize = 300;

/// Files shorter than this are returned in full (no truncation).
const FULL_READ_THRESHOLD: usize = 300;

/// Maximum file size in bytes that read will load (10 MB).
/// Prevents OOM from reading huge single-line files.
const MAX_FILE_SIZE_BYTES: u64 = 10 * 1024 * 1024;

pub struct ReadTool;

impl ReadTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "read",
                "description": "Read the contents of a file. Returns file content with line numbers. For understanding a file, use a large limit (e.g. 2000) to read more at once and avoid slow multiple round-trips. Use offset and limit for large files. Use context to read around a specific line (e.g. after grep).",
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
                            "description": "Maximum number of lines to read (default: 500, max: 2000). Use large values (e.g. 2000) when you need to read most of a file.",
                            "default": 500
                        },
                        "context": {
                            "type": "integer",
                            "description": "Read N lines of context around `offset` (before and after). Mutually exclusive with limit. Example: offset=50, context=10 reads lines 40-60."
                        }
                    },
                    "required": ["path"]
                }
            }
        })
    }

    async fn execute(&self, arguments: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let path = arguments["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("missing 'path'".into()))?;

        let resolved = ctx.resolve_path(path);

        if !resolved.exists() {
            return Err(ToolError::NotFound(format!(
                "File not found: {}",
                resolved.display()
            )));
        }

        // Check file size before reading to prevent OOM
        let file_size = tokio::fs::metadata(&resolved)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        if file_size > MAX_FILE_SIZE_BYTES {
            return Err(ToolError::Execution(format!(
                "File too large ({} bytes, max {} bytes). Use grep or read in chunks.",
                file_size, MAX_FILE_SIZE_BYTES
            )));
        }

        let content = tokio::fs::read_to_string(&resolved).await?;
        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        // Parse parameters
        let has_offset = arguments.get("offset").is_some();
        let has_limit = arguments.get("limit").is_some();
        let has_context = arguments.get("context").is_some();

        let offset = arguments
            .get("offset")
            .and_then(|v| v.as_u64())
            .unwrap_or(1) as usize;

        // Determine read range
        let (start, end) = if has_context {
            // Context mode: read N lines around offset
            let ctx_lines = arguments["context"].as_u64().unwrap_or(10) as usize;
            let center = offset.saturating_sub(1); // 0-indexed
            let read_start = center.saturating_sub(ctx_lines);
            let read_end = (center + ctx_lines + 1).min(total_lines);
            (read_start, read_end)
        } else if has_offset || has_limit {
            // Explicit offset/limit mode
            let limit = arguments
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(500)
                .min(2000) as usize;
            let start = offset.saturating_sub(1);
            let end = (start + limit).min(total_lines);
            (start, end)
        } else if total_lines <= FULL_READ_THRESHOLD {
            // Small file: return in full
            (0, total_lines)
        } else {
            // Large file, no parameters: preview mode
            (0, DEFAULT_PREVIEW_LINES.min(total_lines))
        };

        if start >= total_lines {
            return Ok(format!(
                "(file has {} lines, offset {} is past end)",
                total_lines, offset
            ));
        }

        let mut result = String::new();
        for (i, line) in lines[start..end].iter().enumerate() {
            let line_num = start + i + 1;
            result.push_str(&format!("{:>6} | {}\n", line_num, line));
        }

        // Metadata footer
        if end < total_lines {
            result.push_str(&format!(
                "\n(showing lines {}-{} of {} total)\n",
                start + 1,
                end,
                total_lines
            ));
            if !has_offset && !has_context {
                result.push_str(&format!(
                    "TIP: Use offset+limit or offset+context to read specific sections.\n\
                     Example: offset={}, context=20 to read lines {}-{}.\n",
                    total_lines / 2,
                    total_lines / 2 - 20,
                    total_lines / 2 + 20,
                ));
            }
        }

        Ok(result)
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }
}
