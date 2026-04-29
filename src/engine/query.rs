//! Core tool-aware conversation loop.
//!
//! Flow:
//! 1. Assemble messages (system + history + user)
//! 2. Call api_client.stream_messages() with tool schemas
//! 3. Consume StreamEvent:
//!    - TextDelta → push to TUI render
//!    - ToolUseStart/Delta → accumulate tool input
//!    - MessageComplete → record usage
//! 4. If stop_reason == ToolUse:
//!    a. Execute each tool
//!    b. Append tool results to messages
//!    c. goto 1
//! 5. If stop_reason == EndTurn or turn >= max_turns, done

use std::io::Write;
use std::pin::Pin;

use futures::{Stream, StreamExt};
use serde_json::Value;

use crate::api::types::{ApiError, ContentBlock, StopReason, StreamEvent, Usage};
use crate::engine::query_engine::QueryEngine;
use crate::permissions::checker;
use crate::tools::base::{ToolContext, ToolError};

/// Result of a completed query.
#[derive(Debug)]
pub struct QueryResult {
    pub text: String,
    pub stop_reason: StopReason,
    pub usage: Usage,
    pub tool_calls: u32,
}

/// A collected tool use from the stream.
#[derive(Debug, Clone)]
struct CollectedToolUse {
    id: String,
    name: String,
    input_json: String,
}

