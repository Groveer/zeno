//! System prompt builder — assembles the full system prompt from components.
//!
//! The system prompt is built from:
//! 1. Core identity & role declaration (always present)
//! 2. Key principles (always present)
//! 3. Tool name list (descriptions are in API schemas, not duplicated here)
//! 4. Skills Tier 0: category index + skill loading workflow
//! 5. Runtime context (cwd, OS, git branch, build system)
//! 6. Project instructions (CLAUDE.md / AGENTS.md)
//! 7. Memory (persistent user/memory store)
//!
//! The skills section follows a 3-tier progressive disclosure design:
//! - Tier 0 (system prompt): category index + loading workflow instructions
//! - Tier 1 (skill_list): browse skill summaries within a category
//! - Tier 2 (skill_view): load a skill's full instructions
//!
//! The loading workflow in Tier 0 is the **driver** that makes the progressive
//! disclosure system work — without it, the LLM does not know when or how to
//! descend from Tier 0 → Tier 1 → Tier 2. This was previously implemented via
//! `always_inject: true` on a `skill-usage-workflow` skill, but is now inlined
//! directly into the skills_block as an inherent part of Tier 0.

use std::path::Path;

use crate::config::settings::{IdentityConfig, RoleConfig};
use crate::prompts::claudemd;
use crate::prompts::context::RuntimeContext;
use crate::skills::registry::SkillRegistry;
use crate::tools::base::{ToolRegistry, tool_kind};

/// Build the complete system prompt.
///
/// When `memory_block` is Some, memory guidance + the frozen snapshot are
/// injected at the end, after project instructions.
///
/// When `active_identity` is Some, its fields override the corresponding
/// `role_config` fields for identity and guidelines.
///
/// `config_dir` is used to resolve external file references in guidelines.
pub fn build(
    cwd: &Path,
    config_dir: &Path,
    tool_registry: &ToolRegistry,
    skill_registry: &SkillRegistry,
    memory_block: Option<&str>,
    role_config: &RoleConfig,
    active_identity: Option<&IdentityConfig>,
) -> String {
    // Merge: identity fields override role_config when active_identity is set
    let effective_role = match active_identity {
        Some(id) => RoleConfig {
            identity: id.identity.clone().or_else(|| role_config.identity.clone()),
            guidelines: id
                .guidelines
                .clone()
                .or_else(|| role_config.guidelines.clone()),
        },
        None => role_config.clone(),
    };

    // Resolve guidelines to a plain string (reading external files if needed)
    let guidelines_text = effective_role
        .guidelines
        .as_ref()
        .and_then(|gc| {
            gc.resolve(config_dir)
                .inspect_err(|e| tracing::warn!(error = %e, "Failed to resolve guidelines"))
                .ok()
        })
        .filter(|s| !s.trim().is_empty());

    let mut parts = vec![
        core_identity(&effective_role, !skill_registry.is_empty()),
        guidelines(guidelines_text.as_deref()),
        tools_block(&tool_registry.names()),
    ];

    // 3. Skills Tier 0: category index + loading workflow
    if !skill_registry.is_empty() {
        parts.push(skills_block(skill_registry));
    }

    // 4. Runtime context
    let ctx = RuntimeContext::collect(cwd);
    parts.push(format!("## Environment\n\n{}", ctx.to_prompt_block()));

    // 5. Project instructions (CLAUDE.md / AGENTS.md)
    if let Some((path, content)) = claudemd::load_instruction_file(cwd) {
        parts.push(format!(
            "## Project Instructions\n\nLoaded from: {}\n\n{}",
            path.display(),
            content.trim()
        ));
    }

    // 6. Memory guidance + frozen snapshot
    if let Some(block) = memory_block
        && !block.is_empty()
    {
        parts.push(MEMORY_GUIDANCE.to_string());
        parts.push(block.to_string());
    }

    parts.join("\n\n")
}

/// Core identity and role declaration — always present.
///
/// When the user provides a custom identity, it replaces the *identity portion*
/// (who you are), but the functional guidance (how to use Tools & Skills) is always
/// appended so the model never loses awareness of its capabilities.
fn core_identity(role: &RoleConfig, has_skills: bool) -> String {
    let identity_text = match role.identity {
        Some(ref custom) => custom.trim().to_string(),
        None => "You are zeno (zn), a helpful AI assistant.\n\n\
You help users with a wide variety of tasks: answering questions, writing and editing text,\n\
analyzing information, and more. When tools are available, use them proactively to assist the user."
            .to_string(),
    };

    // Functional guidance is always present — never overridden by custom identity.
    let mut functional = "\
**Tools** are executable capabilities (bash, read, etc.) that you call via function calling."
        .to_string();
    if has_skills {
        functional.push_str(
            "\n\
**Skills** are knowledge guides organized by category. Use `skill_list` to browse a category\n\
and `skill_view` to load a specific skill's full instructions.",
        );
    }

    format!("{}\n\n{}", identity_text.trim(), functional)
}

