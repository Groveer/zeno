//! System prompt builder — assembles the full system prompt from components.
//!
//! The system prompt is built from:
//! 1. Core identity & role declaration (always present)
//! 2. Key principles (always present)
//! 3. Tool list with descriptions
//! 4. Skills Tier 0 category index + always-inject behavioral guidelines
//! 5. Runtime context (cwd, OS, git branch)
//! 6. Project instructions (CLAUDE.md / AGENTS.md)
//!
//! Skills with `always_inject: true` in their frontmatter have their full content
//! injected into the system prompt. All other skills show only name + description
//! in the Tier 0 category index and are loaded on-demand via `skill_view`.

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
- **Think aloud**: Before and between tool calls, explain your reasoning — what you're looking for, what you've found so far, and what you plan to do next. Don't just emit one-line transitions; instead, briefly share your analysis at each step so the user can follow your thought process.
- Use your tools proactively to read files, run commands, search information, and verify changes when the task requires it.
- When the user is just chatting, testing connectivity, or asking a question — respond with text only, no tool calls.
- Follow the user's project conventions (CLAUDE.md / AGENTS.md) if present.
- **Batch independent tool calls**: When multiple tools can be called in parallel (e.g., reading different files, running a search while also reading a file, or calling `glob` + `grep` together), issue ALL of them in a single response. The system executes independent calls concurrently, so batching dramatically reduces wait time. Only sequence calls that have data dependencies (where one call's result is needed as another's input) or target the same file for writes.
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
/// 2. **Always-inject behavioral guidelines**: Skills with `always_inject = true`
///    have their full content injected (frontmatter stripped).
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

    // Always-inject behavioral guidelines
    let core = registry.always_inject_skills();
    if !core.is_empty() {
        let blocks: Vec<String> = core
            .iter()
            .map(|s| {
                // Strip YAML frontmatter from content before injection
                let content = strip_frontmatter(&s.content);
                format!("### {}\n{}", s.name, content.trim())
            })
            .collect();
        parts.push(format!(
            "## Active Behavioral Guidelines\n{}",
            blocks.join("\n\n")
        ));
    }

    parts.join("\n\n")
}

/// Strip YAML frontmatter (--- ... ---) from skill content.
/// Uses a lightweight heuristic to validate the frontmatter: every non-empty
/// line must contain a `:` (YAML key-value marker). This avoids false matches
/// when `---` appears in body content (e.g. Markdown horizontal rules), while
/// eliminating the runtime dependency on serde_yaml for this single call.
/// Returns the markdown content after the closing `---`, trimmed.
fn strip_frontmatter(content: &str) -> String {
    if !content.starts_with("---") {
        return content.to_string();
    }
    // Skip the opening --- and optional newline (handle both \n and \r\n)
    let rest = content[3..].trim_start_matches(['\r', '\n']);
    // Find the closing --- on its own line
    let sep = rest.find("\n---").or_else(|| rest.find("\r\n---"));
    let Some(sep) = sep else {
        // No closing --- found, return as-is
        return content.to_string();
    };
    let frontmatter_text = &rest[..sep];
    // Heuristic validation: every non-empty line in the frontmatter must look
    // like YAML (contain a colon). This rejects false matches where `---` in
    // body content is mistaken for a frontmatter delimiter. The heuristic is
    // sufficient for skill files where frontmatter is simple `key: value` pairs.
    if !looks_like_yaml_frontmatter(frontmatter_text) {
        return content.to_string();
    }
    // Skip past the closing --- and any trailing whitespace/newline
    // The fallback to original content is a safety net — in practice,
    // one of the strip_prefix calls should always succeed since `sep`
    // was found by `.find("\n---")` or `.find("\r\n---")`.
    let after = rest[sep..]
        .strip_prefix("\r\n---")
        .or_else(|| rest[sep..].strip_prefix("\n---"));
    let Some(after) = after else {
        return content.to_string();
    };
    after.trim_start_matches(['\r', '\n']).to_string()
}

