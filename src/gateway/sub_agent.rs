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
            SubAgentEvent::Started { task_index, goal } => {
                vec![UiCommand::SubAgentStarted {
                    summary: format!("sub-agent #{}: {}", task_index, truncate(&goal, 60),),
                }]
            }
            SubAgentEvent::Thinking { task_index, text } => {
                let short = truncate(&text, 80);
                if short.trim().is_empty() {
                    vec![]
                } else {
                    vec![UiCommand::SubAgentProgress {
                        task_index,
                        line: short.into_owned(),
                    }]
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
                vec![UiCommand::SubAgentProgress {
                    task_index,
                    line: display,
                }]
            }
            SubAgentEvent::ToolCompleted {
                task_index,
                tool,
                is_error,
            } => {
                if is_error {
                    vec![UiCommand::SubAgentProgress {
                        task_index,
                        line: format!("{} failed", tool),
                    }]
                } else {
                    // Successful tool completion is too noisy — skip.
                    vec![]
                }
            }
            SubAgentEvent::Status {
                task_index,
                message,
            } => {
                vec![UiCommand::SubAgentProgress {
                    task_index,
                    line: message,
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
                vec![UiCommand::SubAgentProgress {
                    task_index,
                    line: format!(
                        "{} ({} calls, {:.1}s)",
                        status, result.api_calls, result.duration_seconds,
                    ),
                }]
            }
        }
    }
}
