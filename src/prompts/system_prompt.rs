//! System prompt builder — assembles the full system prompt from components.
//!
//! The system prompt is built from:
//! 1. Core identity & role declaration (always present)
//! 2. Key principles (always present)
//! 3. Tool list with descriptions
//! 4. Skills Tier 0 category index
//! 5. Runtime context (cwd, OS, git branch)
//! 6. Project instructions (CLAUDE.md / AGENTS.md)
//!
//! All skills are loaded lazily via `skill_view`. The Tier 0 category index

use std::path::Path;

use crate::config::settings::RoleConfig;
use crate::prompts::claudemd;
use crate::prompts::context::RuntimeContext;
use crate::skills::registry::SkillRegistry;
use crate::tools::base::{ToolRegistry, ToolSummary};

/// Build the complete system prompt.
///
/// When `memory_block` is Some, it is injected as a section at the end,
/// after project instructions.
pub fn build(
    cwd: &Path,
    tool_registry: &ToolRegistry,
    skill_registry: &SkillRegistry,
    memory_block: Option<&str>,
    role_config: &RoleConfig,
) -> String {
    let mut parts = vec![
        core_identity(role_config),
        guidelines(role_config),
        tools_block(&tool_registry.summaries()),
    ];

    // 5. Skills Tier 0 category index + always-inject guidelines
    if !skill_registry.is_empty() {
        parts.push(skills_block(skill_registry));
    }

    // 6. Runtime context
    let ctx = RuntimeContext::collect(cwd);
    parts.push(format!("## Environment\n\n{}", ctx.to_prompt_block()));

    // 7. Project instructions (CLAUDE.md / AGENTS.md)
    if let Some((path, content)) = claudemd::load_instruction_file(cwd) {
        parts.push(format!(
            "## Project Instructions\n\nLoaded from: {}\n\n{}",
            path.display(),
            content.trim()
        ));
    }

    // 8. Memory (frozen snapshot from disk + external provider)
    if let Some(block) = memory_block
        && !block.is_empty()
    {
        parts.push(block.to_string());
    }

    parts.join("\n\n")
}

/// Core identity and role declaration — always present.
///
/// When the user provides a custom identity, it replaces the *identity portion*
/// (who you are), but the functional guidance (how to use Tools & Skills) is always
/// appended so the model never loses awareness of its capabilities.
fn core_identity(role: &RoleConfig) -> String {
    let identity_text = match role.identity {
        Some(ref custom) => custom.trim().to_string(),
        None => "You are zeno (zn), a helpful AI assistant.\n\n\
            You help users with a wide variety of tasks: answering questions, writing and editing text,\n\
            analyzing information, and more. When tools are available, use them proactively to assist the user."
            .to_string(),
    };

    // Functional guidance is always present — never overridden by custom identity.
    let functional = "\
**Tools** are executable capabilities (bash, read, etc.) that you call via function calling.\n\
**Skills** are knowledge guides organized by category. Use `skill_list` to browse a category\n\
and `skill_view` to load a specific skill's full instructions.";

    format!("{}\n\n{}", identity_text.trim(), functional)
}

/// Guidelines — always present.
fn guidelines(role: &RoleConfig) -> String {
    if let Some(ref custom) = role.guidelines {
        return format!("## Guidelines\n\n{}", custom.trim());
    }
    r#"
## Guidelines

- Be concise and direct. Prefer showing results over lengthy explanations.
- **Think aloud**: Before and between tool calls, briefly explain what you're looking for and what you've found so the user can follow your reasoning.
- Use tools proactively to read files, run commands, search information, and verify changes when needed.
- When the user is just chatting or asking a question — respond with text only, no tool calls.
- Follow the user's project conventions (CLAUDE.md / AGENTS.md) if present.
- **Batch independent tool calls**: Issue all independent calls in one response (e.g. `glob` + `grep` together). Only sequence calls with data dependencies.
- **Load skills for non-trivial tasks** (skip for greetings, simple questions): direct match → `skill_view` immediately; unknown → `skill_list` to browse → `skill_view`. Err on the side of loading.
"#
 .trim()
 .to_string()
}

/// Format the tool list as a readable block for the system prompt.
fn tools_block(summaries: &[ToolSummary]) -> String {
    if summaries.is_empty() {
        return "## Tools\n\n(No tools registered.)".to_string();
    }

    let mut lines = Vec::new();
    lines.push(format!("## Tools ({} available)\n", summaries.len()));
    lines.push("You have access to the following tools:\n".to_string());

    for s in summaries {
        let desc = truncate_description(&s.description);
        lines.push(format!("- **{}**: {}", s.name, desc));
    }

    lines.join("\n")
}

