//! Runtime context injection — cwd, environment info, git status.
//!
//! Gathers dynamic information about the current session environment
//! and formats it for system prompt injection.

use std::path::Path;

/// Collected runtime context information.
pub struct RuntimeContext {
    pub cwd: String,
    pub os: String,
    pub shell: Option<String>,
    pub git_branch: Option<String>,
}

impl RuntimeContext {
    /// Collect runtime context from the current environment.
    pub fn collect(cwd: &Path) -> Self {
        let cwd_str = cwd.display().to_string();

        let os = if cfg!(target_os = "linux") {
            "Linux".to_string()
        } else if cfg!(target_os = "macos") {
            "macOS".to_string()
        } else if cfg!(target_os = "windows") {
            "Windows".to_string()
        } else {
            "Unknown".to_string()
        };

        let shell = std::env::var("SHELL")
            .ok()
            .or_else(|| std::env::var("ComSpec").ok());

        let git_branch = detect_git_branch(cwd);

        Self {
            cwd: cwd_str,
            os,
            shell,
            git_branch,
        }
    }

    /// Format as a concise block for system prompt injection.
    pub fn to_prompt_block(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!("- Working directory: {}", self.cwd));
        lines.push(format!("- OS: {}", self.os));
        if let Some(ref shell) = self.shell {
            lines.push(format!("- Shell: {}", shell));
        }
        if let Some(ref branch) = self.git_branch {
            lines.push(format!("- Git branch: {}", branch));
        }
        lines.join("\n")
    }
}

/// Detect the current git branch by reading .git/HEAD.
fn detect_git_branch(dir: &Path) -> Option<String> {
    let git_head = find_git_dir(dir)?.join("HEAD");
    let content = std::fs::read_to_string(&git_head).ok()?;
    // Typical content: "ref: refs/heads/main\n" or a detached SHA
    if let Some(ref_path) = content.strip_prefix("ref: refs/heads/") {
        Some(ref_path.trim().to_string())
    } else {
        // Detached HEAD — show abbreviated SHA
        let sha = content.trim();
        if sha.len() >= 8 {
            Some(format!("{} (detached)", &sha[..8]))
        } else {
            None
        }
    }
}

/// Walk upward to find a .git directory.
fn find_git_dir(start: &Path) -> Option<std::path::PathBuf> {
    for dir in std::iter::successors(Some(start), |p| p.parent()) {
        let git_dir = dir.join(".git");
        if git_dir.exists() {
            return Some(git_dir);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_format() {
        let ctx = RuntimeContext {
            cwd: "/home/user/project".into(),
            os: "Linux".into(),
            shell: Some("/bin/bash".into()),
            git_branch: Some("main".into()),
        };
        let block = ctx.to_prompt_block();
        assert!(block.contains("Working directory: /home/user/project"));
        assert!(block.contains("OS: Linux"));
        assert!(block.contains("Shell: /bin/bash"));
        assert!(block.contains("Git branch: main"));
    }

    #[test]
    fn test_context_no_shell_no_git() {
        let ctx = RuntimeContext {
            cwd: "/tmp".into(),
            os: "Linux".into(),
            shell: None,
            git_branch: None,
        };
        let block = ctx.to_prompt_block();
        assert!(!block.contains("Shell"));
        assert!(!block.contains("Git"));
    }
}
