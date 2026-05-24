//! EngineEvent → UiCommand mapping.
//!
//! This module implements the `event_map()` method on Gateway,
//! which converts streaming engine events into discrete UI commands.

use crate::engine::tui_events::EngineEvent;
use crate::ui::status_bar::AppMode;

use super::{Gateway, UiCommand};

impl Gateway {
    /// Map an EngineEvent into one or more UiCommands.
    ///
    /// This is the core event routing function: Engine → Gateway → UI.
    /// Each engine event is translated into zero or more UI commands
    /// that are sent to the appropriate components.
    ///
    /// Some events require side effects (e.g., setting permission_allow_all).
    /// These are handled directly in `drain_engine_events()` before calling
    /// this method.
    pub fn event_map(&self, event: EngineEvent) -> Vec<UiCommand> {
        match event {
            // ── Text stream ──────────────────────────────────────
            EngineEvent::TextDelta(text) => vec![UiCommand::AppendText(text)],
            EngineEvent::ReasoningDelta(text) => vec![UiCommand::AppendReasoning(text)],
            EngineEvent::ClearOutput => vec![UiCommand::ClearOutput],

            // ── Tool events ──────────────────────────────────────
            EngineEvent::ToolStart {
                name,
                input_summary,
            } => {
                vec![UiCommand::ToolStart {
                    name,
                    input_summary,
                }]
            }
            EngineEvent::ToolOutput { name, output } => {
                // ToolOutput is the complete tool execution output,
                // not a streaming chunk. Maps directly to ToolComplete.
                // TODO(protocol): If streaming tool output is introduced,
                // split into ToolOutputChunk and ToolCompleted.
                vec![UiCommand::ToolComplete { name, output }]
            }
            EngineEvent::ToolDiff { name, diff } => {
                vec![UiCommand::ToolDiff { name, diff }]
            }
            EngineEvent::ToolError { name, error } => {
                vec![UiCommand::ToolError { name, error }]
            }

            // ── Query lifecycle ──────────────────────────────────
            EngineEvent::QueryDone {
                text,
                tool_calls: _,
                tokens,
            } => {
                let mut cmds = vec![
                    UiCommand::SetMode(AppMode::Idle),
                    UiCommand::UpdateTokens(tokens),
                    UiCommand::ClearSteerQueue,
                ];
                if !text.is_empty() {
                    cmds.insert(0, UiCommand::AppendText(text));
                }
                cmds
            }
            EngineEvent::Interrupted => {
                vec![
                    UiCommand::ShowStatus("⏸  Interrupted — press Ctrl+C again to quit.".into()),
                    UiCommand::SetMode(AppMode::Idle),
                    UiCommand::ClearSteerQueue,
                ]
            }
            EngineEvent::Error(err) => {
                vec![UiCommand::ShowError(err), UiCommand::SetMode(AppMode::Idle)]
            }
            EngineEvent::Status(msg) => {
                vec![UiCommand::ShowStatus(msg)]
            }
            EngineEvent::TokenUpdate {
                total_tokens,
                turn_count,
            } => {
                vec![
                    UiCommand::UpdateTokens(total_tokens),
                    UiCommand::UpdateTurnCount(turn_count),
                ]
            }
            EngineEvent::CompactProgress {
                method,
                tokens_before,
                tokens_after,
            } => {
                vec![UiCommand::ShowStatus(format!(
                    "󰏖 compact: {} ({} → {} tokens)",
                    method, tokens_before, tokens_after
                ))]
            }

            // ── Interactive events ───────────────────────────────
            EngineEvent::AskUser {
                question,
                response_tx,
            } => {
                vec![UiCommand::ShowAskUser {
                    question,
                    response_tx,
                }]
            }
            EngineEvent::PermissionAsk {
                tool_name,
                reason,
                input,
                response_tx,
            } => {
                // Permission check is now handled by Engine's permission_allow_all
                // No need to check here - just forward to UI
                vec![UiCommand::ShowPermission {
                    tool_name,
                    reason,
                    detail: input,
                    response_tx,
                }]
            }

            // ── Image paste ──────────────────────────────────────
            EngineEvent::ImagePasted {
                media_type,
                base64_data,
                size_kb,
            } => {
                vec![UiCommand::PasteImage {
                    media_type,
                    base64_data,
                    size_kb,
                }]
            }
            EngineEvent::ImagePasteFailed => {
                vec![UiCommand::ShowError(
                    "No image in clipboard, or clipboard tool not available.".into(),
                )]
            }
            EngineEvent::PermissionAllowAllSet => {
                // Side effect handled by drain_engine_events() which sets
                // the shared Arc<AtomicBool> lock-free. No UI commands needed.
                vec![]
            }
            EngineEvent::SteerHandled => {
                vec![UiCommand::ClearSteerQueue]
            }
        }
    }
}
