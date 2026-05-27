//! Skill management tool — create, edit, patch, and delete skills.
//!
//! Allows the agent to create, update, and delete skills, turning successful
//! approaches into reusable procedural knowledge.
//!
//! Instead of reimplementing file I/O, this tool delegates to the existing
//! fuzzy matching engine from the `edit` module, after performing validation
//! against the standard SKILL.md frontmatter format.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::skills::registry::SkillRegistry;
use crate::skills::validation;
use crate::tools::base::{Tool, ToolContext, ToolError};
use zeno_tools::{JsonToolOutput, ToolOutput};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Characters allowed in skill names (filesystem-safe, URL-friendly).
const VALID_NAME_RE: &str = r"^[a-z][a-z0-9._-]*$";

/// Subdirectories allowed for supporting files.
const ALLOWED_SUBDIRS: &[&str] = &["references", "templates", "scripts", "assets"];

// ---------------------------------------------------------------------------
// Regex caching
// ---------------------------------------------------------------------------

fn name_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(VALID_NAME_RE).unwrap())
}

// ---------------------------------------------------------------------------
// Tool definition
// ---------------------------------------------------------------------------

/// The skill management tool.
///
/// Holds an `Arc<Mutex<SkillRegistry>>` so that reads (finding skills) and
/// writes (reloading after mutations) both use the live registry.
pub struct SkillManageTool {
    /// Live registry — shared with the system prompt builder and skill tools.
    /// We lock it briefly for reads and reloads.
    registry: Arc<Mutex<SkillRegistry>>,
    /// Full list of skill directories (user + project) for cache manifest consistency.
    skill_dirs: Vec<PathBuf>,
}

impl SkillManageTool {
    pub fn new(registry: Arc<Mutex<SkillRegistry>>, skill_dirs: Vec<PathBuf>) -> Self {
        Self {
            registry,
            skill_dirs,
        }
    }

    /// Get the skills root directory.
    fn skills_dir() -> PathBuf {
        crate::config::paths::config_dir().join("skills")
    }

    /// Get the user skills directory.
    fn user_skills_dir() -> PathBuf {
        crate::skills::loader::get_user_skills_dir()
    }

    /// Resolve the path for a skill, optionally under a category.
    fn resolve_skill_dir(name: &str, category: Option<&str>) -> PathBuf {
        let root = Self::skills_dir();
        match category {
            Some(cat) => root.join(cat).join(name),
            None => root.join(name),
        }
    }
}

