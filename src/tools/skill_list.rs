use crate::skills::registry::SkillRegistry;
use crate::tools::base::{Tool, ToolContext, ToolError};
use async_trait::async_trait;
use serde_json::{Value, json};

pub struct SkillListTool {
    registry: SkillRegistry,
}

impl SkillListTool {
    pub fn new(registry: SkillRegistry) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for SkillListTool {
    fn name(&self) -> &str {
        "skill_list"
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "skill_list",
                "description": "Browse skills by category or tag.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "category": {
                            "type": "string",
                            "description": "Category to filter by (e.g. 'software-development', 'wayland', 'research')."
                        }
                    }
                }
            }
        })
    }

    async fn execute(&self, arguments: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        let category = arguments.get("category").and_then(|v| v.as_str());

        match category {
            Some(cat) => {
                let skills = self.registry.list_by_category(cat);
                if skills.is_empty() {
                    let cats: Vec<String> = self
                        .registry
                        .categories()
                        .keys()
                        .map(|s| format!("- {}", s))
                        .collect();
                    return Ok(format!(
                        "No skills found in category '{}'. Available categories:\n{}",
                        cat,
                        if cats.is_empty() {
                            "(none — skills are not categorized)".into()
                        } else {
                            cats.join("\n")
                        }
                    ));
                }
                let lines: Vec<String> = skills
                    .iter()
                    .map(|s| format!("- **{}**: {}", s.name, s.description))
                    .collect();
                Ok(format!(
                    "Skills in '{}' ({}):\n{}",
                    cat,
                    skills.len(),
                    lines.join("\n")
                ))
            }
            None => {
                let categories = self.registry.categories();
                if categories.is_empty() {
                    let skills = self.registry.list_skills();
                    if skills.is_empty() {
                        return Ok("No skills available.".into());
                    }
                    let lines: Vec<String> = skills
                        .iter()
                        .map(|s| format!("- **{}** ({}) — {}", s.name, s.category, s.description))
                        .collect();
                    return Ok(format!(
                        "Available skills ({}):\n{}",
                        skills.len(),
                        lines.join("\n")
                    ));
                }
                let mut lines = vec![format!(
                    "Skill categories ({} total skills):\n",
                    self.registry.len()
                )];
                let mut cats: Vec<_> = categories.iter().collect();
                cats.sort_by_key(|(k, _)| *k);
                for (cat, info) in cats {
                    let desc = if info.description.is_empty() {
                        String::new()
                    } else {
                        format!(" — {}", info.description)
                    };
                    let names = info.skill_names.join(", ");
                    lines.push(format!(
                        "- **{}** ({}): {}{}",
                        cat,
                        info.skill_names.len(),
                        names,
                        desc
                    ));
                }
                Ok(lines.join("\n"))
            }
        }
    }
}
