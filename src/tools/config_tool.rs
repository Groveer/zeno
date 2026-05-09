//! Config tool — read current zeno configuration.
use super::base::{Tool, ToolContext, ToolError};
use async_trait::async_trait;
use serde_json::{Value, json};

pub struct ConfigTool;
impl ConfigTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for ConfigTool {
    fn name(&self) -> &str {
        "config"
    }
    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "config",
                "description": "Read or display zeno configuration.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["show"],
                            "description": "Action to perform."
                        }
                    },
                    "required": ["action"]
                }
            }
        })
    }

    async fn execute(&self, arguments: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        let action = arguments["action"].as_str().unwrap_or("show");
        match action {
            "show" => {
                let config_path = crate::config::paths::config_path();
                if config_path.exists() {
                    let content = tokio::fs::read_to_string(&config_path).await?;
                    let masked = mask_api_keys(&content);
                    Ok(format!("Config at {}:\n{}", config_path.display(), masked))
                } else {
                    Ok("No config file found. Using defaults.".into())
                }
            }
            other => Err(ToolError::InvalidArguments(format!(
                "Unknown action: {}",
                other
            ))),
        }
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }
}

fn mask_api_keys(content: &str) -> String {
    let lines: Vec<String> = content
        .lines()
        .map(|line| {
            if line.contains("api_key:")
                && let Some(idx) = line.find(':')
            {
                let prefix = &line[..=idx];
                let rest = line[idx + 1..].trim();
                if rest.starts_with('"') && rest.len() > 12 {
                    return format!("{} \"{}...\"", prefix, &rest[1..9]);
                } else if !rest.starts_with('"') && rest.len() > 8 && !rest.starts_with('$') {
                    return format!("{} {}...", prefix, &rest[..8]);
                }
            }
            line.to_string()
        })
        .collect();
    lines.join("\n")
}