#[async_trait]
impl Tool for SkillManageTool {
    fn name(&self) -> &str {
        "skill_manage"
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        false
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "skill_manage",
                "description": "Create, edit, patch, or delete skills. Skills are reusable procedural knowledge.\n\n\
                    Actions:\n\
                    - `create`: Create a new skill. Requires `name` and `content` (full SKILL.md with frontmatter).\n\
                    - `patch`: Find-and-replace within a skill file. Requires `name`, `old_string`, `new_string`. Optional: `file_path` to patch a supporting file.\n\
                    - `edit`: Full rewrite of a skill's SKILL.md. Requires `name` and `content`.\n\
                    - `delete`: Remove a skill. Requires `name`.\n\
                    - `write_file`: Add/overwrite a supporting file. Requires `name`, `file_path`, `content`.\n\
                    - `pin`: Mark a skill as pinned (excluded from auto-stale/archive transitions). Requires `name`.\n\
                    - `unpin`: Remove pinned status from a skill. Requires `name`.\n\
                    - `restore`: Restore an archived skill back to the active skills directory. Requires `name`.\n\
                    - `list-archived`: List all archived skill names. No arguments needed.\n\
                    - `curator_pause`: Pause the curator (stop automatic lifecycle maintenance). No arguments needed.\n\
                    - `curator_resume`: Resume the curator. No arguments needed.\n\
                    - `curator_status`: Show whether the curator is paused. No arguments needed.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["create", "patch", "edit", "delete", "write_file", "pin", "unpin", "restore", "list-archived", "curator_pause", "curator_resume", "curator_status"],
                            "description": "Required. The management action to perform."
                        },
                        "name": {
                            "type": "string",
                            "description": "Skill name (lowercase, hyphens, dots; e.g. 'git-rebase-workflow')."
                        },
                        "content": {
                            "type": "string",
                            "description": "Full SKILL.md content with YAML frontmatter. Required for create and edit. Also used for write_file."
                        },
                        "category": {
                            "type": "string",
                            "description": "Category directory for the skill (e.g. 'software-development'). Optional for create."
                        },
                        "old_string": {
                            "type": "string",
                            "description": "Text to find (for patch). Uses fuzzy matching."
                        },
                        "new_string": {
                            "type": "string",
                            "description": "Replacement text (for patch). Use empty string to delete."
                        },
                        "file_path": {
                            "type": "string",
                            "description": "Supporting file path relative to skill dir (e.g. 'references/api.md'). For patch and write_file."
                        }
                    },
                    "required": ["action", "name"]
                }
            }
        })
    }

    async fn execute(
        &self,
        arguments: Value,
        ctx: &ToolContext,
    ) -> Result<Box<dyn ToolOutput>, ToolError> {
        let action = arguments["action"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("missing 'action'".into()))?;

        let name = arguments["name"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("missing 'name'".into()))?;

        // Validate name format
        if !name_regex().is_match(name) {
            return Err(ToolError::InvalidArguments(format!(
                "Invalid skill name '{}'. Use lowercase letters, digits, hyphens, dots. \
                 Must start with a letter or digit. Max {} characters.",
                name,
                validation::MAX_NAME_LENGTH,
            )));
        }

        if name.len() > validation::MAX_NAME_LENGTH {
            return Err(ToolError::InvalidArguments(format!(
                "Skill name exceeds {} characters.",
                validation::MAX_NAME_LENGTH,
            )));
        }

        match action {
            "create" => Ok(Box::new(JsonToolOutput::success(
                self.action_create(name, &arguments, ctx).await?,
            ))),
            "patch" => Ok(Box::new(JsonToolOutput::success(
                self.action_patch(name, &arguments).await?,
            ))),
            "edit" => Ok(Box::new(JsonToolOutput::success(
                self.action_edit(name, &arguments).await?,
            ))),
            "delete" => Ok(Box::new(JsonToolOutput::success(
                self.action_delete(name).await?,
            ))),
            "write_file" => Ok(Box::new(JsonToolOutput::success(
                self.action_write_file(name, &arguments).await?,
            ))),
            "pin" => Ok(Box::new(JsonToolOutput::success(
                self.action_pin(name).await?,
            ))),
            "unpin" => Ok(Box::new(JsonToolOutput::success(
                self.action_unpin(name).await?,
            ))),
            "restore" => Ok(Box::new(JsonToolOutput::success(
                self.action_restore(name).await?,
            ))),
            "list-archived" => Ok(Box::new(JsonToolOutput::success(
                self.action_list_archived().await?,
            ))),
            "curator_pause" => Ok(Box::new(JsonToolOutput::success(
                self.action_curator_pause(),
            ))),
            "curator_resume" => Ok(Box::new(JsonToolOutput::success(
                self.action_curator_resume(),
            ))),
            "curator_status" => Ok(Box::new(JsonToolOutput::success(
                self.action_curator_status(),
            ))),
            _ => Err(ToolError::InvalidArguments(format!(
                "Unknown action '{}'. Use: create, patch, edit, delete, write_file, pin, unpin, \
                 restore, list-archived, curator_pause, curator_resume, curator_status.",
                action
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Action implementations
// ---------------------------------------------------------------------------

impl SkillManageTool {
    /// Find a skill's directory by name using the live registry.
    async fn find_skill(&self, name: &str) -> Option<PathBuf> {
        let reg = self.registry.lock().await;
        if let Some(skill) = reg.get_fuzzy(name)
            && let Some(ref path_str) = skill.path
        {
            let path = Path::new(path_str);
            // The registry guarantees the skill exists; path includes SKILL.md filename.
            if let Some(parent) = path.parent() {
                return Some(parent.to_path_buf());
            }
        }
        None
    }

    /// Check if a skill is user-created (deletable).
    async fn is_user_skill(&self, name: &str) -> bool {
        let reg = self.registry.lock().await;
        if let Some(skill) = reg.get_fuzzy(name) {
            return skill.source == "user";
        }
        true // If not found in registry, assume user-created
    }

    async fn action_create(
        &self,
        name: &str,
        args: &Value,
        ctx: &ToolContext,
    ) -> Result<String, ToolError> {
        let content = args["content"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("create requires 'content'".into()))?;

        let category = args["category"].as_str().map(|s| s.trim());

        // Validate category format if provided
        if let Some(cat) = category
            && !name_regex().is_match(cat)
        {
            return Err(ToolError::InvalidArguments(format!(
                "Invalid category '{}'. Use lowercase letters, digits, hyphens, dots.",
                cat
            )));
        }

        // Validate frontmatter
        let fm_info = validation::validate_frontmatter(content).map_err(|e| {
            ToolError::InvalidArguments(format!("SKILL.md validation failed: {}", e))
        })?;

        // Check for name collision
        if let Some(existing) = self.find_skill(name).await {
            return Err(ToolError::Execution(format!(
                "A skill named '{}' already exists at {}. Use 'edit' or 'patch' to modify it.",
                name,
                existing.display(),
            )));
        }

        // Resolve target path
        let skill_dir = Self::resolve_skill_dir(name, category);
        let skill_md = skill_dir.join("SKILL.md");

        // Create directory
        tokio::fs::create_dir_all(&skill_dir).await.map_err(|e| {
            ToolError::Execution(format!("Failed to create skill directory: {}", e))
        })?;

        // Write SKILL.md
        tokio::fs::write(&skill_md, content)
            .await
            .map_err(|e| ToolError::Execution(format!("Failed to write SKILL.md: {}", e)))?;

        // Reload the skill registry
        self.reload_skill_registry().await;

        // Mark as agent-created if this is a background review fork
        if let Some(ref deps) = ctx.sub_agent_deps
            && deps.write_origin == crate::skills::provenance::BACKGROUND_REVIEW
        {
            crate::skills::usage::mark_agent_created(name);
        }

        let mut result = format!("Skill '{}' created at {}", name, skill_md.display());
        if let Some(cat) = category {
            result.push_str(&format!("\nCategory: {}", cat));
        }
        if let Some(desc) = &fm_info.description {
            result.push_str(&format!("\nDescription: {}", desc));
        }

        result.push_str(
            "\n\nTo add supporting files (references, templates, scripts), use \
             `skill_manage(action='write_file', name='<name>', file_path='references/api.md', \
             content='...')` or the `write` tool directly.",
        );

        Ok(result)
    }

    async fn action_patch(&self, name: &str, args: &Value) -> Result<String, ToolError> {
        let old_string = args["old_string"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("patch requires 'old_string'".into()))?;

        let new_string = args["new_string"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("patch requires 'new_string'".into()))?;

        let file_path = args["file_path"].as_str();

        if old_string.is_empty() && file_path.is_none() {
            return Err(ToolError::InvalidArguments(
                "patch requires non-empty 'old_string' for SKILL.md edits. \
                 To create a supporting file, use write_file action or provide 'file_path'."
                    .into(),
            ));
        }

        // Find the skill
        let skill_dir = self.find_skill(name).await.ok_or_else(|| {
            ToolError::NotFound(format!(
                "Skill '{}' not found. Use skill_list() to see available skills.",
                name
            ))
        })?;

        // Resolve target file
        let target = if let Some(fp) = file_path {
            Self::validate_and_resolve_file_path(&skill_dir, fp)?
        } else {
            skill_dir.join("SKILL.md")
        };

        // If target file doesn't exist and file_path is provided with empty
        // old_string, treat this as a file creation (like write_file).
        if !target.exists() {
            if file_path.is_some() && old_string.is_empty() {
                // Guard against creating empty files via patch (likely unintended)
                if new_string.is_empty() {
                    return Err(ToolError::InvalidArguments(
                        "Cannot create a file with empty content via patch. \
                         Use 'write_file' instead."
                            .into(),
                    ));
                }
                // Auto-create parent directories (including skill_dir if removed externally).
                // Note: target.parent() is always Some here because
                // validate_and_resolve_file_path enforces a multi-component path.
                if let Some(parent) = target.parent() {
                    tokio::fs::create_dir_all(parent).await.map_err(|e| {
                        ToolError::Execution(format!("Failed to create directory: {}", e))
                    })?;
                }
                tokio::fs::write(&target, new_string)
                    .await
                    .map_err(|e| ToolError::Execution(format!("Failed to write file: {}", e)))?;

                self.reload_skill_registry().await;
                crate::skills::usage::bump_patch(name);

                return Ok(format!(
                    "Created supporting file: {} ({} bytes)",
                    target.display(),
                    new_string.len(),
                ));
            }
            // Differentiate SKILL.md vs supporting file not found
            let message = if file_path.is_none() {
                format!(
                    "SKILL.md not found for skill '{}'. It may have been deleted externally. \
                     Use 'edit' to recreate it.",
                    name,
                )
            } else {
                format!(
                    "File not found: {}. To create it, use patch with empty old_string or write_file.",
                    target.display(),
                )
            };
            return Err(ToolError::NotFound(message));
        }

        // Read current content
        let content = tokio::fs::read_to_string(&target)
            .await
            .map_err(|e| ToolError::NotFound(format!("Cannot read {}: {}", target.display(), e)))?;

        // Use the fuzzy matching engine from the edit tool
        let (new_content, match_count, strategy, error) =
            crate::tools::edit::fuzzy_find_and_replace(&content, old_string, new_string, false)?;

        if let Some(err) = error {
            return Err(ToolError::Execution(format!(
                "{} (strategy tried: {})",
                err, strategy
            )));
        }

        // If patching SKILL.md, validate frontmatter is still intact
        if file_path.is_none() {
            validation::validate_frontmatter(&new_content).map_err(|e| {
                ToolError::Execution(format!("Patch would break SKILL.md structure: {}", e))
            })?;
        }

        // Write back
        tokio::fs::write(&target, &new_content)
            .await
            .map_err(|e| ToolError::Execution(format!("Failed to write: {}", e)))?;

        self.reload_skill_registry().await;

        // Bump patch telemetry (best-effort)
        crate::skills::usage::bump_patch(name);

        let strategy_info = if strategy != "exact" {
            format!(" [fuzzy: {}]", strategy)
        } else {
            String::new()
        };

        let file_label = file_path.unwrap_or("SKILL.md");
        Ok(format!(
            "Patched {} in skill '{}' ({} replacement(s){})",
            file_label, name, match_count, strategy_info,
        ))
    }

    async fn action_edit(&self, name: &str, args: &Value) -> Result<String, ToolError> {
        let content = args["content"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("edit requires 'content'".into()))?;

        // Find the skill
        let skill_dir = self.find_skill(name).await.ok_or_else(|| {
            ToolError::NotFound(format!(
                "Skill '{}' not found. Use skill_list() to see available skills.",
                name
            ))
        })?;

        // Validate new content
        validation::validate_frontmatter(content).map_err(|e| {
            ToolError::InvalidArguments(format!("SKILL.md validation failed: {}", e))
        })?;

        let skill_md = skill_dir.join("SKILL.md");

        // Ensure skill directory exists (defensive — may have been removed externally)
        tokio::fs::create_dir_all(&skill_dir).await.map_err(|e| {
            ToolError::Execution(format!("Failed to create skill directory: {}", e))
        })?;

        // Write the new content
        tokio::fs::write(&skill_md, content)
            .await
            .map_err(|e| ToolError::Execution(format!("Failed to write SKILL.md: {}", e)))?;

        self.reload_skill_registry().await;

        // Bump patch telemetry (best-effort)
        crate::skills::usage::bump_patch(name);

        Ok(format!(
            "Skill '{}' updated at {}.",
            name,
            skill_md.display(),
        ))
    }

    async fn action_delete(&self, name: &str) -> Result<String, ToolError> {
        let skill_dir = self.find_skill(name).await.ok_or_else(|| {
            ToolError::NotFound(format!(
                "Skill '{}' not found. Use skill_list() to see available skills.",
                name
            ))
        })?;

        // Safety: refuse to delete bundled skills
        if !self.is_user_skill(name).await {
            return Err(ToolError::Execution(format!(
                "Cannot delete bundled skill '{}'. Only user-created skills can be deleted.",
                name,
            )));
        }

        // Safety: verify the skill directory is inside the user skills directory
        let user_skills_dir = Self::user_skills_dir();
        if !skill_dir.starts_with(&user_skills_dir) {
            return Err(ToolError::Execution(format!(
                "Safety check failed: skill '{}' at {} is not under user skills directory {}",
                name,
                skill_dir.display(),
                user_skills_dir.display(),
            )));
        }

        tokio::fs::remove_dir_all(&skill_dir)
            .await
            .map_err(|e| ToolError::Execution(format!("Failed to delete skill: {}", e)))?;

        self.reload_skill_registry().await;

        // Clean up usage telemetry
        crate::skills::usage::forget(name);

        Ok(format!(
            "Skill '{}' deleted (removed {})",
            name,
            skill_dir.display(),
        ))
    }

    async fn action_write_file(&self, name: &str, args: &Value) -> Result<String, ToolError> {
        let file_path = args["file_path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("write_file requires 'file_path'".into()))?;

        let content = args["content"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("write_file requires 'content'".into()))?;

        // Find the skill
        let skill_dir = self.find_skill(name).await.ok_or_else(|| {
            ToolError::NotFound(format!(
                "Skill '{}' not found. Use skill_list() to see available skills.",
                name
            ))
        })?;

        let target = Self::validate_and_resolve_file_path(&skill_dir, file_path)?;

        // Create parent directories
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ToolError::Execution(format!("Failed to create directory: {}", e)))?;
        }

        tokio::fs::write(&target, content)
            .await
            .map_err(|e| ToolError::Execution(format!("Failed to write file: {}", e)))?;

        self.reload_skill_registry().await;

        // Bump patch telemetry (best-effort)
        crate::skills::usage::bump_patch(name);

        Ok(format!(
            "Written supporting file: {} ({} bytes)",
            target.display(),
            content.len(),
        ))
    }

    /// Pin a skill (exclude from auto-stale/archive transitions).
    async fn action_pin(&self, name: &str) -> Result<String, ToolError> {
        // Verify the skill exists
        self.find_skill(name).await.ok_or_else(|| {
            ToolError::NotFound(format!(
                "Skill '{}' not found. Use skill_list() to see available skills.",
                name
            ))
        })?;
        crate::skills::usage::set_pinned(name, true);
        Ok(format!(
            "Skill '{}' pinned (excluded from auto-archival).",
            name
        ))
    }

    /// Unpin a skill.
    async fn action_unpin(&self, name: &str) -> Result<String, ToolError> {
        self.find_skill(name).await.ok_or_else(|| {
            ToolError::NotFound(format!(
                "Skill '{}' not found. Use skill_list() to see available skills.",
                name
            ))
        })?;
        crate::skills::usage::set_pinned(name, false);
        Ok(format!("Skill '{}' unpinned.", name))
    }

    /// Restore an archived skill back to the active skills directory.
    async fn action_restore(&self, name: &str) -> Result<String, ToolError> {
        let user_skills_dir = Self::user_skills_dir();
        let target_dir = user_skills_dir.join(name);
        let (ok, msg) = crate::skills::usage::restore_skill(name, &target_dir);
        if ok {
            self.reload_skill_registry().await;
            Ok(msg)
        } else {
            Err(ToolError::Execution(msg))
        }
    }

    /// List all archived skill names.
    async fn action_list_archived(&self) -> Result<String, ToolError> {
        let names = crate::skills::usage::list_archived();
        if names.is_empty() {
            Ok("No archived skills.".to_string())
        } else {
            Ok(format!("Archived skills:\n{}", names.join("\n")))
        }
    }

    /// Pause the curator.
    fn action_curator_pause(&self) -> String {
        crate::engine::curator::set_paused(true);
        "Curator paused. Automatic lifecycle maintenance will not run.".to_string()
    }

    /// Resume the curator.
    fn action_curator_resume(&self) -> String {
        crate::engine::curator::set_paused(false);
        "Curator resumed.".to_string()
    }

    /// Show curator status.
    fn action_curator_status(&self) -> String {
        if crate::engine::curator::is_paused() {
            "Curator is **paused**. Automatic lifecycle maintenance is disabled.".to_string()
        } else {
            "Curator is **active**. Automatic lifecycle maintenance will run when idle.".to_string()
        }
    }

    /// Validate a supporting file path and resolve it against the skill directory.
    fn validate_and_resolve_file_path(
        skill_dir: &Path,
        file_path: &str,
    ) -> Result<PathBuf, ToolError> {
        let path = Path::new(file_path);

        // Extract first component
        let first_component = path
            .components()
            .next()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .unwrap_or_default();

        if first_component.is_empty() {
            return Err(ToolError::InvalidArguments(
                "file_path cannot be empty.".into(),
            ));
        }

        if !ALLOWED_SUBDIRS.contains(&first_component.as_str()) {
            return Err(ToolError::InvalidArguments(format!(
                "file_path must start with one of: {}. Got: '{}'",
                ALLOWED_SUBDIRS.join(", "),
                first_component,
            )));
        }

        if file_path.contains("..") {
            return Err(ToolError::InvalidArguments(
                "Path traversal ('..') is not allowed.".into(),
            ));
        }

        // Must have a filename (not just a directory)
        if path.components().count() < 2 {
            return Err(ToolError::InvalidArguments(format!(
                "Provide a file path, not just a directory. Example: '{}/myfile.md'",
                first_component,
            )));
        }

        let target = skill_dir.join(file_path);

        // Verify it stays within skill dir
        if !target.starts_with(skill_dir) {
            return Err(ToolError::InvalidArguments(
                "Path escapes the skill directory.".into(),
            ));
        }

        Ok(target)
    }

    /// Reload the skill registry after a mutation.
    ///
    /// Re-scans the user skills directory and rebuilds the registry,
    /// effectively adding new skills and removing deleted ones.
    async fn reload_skill_registry(&self) {
        // Re-scan user skills (only user dir — bundled skills don't change at runtime)
        let user_skills_dir = Self::user_skills_dir();
        let (new_skills, new_categories) =
            crate::skills::loader::load_skills_from_dirs(&[user_skills_dir], "user");

        let mut reg = self.registry.lock().await;

        // Keep only non-user (bundled) skills from current registry
        let bundled: Vec<_> = reg
            .list_skills()
            .into_iter()
            .filter(|s| s.source != "user")
            .cloned()
            .collect();

        // Rebuild: bundled skills + fresh user skills
        let mut all_skills = bundled;
        all_skills.extend(new_skills);

        // Merge categories from bundled + user
        let mut all_categories = indexmap::IndexMap::new();
        // Copy bundled categories (but only the skill names that survived)
        for (cat, info) in reg.categories() {
            let bundled_names: Vec<String> = info
                .skill_names
                .iter()
                .filter(|n| all_skills.iter().any(|s| &s.name == *n))
                .cloned()
                .collect();
            if !bundled_names.is_empty() {
                all_categories.insert(
                    cat.clone(),
                    crate::skills::types::CategoryInfo {
                        description: info.description.clone(),
                        skill_names: bundled_names,
                    },
                );
            }
        }
        // Add user categories
        for (cat, info) in &new_categories {
            let entry = all_categories.entry(cat.clone()).or_insert_with(|| {
                crate::skills::types::CategoryInfo {
                    description: info.description.clone(),
                    skill_names: Vec::new(),
                }
            });
            for name in &info.skill_names {
                if !entry.skill_names.contains(name) {
                    entry.skill_names.push(name.clone());
                }
            }
        }

        *reg = crate::skills::registry::SkillRegistry::from_parts(all_skills, all_categories);

        // Update disk cache with all skill dirs (user + project) for manifest consistency
        if let Err(e) = crate::skills::index_cache::write_cache(
            &self.skill_dirs,
            &reg.list_skills().into_iter().cloned().collect::<Vec<_>>(),
            reg.categories(),
        ) {
            tracing::warn!(error = %e, "Failed to write skills cache after mutation");
        }

        tracing::debug!("Skill registry reloaded after mutation");
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // name_regex
    // -----------------------------------------------------------------------

    #[test]
    fn test_name_regex_valid() {
        assert!(name_regex().is_match("my-skill"));
        assert!(name_regex().is_match("git-workflow"));
        assert!(name_regex().is_match("a"));
        assert!(name_regex().is_match("test123"));
        assert!(name_regex().is_match("my.skill"));
        assert!(name_regex().is_match("my_skill"));
        assert!(name_regex().is_match("a.b-c_d"));
    }

    #[test]
    fn test_name_regex_invalid() {
        assert!(!name_regex().is_match(""));
        assert!(!name_regex().is_match("MySkill"));
        assert!(!name_regex().is_match("my skill"));
        assert!(!name_regex().is_match("123abc"));
        assert!(!name_regex().is_match("-skill"));
        assert!(!name_regex().is_match(".skill"));
        assert!(!name_regex().is_match("my/skill"));
    }

    // -----------------------------------------------------------------------
    // validate_and_resolve_file_path
    // -----------------------------------------------------------------------

    #[test]
    fn test_validate_and_resolve_valid() {
        let dir = Path::new("/tmp/test-skill");
        let result = SkillManageTool::validate_and_resolve_file_path(dir, "references/api.md");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), dir.join("references/api.md"));
    }

    #[test]
    fn test_validate_and_resolve_templates() {
        let dir = Path::new("/tmp/test-skill");
        let result = SkillManageTool::validate_and_resolve_file_path(dir, "templates/script.sh");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), dir.join("templates/script.sh"));
    }

    #[test]
    fn test_validate_and_resolve_empty_path() {
        let dir = Path::new("/tmp/test-skill");
        let result = SkillManageTool::validate_and_resolve_file_path(dir, "");
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_and_resolve_disallowed_subdir() {
        let dir = Path::new("/tmp/test-skill");
        let result = SkillManageTool::validate_and_resolve_file_path(dir, "src/main.rs");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("references"));
    }

    #[test]
    fn test_validate_and_resolve_path_traversal() {
        let dir = Path::new("/tmp/test-skill");
        let result =
            SkillManageTool::validate_and_resolve_file_path(dir, "references/../../etc/passwd");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains(".."));
    }

    #[test]
    fn test_validate_and_resolve_no_filename() {
        let dir = Path::new("/tmp/test-skill");
        let result = SkillManageTool::validate_and_resolve_file_path(dir, "references");
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_and_resolve_scripts_dir() {
        let dir = Path::new("/tmp/test-skill");
        let result = SkillManageTool::validate_and_resolve_file_path(dir, "scripts/deploy.sh");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), dir.join("scripts/deploy.sh"));
    }

    #[test]
    fn test_validate_and_resolve_assets_dir() {
        let dir = Path::new("/tmp/test-skill");
        let result = SkillManageTool::validate_and_resolve_file_path(dir, "assets/logo.png");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), dir.join("assets/logo.png"));
    }

    // -----------------------------------------------------------------------
    // ALLOWED_SUBDIRS
    // -----------------------------------------------------------------------

    #[test]
    fn test_allowed_subdirs_contains_expected() {
        assert!(ALLOWED_SUBDIRS.contains(&"references"));
        assert!(ALLOWED_SUBDIRS.contains(&"templates"));
        assert!(ALLOWED_SUBDIRS.contains(&"scripts"));
        assert!(ALLOWED_SUBDIRS.contains(&"assets"));
    }
}