/// Memory guidance — injected when memory is active.
///
/// Brief reminder of memory principles. Detailed rules are in the memory tool
/// description, which the LLM sees when it calls the tool.
pub const MEMORY_GUIDANCE: &str = "\
You have persistent memory across sessions. Use the `memory` tool to save durable \
facts: user preferences, environment details, tool quirks, and stable conventions. \
Memory is injected into every turn, so keep it compact.\n\
Prioritize what prevents the user from having to correct or remind you again. \
Do NOT save task progress, session outcomes, or anything that will be stale in a week. \
Write declarative facts, not instructions — 'User prefers concise responses' ✓, \
'Always respond concisely' ✗.";

/// Format the "Session Files Already Read" context block for the system prompt.
///
/// This block is injected each turn so the LLM knows what files it has already
/// seen in this session. The goal is to prevent redundant `read` calls for files
/// the LLM already has in its context window.
///
/// Caller guarantees `summary` is non-empty (checked by `read_files_summary()`).
pub fn session_files_block(summary: &str) -> String {
    format!(
        "## Session Files Already Read\n\n\
The following files have been returned to you in this session. \
If you need information from one of them, use `grep` for targeted \
queries instead of re-reading the full file.\n\n\
{}\n\n\
> Use `grep pattern --include=\"*.rs\" path=\"src\"` to search within \
a specific directory rather than re-reading entire files.",
        summary
    )
}

/// Format the "Active Sub-Agents" context block for the system prompt.
///
/// Injected each turn when there are open sub-agents so the LLM knows
/// about work it delegated. Guides the model to use `tool_search` then
/// `list_sub_agents` to query child status.
pub fn sub_agent_block(open_count: usize, total_count: usize) -> String {
    format!(
        "## Sub-Agent Summary\n\n\
You have {open_count} open sub-agent(s) (out of {total_count} total in this session).\n\
Use `tool_search(\"list_sub_agents\")` to discover the `list_sub_agents` tool,\n\
then call it to query open sub-agents and check their status."
    )
}

