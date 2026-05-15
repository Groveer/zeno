//! Read tool — read file contents with optional offset/limit/context.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::base::{Tool, ToolContext, ToolError};

/// Default number of lines to return when no offset is specified and the file
/// is large. Increased from 300 to reduce round-trips for medium-sized files.
const DEFAULT_PREVIEW_LINES: usize = 500;

/// Files shorter than this are returned in full (no truncation).
const FULL_READ_THRESHOLD: usize = 500;

/// Maximum file size in bytes that read will load (10 MB).
/// Prevents OOM from reading huge single-line files.
const MAX_FILE_SIZE_BYTES: u64 = 10 * 1024 * 1024;

/// Maximum lines that can be read in a single call.
/// For larger files, use offset+limit to paginate (e.g. offset=1, limit=5000).
const MAX_LINES_PER_CALL: u64 = 5000;

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
                "description": "Read file contents with line numbers.\n\nBEHAVIOR:\n- Small files (≤500 lines, no params): returns the ENTIRE file.\n- Large files (no params): returns first 500 lines as preview.\n- Use offset+limit to read a specific range (e.g. offset=50, limit=100).\n- Use offset+context to read around a line (e.g. offset=50, context=10 reads lines 40-60).\n- For very large files, paginate by incrementing offset (e.g. offset=1, limit=5000 reads first 5000 lines, then offset=5001, limit=5000).\n- Set limit=5000 to read up to 5000 lines at once.\n\nBEST PRACTICE: For large files, first read without params to get a preview, then use offset+limit to read specific sections. If the file is very large (>5000 lines), paginate in chunks of 5000.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path (relative to cwd or absolute)."
                        },
                        "offset": {
                            "type": "integer",
                            "description": "Start line (1-indexed, default: 1).",
                            "default": 1
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max lines to read (default: 500, max: 5000). Use 5000 to read large sections in one call. For files >5000 lines, paginate by incrementing offset.",
                            "default": 500
                        },
                        "context": {
                            "type": "integer",
                            "description": "Lines of context around offset. Mutually exclusive with limit. Example: offset=50, context=10 reads lines 40-60."
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
            return Err(ToolError::NotFound(format!("{}", resolved.display())));
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
                .min(MAX_LINES_PER_CALL) as usize;
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

        // Metadata footer — compact
        if end < total_lines {
            result.push_str(&format!(
                "\n(lines {}-{} of {})\n",
                start + 1,
                end,
                total_lines
            ));
        }

        Ok(result)
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }
}
