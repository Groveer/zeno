//! Gitignore-aware file filtering — loads `.gitignore` patterns and
//! checks whether a path should be excluded from search results.
//!
//! # Design
//!
//! Scans upward from a base directory to find `.gitignore` files, loads them
//! into a simple pattern matcher, and provides an `is_ignored()` check.
//! Also reads `.git/info/exclude` if the repository root is found.
//!
//! Uses `ignore` crate patterns (simplified: `*` wildcard, `!` negation,
//! trailing `/` for directories, leading `/` for root-anchored).
//! This is intentionally simpler than full gitignore parsing — it handles
//! the common cases that trip up glob/grep results.

use std::path::{Path, PathBuf};

/// A set of gitignore patterns loaded from `.gitignore` files.
#[derive(Debug, Clone, Default)]
pub struct GitIgnoreMatcher {
    /// Patterns that match ignored files.
    patterns: Vec<IgnorePattern>,
}

#[derive(Debug, Clone)]
struct IgnorePattern {
    /// The raw pattern string (e.g. "*.log", "build/").
    raw: String,
    /// Whether the pattern is negated (prefixed with `!`).
    negated: bool,
    /// Whether the pattern is anchored to the root (prefixed with `/`).
    anchored: bool,
    /// Whether the pattern only matches directories (suffixed with `/`).
    dir_only: bool,
}

impl GitIgnoreMatcher {
    /// Load gitignore patterns by scanning upward from `base_dir`.
    ///
    /// Finds the nearest `.gitignore` in `base_dir` or any parent,
    /// plus `.git/info/exclude` if a `.git` directory is found.
    pub fn load(base_dir: &Path) -> Self {
        let mut matcher = Self::default();
        let mut git_dir: Option<PathBuf> = None;

        // Walk up from base_dir to root
        let mut current = Some(base_dir);
        while let Some(dir) = current {
            // Check for .gitignore
            let gitignore = dir.join(".gitignore");
            if gitignore.exists() {
                matcher.load_file(&gitignore);
            }
            // Check for .git directory (for info/exclude)
            let git_dir_candidate = dir.join(".git");
            if git_dir_candidate.is_dir() && git_dir.is_none() {
                git_dir = Some(git_dir_candidate);
            }
            current = dir.parent();
        }

        // Load .git/info/exclude if found
        if let Some(gd) = git_dir {
            let exclude = gd.join("info").join("exclude");
            if exclude.exists() {
                matcher.load_file(&exclude);
            }
        }

        matcher
    }

    /// Load patterns from a single `.gitignore`-format file.
    fn load_file(&mut self, path: &Path) {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return,
        };
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            self.add_pattern(trimmed);
        }
    }

    /// Add a single gitignore pattern.
    fn add_pattern(&mut self, pattern: &str) {
        let negated = pattern.starts_with('!');
        let mut raw = if negated { &pattern[1..] } else { pattern };

        // Strip leading `/` (root-anchored)
        let anchored = raw.starts_with('/');
        if anchored {
            raw = &raw[1..];
        }

        // Check trailing `/` (directory-only)
        let dir_only = raw.ends_with('/');
        if dir_only {
            raw = &raw[..raw.len() - 1];
        }

        self.patterns.push(IgnorePattern {
            raw: raw.to_string(),
            negated,
            anchored,
            dir_only,
        });
    }

    /// Check if a relative path (from the git root / search base) is ignored.
    ///
    /// `is_dir` indicates whether the path is a directory (for dir-only patterns).
    pub fn is_ignored(&self, rel_path: &str, is_dir: bool) -> bool {
        if self.patterns.is_empty() {
            return false;
        }

        let mut ignored = false;
        for p in &self.patterns {
            if p.dir_only && !is_dir {
                // Path is inside a matched directory (e.g. "build/" matches "build/output.o")
                if rel_path.starts_with(&format!("{}/", p.raw)) {
                    ignored = !p.negated;
                }
                continue;
            }

            let path_to_match = if p.anchored {
                // Root-anchored: match from the beginning of the relative path
                rel_path.to_string()
            } else {
                // Unanchored: match the last component (filename)
                Path::new(rel_path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(rel_path)
                    .to_string()
            };

            if simple_gitignore_match(&p.raw, &path_to_match) {
                ignored = !p.negated;
            }
        }

        ignored
    }
}

/// Simple gitignore-style pattern matching.
///
/// Supports `*` (matches anything except `/`), `**` (matches everything),
/// and `?` (matches single char). This is intentionally simpler than
/// full gitignore — enough for common cases like `*.log`, `build/`, `*.pyc`.
fn simple_gitignore_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();

    let mut pi = 0usize;
    let mut ti = 0usize;
    let mut star_pi = usize::MAX;
    let mut star_ti = 0;

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            // Check for ** (matches across directories)
            if pi + 1 < p.len() && p[pi + 1] == '*' {
                // ** matches across directories
                pi += 2;
                // Skip trailing / so **/foo matches "foo" at root level
                if pi < p.len() && p[pi] == '/' {
                    pi += 1;
                }
                // If ** is at end, it matches everything
                if pi >= p.len() {
                    return true;
                }
                // Otherwise, try to match the rest at any position
                while ti <= t.len() {
                    if simple_gitignore_match(&pattern[pi..], &text[ti..]) {
                        return true;
                    }
                    ti += 1;
                }
                return false;
            }

            star_pi = pi;
            star_ti = ti;
            pi += 1;
        } else if star_pi != usize::MAX {
            pi = star_pi + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }

    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }

    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_pattern() {
        let mut m = GitIgnoreMatcher::default();
        m.add_pattern("*.log");
        assert!(m.is_ignored("debug.log", false));
        assert!(!m.is_ignored("debug.txt", false));
        assert!(!m.is_ignored("src/main.rs", false));
    }

    #[test]
    fn test_negated_pattern() {
        let mut m = GitIgnoreMatcher::default();
        m.add_pattern("*.log");
        m.add_pattern("!important.log");
        assert!(m.is_ignored("debug.log", false));
        assert!(!m.is_ignored("important.log", false));
    }

    #[test]
    fn test_directory_pattern() {
        let mut m = GitIgnoreMatcher::default();
        m.add_pattern("build/");
        assert!(m.is_ignored("build", true));
        assert!(!m.is_ignored("build", false));
        assert!(m.is_ignored("build/output.o", false)); // file inside build
    }

    #[test]
    fn test_anchored_pattern() {
        let mut m = GitIgnoreMatcher::default();
        m.add_pattern("/target");
        assert!(m.is_ignored("target", true));
        assert!(!m.is_ignored("src/target", true));
    }

    #[test]
    fn test_doublestar_pattern() {
        let mut m = GitIgnoreMatcher::default();
        m.add_pattern("**/__pycache__");
        assert!(m.is_ignored("__pycache__", true));
        assert!(m.is_ignored("src/__pycache__", true));
    }

    #[test]
    fn test_dot_git_is_always_ignored() {
        let mut m = GitIgnoreMatcher::default();
        m.add_pattern(".git");
        assert!(m.is_ignored(".git", true));
    }

    #[test]
    fn test_load_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let gitignore = dir.path().join(".gitignore");
        std::fs::write(&gitignore, "*.log\n!important.log\nbuild/\n").unwrap();

        let m = GitIgnoreMatcher::load(dir.path());
        assert!(m.is_ignored("debug.log", false));
        assert!(!m.is_ignored("important.log", false));
        assert!(m.is_ignored("build", true));
    }
}
