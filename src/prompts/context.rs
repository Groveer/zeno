//! Runtime context injection — cwd, environment info, git status.
//!
//! Gathers dynamic information about the current session environment
//! and formats it for system prompt injection.

use std::path::Path;

/// Collected runtime context information.
pub struct RuntimeContext {
    pub os: String,
    pub shell: Option<String>,
    pub git_branch: Option<String>,
    pub build_system: Option<String>,
}

impl RuntimeContext {
    /// Collect runtime context from the current environment.
    pub fn collect(cwd: &Path) -> Self {
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
        let build_system = detect_build_system(cwd);

        Self {
            os,
            shell,
            git_branch,
            build_system,
        }
    }

    /// Format as a concise block for system prompt injection.
    ///
    /// Shows `./` as the working directory so the LLM constructs correct
    /// relative paths rather than accidentally doubling path components
    /// when the cwd name matches a subdirectory name.
    pub fn to_prompt_block(&self) -> String {
        let mut lines = Vec::new();
        // Show ./ as the working directory so the LLM constructs correct
        // relative paths (e.g. config.yaml) rather than ./data/config.yaml
        // when the cwd already ends with the directory name.
        lines.push("- Working directory: ./ (use relative paths, e.g. src/file.rs)".to_string());
        lines.push(format!("- OS: {}", self.os));
        if let Some(ref shell) = self.shell {
            lines.push(format!("- Shell: {}", shell));
        }
        if let Some(ref branch) = self.git_branch {
            lines.push(format!("- Git branch: {}", branch));
        }
        if let Some(ref bs) = self.build_system {
            lines.push(format!("- Project build system: {}", bs));
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

/// Detect project build system by checking for common build/config files
/// in the working directory. Returns a human-readable label like "Rust (Cargo)"
/// or "Node.js (npm)" when a recognized build file is found.
fn detect_build_system(dir: &Path) -> Option<String> {
    let build_files: &[(&str, &str)] = &[
        ("Cargo.toml", "Rust (Cargo)"),
        ("package.json", "Node.js (npm/pnpm/yarn)"),
        ("pyproject.toml", "Python (PEP 621)"),
        ("setup.py", "Python (setuptools)"),
        ("requirements.txt", "Python (pip)"),
        ("go.mod", "Go (modules)"),
        ("CMakeLists.txt", "C/C++ (CMake)"),
        ("pom.xml", "Java (Maven)"),
        ("build.gradle", "Java (Gradle)"),
        ("build.gradle.kts", "Kotlin (Gradle)"),
        ("Gemfile", "Ruby (Bundler)"),
        ("composer.json", "PHP (Composer)"),
        ("mix.exs", "Elixir (Mix)"),
        ("project.clj", "Clojure (Leiningen)"),
        ("Cargo.lock", "Rust (Cargo)"),
        ("Makefile", "Make"),
        ("dune-project", "OCaml (Dune)"),
        ("Cabal", "Haskell (Cabal)"),
        ("stack.yaml", "Haskell (Stack)"),
        ("pubspec.yaml", "Dart/Flutter"),
        ("*.csproj", "C# (.NET)"),
        ("DESCRIPTION", "R package"),
        ("configure.ac", "Autotools"),
        ("meson.build", "Meson"),
        ("BUCK", "Buck"),
        ("BUILD", "Bazel"),
        ("WORKSPACE", "Bazel workspace"),
        ("justfile", "Just (command runner)"),
    ];

    // Check for exact filenames first
    for (filename, label) in build_files {
        if !filename.contains('*') && dir.join(filename).exists() {
            return Some(label.to_string());
        }
    }

    // Check for glob patterns (e.g. *.csproj)
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                for (filename, label) in build_files {
                    if filename.contains('*') && name.ends_with(&filename[1..]) {
                        return Some(label.to_string());
                    }
                }
            }
        }
    }

    None
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
            os: "Linux".into(),
            shell: Some("/bin/bash".into()),
            git_branch: Some("main".into()),
            build_system: Some("Rust (Cargo)".into()),
        };
        let block = ctx.to_prompt_block();
        assert!(block.contains("Working directory: ./ "));
        assert!(block.contains("use relative paths"));
        assert!(block.contains("OS: Linux"));
        assert!(block.contains("Shell: /bin/bash"));
        assert!(block.contains("Git branch: main"));
        assert!(block.contains("Project build system: Rust (Cargo)"));
    }

    #[test]
    fn test_context_no_shell_no_git() {
        let ctx = RuntimeContext {
            os: "Linux".into(),
            shell: None,
            git_branch: None,
            build_system: None,
        };
        let block = ctx.to_prompt_block();
        assert!(!block.contains("Shell"));
        assert!(!block.contains("Git"));
        assert!(!block.contains("Project build system"));
    }

    #[test]
    fn test_detect_build_system_cargo() {
        let dir = std::env::temp_dir().join("_test_zeno_build_sys");
        std::fs::create_dir_all(&dir).unwrap();
        // Should be None with no build file
        assert!(detect_build_system(&dir).is_none());
        // Add Cargo.toml
        std::fs::write(dir.join("Cargo.toml"), "").unwrap();
        assert_eq!(detect_build_system(&dir).as_deref(), Some("Rust (Cargo)"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_detect_build_system_package_json() {
        let dir = std::env::temp_dir().join("_test_zeno_build_sys2");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("package.json"), "{}").unwrap();
        assert_eq!(
            detect_build_system(&dir).as_deref(),
            Some("Node.js (npm/pnpm/yarn)")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_detect_build_system_none() {
        let dir = std::env::temp_dir().join("_test_zeno_build_sys3");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(detect_build_system(&dir).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
