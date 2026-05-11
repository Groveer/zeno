//! Ask user tool — prompt the user for input during a tool loop.
//!
//! Supports two modes:
//! 1. **Multiple choice** — provide up to 4 choices. The user picks one
//!    or types their own answer via a 5th 'Other' option.
//! 2. **Open-ended** — omit choices entirely. The user types a free-form response.

use super::base::{Tool, ToolContext, ToolError};
use crate::engine::tui_events::UiEvent;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

/// Maximum number of predefined choices the agent can offer.
/// A 5th "Other (type your answer)" option is always appended by the UI.
const MAX_CHOICES: usize = 4;

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
                "description": concat!(
                    "Ask the user a question and get their response. ",
                    "Use when you need clarification, feedback, or a decision before proceeding. ",
                    "Supports two modes:\n\n",
                    "1. **Multiple choice** — provide up to 4 choices. The user picks one ",
                    "or types their own answer via a 5th 'Other' option.\n",
                    "2. **Open-ended** — omit choices entirely. The user types a free-form ",
                    "response.\n\n",
                    "Use this tool when:\n",
                    "- The task is ambiguous and you need the user to choose an approach\n",
                    "- Requirements are unclear or underspecified\n",
                    "- A decision has meaningful trade-offs the user should weigh in on\n\n",
                    "Do NOT use this tool for simple yes/no confirmation of dangerous ",
                    "commands (the permission system handles that). Prefer making a ",
                    "reasonable default choice yourself when the decision is low-stakes."
                ),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "question": {
                            "type": "string",
                            "description": "The question to present to the user."
                        },
                        "choices": {
                            "type": "array",
                            "items": {"type": "string"},
                            "maxItems": MAX_CHOICES,
                            "description": concat!(
                                "Up to 4 answer choices. Omit this parameter entirely to ",
                                "ask an open-ended question. When provided, the UI ",
                                "automatically appends an 'Other (type your answer)' option."
                            )
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
            .ok_or_else(|| ToolError::InvalidArguments("missing 'question'".into()))?
            .trim()
            .to_string();

        if question.is_empty() {
            return Err(ToolError::InvalidArguments(
                "question cannot be empty".into(),
            ));
        }

        // Parse optional choices (max 4)
        let choices: Option<Vec<String>> = arguments
            .get("choices")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .take(MAX_CHOICES)
                    .collect()
            })
            .filter(|v: &Vec<String>| !v.is_empty());

        if let Some(sender) = &ctx.ask_sender {
            let (tx, rx) = oneshot::channel();
            let response_tx = Arc::new(Mutex::new(Some(tx)));

            // Build display string that includes choices if present
            let display_question = if let Some(ref ch) = choices {
                let choices_str = ch
                    .iter()
                    .enumerate()
                    .map(|(i, c)| format!("{}. {}", i + 1, c))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("{}\n\n{}", question, choices_str)
            } else {
                question.clone()
            };

            sender
                .send(UiEvent::AskUser {
                    question: display_question,
                    response_tx,
                })
                .map_err(|e| ToolError::Execution(format!("Failed to send ask event: {}", e)))?;

            let response = rx
                .await
                .map_err(|_| ToolError::Execution("Ask response channel dropped".into()))?;

            // Return structured JSON so the LLM knows what it asked and what the user said
            let result = json!({
                "question": question,
                "choices_offered": choices,
                "user_response": response.trim(),
            });
            Ok(serde_json::to_string(&result).unwrap_or_else(|_| response))
        } else {
            // Fallback for non-TUI mode (e.g. headless)
            eprint!("[ask] {} ", question);
            if let Some(ref ch) = choices {
                eprintln!();
                for (i, c) in ch.iter().enumerate() {
                    eprintln!("  {}. {}", i + 1, c);
                }
            }
            io::stderr().flush()?;
            let mut response = String::new();
            io::stdin().read_line(&mut response)?;
            let result = json!({
                "question": question,
                "choices_offered": choices,
                "user_response": response.trim(),
            });
            Ok(serde_json::to_string(&result).unwrap_or_else(|_| response.trim().to_string()))
        }
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }
}
