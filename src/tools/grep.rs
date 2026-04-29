//! Grep tool — search file contents with regex pattern.

use std::path::Path;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};
use walkdir::WalkDir;

use super::base::{Tool, ToolContext, ToolError};

pub struct GrepTool;

impl GrepTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "grep",
                "description": "Search file contents using a regex pattern. Returns matching lines with file paths and line numbers.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Regex pattern to search for."
                        },
                        "path": {
                            "type": "string",
                            "description": "Directory or file to search in (default: cwd)."
                        },
                        "include": {
                            "type": "string",
                            "description": "File glob filter (e.g. '*.rs', '*.py')."
                        },
                        "context": {
                            "type": "integer",
                            "description": "Number of context lines before/after each match (default: 0).",
                            "default": 0
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of results (default: 50).",
                            "default": 50
                        }
                    },
                    "required": ["pattern"]
                }
            }
        })
    }

    async fn execute(
        &self,
        arguments: Value,
        ctx: &ToolContext,
    ) -> Result<String, ToolError> {
        let pattern = arguments["pattern"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("missing 'pattern'".into()))?;
        let context = arguments
            .get("context")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let limit = arguments
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(50) as usize;
        let include = arguments.get("include").and_then(|v| v.as_str());

        let base_path = match arguments.get("path").and_then(|v| v.as_str()) {
            Some(p) => ctx.resolve_path(p),
            None => ctx.cwd.clone(),
        };

        if !base_path.exists() {
            return Err(ToolError::NotFound(format!(
                "Path not found: {}",
                base_path.display()
            )));
        }

        let re = Regex::new(pattern)
            .map_err(|e| ToolError::InvalidArguments(format!("Invalid regex: {}", e)))?;

        let is_dir = base_path.is_dir();
        let mut results = Vec::new();
        let mut match_count = 0;

        let entries: Vec<std::path::PathBuf> = if is_dir {
            WalkDir::new(&base_path)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    if !e.file_type().is_file() {
                        return false;
                    }
                    if let Some(glob) = include {
                        simple_glob_match(glob, e.path())
                    } else {
                        true
                    }
                })
                .map(|e| e.path().to_path_buf())
                .collect()
        } else {
            vec![base_path.clone()]
        };

        for path in &entries {
            if match_count >= limit {
                break;
            }

            let content = match tokio::fs::read_to_string(path).await {
                Ok(c) => c,
                Err(_) => continue, // Skip unreadable files
            };

            let lines: Vec<&str> = content.lines().collect();
            let relative = path
                .strip_prefix(&base_path)
                .unwrap_or(path);

            for (line_idx, line) in lines.iter().enumerate() {
                if re.is_match(line) {
                    let line_num = line_idx + 1;

                    // Build context
                    let mut context_lines = Vec::new();
                    let start = line_num.saturating_sub(context);
                    let end = (line_num + context).min(lines.len());

                    for ctx_line_num in start..=end {
                        let marker = if ctx_line_num == line_num {
                            ">>>"
                        } else {
                            "   "
                        };
                        context_lines.push(format!(
                            "{} {:>4} | {}",
                            marker,
                            ctx_line_num,
                            lines[ctx_line_num - 1]
                        ));
                    }

                    results.push(format!(
                        "{}:{}\n{}",
                        relative.display(),
                        line_num,
                        context_lines.join("\n")
                    ));

                    match_count += 1;
                    if match_count >= limit {
                        break;
                    }
                }
            }
        }

        if results.is_empty() {
            return Ok(format!("No matches for '{}' in {}", pattern, base_path.display()));
        }

        Ok(format!(
            "Found {} match(es):\n\n{}",
            match_count,
            results.join("\n\n")
        ))
    }
}

/// Simple file glob matching for the include filter.
fn simple_glob_match(pattern: &str, path: &Path) -> bool {
    let file_name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => return false,
    };
    // Support patterns like "*.rs", "*.py"
    if pattern.starts_with('*') && pattern.contains('.') && !pattern.contains('/') {
        let ext = &pattern[1..]; // ".rs"
        file_name.ends_with(ext)
    } else {
        file_name.contains(pattern)
    }
}
