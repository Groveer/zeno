//! Glob tool — find files by name pattern.
use super::base::{Tool, ToolContext, ToolError};
use async_trait::async_trait;
use serde_json::{Value, json};
use walkdir::WalkDir;

pub struct GlobTool;
impl GlobTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "glob",
                "description": "Find files matching a glob pattern. Supports * and ** wildcards.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Glob pattern (e.g. '**/*.rs', 'src/**/*.py')."
                        },
                        "path": {
                            "type": "string",
                            "description": "Base directory (default: cwd)."
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
        let limit = arguments
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(50) as usize;

        let base_dir = match arguments.get("path").and_then(|v| v.as_str()) {
            Some(p) => ctx.resolve_path(p),
            None => ctx.cwd.clone(),
        };

        if !base_dir.exists() {
            return Err(ToolError::NotFound(format!(
                "Directory not found: {}",
                base_dir.display()
            )));
        }

        let pattern_owned = pattern.to_string();
        let base_dir_display = base_dir.display().to_string();

        // Offload blocking filesystem traversal to tokio's blocking thread pool
        // so we don't starve the async worker threads.
        let matches =
            tokio::task::spawn_blocking(move || glob_sync(&base_dir, &pattern_owned, limit))
                .await
                .map_err(|e| ToolError::Execution(format!("Task join error: {}", e)))?;

        if matches.is_empty() {
            return Ok(format!(
                "No files matching '{}' in {}",
                pattern, base_dir_display
            ));
        }

        Ok(format!(
            "Found {} file(s):\n{}",
            matches.len(),
            matches.join("\n")
        ))
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }
}

/// Directories that are commonly large, vendored, or not useful to search.
const SKIPPED_DIRS: &[&str] = &[
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

fn is_skipped_glob_dir(name: &str) -> bool {
    SKIPPED_DIRS.contains(&name)
}

/// Synchronous glob implementation — safe to run on a blocking thread.
fn glob_sync(base_dir: &std::path::Path, pattern: &str, limit: usize) -> Vec<String> {
    let has_doublestar = pattern.contains("**");
    let mut matches = Vec::new();

    for entry in WalkDir::new(base_dir)
        .max_depth(if has_doublestar { 30 } else { 3 })
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if matches.len() >= limit {
            break;
        }
        // Skip common large/vendored directories
        if entry.file_type().is_dir()
            && let Some(name) = entry.file_name().to_str()
            && is_skipped_glob_dir(name)
        {
            continue;
        }
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let relative = path.strip_prefix(base_dir).unwrap_or(path);
        let rel_str = relative.to_string_lossy();
        if glob_matches(pattern, &rel_str) {
            matches.push(format!("{}", relative.display()));
        }
    }

    matches
}

fn glob_matches(pattern: &str, path: &str) -> bool {
    let pattern_parts: Vec<&str> = pattern.split('/').collect();
    let path_parts: Vec<&str> = path.split('/').collect();
    glob_match_parts(&pattern_parts, &path_parts)
}

fn glob_match_parts(pattern: &[&str], path: &[&str]) -> bool {
    if pattern.is_empty() && path.is_empty() {
        return true;
    }
    if pattern.is_empty() {
        return false;
    }
    let pat = pattern[0];
    if pat == "**" {
        for i in 0..=path.len() {
            if glob_match_parts(&pattern[1..], &path[i..]) {
                return true;
            }
        }
        return false;
    }
    if path.is_empty() {
        return false;
    }
    if simple_match(pat, path[0]) {
        glob_match_parts(&pattern[1..], &path[1..])
    } else {
        false
    }
}

/// Match a simple glob pattern against a filename segment.
/// Supports `*` (any chars except `/`) and `?` (single char).
/// Uses O(n*m) two-pointer algorithm with backtracking — no recursion.
fn simple_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();

    let mut pi = 0usize;
    let mut ti = 0usize;
    let mut star_pi = usize::MAX; // position of last '*' in pattern
    let mut star_ti = 0; // text position when last '*' was matched

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            // Exact match or wildcard '?'
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            // Record star position; try matching 0 chars first
            star_pi = pi;
            star_ti = ti;
            pi += 1;
        } else if star_pi != usize::MAX {
            // Mismatch, but we have a previous '*' to backtrack to.
            // Let the '*' consume one more character.
            pi = star_pi + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            // No match and no '*' to backtrack to
            return false;
        }
    }

    // Remaining pattern chars must all be '*'
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }

    pi == p.len()
}
