//! Session persistence: save/load full conversation history with intelligent summary.
//!
//! **Multi-session storage**: sessions are saved to `~/.local/share/zeno/sessions/{id}.json`
//! with an index file for quick listing.
//!
//! **Intelligent summary**: the summary preserves the LLM's last substantive response
//! (the most valuable recall context) and recent user messages, rather than just
//! counting statistics.

use crate::api::types::ContentBlock;
use crate::engine::messages::ConversationEntry;
use serde::{Deserialize, Serialize};

use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// Format a SystemTime as a simple ISO-like string "YYYY-MM-DD HH:MM:SS".
pub fn format_timestamp(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Convert to date/time components
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;

    // Days since epoch (1970-01-01)
    let mut y = 1970i64;
    let mut remaining_days = days as i64;
    loop {
        let days_in_year = if is_leap_year(y) { 366 } else { 365 };
        if remaining_days < days_in_year {
            break;
        }
        remaining_days -= days_in_year;
        y += 1;
    }
    let month_days = if is_leap_year(y) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut m = 1usize;
    for &md in &month_days {
        if remaining_days < md as i64 {
            break;
        }
        remaining_days -= md as i64;
        m += 1;
    }
    let d = remaining_days + 1; // day of month, 1-based
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        y, m, d, hours, minutes, seconds
    )
}

/// Generate a timestamp-based session ID (epoch seconds, unique enough).
pub fn generate_session_id() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{}", secs)
}

fn is_leap_year(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Complete session data persisted to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionData {
    /// Unique session identifier (epoch seconds).
    pub id: String,
    /// ISO 8601 timestamp of when the session was saved.
    pub saved_at: String,
    /// Active model at save time.
    pub model: String,
    /// Active provider at save time.
    pub provider: String,
    /// Working directory at session start.
    pub cwd: String,
    /// Full conversation entries (serialized).
    pub entries: Vec<ConversationEntry>,
    /// Cumulative token usage across the session.
    pub total_tokens: u64,
    /// Human-readable summary of the session contents.
    pub summary: String,
    /// The last assistant text response — the most valuable recall context.
    /// Captured verbatim (up to 2000 chars) so the user can recall what was concluded.
    pub final_response: String,
}

/// Lightweight entry for the session index — avoids loading full session data for listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionIndexEntry {
    pub id: String,
    pub saved_at: String,
    pub model: String,
    pub provider: String,
    pub total_tokens: u64,
    /// Short one-line summary for the session picker.
    pub one_liner: String,
    /// Entry count for quick reference.
    pub entry_count: usize,
}

// ---------------------------------------------------------------------------
// Save
// ---------------------------------------------------------------------------

/// Serialise the current engine state to the session directory.
///
/// Saves to `~/.local/share/zeno/sessions/{id}.json` and updates the index.
/// Returns the session ID on success, `None` on failure.
pub fn save_session(data: &SessionData) -> bool {
    // Save to the sessions directory
    let sessions_dir = crate::config::paths::sessions_dir();
    let session_path = sessions_dir.join(format!("{}.json", data.id));

    match serde_json::to_string_pretty(data) {
        Ok(json) => {
            // Write individual session file
            if let Err(e) = std::fs::write(&session_path, &json) {
                tracing::warn!(
                    error = %e,
                    path = %session_path.display(),
                    "Failed to write session file"
                );
                return false;
            }
            tracing::info!(
                path = %session_path.display(),
                entries = data.entries.len(),
                "Session saved"
            );

            // Update the index
            update_session_index(data);

            true
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to serialize session data");
            false
        }
    }
}

/// Append or update an entry in the session index file.
fn update_session_index(data: &SessionData) {
    let index_path = crate::config::paths::session_index_path();
    let mut index = load_session_index();

    let entry = SessionIndexEntry {
        id: data.id.clone(),
        saved_at: data.saved_at.clone(),
        model: data.model.clone(),
        provider: data.provider.clone(),
        total_tokens: data.total_tokens,
        one_liner: build_index_liner(data),
        entry_count: data.entries.len(),
    };

    // Remove existing entry with same ID (shouldn't happen, but be safe)
    index.retain(|e| e.id != data.id);
    index.push(entry);

    // Sort newest first
    index.sort_by(|a, b| b.saved_at.cmp(&a.saved_at));

    // Keep at most 50 sessions in the index
    if index.len() > 50 {
        index.truncate(50);
    }

    if let Ok(json) = serde_json::to_string_pretty(&index) {
        let _ = std::fs::write(&index_path, json);
    }
}

// ---------------------------------------------------------------------------
// Load
// ---------------------------------------------------------------------------

/// Load the session index for listing available sessions.
///
/// If the index file is missing but session `.json` files exist on disk,
/// automatically rebuilds the index — this is the recovery path for when
/// the index was lost but session data is still valid.
pub fn load_session_index() -> Vec<SessionIndexEntry> {
    let index_path = crate::config::paths::session_index_path();

    // Fast path: index file exists, try to parse it
    if index_path.exists() {
        if let Ok(json) = std::fs::read_to_string(&index_path) {
            let index: Vec<SessionIndexEntry> = serde_json::from_str(&json).unwrap_or_default();
            if !index.is_empty() {
                return index;
            }
        }
    }

    // Slow path: index missing or empty — scan the sessions directory and rebuild
    rebuild_session_index()
}

