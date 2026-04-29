//! Glob tool — find files by name pattern.

use async_trait::async_trait;
use serde_json::{json, Value};
use walkdir::WalkDir;

use super::base::{Tool, ToolContext, ToolError};

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
                            "description": "Base directory to search in (default: cwd)."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum results to return (default: 50).",
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

        // Simple glob matching: convert pattern to a regex-free walkdir filter
        // Support: *, **, ?
        let mut matches = Vec::new();
        let has_doublestar = pattern.contains("**");

        for entry in WalkDir::new(&base_dir)
            .max_depth(if has_doublestar { usize::MAX } else { 3 })
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if matches.len() >= limit {
                break;
            }
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            let relative = path
                .strip_prefix(&base_dir)
                .unwrap_or(path);
            let rel_str = relative.to_string_lossy();

            if glob_matches(pattern, &rel_str) {
                matches.push(format!("{}", relative.display()));
            }
        }

        if matches.is_empty() {
            return Ok(format!("No files matching '{}' in {}", pattern, base_dir.display()));
        }

        Ok(format!(
            "Found {} file(s):\n{}",
            matches.len(),
            matches.join("\n")
        ))
    }
}

/// Simple glob pattern matching (no regex dependency).
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
        // ** matches zero or more path segments
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

fn simple_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    simple_match_inner(&p, &t, 0, 0)
}

fn simple_match_inner(p: &[char], t: &[char], pi: usize, ti: usize) -> bool {
    if pi == p.len() && ti == t.len() {
        return true;
    }
    if pi == p.len() {
        return false;
    }
    match p[pi] {
        '*' => {
            // * matches any remaining characters in this segment
            for i in ti..=t.len() {
                if simple_match_inner(p, t, pi + 1, i) {
                    return true;
                }
            }
            false
        }
        '?' => {
            if ti < t.len() {
                simple_match_inner(p, t, pi + 1, ti + 1)
            } else {
                false
            }
        }
        c => {
            if ti < t.len() && t[ti] == c {
                simple_match_inner(p, t, pi + 1, ti + 1)
            } else {
                false
            }
        }
    }
}
