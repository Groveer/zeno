use std::path::{Component, Path};
use std::sync::Arc;

use crate::skills::registry::SkillRegistry;
use crate::tools::base::{Tool, ToolContext, ToolError};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Mutex;

pub struct SkillViewTool {
    registry: Arc<Mutex<SkillRegistry>>,
}

impl SkillViewTool {
    pub fn new(registry: Arc<Mutex<SkillRegistry>>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for SkillViewTool {
    fn name(&self) -> &str {
        "skill_view"
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "skill_view",
                "description": "Load a skill's full instructions (Tier 2). After loading the main content, check the linked files section for references, templates, scripts, or assets that can be accessed via file_path.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Skill name to load (e.g. 'coding-principles', 'tdd')."
                        },
                        "file_path": {
                            "type": "string",
                            "description": "Optional: load a specific linked file within the skill directory (e.g. 'references/api.md', 'templates/config.yaml', 'scripts/validate.py'). After loading the main SKILL.md, check the linked files section for available paths."
                        }
                    },
                    "required": ["name"]
                }
            }
        })
    }

    async fn execute(&self, arguments: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        let name = arguments.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let file_path = arguments.get("file_path").and_then(|v| v.as_str());

        if name.is_empty() {
            return Ok("Specify a skill name to view.".into());
        }

        let registry = self.registry.lock().await;

        let skill = match registry.get_fuzzy(name) {
            Some(s) => s,
            None => {
                let available: Vec<String> = registry
                    .list_skills()
                    .iter()
                    .map(|s| format!("{} ({})", s.name, s.category))
                    .collect();
                if available.is_empty() {
                    return Ok(format!(
                        "Skill '{}' not found. No skills are currently loaded.",
                        name
                    ));
                }
                return Ok(format!(
                    "Skill '{}' not found. Available:\n{}",
                    name,
                    available.join("\n")
                ));
            }
        };

        // Bump view telemetry (best-effort)
        crate::skills::usage::bump_view(&skill.name);
        // Bump use telemetry (best-effort) — skill_view means the skill is
        // actively loaded for use.
        crate::skills::usage::bump_use(&skill.name);

        if let Some(fp) = file_path
            && let Some(ref skill_path) = skill.path
        {
            // P0: Path traversal protection
            if has_path_traversal(fp) {
                return Err(ToolError::NotFound(
                    "Path traversal ('..') is not allowed. Use a relative path within the skill directory.".into(),
                ));
            }

            let skill_dir = Path::new(skill_path).parent();
            if let Some(dir) = skill_dir {
                let full_path = dir.join(fp);
                if full_path.exists() {
                    match std::fs::read_to_string(&full_path) {
                        Ok(content) => return Ok(content),
                        Err(e) => {
                            return Err(ToolError::NotFound(format!(
                                "Cannot read '{}': {}",
                                full_path.display(),
                                e
                            )));
                        }
                    }
                } else {
                    return Err(ToolError::NotFound(format!(
                        "File '{}' not found in skill '{}'.\n{}",
                        fp,
                        skill.name,
                        format_linked_files_hint(dir)
                    )));
                }
            }
        }

        // Load SKILL.md content (Tier 2)
        let content = if skill.content.is_empty()
            && let Some(ref path_str) = skill.path
        {
            match std::fs::read_to_string(path_str) {
                Ok(c) => c,
                Err(e) => {
                    return Err(ToolError::Execution(format!(
                        "Cannot read skill file '{}': {}",
                        path_str, e
                    )));
                }
            }
        } else {
            skill.content.clone()
        };

        // Append linked files hint (Tier 2 → linked files progressive disclosure)
        if let Some(ref path_str) = skill.path {
            if let Some(dir) = Path::new(path_str).parent() {
                if let Some(linked) = list_linked_files(dir) {
                    return Ok(format!(
                        "{}\n\n---\nLinked files (use skill_view with file_path to access):\n{}",
                        content, linked
                    ));
                }
            }
        }

        Ok(content)
    }
}

/// Check if a path contains `..` traversal components.
fn has_path_traversal(path: &str) -> bool {
    Path::new(path)
        .components()
        .any(|c| matches!(c, Component::ParentDir))
}

/// List linked files in a skill directory, categorized by subdirectory.
/// Returns `None` if no linked files exist.
fn list_linked_files(dir: &Path) -> Option<String> {
    let mut refs: Vec<String> = Vec::new();
    let mut templates: Vec<String> = Vec::new();
    let mut assets: Vec<String> = Vec::new();
    let mut scripts: Vec<String> = Vec::new();
    let mut other: Vec<String> = Vec::new();

    let Ok(entries) = std::fs::read_dir(dir) else {
        return None;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        if path.is_file() && file_name != "SKILL.md" {
            other.push(file_name);
        } else if path.is_dir() {
            let dir_name = file_name;
            let Ok(sub_entries) = std::fs::read_dir(&path) else {
                continue;
            };
            for f in sub_entries.flatten() {
                if f.path().is_file() {
                    let fname = f
                        .path()
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                        .to_string();
                    let rel = format!("{}/{}", dir_name, fname);
                    match dir_name.as_str() {
                        "references" => refs.push(rel),
                        "templates" => templates.push(rel),
                        "assets" => assets.push(rel),
                        "scripts" => scripts.push(rel),
                        _ => other.push(rel),
                    }
                }
            }
        }
    }

    let mut sections: Vec<(Vec<String>, &str)> = Vec::new();
    for (items, label) in [
        (&refs, "references"),
        (&templates, "templates"),
        (&assets, "assets"),
        (&scripts, "scripts"),
        (&other, "other"),
    ] {
        if !items.is_empty() {
            sections.push((items.clone(), label));
        }
    }

    if sections.is_empty() {
        return None;
    }

    let mut output = String::new();
    for (items, label) in sections {
        let mut sorted = items;
        sorted.sort();
        output.push_str(&format!("{}/\n", label));
        for item in &sorted {
            output.push_str(&format!("  - {}\n", item));
        }
    }

    Some(output.trim_end().to_string())
}

/// Format a hint message showing all linked files for a skill.
/// Used when a requested file_path is not found.
fn format_linked_files_hint(dir: &Path) -> String {
    match list_linked_files(dir) {
        Some(files) => format!("Available files:\n{}", files),
        None => "No additional files in this skill.".into(),
    }
}