/// Scan the sessions directory for `{id}.json` files, load each one,
/// build index entries, sort newest-first, persist the index, and return it.
fn rebuild_session_index() -> Vec<SessionIndexEntry> {
    let sessions_dir = crate::config::paths::sessions_dir();
    if !sessions_dir.exists() {
        return Vec::new();
    }

    let mut index: Vec<SessionIndexEntry> = Vec::new();

    let entries = match std::fs::read_dir(&sessions_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        // Only process .json files, skip index.json itself
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        if path.file_stem().is_some_and(|s| s == "index") {
            continue;
        }

        // Try to load the session and build an index entry
        if let Some(data) = load_session_from_path(&path) {
            let idx_entry = SessionIndexEntry {
                id: data.id.clone(),
                saved_at: data.saved_at.clone(),
                model: data.model.clone(),
                provider: data.provider.clone(),
                total_tokens: data.total_tokens,
                one_liner: build_index_liner(&data),
                entry_count: data.entries.len(),
            };
            index.push(idx_entry);
        }
    }

    // Sort newest first
    index.sort_by(|a, b| b.saved_at.cmp(&a.saved_at));

    // Persist the rebuilt index
    let index_path = crate::config::paths::session_index_path();
    if let Ok(json) = serde_json::to_string_pretty(&index) {
        let _ = std::fs::write(&index_path, json);
        tracing::info!(
            sessions = index.len(),
            path = %index_path.display(),
            "Rebuilt session index from disk"
        );
    }

    index
}

/// Load a specific session by ID.
pub fn load_session_by_id(id: &str) -> Option<SessionData> {
    let sessions_dir = crate::config::paths::sessions_dir();
    let path = sessions_dir.join(format!("{}.json", id));
    load_session_from_path(&path)
}

/// Load the most recent session from the sessions directory.
///
/// Loads the newest session by index, then falls back to the session file.
pub fn load_latest_session() -> Option<SessionData> {
    let index = load_session_index();
    if let Some(newest) = index.first()
        && let Some(data) = load_session_by_id(&newest.id)
    {
        return Some(data);
    }
    None
}

/// Internal helper: deserialize a session from a file path.
fn load_session_from_path(path: &std::path::Path) -> Option<SessionData> {
    if !path.exists() {
        return None;
    }
    match std::fs::read_to_string(path) {
        Ok(json) => match serde_json::from_str::<SessionData>(&json) {
            Ok(data) => {
                tracing::info!(
                    id = %data.id,
                    saved_at = %data.saved_at,
                    entries = data.entries.len(),
                    "Session loaded from disk"
                );
                Some(data)
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "Failed to parse session file");
                None
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "Failed to read session file");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Summary builder
// ---------------------------------------------------------------------------

/// Build an intelligent human-readable summary from conversation entries.
///
/// The summary preserves:
/// 1. **The last assistant text response** (up to 2000 chars) — the most valuable
///    recall context, since it contains the LLM's conclusions and final output.
/// 2. **Recent user messages** (last 3) — to recall what was being discussed.
/// 3. **Statistics** — entry counts, tool calls, token volume.
///
/// This approach prioritizes *recall value* over raw statistics. When a user
/// resumes a session, the most important question is "what was the LLM telling me?"
/// not "how many tool calls were made?"
pub fn build_summary(entries: &[ConversationEntry]) -> String {
    if entries.is_empty() {
        return "Empty session — no messages.".to_string();
    }

    let mut parts: Vec<String> = Vec::new();

    // ── Statistics ──
    let mut user_count = 0u32;
    let mut tool_use_count = 0u32;
    let mut tool_result_count = 0u32;
    let mut assistant_text_chars = 0usize;

    for entry in entries {
        match entry.role {
            crate::api::types::Role::User => {
                let has_text = entry
                    .content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Text { .. }));
                let has_tool_result = entry
                    .content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolResult { .. }));
                if has_text && !has_tool_result {
                    user_count += 1;
                }
                tool_result_count += entry
                    .content
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::ToolResult { .. }))
                    .count() as u32;
            }
            crate::api::types::Role::Assistant => {
                for block in &entry.content {
                    match block {
                        ContentBlock::Text { text } => assistant_text_chars += text.len(),
                        ContentBlock::ToolUse { .. } => tool_use_count += 1,
                        _ => {}
                    }
                }
            }
        }
    }

    parts.push(format!(
        "{} entries | {} user messages | {} tool calls | {} tool results | ~{:.1} KB AI text",
        entries.len(),
        user_count,
        tool_use_count,
        tool_result_count,
        assistant_text_chars as f64 / 1024.0,
    ));

    // ── Recent user messages (last 3) ──
    let recent_user: Vec<String> = entries
        .iter()
        .rev() // iterate backwards
        .filter(|e| {
            e.role == crate::api::types::Role::User
                && e.content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Text { .. }))
                && !e
                    .content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
        })
        .take(3)
        .filter_map(|e| {
            e.content.iter().find_map(|b| {
                if let ContentBlock::Text { text } = b {
                    let clean = text.trim();
                    if !clean.is_empty() {
                        let preview: String = clean.chars().take(120).collect();
                        Some(if clean.len() > 120 {
                            format!("{}…", preview)
                        } else {
                            preview
                        })
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
        })
        .collect();

    if !recent_user.is_empty() {
        // Reverse so they appear in chronological order
        let mut msgs = recent_user;
        msgs.reverse();
        parts.push(format!("Recent messages:\n{}", msgs.join("\n")));
    }

    // ── Last assistant text (the most valuable recall context) ──
    if let Some(last_response) = extract_final_response(entries) {
        const MAX_RESPONSE_LEN: usize = 2000;
        let truncated = if last_response.len() > MAX_RESPONSE_LEN {
            // Find the byte offset of the last char that fits within MAX_RESPONSE_LEN bytes
            let safe_end = {
                let mut end = 0usize;
                for (i, ch) in last_response.char_indices() {
                    if i + ch.len_utf8() > MAX_RESPONSE_LEN {
                        break;
                    }
                    end = i + ch.len_utf8();
                }
                end
            };
            format!(
                "{}…\n[truncated — {} total chars]",
                &last_response[..safe_end],
                last_response.len()
            )
        } else {
            last_response
        };
        parts.push(format!("Last AI response:\n{}", truncated));
    }

    parts.join("\n\n")
}

/// Extract the final substantive assistant text response from conversation entries.
///
/// Scans backwards to find the last assistant message that has a text block
/// (skipping tool-call-only entries). This is the most valuable piece of
/// context for session recall — it contains the LLM's conclusions, summaries,
/// or final output.
pub fn extract_final_response(entries: &[ConversationEntry]) -> Option<String> {
    for entry in entries.iter().rev() {
        if entry.role == crate::api::types::Role::Assistant {
            // Only consider entries that have at least one text block,
            // skip tool-call-only assistant entries
            let has_text = entry
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text } if !text.trim().is_empty()));
            if !has_text {
                continue;
            }
            for block in &entry.content {
                if let ContentBlock::Text { text } = block {
                    let clean = text.trim();
                    if !clean.is_empty() {
                        return Some(clean.to_string());
                    }
                }
            }
        }
    }
    None
}

