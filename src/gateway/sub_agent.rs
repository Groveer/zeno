//! Sub-agent event handling — SubAgentEvent → UiCommand mapping.
//!
//! Sub-agent events (`delegate_task` tool) travel through a dedicated
//! `mpsc::UnboundedSender<SubAgentEvent>` channel (owned by `ToolContext`),
//! separate from the main `EngineEvent` channel to avoid head-of-line blocking.
//!
//! The Gateway owns the receiver end of this channel and drains it each
//! render cycle, converting SubAgentEvent into UiCommands that flow through
//! the standard transport → App.update() → component tree path.

use crate::engine::sub_agent::SubAgentEvent;
use crate::gateway::UiCommand;
use crate::utils::truncate;

use super::Gateway;

impl Gateway {
    /// Map a SubAgentEvent into one or more UiCommands.
    ///
    /// Called from `drain_sub_agent_events()` each render cycle. Each event
    /// is translated into one or more UI commands for display in the output panel.
    pub fn handle_sub_agent_event(&self, event: SubAgentEvent) -> Vec<UiCommand> {
        match event {
            SubAgentEvent::Started {
                task_index,
                goal,
                tools,
            } => {
                let tools_str = if tools.is_empty() {
                    String::new()
                } else {
                    format!(" [tools: {}]", tools.join(", "))
                };
                vec![UiCommand::SubAgentStarted {
                    summary: format!(
                        "sub-agent #{}: {}{}",
                        task_index,
                        truncate(&goal, 60),
                        tools_str,
                    ),
                }]
            }
            SubAgentEvent::Thinking { task_index, text } => {
                let short = truncate(&text, 80);
                if short.trim().is_empty() {
                    vec![]
                } else {
                    vec![UiCommand::SubAgentThought(format!(
                        "#{}: {}",
                        task_index,
                        short.to_string(),
                    ))]
                }
            }
            SubAgentEvent::ToolStarted {
                task_index,
                tool,
                input_summary,
            } => {
                let display = if input_summary.is_empty() {
                    tool
                } else {
                    format!("{} ({})", tool, input_summary)
                };
                vec![UiCommand::SubAgentToolStart {
                    label: format!("sub-agent #{}: {}", task_index, display),
                }]
            }
            SubAgentEvent::ToolCompleted {
                task_index,
                tool,
                result_bytes,
                is_error,
            } => {
                let icon = if is_error { "✗" } else { "✓" };
                vec![UiCommand::SubAgentToolEnd {
                    label: format!("#{} {} {} ({} bytes)", task_index, icon, tool, result_bytes),
                }]
            }
            SubAgentEvent::Status {
                task_index,
                message,
            } => {
                vec![UiCommand::SubAgentStatus {
                    message: format!("sub-agent #{}: {}", task_index, message),
                }]
            }
            SubAgentEvent::Completed { task_index, result } => {
                let status = if result.interrupted {
                    "interrupted"
                } else if result.error.is_some() {
                    "failed"
                } else {
                    "completed"
                };
                vec![UiCommand::SubAgentCompleted {
                    summary: format!(
                        "sub-agent #{} {} ({} calls, {:.1}s, {} chars)",
                        task_index,
                        status,
                        result.api_calls,
                        result.duration_seconds,
                        result.summary.len(),
                    ),
                }]
            }
        }
    }
}
