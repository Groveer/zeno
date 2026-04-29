//! Ask user tool — prompt the user for input during a tool loop.

use std::io::{self, Write};

use async_trait::async_trait;
use serde_json::{json, Value};

use super::base::{Tool, ToolContext, ToolError};

pub struct AskUserTool;

impl AskUserTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "ask_user",
                "description": "Ask the user a question and get their response. Use when you need clarification or a decision from the user.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "question": {
                            "type": "string",
                            "description": "The question to ask the user."
                        }
                    },
                    "required": ["question"]
                }
            }
        })
    }

    async fn execute(
        &self,
        arguments: Value,
        _ctx: &ToolContext,
    ) -> Result<String, ToolError> {
        let question = arguments["question"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("missing 'question'".into()))?;

        eprint!("[ask] {} ", question);
        io::stderr().flush()?;

        let mut response = String::new();
        io::stdin().read_line(&mut response)?;
        let response = response.trim().to_string();

        if response.is_empty() {
            Ok("(user provided no response)".into())
        } else {
            Ok(response)
        }
    }
}