/// Built-in guidelines — always present in the system prompt body.
fn builtin_guidelines() -> String {
    r#"
## Guidelines

- Be concise and direct. Prefer showing results over lengthy explanations.
- **Think aloud**: Before and between tool calls, briefly explain what you're looking for and what you've found so the user can follow your reasoning.
- Use tools proactively to read files, run commands, search information, and verify changes when needed.
- When the user is just chatting or asking a question — respond with text only, no tool calls.
- Follow the user's project conventions (CLAUDE.md / AGENTS.md) if present.
- **Batch independent tool calls**: Issue all independent calls in one response (e.g. `glob` + `grep` together). Only sequence calls that have dependencies.
- **Tool Use Enforcement (CRITICAL)**: You MUST use tools to perform actions. Do NOT explain your plan or describe what you would do without actually calling a tool immediately. NEVER end your turn with a promise of future action — execute it now!
- **Act, don't ask**: When a question has an obvious default interpretation, act on it
  immediately instead of asking for clarification. Examples:
  - "Is port 443 open?" → check THIS machine (don't ask "open where?")
  - "What OS am I running?" → check the live system (don't use user profile)
  - "What time is it?" → run `date` (don't guess)
  Only ask for clarification when the ambiguity genuinely changes what tool you would call.
- **MCP First**: Check `mcp_list_servers` before using web_search, web_fetch, read, or grep. Servers show [stopped] by default — that's normal, call `mcp_list_tools(name)` to activate them and see their tools. `mcp_describe_tool` is rarely needed — Step 2 already returns full schemas.
- **Use `delegate_task` only for truly parallel subtasks** (batch mode with `tasks` array). \
  Never delegate a single tool call — call `web_search`, `web_fetch`, `read`, etc. directly. \
  Delegating a single search or read wastes tokens, loses context, and can cause infinite loops.
- **Scope searches to the project structure**: Before using `grep` or `glob`, check
  the Environment section for the detected build system. Target searches at the source
  tree (e.g. `path="src"`) rather than the project root. Start shallow (`glob("*")`)
  then drill down — don't `glob("**")` on the whole project.
- **Prefer grep before read**: Before calling `read`, first use `grep` to locate the
  exact lines you need. Only call `read` when you know the specific file and line range.
  This avoids reading large files unnecessarily.
- **Avoid redundant re-reads**: Before reading a file, check the "Session Files Already Read"
  section below — if you already have the file in context, use `grep` for targeted queries
  instead of re-reading. Redundant reads waste tokens and context window space.
  The system tracks your reads and reports overlap in read output as
  `[Note: lines ... were already returned]`.
- **Read minimum range**: Always specify `offset` + `limit` (or `offset` + `context`)
  to read the smallest possible range. Never read an entire file when you only need
  a few lines. Exception: files ≤500 lines where you need the full context.
- **Glob only with keywords**: Only use `glob` when you have a specific pattern or
  keyword to match (e.g. `glob("*.rs")`, `glob("src/**/*.py")`). Avoid bare `glob("**")`
  or `glob("*")` on the project root — these waste tokens by returning hundreds of results.
- **Read before edit**: Before calling `edit`, you MUST first `read` the file to confirm
  the exact content, line positions, and indentation. Copy-paste the exact text from
  `read` output as `old_string` — include 2-3 surrounding lines for uniqueness.
- **Edit indentation**: When constructing `edit` calls, match the file's actual
  indentation style (spaces vs tabs, depth). Copy the indentation directly from
  `read` output rather than guessing or re-typing it.
- **Missing context**: If required context is missing, do NOT guess or hallucinate. Follow MCP First (above), then fall back to tools. If still stuck, ask the user. Label assumptions explicitly.
"#
    .trim()
    .to_string()
}

/// User-defined guidelines — appended right after the built-in guidelines.
/// Uses `##` heading and a priority declaration so the LLM assigns them
/// equal or higher weight than the default rules.
fn user_guidelines_block(custom: &str) -> String {
    format!(
        "## User Guidelines (MUST FOLLOW)\n\n\
**These user-defined rules take PRIORITY over the default Guidelines above.** \
When there is a conflict, always follow the user's rules below.\n\n\
{}",
        custom.trim()
    )
}

/// Guidelines — combines built-in and optional user guidelines into one block.
fn guidelines(custom: Option<&str>) -> String {
    let builtin = builtin_guidelines();
    match custom {
        Some(text) if !text.trim().is_empty() => {
            format!("{}\n\n{}", builtin, user_guidelines_block(text))
        }
        _ => builtin,
    }
}

/// Format the tool list as a readable block for the system prompt.
///
/// Tools are grouped by category to give the LLM a visual hierarchy:
/// MCP tools (preferred for structured data) → Web tools → all others.
/// This helps the LLM choose the right tool for the task at a glance.
///
/// Only lists tool names — descriptions are already in the API tool schemas
/// sent with every request, so repeating them here would be redundant and
/// waste tokens. The system prompt just needs to tell the LLM what tools
/// exist so it can decide which to use; the API schemas provide the details.
fn tools_block(names: &[&str]) -> String {
    if names.is_empty() {
        return "## Tools\n\n(No tools registered.)".to_string();
    }

    // Group tools by category using tool_kind() for single source of truth
    // Display order matches tool_priority tiers: MCP → delegate → other
    let mcp: Vec<&str> = names
        .iter()
        .filter(|n| tool_kind(n) == "mcp")
        .copied()
        .collect();
    let delegate: Vec<&str> = names
        .iter()
        .filter(|n| tool_kind(n) == "delegate")
        .copied()
        .collect();
    let other: Vec<&str> = names
        .iter()
        .filter(|n| tool_kind(n) == "other")
        .copied()
        .collect();

    let mut lines = vec![
        format!("## Tools ({} available)\n", names.len()),
        "> **⚠️ MCP First** — Call `mcp_list_servers` before web_search, web_fetch, or any other tool for external data. MCP provides structured, authoritative data that generic tools cannot match.".to_string(),
    ];

    if !mcp.is_empty() {
        lines.push(format!(
            "### MCP (Model Context Protocol) — Always check first before using built-in tools\n{}",
            mcp.join(", ")
        ));
    }
    if !delegate.is_empty() {
        lines.push(format!("### Delegation\n{}", delegate.join(", ")));
    }
    if !other.is_empty() {
        lines.push(format!("### Built-in Tools\n{}", other.join(", ")));
    }

    lines.join("\n\n")
}

// ---------------------------------------------------------------------------
// Skill loading workflow — the driver for 3-tier progressive disclosure
// ---------------------------------------------------------------------------

/// Single template for the 3-tier progressive disclosure workflow.
/// Three placeholders ({tier0}, {tier1}, {if_unsure}) are filled based on
/// whether skills are organized into categories or flat.
const SKILL_LOADING_WORKFLOW: &str = "\
## Loading Workflow (MANDATORY)

You **MUST** load relevant skills before attempting non-trivial tasks.

**Direct match** — When the task clearly matches a skill name listed above, load it directly:\
```\
skill_view(name=\"<skill-name>\")\
```\
Then follow the loaded instructions before proceeding. Do NOT start the task without loading the skill — skills contain critical steps, pitfalls, and established workflows that prevent mistakes.

**3-Tier Progressive Disclosure** — When no skill name directly matches the task (most cases), you **MUST** follow these steps before starting any work:\
1. **Tier 0** — {tier0}\
2. **Tier 1** — {tier1}\
3. **Tier 2** — Call `skill_view(name=<skill>)` to load the selected skill's full instructions.\
Do NOT start the task until you have loaded the relevant skill. Skill names alone are NOT enough to determine relevance — descriptions are required for informed selection.\
{if_unsure}

**When to skip** — Only for trivial interactions (greetings, simple questions, connectivity tests).\
For everything else — even tasks that seem simple — follow the 3-Tier process above to load the relevant skill first.\
Err on the side of loading — it is always better to have context you don't need than to miss critical steps, pitfalls, or established workflows.";

/// Format the skills section for the system prompt.
///
/// This includes two parts:
/// 1. **Tier 0 category index**: Compact listing of categories with skill names.
/// 2. **Loading workflow**: MUST-level instructions driving LLM to use the
///    3-tier progressive disclosure system (Tier 0 → Tier 1 → Tier 2).
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
        for s in &skills {
            let desc = truncate_description(&s.description);
            lines.push(format!("- **{}**: {}", s.name, desc));
        }
        parts.push(lines.join("\n"));
        parts.push(SKILL_LOADING_WORKFLOW
            .replace("{tier0}", "Scan the skill list above. Identify potentially relevant skills.")
            .replace("{tier1}", "Call `skill_list` to see full descriptions of relevant skills and narrow down to the best match.")
            .replace("{if_unsure}", "If still unsure, call `skill_list` again to compare descriptions of other skills."));
        return parts.join("\n\n");
    }

    let mut lines = Vec::new();
    lines.push(format!(
        "## Skills ({} skills in {} categories)\n",
        registry.len(),
        categories.len()
    ));

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
    parts.push(SKILL_LOADING_WORKFLOW
        .replace("{tier0}", "Scan the category list above. Identify the most relevant category.")
        .replace("{tier1}", "Call `skill_list(category=<cat>)` to see each skill's full description within that category.")
        .replace("{if_unsure}", "If unsure which category fits, call `skill_list` on multiple candidates."));

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
        let names = vec!["bash", "read"];
        let block = tools_block(&names);
        assert!(block.contains("2 available"));
        assert!(block.contains("### Built-in Tools"));
        assert!(block.contains("bash"));
        assert!(block.contains("read"));
        // Descriptions should NOT be in the system prompt block (they're in API schemas)
        assert!(!block.contains("Execute"));
        assert!(!block.contains("Read the"));
    }

    #[test]
    fn test_tools_block_grouping() {
        let names = vec![
            "mcp_list_servers",
            "mcp_call_tool",
            "delegate_task",
            "web_search",
            "web_fetch",
            "bash",
            "read",
        ];
        let block = tools_block(&names);
        assert!(block.contains("7 available"));
        // MCP group
        assert!(block.contains("### MCP (Model Context Protocol)"));
        assert!(block.contains("mcp_list_servers"));
        assert!(block.contains("mcp_call_tool"));
        // Delegate group
        assert!(block.contains("### Delegation"));
        assert!(block.contains("delegate_task"));
        // Built-in group
        assert!(block.contains("### Built-in Tools"));
        assert!(block.contains("web_search"));
        assert!(block.contains("web_fetch"));
        assert!(block.contains("bash"));
        assert!(block.contains("read"));
        // Priority order: MCP < Delegate < Built-in
        let mcp_pos = block.find("### MCP").unwrap();
        let delegate_pos = block.find("### Delegation").unwrap();
        let builtin_pos = block.find("### Built-in").unwrap();
        assert!(
            mcp_pos < delegate_pos,
            "MCP group should appear before Delegation group"
        );
        assert!(
            delegate_pos < builtin_pos,
            "Delegation group should appear before Built-in group"
        );
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
        let id = core_identity(&RoleConfig::default(), false);
        assert!(id.contains("zeno"));
        // Skill usage workflow should NOT be in core identity
        assert!(!id.contains("MANDATORY"));
        assert!(!id.contains("Tier 0"));
    }

    #[test]
    fn test_core_identity_no_reading() {
        let id = core_identity(&RoleConfig::default(), false);
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
        let id = core_identity(&role, true);
        assert!(id.contains("Alice"));
        assert!(!id.contains("zeno"));
        // Functional guidance must always be present, even with custom identity
        assert!(id.contains("Tools"));
        assert!(id.contains("Skills"));
        assert!(id.contains("skill_list"));
        assert!(id.contains("skill_view"));
    }

    #[test]
    fn test_custom_guidelines_appended() {
        let guidelines_text = "- Always think step by step.\n- Never guess.";
        let p = guidelines(Some(guidelines_text));
        assert!(p.contains("## Guidelines"));
        // Built-in rules are always present
        assert!(p.contains("Be concise"));
        assert!(p.contains("Batch independent tool calls"));
        assert!(p.contains("Prefer grep before read"));
        // Custom rules are appended under "## User Guidelines (MUST FOLLOW)"
        assert!(p.contains("## User Guidelines (MUST FOLLOW)"));
        assert!(p.contains("PRIORITY"));
        assert!(p.contains("Always think step by step"));
        assert!(p.contains("Never guess"));
    }

    #[test]
    fn test_default_role_uses_builtin() {
        let role = RoleConfig::default();
        assert!(core_identity(&role, false).contains("zeno"));
        assert!(guidelines(None).contains("Be concise"));
    }

    // --- New tests for loading workflow ---

    #[test]
    fn test_skills_block_categorized_contains_loading_workflow() {
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
                description: "Coding workflows".into(),
                skill_names: vec!["tdd".into()],
            },
        );
        let registry = SkillRegistry::from_parts(
            registry.list_skills().into_iter().cloned().collect(),
            categories,
        );
        let block = skills_block(&registry);

        // Loading workflow must be present with MUST-level language
        assert!(block.contains("MANDATORY"), "should contain MANDATORY");
        assert!(block.contains("MUST"), "should contain MUST directive");
        assert!(block.contains("Tier 0"), "should reference Tier 0");
        assert!(block.contains("Tier 1"), "should reference Tier 1");
        assert!(block.contains("Tier 2"), "should reference Tier 2");
        assert!(
            block.contains("Err on the side of loading"),
            "should encourage loading"
        );
        assert!(
            block.contains("skill_list(category="),
            "should show Tier 1 tool usage"
        );
        assert!(
            block.contains("skill_view(name="),
            "should show Tier 2 tool usage"
        );
    }

    #[test]
    fn test_skills_block_flat_contains_loading_workflow() {
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

        assert!(
            block.contains("MANDATORY"),
            "flat block should contain MANDATORY"
        );
        assert!(block.contains("MUST"), "flat block should contain MUST");
        assert!(
            block.contains("Err on the side of loading"),
            "flat block should encourage loading"
        );
    }

    #[test]
    fn test_skills_loading_workflow_separate_from_index() {
        // The loading workflow should be a separate section after the category index,
        // not interleaved with it.
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
                description: "Coding workflows".into(),
                skill_names: vec!["tdd".into()],
            },
        );
        let registry = SkillRegistry::from_parts(
            registry.list_skills().into_iter().cloned().collect(),
            categories,
        );
        let block = skills_block(&registry);

        // Category index and loading workflow are separated by double newline
        assert!(
            block.contains("\n\n## Loading Workflow"),
            "Loading workflow should be a separate ## section after the category index"
        );
    }

    #[test]
    fn test_scope_searches_guideline_in_system_prompt() {
        // The lightweight guideline rule should be present
        let role = RoleConfig::default();
        let tool_registry = ToolRegistry::new();
        let skill_registry = SkillRegistry::new();
        let prompt = build(
            std::path::Path::new("/tmp"),
            std::path::Path::new("/tmp"),
            &tool_registry,
            &skill_registry,
            None,
            &role,
            None,
        );
        assert!(
            prompt.contains("Scope searches to the project structure"),
            "Guideline rule for scoping searches should be present"
        );
        assert!(
            prompt.contains("path=\"src\""),
            "Should hint about path parameter"
        );
    }

    #[test]
    fn test_build_system_in_environment() {
        // Build system detection should appear in Environment section
        let role = RoleConfig::default();
        let tool_registry = ToolRegistry::new();
        let skill_registry = SkillRegistry::new();
        let prompt = build(
            std::path::Path::new("/tmp"),
            std::path::Path::new("/tmp"),
            &tool_registry,
            &skill_registry,
            None,
            &role,
            None,
        );
        // /tmp has no build files, so build_system should be absent
        assert!(
            !prompt.contains("Project build system"),
            "No build system should be reported for /tmp"
        );
    }
}
