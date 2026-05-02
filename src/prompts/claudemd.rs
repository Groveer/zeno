//! CLAUDE.md / AGENTS.md parser — load project-level instructions.
//!
//! Searches upward from cwd for CLAUDE.md or AGENTS.md files
//! and returns their content for injection into the system prompt.

use std::path::{Path, PathBuf};

/// Filenames to search for, in priority order.
const INSTRUCTION_FILES: &[&str] = &["CLAUDE.md", "AGENTS.md"];

/// Find and read the first instruction file found by walking upward from `start_dir`.
/// Returns `Some((path, content))` if found, `None` otherwise.
pub fn load_instruction_file(start_dir: &Path) -> Option<(PathBuf, String)> {
    for dir in ancestors(start_dir) {
        for filename in INSTRUCTION_FILES {
            let candidate = dir.join(filename);
            if candidate.is_file() {
                match std::fs::read_to_string(&candidate) {
                    Ok(content) if !content.trim().is_empty() => {
                        tracing::debug!(path = %candidate.display(), "Loaded instruction file");
                        return Some((candidate, content));
                    }
                    Ok(_) => {
                        tracing::debug!(path = %candidate.display(), "Instruction file is empty");
                    }
                    Err(e) => {
                        tracing::debug!(path = %candidate.display(), error = %e, "Failed to read instruction file");
                    }
                }
            }
        }
    }
    None
}

/// Generate ancestor iterator: start_dir, parent, grandparent, ... up to root.
fn ancestors(path: &Path) -> impl Iterator<Item = &Path> {
    std::iter::successors(Some(path), |p| p.parent())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_ancestors() {
        let path = Path::new("/home/user/project/src");
        let dirs: Vec<&Path> = ancestors(path).collect();
        assert_eq!(dirs[0], Path::new("/home/user/project/src"));
        assert_eq!(dirs[1], Path::new("/home/user/project"));
        assert_eq!(dirs[2], Path::new("/home/user"));
    }

    #[test]
    fn test_load_instruction_file_found() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("CLAUDE.md");
        fs::write(&file_path, "# Project Rules\nUse Rust style.").unwrap();

        let result = load_instruction_file(dir.path());
        assert!(result.is_some());
        let (path, content) = result.unwrap();
        assert_eq!(path, file_path);
        assert!(content.contains("Rust style"));
    }

    #[test]
    fn test_load_instruction_file_agents_md() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("AGENTS.md");
        fs::write(&file_path, "# Agent Instructions").unwrap();

        let result = load_instruction_file(dir.path());
        assert!(result.is_some());
        assert!(result.unwrap().1.contains("Agent Instructions"));
    }

    #[test]
    fn test_load_instruction_file_claude_md_priority() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("CLAUDE.md"), "CLAUDE content").unwrap();
        fs::write(dir.path().join("AGENTS.md"), "AGENTS content").unwrap();

        let result = load_instruction_file(dir.path());
        assert!(result.is_some());
        assert_eq!(result.unwrap().1, "CLAUDE content");
    }

    #[test]
    fn test_load_instruction_file_not_found() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_instruction_file(dir.path()).is_none());
    }

    #[test]
    fn test_load_instruction_file_empty() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("CLAUDE.md"), "   \n  ").unwrap();
        assert!(load_instruction_file(dir.path()).is_none());
    }

    #[test]
    fn test_load_instruction_file_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("src");
        fs::create_dir_all(&child).unwrap();
        fs::write(dir.path().join("CLAUDE.md"), "Parent rules").unwrap();

        let result = load_instruction_file(&child);
        assert!(result.is_some());
        assert!(result.unwrap().1.contains("Parent rules"));
    }
}
