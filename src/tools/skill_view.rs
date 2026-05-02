use crate::skills::registry::SkillRegistry;
use crate::tools::base::{Tool, ToolContext, ToolError};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::Path;

pub struct SkillViewTool {
    registry: SkillRegistry,
}

impl SkillViewTool {
    pub fn new(registry: SkillRegistry) -> Self {
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
                "description": "Load a specific skill's full instructions.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Skill name to load (e.g. 'coding-principles', 'tdd')."
                        },
                        "file_path": {
                            "type": "string",
                            "description": "Optional: load a specific file within the skill directory (e.g. 'references/api.md')."
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

        let skill = match self.registry.get_fuzzy(name) {
            Some(s) => s,
            None => {
                let available: Vec<String> = self
                    .registry
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

        if let Some(fp) = file_path
            && let Some(ref skill_path) = skill.path
        {
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
                    if let Some(listed) = list_skill_files(dir) {
                        return Err(ToolError::NotFound(format!(
                            "File '{}' not found in skill '{}'. Available files:\n{}",
                            fp, skill.name, listed
                        )));
                    }
                    return Err(ToolError::NotFound(format!(
                        "File '{}' not found in skill '{}'.",
                        fp, skill.name
                    )));
                }
            }
        }

        if skill.content.is_empty()
            && let Some(ref path_str) = skill.path
        {
            return match std::fs::read_to_string(path_str) {
                Ok(content) => Ok(content),
                Err(e) => Err(ToolError::Execution(format!(
                    "Cannot read skill file '{}': {}",
                    path_str, e
                ))),
            };
        }
        Ok(skill.content.clone())
    }
}

fn list_skill_files(dir: &Path) -> Option<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return None;
    };
    let mut files: Vec<String> = entries
        .flatten()
        .filter(|e| {
            e.path().is_file()
                && e.path()
                    .file_name()
                    .map(|n| n != "SKILL.md")
                    .unwrap_or(false)
        })
        .map(|e| {
            e.path()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
        .collect();

    if let Ok(sub_entries) = std::fs::read_dir(dir) {
        for sub in sub_entries.flatten() {
            if sub.path().is_dir() {
                let dir_name = sub
                    .path()
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if let Ok(sub_files) = std::fs::read_dir(sub.path()) {
                    for f in sub_files.flatten() {
                        if f.path().is_file() {
                            files.push(format!(
                                "{}/{}",
                                dir_name,
                                f.path()
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_default()
                            ));
                        }
                    }
                }
            }
        }
    }

    if files.is_empty() {
        None
    } else {
        files.sort();
        Some(
            files
                .iter()
                .map(|f| format!("- {}", f))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}
