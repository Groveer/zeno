#![allow(dead_code)]
//! Skill loading from directories.
//!
//! Scans directories for `<category>/<name>/SKILL.md` files, parses YAML frontmatter
//! for name, description, and conditional fields, and returns SkillDefinition objects.
//!
//! Supported layout:
//! ```text
//! skills/
//! ├── software-development/
//! │   ├── DESCRIPTION.md          # optional category description
//! │   ├── coding-principles/
//! │   │   └── SKILL.md
//! │   └── tdd/
//! │       └── SKILL.md
//! └── research/
//!     ├── DESCRIPTION.md
//!     └── arxiv/
//!         └── SKILL.md
//! ```

use std::path::{Path, PathBuf};

use indexmap::IndexMap;

use crate::skills::types::{CategoryInfo, SkillDefinition};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Load skills from one or more directories with category support.
///
/// Two-level directory structure: `<skills_dir>/<category>/<skill_name>/SKILL.md`.
/// If a directory doesn't have the two-level structure, it falls back to treating
/// the first-level subdirectories as skills (flat mode, category = "general").
pub fn load_skills_from_dirs(
    directories: &[PathBuf],
    source: &str,
) -> (Vec<SkillDefinition>, IndexMap<String, CategoryInfo>) {
    let mut skills = Vec::new();
    let mut categories: IndexMap<String, CategoryInfo> = IndexMap::new();
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    for directory in directories {
        let root = directory.clone();
        if !root.exists() {
            continue;
        }

        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };

        // First pass: collect category-level descriptions
        let mut category_dirs: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            let child = entry.path();
            if child.is_dir() {
                // Check if this is a category directory (contains SKILL.md subdirs)
                if is_category_dir(&child) {
                    category_dirs.push(child);
                } else {
                    // Could be a flat skill directory — handle later
                    category_dirs.push(child);
                }
            }
        }
        category_dirs.sort();

        // Load category descriptions from DESCRIPTION.md
        for cat_dir in &category_dirs {
            let desc_path = cat_dir.join("DESCRIPTION.md");
            if desc_path.exists()
                && let Ok(content) = std::fs::read_to_string(&desc_path)
            {
                let desc = parse_description_md(&content);
                let cat_name = cat_dir
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("general")
                    .to_string();
                categories.insert(
                    cat_name.clone(),
                    CategoryInfo {
                        description: desc,
                        skill_names: Vec::new(),
                    },
                );
            }
        }

        // Second pass: load skills
        for cat_dir in &category_dirs {
            let cat_name = cat_dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("general")
                .to_string();

            // Check if this is a category with subdirectories containing SKILL.md
            let has_skill_subdirs = has_skill_subdirs(cat_dir);

            if has_skill_subdirs {
                // Category mode: cat_dir/<skill_name>/SKILL.md
                load_skills_from_category(
                    cat_dir,
                    &cat_name,
                    source,
                    &mut skills,
                    &mut categories,
                    &mut seen,
                );
            } else {
                // Flat mode: cat_dir itself might be a skill directory
                let skill_path = cat_dir.join("SKILL.md");
                if skill_path.exists() && !seen.contains(&skill_path) {
                    seen.insert(skill_path.clone());
                    let default_name = cat_dir
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown")
                        .to_string();

                    if let Ok(content) = std::fs::read_to_string(&skill_path) {
                        let (name, description, always_inject) =
                            parse_skill_markdown(&default_name, &content);

                        skills.push(SkillDefinition {
                            name,
                            description,
                            content: if always_inject {
                                content
                            } else {
                                String::new()
                            },
                            source: source.to_string(),
                            path: Some(skill_path.display().to_string()),
                            category: "general".into(),
                            always_inject,
                        });
                    }
                }
            }
        }
    }

    (skills, categories)
}

/// Get the default user skills directory: ~/.config/zeno/skills/
pub fn get_user_skills_dir() -> PathBuf {
    crate::config::paths::config_dir().join("skills")
}

