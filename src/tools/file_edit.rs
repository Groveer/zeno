//! File edit tool — find-and-replace within a file.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::base::{Tool, ToolContext, ToolError};

pub struct FileEditTool;

impl FileEditTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for FileEditTool {
    fn name(&self) -> &str {
        "file_edit"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "file_edit",
                "description": "Perform a find-and-replace edit within a file. The old_string must be unique in the file. Use this for targeted edits instead of rewriting entire files.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file to edit."
                        },
                        "old_string": {
                            "type": "string",
                            "description": "The exact text to find and replace. Must be unique within the file."
                        },
                        "new_string": {
                            "type": "string",
                            "description": "The replacement text. Use empty string to delete the matched text."
                        },
                        "replace_all": {
                            "type": "boolean",
                            "description": "Replace all occurrences instead of just the first (default: false).",
                            "default": false
                        }
                    },
                    "required": ["path", "old_string", "new_string"]
                }
            }
        })
    }

    async fn execute(
        &self,
        arguments: Value,
        ctx: &ToolContext,
    ) -> Result<String, ToolError> {
        let path = arguments["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("missing 'path'".into()))?;
        let old_string = arguments["old_string"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("missing 'old_string'".into()))?;
        let new_string = arguments["new_string"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("missing 'new_string'".into()))?;
        let replace_all = arguments.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false);

        let resolved = ctx.resolve_path(path);

        if !resolved.exists() {
            return Err(ToolError::NotFound(format!("File not found: {}", resolved.display())));
        }

        let content = tokio::fs::read_to_string(&resolved).await?;

        if old_string.is_empty() {
            return Err(ToolError::InvalidArguments(
                "old_string cannot be empty".into(),
            ));
        }

        let count = if replace_all {
            let matches = content.matches(old_string).count();
            if matches == 0 {
                return Err(ToolError::Execution(format!(
                    "old_string not found in {}",
                    resolved.display()
                )));
            }
            let new_content = content.replace(old_string, new_string);
            tokio::fs::write(&resolved, &new_content).await?;
            matches
        } else {
            match content.find(old_string) {
                None => {
                    return Err(ToolError::Execution(format!(
                        "old_string not found in {}",
                        resolved.display()
                    )));
                }
                Some(idx) => {
                    // Verify uniqueness
                    if content[idx + old_string.len()..].contains(old_string) {
                        return Err(ToolError::Execution(
                            "old_string is not unique in the file. Use replace_all=true to replace all occurrences.".into(),
                        ));
                    }
                    let mut new_content = String::with_capacity(
                        content.len() - old_string.len() + new_string.len(),
                    );
                    new_content.push_str(&content[..idx]);
                    new_content.push_str(new_string);
                    new_content.push_str(&content[idx + old_string.len()..]);
                    tokio::fs::write(&resolved, &new_content).await?;
                    1
                }
            }
        };

        Ok(format!(
            "Replaced {} occurrence(s) in {}",
            count,
            resolved.display()
        ))
    }
}
