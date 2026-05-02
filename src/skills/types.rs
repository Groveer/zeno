//! Skill data types.
//!
//! A Skill is a markdown file (SKILL.md) with optional YAML frontmatter
//! that provides domain knowledge and behavioral guidelines to the LLM.
//! Skills are NOT tools — they are injected into the system prompt or
//! loaded on-demand via the `skill_list`/`skill_view` tools.
//!
//! Skills are organized in a category hierarchy:
//!
//! ```text
//! skills/
//! ├── category-a/
//! │   ├── DESCRIPTION.md
//! │   ├── skill-1/
//! │   │   └── SKILL.md
//! │   └── skill-2/
//! │       └── SKILL.md
//! └── category-b/
//!     └── skill-3/
//!         └── SKILL.md
//! ```

/// Information about a skill category — used for Tier 0 index.
#[derive(Debug, Clone)]
pub struct CategoryInfo {
    /// Category description (from DESCRIPTION.md frontmatter).
    pub description: String,
    /// All skill names belonging to this category.
    pub skill_names: Vec<String>,
}

/// A loaded skill definition.
#[derive(Debug, Clone)]
pub struct SkillDefinition {
    /// Unique skill name (from frontmatter or directory name).
    pub name: String,
    /// Short description (≤120 chars, truncated to first sentence).
    pub description: String,
    /// Full markdown content of the SKILL.md file. Empty for non-always_inject
    /// skills (loaded on demand via `skill_view`).
    pub content: String,
    /// Where this skill was loaded from: "bundled", "user", "project".
    pub source: String,
    /// Absolute path to the SKILL.md file (if available).
    pub path: Option<String>,

    // --- Category ---
    /// Category derived from directory hierarchy (e.g. "software-development").
    pub category: String,

    // --- Injection control ---
    /// When true, this skill's full content is injected into the system prompt
    /// when its tool dependencies are met. Used for core behavioral guidelines
    /// that must be present without manual `skill_view` calls.
    pub always_inject: bool,
}

impl SkillDefinition {
    /// Create a minimal SkillDefinition with default values.
    #[allow(dead_code)]
    pub fn new(
        name: String,
        description: String,
        content: String,
        source: String,
        path: Option<String>,
        category: String,
    ) -> Self {
        Self {
            name,
            description,
            content,
            source,
            path,
            category,
            always_inject: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_skill_definition_new() {
        let skill = SkillDefinition::new(
            "tdd".into(),
            "Test-driven development".into(),
            "# TDD\n...".into(),
            "user".into(),
            Some("/skills/tdd/SKILL.md".into()),
            "software-development".into(),
        );
        assert_eq!(skill.name, "tdd");
        assert_eq!(skill.category, "software-development");
        assert!(!skill.always_inject);
    }

    #[test]
    fn test_category_info() {
        let info = CategoryInfo {
            description: "Coding, debugging, testing".into(),
            skill_names: vec!["tdd".into(), "debugging".into()],
        };
        assert_eq!(info.skill_names.len(), 2);
    }
}