/// Get the project-level skills directories: .claude/skills/ or .agents/skills/
pub fn get_project_skills_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    // .claude/skills/
    let claude_dir = cwd.join(".claude").join("skills");
    if claude_dir.is_dir() {
        dirs.push(claude_dir);
    }

    // .agents/skills/
    let agents_dir = cwd.join(".agents").join("skills");
    if agents_dir.is_dir() {
        dirs.push(agents_dir);
    }

    dirs
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Check if a directory is a category directory (contains subdirs with SKILL.md).
fn is_category_dir(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries
        .flatten()
        .any(|e| e.path().is_dir() && e.path().join("SKILL.md").exists())
}

/// Check if a directory has subdirectories containing SKILL.md.
fn has_skill_subdirs(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .any(|e| e.path().join("SKILL.md").exists())
}

/// Load skills from a category directory.
fn load_skills_from_category(
    cat_dir: &Path,
    category: &str,
    source: &str,
    skills: &mut Vec<SkillDefinition>,
    categories: &mut IndexMap<String, CategoryInfo>,
    seen: &mut std::collections::HashSet<PathBuf>,
) {
    let Ok(entries) = std::fs::read_dir(cat_dir) else {
        return;
    };

    let mut candidates: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let child = entry.path();
        if child.is_dir() {
            let skill_path = child.join("SKILL.md");
            if skill_path.exists() {
                candidates.push(skill_path);
            }
        }
    }
    candidates.sort();

    for path in candidates {
        if seen.contains(&path) {
            continue;
        }
        seen.insert(path.clone());

        let default_name = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(path = %path.display(), error = %e, "Failed to read skill file");
                continue;
            }
        };

        let (name, description, always_inject) = parse_skill_markdown(&default_name, &content);

        // Update category info
        let cat_entry = categories
            .entry(category.to_string())
            .or_insert_with(|| CategoryInfo {
                description: String::new(),
                skill_names: Vec::new(),
            });
        cat_entry.skill_names.push(name.clone());

        skills.push(SkillDefinition {
            name,
            description,
            content: if always_inject {
                content
            } else {
                String::new()
            },
            source: source.to_string(),
            path: Some(path.display().to_string()),
            category: category.to_string(),
            always_inject,
        });
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse a DESCRIPTION.md file to extract the category description.
fn parse_description_md(content: &str) -> String {
    if let Some(rest) = content.strip_prefix("---\n") {
        // Find closing ---: either "\n---\n" or "\n---" at end of string
        let end_index = rest.find("\n---\n").or_else(|| {
            if rest.ends_with("\n---") {
                Some(rest.len() - 4)
            } else {
                None
            }
        });
        if let Some(end_index) = end_index {
            let frontmatter = &rest[..end_index];
            if let Ok(metadata) = serde_yaml::from_str::<serde_yaml::Value>(frontmatter)
                && let Some(d) = metadata.get("description").and_then(|v| v.as_str())
            {
                let trimmed = d.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }
        }
    }
    String::new()
}

