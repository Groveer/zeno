//! Conversation history management.
//!
//! Supports multi-content-block messages (text + tool_use + tool_result + image).

use crate::api::types::{ContentBlock, Message, Role};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// A single entry in the conversation history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationEntry {
    pub role: Role,
    pub content: Vec<ContentBlock>,
    /// Provider-facing reasoning content (echoed back to API for thinking-mode providers).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reasoning_content: Option<String>,
}

impl ConversationEntry {}

/// Manages conversation history.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConversationHistory {
    entries: Vec<ConversationEntry>,
}

impl ConversationHistory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a user message (text only).
    pub fn push_user(&mut self, text: &str) {
        self.entries.push(ConversationEntry {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
            reasoning_content: None,
        });
    }

    /// Add a user message with multiple content blocks (e.g. text + image).
    pub fn push_user_blocks(&mut self, blocks: Vec<ContentBlock>) {
        self.entries.push(ConversationEntry {
            role: Role::User,
            content: blocks,
            reasoning_content: None,
        });
    }

    /// Add an assistant message with multiple content blocks.
    pub fn push_assistant_blocks(&mut self, blocks: Vec<ContentBlock>) {
        self.entries.push(ConversationEntry {
            role: Role::Assistant,
            content: blocks,
            reasoning_content: None,
        });
    }

    /// Add an assistant message with content blocks and reasoning content.
    pub fn push_assistant_with_reasoning(
        &mut self,
        blocks: Vec<ContentBlock>,
        reasoning_content: Option<String>,
    ) {
        self.entries.push(ConversationEntry {
            role: Role::Assistant,
            content: blocks,
            reasoning_content,
        });
    }

    /// Add a tool result message (user role with tool_result content blocks).
    pub fn push_tool_results(&mut self, results: Vec<ContentBlock>) {
        self.entries.push(ConversationEntry {
            role: Role::User,
            content: results,
            reasoning_content: None,
        });
    }

    /// Append steer text to the last tool-result entry's content.
    ///
    /// This preserves role alternation — instead of inserting a new user
    /// message (which would create two consecutive user-role messages), we
    /// add `Text` blocks alongside the existing `ToolResult` blocks so the
    /// model understands it came from the user, not the tool itself.
    /// Using separate `Text` blocks avoids polluting tool result content
    /// and makes the user guidance more visible to the model.
    ///
    /// If the steer text contains multiple lines (from multiple user
    /// submissions concatenated with `\n`), each line is emitted as a
    /// separate numbered guidance block so the model can distinguish
    /// independent instructions.
    ///
    /// Returns `true` if the steer was appended, `false` if there is no
    /// last entry or the last entry has no ToolResult blocks.
    pub fn append_steer_to_last_tool_result(&mut self, steer_text: &str) -> bool {
        let last = match self.entries.last_mut() {
            Some(entry) => entry,
            None => return false,
        };
        // Only append if the entry contains at least one ToolResult block
        let has_tool_result = last
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolResult { .. }));
        if !has_tool_result {
            return false;
        }
        let lines: Vec<&str> = steer_text.split('\n').filter(|s| !s.is_empty()).collect();
        for (i, line) in lines.iter().enumerate() {
            let label = if lines.len() == 1 {
                format!("[User guidance]: {}", line)
            } else {
                format!("[User guidance {}/{}]: {}", i + 1, lines.len(), line)
            };
            last.content.push(ContentBlock::Text { text: label });
        }
        true
    }

    /// Compress `old_string`/`new_string` in edit tool ToolUse.input blocks
    /// by stripping common prefix/suffix context lines.
    ///
    /// For each tool_use_id in `successful_edit_ids`, find the matching
    /// ToolUse block in the most recent assistant entry and compress its
    /// input. This reduces token count for future API calls since the
    /// context lines are no longer needed after the edit succeeded.
    pub fn compress_edit_inputs(
        &mut self,
        successful_edit_ids: &std::collections::HashSet<String>,
    ) {
        if successful_edit_ids.is_empty() {
            return;
        }
        for entry in self.entries.iter_mut().rev() {
            if entry.role != Role::Assistant {
                continue;
            }
            for block in &mut entry.content {
                if let ContentBlock::ToolUse { id, input, .. } = block {
                    if successful_edit_ids.contains(id) {
                        crate::utils::diff::compress_edit_input(input);
                    }
                }
            }
            break; // Only the most recent assistant entry
        }
    }

    /// Convert the full history to API `Message` format.
    pub fn to_api_messages(&self) -> Vec<Message> {
        self.entries
            .iter()
            .map(|e| Message {
                role: e.role.clone(),
                content: e.content.clone(),
                reasoning_content: e.reasoning_content.clone(),
            })
            .collect()
    }

    /// Number of entries in history.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Read-only access to raw entries.
    pub fn entries_raw(&self) -> &[ConversationEntry] {
        &self.entries
    }

    /// Mutable access to raw entries (for compact helpers).
    pub fn entries_mut(&mut self) -> &mut Vec<ConversationEntry> {
        &mut self.entries
    }

    /// Construct from pre-existing entries (for session restore).
    pub fn from_entries(entries: Vec<ConversationEntry>) -> Self {
        Self { entries }
    }

    /// Clear all history.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    // -----------------------------------------------------------------------
    // Sanitize
    // -----------------------------------------------------------------------

    /// Sanitize conversation history for API safety.
    /// - Remove empty assistant messages
    /// - Trim orphaned tool_use (no matching tool_result)
    /// - Trim orphaned tool_result (no matching tool_use)
    /// - Remove trailing assistant tool_use when session was interrupted
    ///   mid-turn (no matching tool_result follows — prevents API rejection)
    pub fn sanitize(&mut self) {
        // Phase 1: Collect all tool_use and tool_result IDs
        let mut tool_use_ids: HashSet<String> = HashSet::new();
        let mut tool_result_ids: HashSet<String> = HashSet::new();

        for entry in &self.entries {
            for block in &entry.content {
                match block {
                    ContentBlock::ToolUse { id, .. } => {
                        tool_use_ids.insert(id.clone());
                    }
                    ContentBlock::ToolResult { tool_use_id, .. } => {
                        tool_result_ids.insert(tool_use_id.clone());
                    }
                    _ => {}
                }
            }
        }

        // Phase 2: Find orphaned IDs
        let orphaned_use: HashSet<String> =
            tool_use_ids.difference(&tool_result_ids).cloned().collect();
        let orphaned_result: HashSet<String> =
            tool_result_ids.difference(&tool_use_ids).cloned().collect();

        // Phase 2.5: Handle trailing orphan — session interrupted mid-turn.
        // If the last message is an assistant with tool_use blocks and there's
        // no subsequent user message with matching tool_result, remove the
        // orphaned tool_use blocks (or the entire message if nothing else remains).
        if !self.entries.is_empty() {
            let last_idx = self.entries.len() - 1;
            let last = &self.entries[last_idx];
            if last.role == Role::Assistant {
                let has_unmatched_tool_use = last.content.iter().any(
                    |b| matches!(b, ContentBlock::ToolUse { id, .. } if orphaned_use.contains(id)),
                );
                if has_unmatched_tool_use {
                    // Remove orphaned tool_use blocks, keep text
                    let cleaned: Vec<ContentBlock> = last
                        .content
                        .iter()
                        .filter(|b| match b {
                            ContentBlock::ToolUse { id, .. } => !orphaned_use.contains(id),
                            _ => true,
                        })
                        .cloned()
                        .collect();
                    if cleaned.is_empty() {
                        self.entries.pop();
                    } else {
                        self.entries[last_idx].content = cleaned;
                    }
                }
            }
        }

        // Phase 3: Clean entries
        let mut i = 0;
        while i < self.entries.len() {
            let role = self.entries[i].role.clone();

            // Remove empty assistant messages
            if role == Role::Assistant {
                let has_reasoning = self.entries[i]
                    .reasoning_content
                    .as_ref()
                    .is_some_and(|rc| !rc.trim().is_empty());
                let is_empty_content = self.entries[i].content.iter().all(|b| match b {
                    ContentBlock::Text { text } => text.trim().is_empty(),
                    _ => false,
                });
                if !has_reasoning && is_empty_content {
                    self.entries.remove(i);
                    continue;
                }
            }

            // Remove orphaned tool_result blocks from user messages
            if role == Role::User {
                let cleaned: Vec<ContentBlock> = self.entries[i]
                    .content
                    .iter()
                    .filter(|b| match b {
                        ContentBlock::ToolResult { tool_use_id, .. } => {
                            !orphaned_result.contains(tool_use_id)
                        }
                        _ => true,
                    })
                    .cloned()
                    .collect();

                if cleaned.len() != self.entries[i].content.len() {
                    self.entries[i].content = cleaned;
                    if self.entries[i].content.is_empty() {
                        self.entries.remove(i);
                        continue;
                    }
                }
            }

            // Remove orphaned tool_use blocks from assistant messages
            if role == Role::Assistant {
                let cleaned: Vec<ContentBlock> = self.entries[i]
                    .content
                    .iter()
                    .filter(|b| match b {
                        ContentBlock::ToolUse { id, .. } => !orphaned_use.contains(id),
                        _ => true,
                    })
                    .cloned()
                    .collect();

                if cleaned.len() != self.entries[i].content.len() {
                    self.entries[i].content = cleaned;
                    let has_reasoning = self.entries[i]
                        .reasoning_content
                        .as_ref()
                        .is_some_and(|rc| !rc.trim().is_empty());
                    if !has_reasoning && self.entries[i].content.is_empty() {
                        self.entries.remove(i);
                        continue;
                    }
                }
            }

            i += 1;
        }
    }

    // -----------------------------------------------------------------------
    // Compact helpers
    // -----------------------------------------------------------------------

    /// Truncate tool_result content in older entries.
    /// Uses `find_safe_split_point` to avoid cutting through a tool_use/result pair.
    pub fn truncate_old_tool_results(&mut self, keep_recent: usize, max_chars: usize) -> bool {
        let total = self.entries.len();
        if total <= keep_recent {
            return false;
        }

        let cutoff = find_safe_split_point(&self.entries, keep_recent);
        let mut modified = false;

        for i in 0..cutoff {
            for block in &mut self.entries[i].content {
                if let ContentBlock::ToolResult { content, .. } = block
                    && content.len() > max_chars
                {
                    // Find a safe char-boundary at or before `max_chars` bytes.
                    // `max_chars` semantically means "character count", but the
                    // original code used it as a byte length, which panics on
                    // multi-byte UTF-8.  We now truncate at a proper boundary.
                    let end = content
                        .char_indices()
                        .nth(max_chars)
                        .map_or(content.len(), |(i, _)| i);
                    content.truncate(end);
                    content.push_str("...");
                    modified = true;
                }
            }
        }

        modified
    }

    /// Replace older history with a compressed summary, keeping recent entries intact.
    /// Uses `find_safe_split_point` to avoid cutting through a tool_use/result pair.
    pub fn replace_with_summary(&mut self, summary: String, keep_recent: usize) {
        let total = self.entries.len();
        if total <= keep_recent {
            return;
        }

        let split_point = find_safe_split_point(&self.entries, keep_recent);
        let recent: Vec<ConversationEntry> = self.entries.split_off(split_point);

        self.entries.clear();
        self.entries.push(ConversationEntry {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: format!(
                    "[Compressed conversation history ({} entries)]\n\n{}",
                    split_point, summary
                ),
            }],
            reasoning_content: None,
        });
        self.entries.extend(recent);
    }
}

