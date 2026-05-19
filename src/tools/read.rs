//! Read tool — read file contents with optional offset/limit/context.
//!
//! Uses a file content pool to cache file contents in memory, avoiding
//! redundant disk I/O on repeated reads of the same file. Tracks which
//! line ranges have been returned so the LLM gets clear feedback on
//! overlapping requests.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::base::{Tool, ToolContext, ToolError};
use super::file_content_pool::ReadOutcome;

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

/// Default offset (1-indexed) when not specified.
pub const DEFAULT_OFFSET: u64 = 1;

/// Default maximum lines to return when limit is not specified.
pub const DEFAULT_LIMIT: u64 = 500;

/// Default context lines (lines before/after offset) when not specified.
pub const DEFAULT_CONTEXT: u64 = 10;

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
                            "description": "Path to the file (absolute, relative, or ~/path)."
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

        // Record read for file-staleness tracking
        crate::tools::file_state::record_read(&ctx.task_id, &resolved).await;

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

        // --- File content pool integration ---
        // Strategy: single lock hold for the hot path. If the file isn't
        // cached, we drop the lock for I/O, re-lock, insert, then immediately
        // read_range in the same critical section.
        let resolved_str = resolved.to_string_lossy().to_string();
        if let Some(ref pool_arc) = ctx.file_content_pool {
            let mut pool = pool_arc.lock().await;

            // Ensure file is cached — drop lock for disk I/O if needed.
            //
            // Concurrency note: two tasks may race here, both finding the file
            // uncached, both dropping the lock to read disk, then both inserting.
            // `insert_preserving_ranges` handles this safely: if the content is
            // identical, read_ranges are preserved; if the file was modified
            // between the two reads (rare — external process), the later insert
            // wins and resets ranges. This is acceptable because write/edit
            // already invalidate the pool entry anyway.
            if pool.total_lines(&resolved_str).is_none() {
                drop(pool);
                let content = tokio::fs::read_to_string(&resolved).await?;
                pool = pool_arc.lock().await;
                // insert() preserves existing read_ranges if content is unchanged,
                // so a concurrent insert by another task won't reset tracking.
                pool.insert_preserving_ranges(&resolved_str, &content);
            }

            // Retry loop: at most 2 iterations.
            //   Iteration 1: read from pool (populate on miss).
            //   Iteration 2: guaranteed hit after re-insert, or safety-net fallback.
            let mut retries = 0usize;
            let mut disk_content: Option<String> = None;
            loop {
                let total_lines = pool.total_lines(&resolved_str).unwrap_or(0);
                let (start, end) = parse_read_range(&arguments, total_lines);

                match pool.read_range(&resolved_str, start, end) {
                    ReadOutcome::Hit {
                        lines,
                        start: s,
                        end: e,
                        covered_prefix,
                    } => {
                        if lines.is_empty() && start >= total_lines {
                            return Ok(format!(
                                "(file has {} lines, offset {} is past end)",
                                total_lines,
                                start + 1
                            ));
                        }
                        return Ok(format_pool_output(lines, s, e, covered_prefix, total_lines));
                    }
                    ReadOutcome::Miss if retries == 0 => {
                        // First miss — read from disk, insert, and retry.
                        retries += 1;
                        drop(pool);
                        let content = tokio::fs::read_to_string(&resolved).await?;
                        pool = pool_arc.lock().await;
                        pool.insert_preserving_ranges(&resolved_str, &content);
                        disk_content = Some(content);
                    }
                    ReadOutcome::Miss => {
                        // Should not happen — we just inserted.
                        // Fall back to disk formatting as a safety net,
                        // reusing content already read above.
                        let content = disk_content.take().unwrap_or_default();
                        let total = content.lines().count();
                        return format_from_disk_with_content(&content, start, end, total);
                    }
                }
            }
        } else {
            // No pool — fall back to direct formatting (original behavior)
            let content = tokio::fs::read_to_string(&resolved).await?;
            let total_lines = content.lines().count();
            let (start, end) = parse_read_range(&arguments, total_lines);
            format_from_disk_with_content(&content, start, end, total_lines)
        }
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }
}

/// Format pool output with line numbers, metadata footer, and overlap hint.
fn format_pool_output(
    lines: Vec<String>,
    start: usize,
    end: usize,
    covered_prefix: usize,
    total_lines: usize,
) -> String {
    let mut result = String::new();
    for (i, line) in lines.iter().enumerate() {
        let line_num = start + i + 1;
        result.push_str(&format!("{:>6} | {}\n", line_num, line));
    }

    if end < total_lines {
        result.push_str(&format!(
            "\n(lines {}-{} of {})\n",
            start + 1,
            end,
            total_lines
        ));
    }

    // Overlap hint — tells the LLM that some lines were already
    // returned in a previous read, reducing confusion.
    if covered_prefix > 0 {
        let covered_end = start + covered_prefix;
        result.push_str(&format!(
            "\n[Note: lines {}-{} were already returned in a previous read]\n",
            start + 1,
            covered_end
        ));
    }

    result
}

/// Parse read arguments and compute the 0-indexed range [start, end).
///
/// Shared by both the pool path and the no-pool fallback to avoid duplication.
fn parse_read_range(arguments: &Value, total_lines: usize) -> (usize, usize) {
    if total_lines == 0 {
        return (0, 0);
    }

    let has_offset = arguments.get("offset").is_some();
    let has_limit = arguments.get("limit").is_some();
    let has_context = arguments.get("context").is_some();

    let offset = arguments
        .get("offset")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_OFFSET) as usize;

    let (start, end) = if has_context {
        let ctx_lines = arguments["context"].as_u64().unwrap_or(DEFAULT_CONTEXT) as usize;
        let center = offset.saturating_sub(1);
        let read_start = center.saturating_sub(ctx_lines);
        let read_end = (center + ctx_lines + 1).min(total_lines);
        (read_start, read_end)
    } else if has_offset || has_limit {
        let limit = arguments
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_LIMIT)
            .min(MAX_LINES_PER_CALL) as usize;
        let start = offset.saturating_sub(1);
        let end = (start + limit).min(total_lines);
        (start, end)
    } else if total_lines <= FULL_READ_THRESHOLD {
        (0, total_lines)
    } else {
        (0, DEFAULT_PREVIEW_LINES.min(total_lines))
    };

    // Clamp start to valid range (end is already clamped via .min(total_lines))
    if start >= total_lines {
        (total_lines, total_lines) // empty range — caller shows "(file has N lines, ...)"
    } else {
        (start, end)
    }
}

/// Format pre-read content with line numbers and metadata footer.
/// Used in the no-pool path where content is already in memory.
fn format_from_disk_with_content(
    content: &str,
    start: usize,
    end: usize,
    total_lines: usize,
) -> Result<String, ToolError> {
    if start >= total_lines {
        return Ok(format!(
            "(file has {} lines, offset {} is past end)",
            total_lines,
            start + 1
        ));
    }

    let lines: Vec<&str> = content.lines().collect();
    let mut result = String::new();
    for (i, line) in lines[start..end].iter().enumerate() {
        let line_num = start + i + 1;
        result.push_str(&format!("{:>6} | {}\n", line_num, line));
    }

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