/// Parse a SKILL.md: name, description, always_inject.
///
/// Supported frontmatter fields:
/// - `name` (string): Skill name override
/// - `description` (string): Short description
/// - `always_inject` (bool): When true, content is loaded at startup for system prompt injection
///
/// Returns `(name, description, always_inject)`.
pub fn parse_skill_markdown(default_name: &str, content: &str) -> (String, String, bool) {
    let mut name = default_name.to_string();
    let mut description = String::new();
    let mut always_inject = false;

    // Try YAML frontmatter
    if content.starts_with("---\n")
        && let Some(end_index) = content[4..].find("\n---\n")
    {
        let frontmatter = &content[4..4 + end_index];
        if let Ok(metadata) = serde_yaml::from_str::<serde_yaml::Value>(frontmatter) {
            // Name
            if let Some(n) = metadata.get("name").and_then(|v| v.as_str()) {
                let trimmed = n.trim();
                if !trimmed.is_empty() {
                    name = trimmed.to_string();
                }
            }
            // Description
            if let Some(d) = metadata.get("description").and_then(|v| v.as_str()) {
                let trimmed = d.trim();
                if !trimmed.is_empty() {
                    description = trimmed.to_string();
                }
            }
            // Always-inject flag (top-level frontmatter field)
            if let Some(v) = metadata.get("always_inject") {
                if let Some(b) = v.as_bool() {
                    always_inject = b;
                } else if let Some(s) = v.as_str() {
                    always_inject = s.trim().eq_ignore_ascii_case("true");
                }
            }
        }
    }

    // Fallback description: first non-heading, non-frontmatter line
    if description.is_empty() {
        for line in content.lines() {
            let stripped = line.trim();
            if let Some(heading) = stripped.strip_prefix("# ") {
                if name == default_name {
                    let heading = heading.trim();
                    if !heading.is_empty() {
                        name = heading.to_string();
                    }
                }
                continue;
            }
            if !stripped.is_empty() && !stripped.starts_with("---") && !stripped.starts_with("#") {
                description = truncate_description(stripped);
                break;
            }
        }
    }

    if description.is_empty() {
        description = format!("Skill: {}", name);
    }

    (name, description, always_inject)
}

