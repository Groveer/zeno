//! Ask user tool — prompt the user for input during a tool loop.
use super::base::{Tool, ToolContext, ToolError};
use crate::engine::tui_events::UiEvent;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

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
                "description": "Ask the user a question and get their response. Use when you need clarification or a decision.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "question": {
                            "type": "string",
                            "description": "Question to ask."
                        }
                    },
                    "required": ["question"]
                }
            }
        })
    }

    async fn execute(&self, arguments: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let question = arguments["question"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("missing 'question'".into()))?;

        if let Some(sender) = &ctx.ask_sender {
            let (tx, rx) = oneshot::channel();
            let response_tx = Arc::new(Mutex::new(Some(tx)));
            sender
                .send(UiEvent::AskUser {
                    question: question.to_string(),
                    response_tx,
                })
                .map_err(|e| ToolError::Execution(format!("Failed to send ask event: {}", e)))?;
            let response = rx
                .await
                .map_err(|_| ToolError::Execution("Ask response channel dropped".into()))?;
            if response.is_empty() {
                Ok("(user provided no response)".into())
            } else {
                Ok(response)
            }
        } else {
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

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }
}
