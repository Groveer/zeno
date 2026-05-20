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
use chrono::{Local, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// Format a SystemTime as a local time string "YYYY-MM-DD HH:MM:SS".
pub fn format_timestamp(time: SystemTime) -> String {
    let datetime: chrono::DateTime<Local> = time.into();
    datetime.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Convert a UTC time string ("YYYY-MM-DD HH:MM:SS") to local time display.
/// Returns the input unchanged if parsing fails.
pub fn utc_to_local_display(utc_str: &str) -> String {
    // Try parsing as "YYYY-MM-DD HH:MM:SS" (UTC)
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(utc_str, "%Y-%m-%d %H:%M:%S") {
        let utc_dt = Utc.from_utc_datetime(&dt);
        let local_dt = utc_dt.with_timezone(&Local);
        return local_dt.format("%Y-%m-%d %H:%M:%S").to_string();
    }
    // Try parsing as "YYYY-MM-DDTHH:MM:SS" (ISO format without Z)
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(utc_str, "%Y-%m-%dT%H:%M:%S") {
        let utc_dt = Utc.from_utc_datetime(&dt);
        let local_dt = utc_dt.with_timezone(&Local);
        return local_dt.format("%Y-%m-%d %H:%M:%S").to_string();
    }
    // Return original if parsing fails
    utc_str.to_string()
}

/// Generate a unique session ID using epoch microseconds + PID.
///
/// Combines sub-millisecond precision (microseconds) with the process ID to
/// guarantee uniqueness even when multiple sessions start in the same
/// microsecond. Format: `{epoch_us}-{pid}` (e.g. `1721743567890123-12345`).
pub fn generate_session_id() -> String {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let micros = duration.as_micros();
    let pid = std::process::id();
    format!("{}-{}", micros, pid)
}

/// Complete session data persisted to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionData {
    /// Unique session identifier (epoch_us-pid, e.g. "1721743567890123-12345").
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
    /// AI-generated short title for the session.
    #[serde(default)]
    pub title: String,
    /// Identity name active when the session was saved.
    /// None means no active identity (default role).
    #[serde(default)]
    pub identity: Option<String>,
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
    /// AI-generated short title for the session.
    #[serde(default)]
    pub title: String,
    /// Identity name active when the session was saved.
    /// None means no active identity (default role).
    #[serde(default)]
    pub identity: Option<String>,
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
        title: data.title.clone(),
        identity: data.identity.clone(),
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
    if index_path.exists()
        && let Ok(json) = std::fs::read_to_string(&index_path)
    {
        let index: Vec<SessionIndexEntry> = serde_json::from_str(&json).unwrap_or_default();
        if !index.is_empty() {
            return index;
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
                title: data.title.clone(),
                identity: data.identity.clone(),
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

    // Statistics
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

    // Recent user messages (last 3)
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

    // Last assistant text (the most valuable recall context)
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

    // Prefer AI-generated title if available
    if !data.title.is_empty() {
        return format!("{} msgs, {} — {}", user_msgs, data.model, data.title);
    }

    // Fallback: use the first ~100 chars of the last user message as a topic indicator
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
                    let clean = text.trim().replace('\n', " ");
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

/// Build a short one-line summary suitable for the `/restore` response header.
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

/// Format the session index as a human-readable markdown list for the `/restore` picker.
pub fn format_session_list(index: &[SessionIndexEntry]) -> String {
    if index.is_empty() {
        return "No saved sessions found.".to_string();
    }

    let mut lines = vec![format!("### Saved Sessions ({})\n", index.len())];

    for (i, entry) in index.iter().enumerate() {
        // Convert stored UTC time to local time for display
        let local_time = utc_to_local_display(&entry.saved_at);
        let date = local_time
            .get(..16)
            .unwrap_or(&local_time)
            .replace('T', " ");
        let label = if entry.title.is_empty() {
            &entry.one_liner
        } else {
            &entry.title
        };
        let identity_tag = match &entry.identity {
            Some(id) => format!(" [{}]", id),
            None => String::new(),
        };
        lines.push(format!(
            "{}. **{}** — {}{}",
            i + 1,
            date,
            label,
            identity_tag
        ));
    }

    lines.push(String::new());
    lines.push("Use `/restore N` to load a session.".to_string());

    lines.join("\n")
}

/// Filter session index entries by identity name.
/// Returns only entries that match the given identity (or all entries when identity is None).
pub fn filter_index_by_identity(
    index: &[SessionIndexEntry],
    identity: Option<&str>,
) -> Vec<SessionIndexEntry> {
    match identity {
        Some(id) if !id.is_empty() => index
            .iter()
            .filter(|e| e.identity.as_deref() == Some(id))
            .cloned()
            .collect(),
        _ => index.to_vec(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::{ContentBlock, Role};

    fn make_text_entry(role: Role, text: &str) -> ConversationEntry {
        ConversationEntry {
            role,
            content: vec![ContentBlock::Text { text: text.into() }],
            reasoning_content: None,
        }
    }

    fn make_tool_result_entry(text: &str) -> ConversationEntry {
        ConversationEntry {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tool1".into(),
                content: text.into(),
                is_error: None,
            }],
            reasoning_content: None,
        }
    }

    #[test]
    fn test_generate_session_id_unique() {
        let id1 = generate_session_id();
        // Sleep for 1 microsecond to ensure different timestamps
        std::thread::sleep(std::time::Duration::from_micros(1));
        let id2 = generate_session_id();
        assert_ne!(id1, id2, "consecutive session IDs must differ");
    }

    #[test]
    fn test_generate_session_id_format() {
        let id = generate_session_id();
        // Format: "{epoch_ms}-{pid}"
        assert!(id.contains('-'), "session ID must contain '-' separator");
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 2, "session ID must have exactly 2 parts");
        let pid: u32 = parts[1].parse().expect("second part must be PID");
        assert!(pid > 0, "PID must be positive");
    }

    #[test]
    fn test_format_timestamp_epoch() {
        let epoch = std::time::UNIX_EPOCH;
        let formatted = format_timestamp(epoch);
        // The result depends on local timezone, but should be a valid date
        assert!(formatted.starts_with("1969-12-31") || formatted.starts_with("1970-01-01"));
        assert!(formatted.contains(":"));
    }

    #[test]
    fn test_count_user_messages_empty() {
        assert_eq!(count_user_messages(&[]), 0);
    }

    #[test]
    fn test_count_user_messages_mixed() {
        let entries = vec![
            make_text_entry(Role::User, "hello"),
            make_text_entry(Role::Assistant, "hi there"),
            make_text_entry(Role::User, "how are you?"),
            make_tool_result_entry("ls output"),
            make_text_entry(Role::User, "thanks"),
        ];
        // 3 user messages with text, 1 tool result (excluded) = 3
        assert_eq!(count_user_messages(&entries), 3);
    }

    #[test]
    fn test_count_user_messages_tool_results_excluded() {
        let entries = vec![
            make_tool_result_entry("cat output"),
            make_tool_result_entry("build log"),
        ];
        assert_eq!(count_user_messages(&entries), 0);
    }

    #[test]
    fn test_format_session_list_empty() {
        let result = format_session_list(&[]);
        assert_eq!(result, "No saved sessions found.");
    }

    #[test]
    fn test_format_session_list_non_empty() {
        let entries = vec![SessionIndexEntry {
            id: "1".into(),
            saved_at: "2025-07-23T10:30:00".into(),
            model: "claude-sonnet-4".into(),
            provider: "anthropic".into(),
            total_tokens: 5000,
            one_liner: "3 msgs, claude — code review".into(),
            entry_count: 5,
            title: String::new(),
            identity: None,
        }];
        let result = format_session_list(&entries);
        assert!(result.contains("### Saved Sessions (1)"));
        // Check that date is present (converted to local time)
        assert!(result.contains("2025-07-23"));
        assert!(result.contains("code review"));
        assert!(result.contains("/restore"));
    }

    #[test]
    fn test_extract_final_response_empty() {
        assert_eq!(extract_final_response(&[]), None);
    }

    #[test]
    fn test_extract_final_response_only_user() {
        let entries = vec![make_text_entry(Role::User, "hello")];
        assert_eq!(extract_final_response(&entries), None);
    }

    #[test]
    fn test_extract_final_response_with_assistant() {
        let entries = vec![
            make_text_entry(Role::User, "hello"),
            make_text_entry(Role::Assistant, "Hello! How can I help?"),
        ];
        assert_eq!(
            extract_final_response(&entries),
            Some("Hello! How can I help?".to_string())
        );
    }

    #[test]
    fn test_extract_final_response_last_assistant() {
        let entries = vec![
            make_text_entry(Role::User, "q1"),
            make_text_entry(Role::Assistant, "a1"),
            make_text_entry(Role::User, "q2"),
            make_text_entry(Role::Assistant, "a2"),
        ];
        assert_eq!(extract_final_response(&entries), Some("a2".to_string()));
    }

    #[test]
    fn test_build_one_liner_basic() {
        let data = SessionData {
            id: "test-id".into(),
            saved_at: "2025-07-23T12:00:00".into(),
            model: "claude-sonnet-4".into(),
            provider: "anthropic".into(),
            cwd: "/home/user".into(),
            entries: vec![
                make_text_entry(Role::User, "hello"),
                make_text_entry(Role::Assistant, "world"),
            ],
            total_tokens: 1500,
            summary: "test".into(),
            final_response: "world".into(),
            title: String::new(),
            identity: None,
        };
        let result = build_one_liner(&data);
        assert!(result.contains("2025-07-23 12:00:00"));
        assert!(result.contains("1500 tokens"));
        assert!(result.contains("claude-sonnet-4"));
        assert!(result.contains("2 entries"));
    }
}