/// Truncate a description to the first sentence (or 120 chars), whichever is shorter.
fn truncate_description(desc: &str) -> String {
    if let Some(idx) = desc.find(". ") {
        let first = &desc[..idx + 1];
        if first.len() <= 120 {
            return first.to_string();
        }
    }
    if desc.ends_with('.') {
        let trimmed = desc.trim_end_matches('.');
        if trimmed.len() <= 120 {
            return desc.to_string();
        }
    }
    if desc.len() > 120 {
        format!("{}...", &desc[..117])
    } else {
        desc.to_string()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_frontmatter_basic() {
        let content =
            "---\nname: my-skill\ndescription: A test skill\n---\n# My Skill\nContent here";
        let (name, desc, always_inject) = parse_skill_markdown("default", content);
        assert_eq!(name, "my-skill");
        assert_eq!(desc, "A test skill");
        assert!(!always_inject);
    }

    #[test]
    fn test_parse_always_inject_true() {
        let content = "---\nname: core-skill\nalways_inject: true\n---\n# Core\nContent";
        let (name, _, always_inject) = parse_skill_markdown("default", content);
        assert_eq!(name, "core-skill");
        assert!(always_inject);
    }

    #[test]
    fn test_parse_always_inject_false_by_default() {
        let content = "---\nname: normal-skill\ndescription: A skill\n---\n# Normal\nContent";
        let (_, _, always_inject) = parse_skill_markdown("default", content);
        assert!(!always_inject);
    }

    #[test]
    fn test_parse_no_frontmatter() {
        let content = "# TDD Guide\nWrite tests first, then code.";
        let (name, desc, _) = parse_skill_markdown("default-name", content);
        assert_eq!(name, "TDD Guide");
        assert!(desc.contains("Write tests"));
    }

    #[test]
    fn test_parse_empty_frontmatter() {
        let content = "---\n---\n# Hello\nSome content";
        let (name, _, _) = parse_skill_markdown("fallback", content);
        assert_eq!(name, "Hello");
    }

    #[test]
    fn test_parse_invalid_yaml() {
        let content = "---\n: invalid yaml : [\n---\n# Fallback\nDescription here";
        let (name, _, _) = parse_skill_markdown("default", content);
        assert!(!name.is_empty());
    }

    #[test]
    fn test_parse_legacy_compat() {
        let content =
            "---\nname: my-skill\ndescription: A test skill\n---\n# My Skill\nContent here";
        let (name, desc, _) = parse_skill_markdown("default", content);
        assert_eq!(name, "my-skill");
        assert_eq!(desc, "A test skill");
    }

    #[test]
    fn test_parse_description_md() {
        let content = "---\ndescription: Coding, debugging, testing workflows\n---";
        let desc = parse_description_md(content);
        assert_eq!(desc, "Coding, debugging, testing workflows");
    }

    #[test]
    fn test_truncate_description_short() {
        assert_eq!(
            truncate_description("Read file contents."),
            "Read file contents."
        );
    }

    #[test]
    fn test_truncate_description_two_sentences() {
        assert_eq!(
            truncate_description("Read a file. Supports offset and limit parameters."),
            "Read a file."
        );
    }

    #[test]
    fn test_truncate_description_long() {
        let long = "A".repeat(200);
        let result = truncate_description(&long);
        assert!(result.len() <= 123);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_load_skills_from_dirs_flat() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: Test skill\n---\n# My Skill",
        )
        .unwrap();

        let (skills, categories) = load_skills_from_dirs(&[dir.path().to_path_buf()], "test");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "my-skill");
        assert_eq!(skills[0].source, "test");
        assert_eq!(skills[0].category, "general");
        // Flat mode — no categories
        assert!(categories.is_empty());
    }

    #[test]
    fn test_load_skills_from_dirs_category() {
        let dir = tempfile::tempdir().unwrap();
        let cat_dir = dir.path().join("devops");
        let skill_dir = cat_dir.join("deploy");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: deploy\ndescription: Deployment\n---\n# Deploy",
        )
        .unwrap();

        let (skills, categories) = load_skills_from_dirs(&[dir.path().to_path_buf()], "test");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].category, "devops");
        assert!(categories.contains_key("devops"));
        assert_eq!(categories["devops"].skill_names, vec!["deploy"]);
    }

    #[test]
    fn test_load_skills_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let (skills, categories) = load_skills_from_dirs(&[dir.path().to_path_buf()], "test");
        assert!(skills.is_empty());
        assert!(categories.is_empty());
    }

    #[test]
    fn test_load_skills_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\n---\n# Skill",
        )
        .unwrap();

        // Same directory twice — should dedup
        let (skills, _) = load_skills_from_dirs(
            &[dir.path().to_path_buf(), dir.path().to_path_buf()],
            "test",
        );
        assert_eq!(skills.len(), 1);
    }

    #[test]
    fn test_lazy_content_loading() {
        // always_inject=false → content should be empty (lazy)
        let dir = tempfile::tempdir().unwrap();
        let cat_dir = dir.path().join("general").join("lazy-skill");
        std::fs::create_dir_all(&cat_dir).unwrap();
        std::fs::write(
            cat_dir.join("SKILL.md"),
            "---\nname: lazy-skill\ndescription: Lazy loaded\n---\n# Full Content Here",
        )
        .unwrap();

        let (skills, _) = load_skills_from_dirs(&[dir.path().to_path_buf()], "test");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "lazy-skill");
        assert!(
            skills[0].content.is_empty(),
            "non-always_inject skill should have empty content"
        );
        assert!(!skills[0].always_inject);
    }

    #[test]
    fn test_always_inject_content_loaded() {
        // always_inject=true → content should be populated
        let dir = tempfile::tempdir().unwrap();
        let cat_dir = dir.path().join("builtin").join("core-skill");
        std::fs::create_dir_all(&cat_dir).unwrap();
        std::fs::write(
            cat_dir.join("SKILL.md"),
            "---\nname: core-skill\nalways_inject: true\n---\n# Core Content",
        )
        .unwrap();

        let (skills, _) = load_skills_from_dirs(&[dir.path().to_path_buf()], "test");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "core-skill");
        assert!(
            !skills[0].content.is_empty(),
            "always_inject skill should have content loaded"
        );
        assert!(skills[0].content.contains("Core Content"));
        assert!(skills[0].always_inject);
    }

    #[test]
    fn test_description_truncated_at_200() {
        let long_desc = "x".repeat(300);
        let content = format!("---\n---\n# Skill\n{}", long_desc);
        let (_, desc, _) = parse_skill_markdown("default", &content);
        assert!(desc.len() <= 123);
    }
}