/// Format the skills section for the system prompt.
///
/// This includes:
/// 1. **Tier 0 category index**: Compact listing of categories with skill names.
fn skills_block(registry: &SkillRegistry) -> String {
    let mut parts = Vec::new();

    // Tier 0: Category index
    let categories = registry.categories();
    if categories.is_empty() {
        // Fallback for flat (non-categorized) skill layout
        let skills = registry.list_skills();
        if skills.is_empty() {
            return String::new();
        }
        let mut lines = Vec::new();
        lines.push(format!("## Skills ({} available)\n", skills.len()));
        lines.push(
            "Skills are knowledge guides you can load on demand. Use `skill_list` to browse, `skill_view` to load.\n"
                .to_string(),
        );
        for s in &skills {
            let desc = truncate_description(&s.description);
            lines.push(format!("- **{}**: {}", s.name, desc));
        }
        return lines.join("\n");
    }

    let mut lines = Vec::new();
    lines.push(format!(
        "## Skills ({} skills in {} categories)\n",
        registry.len(),
        categories.len()
    ));
    lines.push(
        "Skills are knowledge guides organized by category. Use `skill_list` to browse a category, `skill_view` to load one.\n"
            .to_string(),
    );

    for (cat, info) in categories {
        let desc = if info.description.is_empty() {
            String::new()
        } else {
            format!(" — {}", truncate_description(&info.description))
        };
        let names = info.skill_names.join(", ");
        lines.push(format!(
            "- **{}** ({} skills): {}{}",
            cat,
            info.skill_names.len(),
            names,
            desc
        ));
    }

    parts.push(lines.join("\n"));

    parts.join("\n\n")
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
    use crate::skills::types::{CategoryInfo, SkillDefinition};

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
    fn test_tools_block_empty() {
        let block = tools_block(&[]);
        assert!(block.contains("No tools registered"));
    }

    #[test]
    fn test_tools_block_with_tools() {
        let summaries = vec![
            ToolSummary {
                name: "bash".into(),
                description: "Execute a shell command.".into(),
            },
            ToolSummary {
                name: "read".into(),
                description: "Read the contents of a file.".into(),
            },
        ];
        let block = tools_block(&summaries);
        assert!(block.contains("2 available"));
        assert!(block.contains("**bash**"));
        assert!(block.contains("**read**"));
    }

    #[test]
    fn test_skills_block_empty() {
        let registry = SkillRegistry::new();
        let block = skills_block(&registry);
        assert!(block.is_empty());
    }

    #[test]
    fn test_skills_block_with_categories() {
        let mut registry = SkillRegistry::new();
        registry.register(SkillDefinition {
            name: "tdd".into(),
            description: "Test-driven development workflow.".into(),
            content: "# TDD".into(),
            source: "user".into(),
            path: None,
            category: "software-development".into(),
        });

        let mut categories = indexmap::IndexMap::new();
        categories.insert(
            "software-development".into(),
            CategoryInfo {
                description: "Coding, debugging, testing workflows".into(),
                skill_names: vec!["tdd".into()],
            },
        );

        let registry = SkillRegistry::from_parts(
            registry.list_skills().into_iter().cloned().collect(),
            categories,
        );
        let block = skills_block(&registry);
        assert!(block.contains("software-development"));
        assert!(block.contains("1 skills"));
        assert!(block.contains("Coding, debugging"));
    }

    #[test]
    fn test_skills_block_appears_in_index() {
        // Every skill should appear in the category index
        let skill = SkillDefinition {
            name: "git-workflow".into(),
            description: "Git branch and PR workflow.".into(),
            content: "# Git".into(),
            source: "user".into(),
            path: None,
            category: "devops".into(),
        };
        let mut categories = indexmap::IndexMap::new();
        categories.insert(
            "devops".into(),
            crate::skills::types::CategoryInfo {
                description: "Infrastructure".into(),
                skill_names: vec!["git-workflow".into()],
            },
        );
        let registry = SkillRegistry::from_parts(vec![skill], categories);

        let block = skills_block(&registry);
        assert!(block.contains("git-workflow"));
    }

    #[test]
    fn test_skills_block_flat_fallback() {
        // Use from_parts with empty categories to trigger flat fallback
        let skill = SkillDefinition::new(
            "tdd".into(),
            "Test-driven development workflow.".into(),
            "# TDD".into(),
            "user".into(),
            None,
            "general".into(),
        );
        let registry = SkillRegistry::from_parts(vec![skill], indexmap::IndexMap::new());

        let block = skills_block(&registry);
        assert!(block.contains("1 available"));
        assert!(block.contains("**tdd**"));
    }

    #[test]
    fn test_core_identity_no_skill_usage() {
        let id = core_identity(&RoleConfig::default());
        assert!(id.contains("zeno"));
        // Skill usage workflow should NOT be in core identity
        assert!(!id.contains("MANDATORY"));
        assert!(!id.contains("Tier 0"));
    }

    #[test]
    fn test_core_identity_no_reading() {
        let id = core_identity(&RoleConfig::default());
        // File reading strategy should NOT be in core identity
        assert!(!id.contains("File Reading Strategy"));
        assert!(!id.contains("offset"));
    }

    #[test]
    fn test_custom_identity() {
        let role = RoleConfig {
            identity: Some("You are Alice, a helpful research assistant.".into()),
            guidelines: None,
        };
        let id = core_identity(&role);
        assert!(id.contains("Alice"));
        assert!(!id.contains("zeno"));
        // Functional guidance must always be present, even with custom identity
        assert!(id.contains("Tools"));
        assert!(id.contains("Skills"));
        assert!(id.contains("skill_list"));
        assert!(id.contains("skill_view"));
    }

    #[test]
    fn test_custom_guidelines() {
        let role = RoleConfig {
            identity: None,
            guidelines: Some("- Always think step by step.\n- Never guess.".into()),
        };
        let p = guidelines(&role);
        assert!(p.contains("## Guidelines"));
        assert!(p.contains("Always think step by step"));
        assert!(!p.contains("Be concise"));
    }

    #[test]
    fn test_default_role_uses_builtin() {
        let role = RoleConfig::default();
        assert!(core_identity(&role).contains("zeno"));
        assert!(guidelines(&role).contains("Be concise"));
    }
}