// ---------------------------------------------------------------------------
// Split-point helpers (module-level)
// ---------------------------------------------------------------------------

/// Find a split point that preserves tool_use/result pairs.
///
/// Starting from `total - keep_recent`, walk backwards while the boundary
/// falls between an assistant message with tool_use blocks and a user
/// message with matching tool_result blocks — cutting there would orphan
/// the tool_use blocks and cause API rejection on the next request.
pub fn find_safe_split_point(entries: &[ConversationEntry], keep_recent: usize) -> usize {
    let total = entries.len();
    if total <= keep_recent {
        return 0;
    }

    let mut split = total - keep_recent;

    // Walk backwards while the boundary crosses a tool pair
    while split > 0 && boundary_crosses_tool_pair(entries, split) {
        split -= 1;
    }

    split
}

/// Return true when splitting at `idx` would separate an assistant tool_use
/// from its matching user tool_result.
fn boundary_crosses_tool_pair(entries: &[ConversationEntry], idx: usize) -> bool {
    if idx == 0 {
        return false;
    }

    let prev = &entries[idx - 1];
    let curr = &entries[idx];

    if prev.role != Role::Assistant || curr.role != Role::User {
        return false;
    }

    let pending_ids: HashSet<String> = prev
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();

    if pending_ids.is_empty() {
        return false;
    }

    let result_ids: HashSet<String> = curr
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
            _ => None,
        })
        .collect();

    !pending_ids.is_disjoint(&result_ids)
}
