//! Session search — search past sessions using the auxiliary model.
//!
//! Takes a user query, loads the session index, and asks the auxiliary model
//! to find the most relevant sessions based on their summaries.

use crate::config::settings::Settings;
use crate::engine::session::SessionIndexEntry;

use super::client::{AuxiliaryMessage, call_auxiliary};
use super::router::AuxiliaryError;
use super::router::AuxiliaryTask;

/// Search past sessions using the auxiliary model.
///
/// Takes a user query and returns a formatted list of matching sessions.
pub async fn search_sessions(
    settings: &Settings,
    query: &str,
    index: &[SessionIndexEntry],
) -> Result<String, AuxiliaryError> {
    if index.is_empty() {
        return Ok("No saved sessions found.".into());
    }

    // Format session index for the model
    let session_list = format_index_for_search(index);

    let messages = vec![
        AuxiliaryMessage {
            role: "system".into(),
            content: SEARCH_SYSTEM_PROMPT.to_string(),
        },
        AuxiliaryMessage {
            role: "user".into(),
            content: format!("Query: {}\n\nPast sessions:\n{}", query, session_list),
        },
    ];

    let result = call_auxiliary(settings, AuxiliaryTask::SessionSearch, messages).await?;
    Ok(result.content)
}

/// Format the session index into a compact text for the search model.
fn format_index_for_search(index: &[SessionIndexEntry]) -> String {
    index
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            format!(
                "[{}] {} — {} ({}, {} tokens)",
                i + 1,
                entry.saved_at,
                entry.one_liner,
                entry.model,
                entry.total_tokens,
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// System prompt for the session search task.
const SEARCH_SYSTEM_PROMPT: &str = r#"You are a session search assistant. Given a user query and a list of past conversation sessions, find the most relevant sessions.

Rules:
- Return the numbers of the most relevant sessions (e.g., [1], [3], [5]).
- Order by relevance, most relevant first.
- Briefly explain why each session matches the query.
- If no sessions match, say so.
- Keep your response concise."#;