/// Lightweight heuristic to check if text looks like YAML frontmatter.
/// Every non-empty line must contain a `:` (YAML key-value marker).
/// This is sufficient for skill files where frontmatter is simple `key: value` pairs.
fn looks_like_yaml_frontmatter(text: &str) -> bool {
    let mut has_yaml_line = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Skip nested YAML lines (indented, may not have a colon at the top level)
        if trimmed.starts_with('-') || line.starts_with(char::is_whitespace) {
            has_yaml_line = true;
            continue;
        }
        if !trimmed.contains(':') {
            return false;
        }
        has_yaml_line = true;
    }
    has_yaml_line
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
            always_inject: false,
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
    fn test_skills_block_non_inject_not_in_guidelines() {
        // A non-always_inject skill should appear in the category index
        // but NOT in Active Behavioral Guidelines
        let skill = SkillDefinition {
            name: "git-workflow".into(),
            description: "Git branch and PR workflow.".into(),
            content: "# Git".into(),
            source: "user".into(),
            path: None,
            category: "devops".into(),
            always_inject: false,
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
        assert!(!block.contains("Active Behavioral Guidelines"));
    }

    #[test]
    fn test_skills_block_core_behavioral_full_injection() {
        let skill = SkillDefinition {
            name: "skill-usage-workflow".into(),
            description: "How to discover and load skills.".into(),
            content: "---\nname: skill-usage-workflow\n---\n# Skill Usage\nAlways load skills before tasks.".into(),
            source: "builtin".into(),
            path: None,
            category: "builtin".into(),
            always_inject: true,
        };
        let mut categories = indexmap::IndexMap::new();
        categories.insert(
            "builtin".into(),
            crate::skills::types::CategoryInfo {
                description: "Core behavioral guidelines".into(),
                skill_names: vec!["skill-usage-workflow".into()],
            },
        );
        let registry = SkillRegistry::from_parts(vec![skill], categories);

        let block = skills_block(&registry);
        // Core skill should be in Behavioral Guidelines with full content
        assert!(block.contains("Active Behavioral Guidelines"));
        assert!(block.contains("skill-usage-workflow"));
        // Frontmatter should be stripped
        assert!(!block.contains("---"));
        assert!(block.contains("Always load skills before tasks"));
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
    fn test_strip_frontmatter() {
        let with_frontmatter =
            "---\nname: test\ndescription: A test\n---\n# Real Content\nBody here";
        let stripped = strip_frontmatter(with_frontmatter);
        assert!(!stripped.contains("---"));
        assert!(!stripped.contains("name: test"));
        assert!(stripped.contains("# Real Content"));
        assert!(stripped.contains("Body here"));
    }

    #[test]
    fn test_strip_frontmatter_no_frontmatter() {
        let no_frontmatter = "# Just Content\nNo frontmatter here";
        let stripped = strip_frontmatter(no_frontmatter);
        assert_eq!(stripped, no_frontmatter);
    }

    #[test]
    fn test_strip_frontmatter_crlf() {
        let with_frontmatter = "---\r\nname: test\r\n---\r\n# Real Content\r\nBody here";
        let stripped = strip_frontmatter(with_frontmatter);
        assert!(!stripped.contains("---"));
        assert!(stripped.contains("# Real Content"));
    }

    #[test]
    fn test_strip_frontmatter_body_contains_delimiter() {
        // Body content with --- should not cause false split
        let content = "---\nname: test\n---\n# Title\nSome --- text\nMore content";
        let stripped = strip_frontmatter(content);
        assert!(stripped.contains("Some --- text"));
        assert!(stripped.contains("More content"));
        assert!(!stripped.contains("name: test"));
    }

    #[test]
    fn test_strip_frontmatter_invalid_yaml() {
        // If frontmatter doesn't look like YAML (no colon on a non-empty line),
        // return content as-is to avoid false splits
        let content = "---\nnot yaml at all\n---\n# Content";
        let stripped = strip_frontmatter(content);
        // Should return original since validation failed
        assert!(stripped.contains("---"));
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
    fn test_looks_like_yaml_frontmatter_valid() {
        assert!(looks_like_yaml_frontmatter(
            "name: test\ndescription: A test"
        ));
    }

    #[test]
    fn test_looks_like_yaml_frontmatter_with_empty_lines() {
        assert!(looks_like_yaml_frontmatter(
            "name: test\n\ndescription: A test"
        ));
    }

    #[test]
    fn test_looks_like_yaml_frontmatter_not_yaml() {
        assert!(!looks_like_yaml_frontmatter("not yaml at all"));
    }

    #[test]
    fn test_looks_like_yaml_frontmatter_empty() {
        assert!(!looks_like_yaml_frontmatter(""));
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
