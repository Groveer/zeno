//! Grep tool — search file contents with regex pattern.

use std::path::Path;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};
use walkdir::WalkDir;

use super::base::{Tool, ToolContext, ToolError};

pub struct GrepTool {
    skip_dirs: Vec<String>,
}

impl GrepTool {
    pub fn new(skip_dirs: Vec<String>) -> Self {
        Self { skip_dirs }
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
                "description": "Search file contents with regex. Returns matching lines with file paths and line numbers.",
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
                            "description": "Context lines before/after each match (default: 0).",
                            "default": 0
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max results (default: 50).",
                            "default": 50
                        }
                    },
                    "required": ["pattern"]
                }
            }
        })
    }

    async fn execute(&self, arguments: Value, ctx: &ToolContext) -> Result<String, ToolError> {
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

        let pattern_owned = pattern.to_string();
        let include_owned = include.map(String::from);
        let base_path_display = base_path.display().to_string();
        let skip_dirs = self.skip_dirs.clone();

        // Offload all blocking filesystem I/O (WalkDir traversal, file reads,
        // regex matching) to tokio's blocking thread pool so we don't starve
        // the async worker threads.
        let (match_count, results) = tokio::task::spawn_blocking(move || {
            grep_sync(
                &base_path,
                &re,
                &pattern_owned,
                include_owned.as_deref(),
                context,
                limit,
                &skip_dirs,
            )
        })
        .await
        .map_err(|e| ToolError::Execution(format!("Task join error: {}", e)))?;

        if results.is_empty() {
            return Ok(format!(
                "No matches for '{}' in {}",
                pattern, base_path_display,
            ));
        }

        Ok(format!(
            "Found {} match(es):\n\n{}",
            match_count,
            results.join("\n\n")
        ))
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }
}

/// Purely synchronous grep implementation — safe to run on a blocking thread.
fn grep_sync(
    base_path: &Path,
    re: &Regex,
    _pattern: &str,
    include: Option<&str>,
    context: usize,
    limit: usize,
    extra_skip_dirs: &[String],
) -> (usize, Vec<String>) {
    // Max number of file entries to scan (prevent OOM on huge repos)
    const MAX_FILE_ENTRIES: usize = 10_000;

    let mut results = Vec::new();
    let mut match_count = 0;

    if base_path.is_dir() {
        // Load gitignore patterns for the base directory
        let gitignore = crate::tools::gitignore::GitIgnoreMatcher::load(base_path);

        for entry in WalkDir::new(base_path).into_iter().filter_map(|e| e.ok()) {
            if match_count >= limit {
                break;
            }

            if entry.file_type().is_dir() && is_skipped_dir(entry.path(), extra_skip_dirs) {
                continue;
            }
            if !entry.file_type().is_file() {
                continue;
            }
            if let Some(glob) = include
                && !simple_glob_match(glob, entry.path())
            {
                continue;
            }

            let path = entry.path().to_path_buf();

            // Skip gitignored files
            if let Ok(rel) = path.strip_prefix(base_path) {
                let rel_str = rel.to_string_lossy();
                if gitignore.is_ignored(&rel_str, false) {
                    continue;
                }
            }

            // Skip binary files: read first 8KB and check for null bytes
            if is_likely_binary_sync(&path) {
                continue;
            }

            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let lines: Vec<&str> = content.lines().collect();
            let relative = path.strip_prefix(base_path).unwrap_or(&path);

            for (line_idx, line) in lines.iter().enumerate() {
                if re.is_match(line) {
                    let line_num = line_idx + 1;

                    let mut context_lines = Vec::new();
                    let start = line_num.saturating_sub(context).max(1);
                    let end = (line_num + context).min(lines.len());

                    for ctx_line_num in start..=end {
                        let marker = if ctx_line_num == line_num { ">>>" } else { " " };
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

            // Safety limit: stop scanning after too many files
            if results.len() >= MAX_FILE_ENTRIES {
                break;
            }
        }
    } else {
        // Single file mode
        let content = match std::fs::read_to_string(base_path) {
            Ok(c) => c,
            Err(e) => {
                // Return error via an error result — the caller formats a friendly message
                results.push(format!("[error] Cannot read file: {}", e));
                return (0, results);
            }
        };

        let lines: Vec<&str> = content.lines().collect();

        for (line_idx, line) in lines.iter().enumerate() {
            if re.is_match(line) {
                let line_num = line_idx + 1;

                let mut context_lines = Vec::new();
                let start = line_num.saturating_sub(context).max(1);
                let end = (line_num + context).min(lines.len());

                for ctx_line_num in start..=end {
                    let marker = if ctx_line_num == line_num { ">>>" } else { " " };
                    context_lines.push(format!(
                        "{} {:>4} | {}",
                        marker,
                        ctx_line_num,
                        lines[ctx_line_num - 1]
                    ));
                }

                results.push(format!(
                    "{}:{}\n{}",
                    base_path.display(),
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

    (match_count, results)
}

fn simple_glob_match(pattern: &str, path: &Path) -> bool {
    let file_name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => return false,
    };
    if pattern.starts_with('*') && pattern.contains('.') && !pattern.contains('/') {
        let ext = &pattern[1..];
        file_name.ends_with(ext)
    } else {
        file_name.contains(pattern)
    }
}

/// Built-in directories that are commonly large, vendored, or not useful to search.
const DEFAULT_SKIPPED_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "vendor",
    ".venv",
    "venv",
    "__pycache__",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    "dist",
    "build",
    ".next",
    ".nuxt",
    ".cache",
];

/// Check if a directory should be skipped during traversal.
fn is_skipped_dir(path: &Path, extra_skip_dirs: &[String]) -> bool {
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        DEFAULT_SKIPPED_DIRS.contains(&name) || extra_skip_dirs.iter().any(|d| d == name)
    } else {
        false
    }
}

/// Synchronous version — safe to call from a blocking thread.
/// Check if a file is likely binary by reading the first 8KB and looking for null bytes.
fn is_likely_binary_sync(path: &Path) -> bool {
    use std::io::Read;
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut buf = [0u8; 8192];
    let n = match file.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return false,
    };
    buf[..n].contains(&0)
}
