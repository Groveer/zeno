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
                "description": "Search file contents with regex. Returns matching lines with file paths and line numbers.\n\nHINT: Scope to source tree via `path` + `include` (e.g. path=\"src\", include=\"*.rs\"). Avoid searching the whole project root.",
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
    pattern: &str,
    include: Option<&str>,
    context: usize,
    limit: usize,
    extra_skip_dirs: &[String],
) -> (usize, Vec<String>) {
    // Max number of file entries to scan (prevent OOM on huge repos)
    const MAX_FILE_ENTRIES: usize = 10_000;
    /// Max matches per individual file — prevents a single hot file from
    /// consuming the entire result limit (inspired by RTK's per-file cap).
    const MAX_PER_FILE: usize = 10;
    /// Max displayed line length — very long lines (minified JSON, etc.)
    /// waste tokens. Truncate with a hint toward the matched region.
    const MAX_LINE_LEN: usize = 500;

    let mut results = Vec::new();
    let mut match_count = 0;
    // Track per-file match counts for the per-file cap
    let mut file_match_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

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
            let file_key = relative.to_string_lossy().to_string();
            let file_count = file_match_counts.entry(file_key.clone()).or_insert(0);

            for (line_idx, line) in lines.iter().enumerate() {
                if re.is_match(line) {
                    // Per-file cap
                    if *file_count >= MAX_PER_FILE {
                        break;
                    }

                    let line_num = line_idx + 1;

                    let mut context_lines = Vec::new();
                    let start = line_num.saturating_sub(context).max(1);
                    let end = (line_num + context).min(lines.len());

                    for ctx_line_num in start..=end {
                        let marker = if ctx_line_num == line_num { ">>>" } else { " " };
                        let display_line =
                            truncate_line(lines[ctx_line_num - 1], MAX_LINE_LEN, pattern);
                        context_lines
                            .push(format!("{} {:>4} | {}", marker, ctx_line_num, display_line));
                    }

                    results.push(format!(
                        "{}:{}\n{}",
                        compact_path(&file_key),
                        line_num,
                        context_lines.join("\n")
                    ));

                    match_count += 1;
                    *file_count += 1;
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
                    let display_line =
                        truncate_line(lines[ctx_line_num - 1], MAX_LINE_LEN, pattern);
                    context_lines
                        .push(format!("{} {:>4} | {}", marker, ctx_line_num, display_line));
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

#[cfg(test)]
mod tests {
    use super::*;

    // --- simple_glob_match ---

    #[test]
    fn test_simple_glob_match_star_dot_ext() {
        let p = Path::new("main.rs");
        assert!(simple_glob_match("*.rs", p));
        assert!(!simple_glob_match("*.py", p));
    }

    #[test]
    fn test_simple_glob_match_contains() {
        let p = Path::new("hello_world.rs");
        assert!(simple_glob_match("hello", p));
        assert!(!simple_glob_match("xyz", p));
    }

    #[test]
    fn test_simple_glob_match_no_extension() {
        let p = Path::new("Makefile");
        assert!(simple_glob_match("Makefile", p));
        assert!(!simple_glob_match("makefile", p));
    }

    #[test]
    fn test_simple_glob_match_empty() {
        let p = Path::new("foo.rs");
        assert!(simple_glob_match("", p)); // empty is contained in everything
    }

    // --- is_skipped_dir ---

    #[test]
    fn test_is_skipped_dir_default() {
        assert!(is_skipped_dir(Path::new(".git"), &[]));
        assert!(is_skipped_dir(Path::new("node_modules"), &[]));
    }

    #[test]
    fn test_is_skipped_dir_not() {
        assert!(!is_skipped_dir(Path::new("src"), &[]));
    }

    #[test]
    fn test_is_skipped_dir_extra() {
        assert!(is_skipped_dir(
            Path::new("my_build"),
            &["my_build".to_string()]
        ));
    }

    // --- is_likely_binary_sync ---

    #[test]
    fn test_is_likely_binary_text_file() {
        let dir = std::env::temp_dir();
        let path = dir.join("_test_zeno_grep_text.txt");
        std::fs::write(&path, b"hello world\n").unwrap();
        assert!(!is_likely_binary_sync(&path));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn test_is_likely_binary_with_null() {
        let dir = std::env::temp_dir();
        let path = dir.join("_test_zeno_grep_bin.bin");
        std::fs::write(&path, b"hello\x00world\n").unwrap();
        assert!(is_likely_binary_sync(&path));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn test_is_likely_binary_nonexistent() {
        assert!(!is_likely_binary_sync(Path::new("/nonexistent/path.bin")));
    }

    // --- grep_sync helpers (single file mode via the actual grep_sync) ---

    #[test]
    fn test_grep_sync_single_file_match() {
        let dir = std::env::temp_dir();
        let path = dir.join("_test_zeno_grep_sync.txt");
        std::fs::write(&path, "hello world\nfoo bar\nbaz qux\n").unwrap();
        let re = regex::Regex::new("foo").unwrap();
        let (count, results) = grep_sync(&path, &re, "foo", None, 0, 10, &[]);
        assert_eq!(count, 1);
        assert!(results[0].contains("foo bar"));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn test_grep_sync_single_file_no_match() {
        let dir = std::env::temp_dir();
        let path = dir.join("_test_zeno_grep_sync_none.txt");
        std::fs::write(&path, "hello world\n").unwrap();
        let re = regex::Regex::new("zzz").unwrap();
        let (count, results) = grep_sync(&path, &re, "zzz", None, 0, 10, &[]);
        assert_eq!(count, 0);
        assert!(results.is_empty());
        std::fs::remove_file(&path).unwrap();
    }

    // --- truncate_line ---

    #[test]
    fn test_truncate_line_short() {
        assert_eq!(truncate_line("hello world", 50, "hello"), "hello world");
    }

    #[test]
    fn test_truncate_line_exact() {
        let s = "a".repeat(500);
        assert_eq!(truncate_line(&s, 500, "a"), s);
    }

    #[test]
    fn test_truncate_line_long_with_match() {
        let line = "prefix ".to_string() + &"x".repeat(400) + " TARGET " + &"y".repeat(400);
        let result = truncate_line(&line, 500, "TARGET");
        assert!(result.len() <= 510); // allow some margin for "..." markers
        assert!(
            result.contains("TARGET"),
            "truncated line should contain the match"
        );
    }

    #[test]
    fn test_truncate_line_long_no_match() {
        let line = "z".repeat(1000);
        let result = truncate_line(&line, 500, "nomatch");
        assert!(result.len() <= 510);
        assert!(result.ends_with("..."));
    }

    // --- compact_path ---

    #[test]
    fn test_compact_path_short() {
        assert_eq!(compact_path("src/main.rs"), "src/main.rs");
        assert_eq!(compact_path("a/b/c.rs"), "a/b/c.rs");
    }

    #[test]
    fn test_compact_path_long() {
        let p = "very/long/deep/nested/path/to/components/buttons/primary/Button.tsx";
        let result = compact_path(p);
        assert_eq!(result, "very/.../primary/Button.tsx");
        assert!(result.len() < p.len());
    }

    #[test]
    fn test_compact_path_boundary() {
        // Exactly 60 chars should not be compacted
        let p = "a".repeat(60);
        assert_eq!(compact_path(&p), p);
    }
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

/// Truncate a line to `max_len` characters. If the line is too long, try to
/// center the truncation around the first occurrence of `pattern` so the
/// match context is preserved.
fn truncate_line(line: &str, max_len: usize, pattern: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= max_len {
        return line.to_string();
    }

    let lower_line: String = chars.iter().collect();
    let lower_line = lower_line.to_lowercase();
    let pattern_lower = pattern.to_lowercase();

    // Try to center around the match
    if let Some(pos) = lower_line.find(&pattern_lower) {
        let start = pos.saturating_sub(max_len / 3);
        let end = (start + max_len).min(chars.len());
        let start = if end == chars.len() {
            end.saturating_sub(max_len)
        } else {
            start
        };

        let slice: String = chars[start..end].iter().collect();
        if start > 0 && end < chars.len() {
            format!("...{}...", slice)
        } else if start > 0 {
            format!("...{}", slice)
        } else {
            format!("{}...", slice)
        }
    } else {
        // No match found in line — just truncate from the end
        let truncated: String = chars.iter().take(max_len - 3).collect();
        format!("{}...", truncated)
    }
}

/// Compact a file path for display. Long paths like
/// `src/components/buttons/primary/Button.tsx` become
/// `src/.../buttons/Button.tsx` to save tokens.
fn compact_path(path: &str) -> String {
    if path.len() <= 60 {
        return path.to_string();
    }
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() <= 3 {
        return path.to_string();
    }
    format!(
        "{}/.../{}/{}",
        parts[0],
        parts[parts.len() - 2],
        parts[parts.len() - 1]
    )
}