/// Build a short one-line summary for the session picker / index.
pub fn build_index_liner(data: &SessionData) -> String {
    let user_msgs = count_user_messages(&data.entries);

    // Use the first ~100 chars of the last user message as a topic indicator
    let topic = data
        .entries
        .iter()
        .rev()
        .find(|e| {
            e.role == crate::api::types::Role::User
                && e.content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Text { .. }))
                && !e
                    .content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
        })
        .and_then(|e| {
            e.content.iter().find_map(|b| {
                if let ContentBlock::Text { text } = b {
                    let clean = text.trim();
                    let preview: String = clean.chars().take(80).collect();
                    Some(if clean.len() > 80 {
                        format!("{}…", preview)
                    } else {
                        preview
                    })
                } else {
                    None
                }
            })
        })
        .unwrap_or_else(|| "(no user messages)".to_string());

    format!("{} msgs, {} model — {}", user_msgs, data.model, topic,)
}

/// Build a short one-line summary suitable for the `/resume` response header.
pub fn build_one_liner(data: &SessionData) -> String {
    let entry_count = data.entries.len();
    let user_msgs = count_user_messages(&data.entries);
    let date = data
        .saved_at
        .get(..19.min(data.saved_at.len()))
        .unwrap_or(&data.saved_at)
        .replace('T', " ");
    format!(
        "Session from {} — {} entries, {} user messages, {} model, ~{} tokens",
        date, entry_count, user_msgs, data.model, data.total_tokens,
    )
}

/// Count non-tool-result user messages.
fn count_user_messages(entries: &[ConversationEntry]) -> usize {
    entries
        .iter()
        .filter(|e| {
            e.role == crate::api::types::Role::User
                && e.content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Text { .. }))
                && !e
                    .content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
        })
        .count()
}

/// Format the session index as a human-readable list for the `/resume` picker.
pub fn format_session_list(index: &[SessionIndexEntry]) -> String {
    if index.is_empty() {
        return "No saved sessions found.".to_string();
    }

    let mut lines = vec![format!("Found {} saved session(s):\n", index.len())];

    for (i, entry) in index.iter().enumerate() {
        let date = entry
            .saved_at
            .get(..19)
            .unwrap_or(&entry.saved_at)
            .replace('T', " ");
        lines.push(format!("  [{}] {} — {}", i + 1, date, entry.one_liner,));
    }

    lines.push(String::new());
    lines.push("Usage:".to_string());
    lines.push("  /resume       — load the most recent session".to_string());
    lines.push("  /resume N     — load session #N from the list above".to_string());

    lines.join("\n")
}