impl QueryEngine {
    /// Run a user query through the LLM with streaming output and tool execution.
    pub async fn query(&mut self, user_input: &str) -> Result<QueryResult, ApiError> {
        self.history.push_user(user_input);

        let mut turn = 0;
        let mut final_text = String::new();
        let mut final_usage = Usage::default();
        let mut final_stop = StopReason::EndTurn;
        let mut total_tool_calls = 0u32;

        // Get tool schemas for the API (OpenAI format)
        let tool_schemas = self.tools.schemas();

        loop {
            turn += 1;
            if turn > self.max_turns {
                println!("\n[warning] max turns ({}) reached", self.max_turns);
                break;
            }

            let messages = self.history.to_api_messages();

            let stream = self
                .client
                .stream_messages(
                    &self.model,
                    &self.system_prompt,
                    &messages,
                    &tool_schemas,
                    self.max_tokens,
                )
                .await?;

            let mut assistant_text = String::new();
            let mut tool_uses: Vec<CollectedToolUse> = Vec::new();
            let mut current_tool: Option<CollectedToolUse> = None;

            // Consume the stream
            let mut pinned: Pin<Box<dyn Stream<Item = Result<StreamEvent, ApiError>> + Send>> =
                Box::pin(stream);

            while let Some(event) = pinned.as_mut().next().await {
                match event? {
                    StreamEvent::TextDelta(delta) => {
                        print!("{}", delta);
                        let _ = std::io::stdout().flush();
                        assistant_text.push_str(&delta);
                    }
                    StreamEvent::ToolUseStart { id, name, input_json } => {
                        tracing::debug!("ToolUseStart: id={}, name={}, has_input={}", id, name, input_json.is_some());
                        // Finalize previous tool if any
                        if let Some(tool) = current_tool.take() {
                            tool_uses.push(tool);
                        }
                        current_tool = Some(CollectedToolUse {
                            id,
                            name,
                            input_json: input_json.unwrap_or_default(),
                        });
                    }
                    StreamEvent::ToolUseDelta { id, delta_json } => {
                        tracing::debug!("ToolUseDelta: id={}, delta={}", id, delta_json);
                        if let Some(ref mut tool) = current_tool {
                            if tool.id == id {
                                tool.input_json.push_str(&delta_json);
                            }
                        }
                    }
                    StreamEvent::MessageComplete { stop_reason, usage } => {
                        final_usage.input_tokens += usage.input_tokens;
                        final_usage.output_tokens += usage.output_tokens;
                        final_stop = stop_reason;
                    }
                    StreamEvent::Error(e) => {
                        return Err(ApiError::Stream(e));
                    }
                }
            }

            // Finalize last tool
            if let Some(tool) = current_tool.take() {
                tool_uses.push(tool);
            }

            // Build assistant content blocks
            let mut assistant_blocks: Vec<ContentBlock> = Vec::new();
            if !assistant_text.is_empty() {
                assistant_blocks.push(ContentBlock::Text {
                    text: assistant_text.clone(),
                });
            }
            for tu in &tool_uses {
                let input: Value = if tu.input_json.is_empty() {
                    Value::Object(Default::default())
                } else {
                    serde_json::from_str(&tu.input_json).unwrap_or(Value::Object(Default::default()))
                };
                assistant_blocks.push(ContentBlock::ToolUse {
                    id: tu.id.clone(),
                    name: tu.name.clone(),
                    input,
                });
            }

            // Push assistant message to history
            self.history.push_assistant_blocks(assistant_blocks);
            final_text.clone_from(&assistant_text);

            // If no tool uses, we're done
            if tool_uses.is_empty() {
                break;
            }

            // Execute tools
            let ctx = ToolContext::new(std::env::current_dir().unwrap_or_default());
            let mut tool_results: Vec<ContentBlock> = Vec::new();

            for tu in &tool_uses {
                println!(); // newline before tool output
                println!("[tool: {}]", tu.name);
                tracing::debug!("Tool input_json: {}", tu.input_json);
                total_tool_calls += 1;

                // Check permissions
                let permitted = checker::check_permission(
                    &self.permission_mode,
                    &tu.name,
                    &format!("Execute tool '{}'", tu.name),
                    &tu.input_json,
                )
                .unwrap_or(false);

                if !permitted {
                    tool_results.push(ContentBlock::ToolResult {
                        tool_use_id: tu.id.clone(),
                        content: "Permission denied by user.".into(),
                        is_error: Some(true),
                    });
                    println!("[denied by user]");
                    continue;
                }

                // Execute the tool
                let input: Value = if tu.input_json.is_empty() {
                    Value::Object(Default::default())
                } else {
                    serde_json::from_str(&tu.input_json).unwrap_or(Value::Object(Default::default()))
                };

                match self.tools.execute(&tu.name, input, &ctx).await {
                    Ok(result) => {
                        // Truncate very long outputs
                        let display = if result.len() > 2000 {
                            format!("{}...(truncated, {} chars total)", &result[..2000], result.len())
                        } else {
                            result.clone()
                        };
                        println!("{}", display);
                        tool_results.push(ContentBlock::ToolResult {
                            tool_use_id: tu.id.clone(),
                            content: result,
                            is_error: None,
                        });
                    }
                    Err(ToolError::Execution(e)) => {
                        println!("[error] {}", e);
                        tool_results.push(ContentBlock::ToolResult {
                            tool_use_id: tu.id.clone(),
                            content: format!("Error: {}", e),
                            is_error: Some(true),
                        });
                    }
                    Err(ToolError::InvalidArguments(e)) => {
                        println!("[invalid arguments] {}", e);
                        tool_results.push(ContentBlock::ToolResult {
                            tool_use_id: tu.id.clone(),
                            content: format!("Invalid arguments: {}", e),
                            is_error: Some(true),
                        });
                    }
                    Err(e) => {
                        println!("[error] {}", e);
                        tool_results.push(ContentBlock::ToolResult {
                            tool_use_id: tu.id.clone(),
                            content: format!("Error: {}", e),
                            is_error: Some(true),
                        });
                    }
                }
            }

            // Push tool results as a user message
            self.history.push_tool_results(tool_results);

            // Continue the loop to get the next LLM response
        }

        Ok(QueryResult {
            text: final_text,
            stop_reason: final_stop,
            usage: final_usage,
            tool_calls: total_tool_calls,
        })
    }
}
