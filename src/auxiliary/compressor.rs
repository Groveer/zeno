//! Conversation history compressor and title generator — uses an auxiliary
//! model to compress long conversation history into a concise summary, and
//! to generate short session titles.
//!
//! `compress_history()` is actively used by `engine/compact.rs`.
//! Title generation functions are reserved for future session management UI.
#![allow(
    dead_code,
    reason = "generate_title/clean_title reserved for session UI"
)]

use crate::config::settings::Settings;
use crate::engine::messages::ConversationHistory;

use super::client::{AuxiliaryMessage, call_auxiliary};
use super::router::AuxiliaryError;
use super::router::AuxiliaryTask;

/// Compress the conversation history into a summary.
///
/// If carryover context is provided, it is prepended to the
/// compression prompt so the LLM preserves working memory
/// (files read, artifacts modified, work done) even after
/// the conversation history is compressed.
///
/// Returns the compressed summary text, or an error if compression fails.
pub async fn compress_history(
    settings: &Settings,
    history: &ConversationHistory,
    carryover_context: Option<&str>,
) -> Result<String, AuxiliaryError> {
    let history_text = format_history(history);

    if history_text.len() < 500 {
        // Too short to compress meaningfully
        return Ok(history_text);
    }

    let carryover_block = match carryover_context {
        Some(ctx) if !ctx.is_empty() => format!(
            "\n\n## Working Memory (must preserve in summary)\n{}\n\n\
            Important: The above working memory tracks what has been done in this \
            session. Ensure all file paths, artifacts, and key decisions from this \
            working memory are reflected in your summary.",
            ctx
        ),
        _ => String::new(),
    };

    let messages = vec![
        AuxiliaryMessage {
            role: "system".into(),
            content: COMPRESS_SYSTEM_PROMPT.to_string(),
        },
        AuxiliaryMessage {
            role: "user".into(),
            content: format!(
                "Compress the following conversation history into a concise summary. \
                Preserve all key decisions, code changes, file paths, and error fixes. \
                Omit pleasantries and redundant exchanges.{}\n\n```{}```",
                carryover_block, history_text
            ),
        },
    ];

    let result = call_auxiliary(settings, AuxiliaryTask::Compression, messages).await?;
    Ok(result.content)
}

/// Generate a session title from the first user-assistant exchange.
///
/// Uses the auxiliary model with the TitleGeneration task config.
/// Returns the title string, or an error if generation fails.
///
/// Reference: hermes-agent `title_generator.generate_title`.
pub async fn generate_title(
    settings: &Settings,
    user_message: &str,
    assistant_response: Option<&str>,
) -> Result<String, AuxiliaryError> {
    // Truncate long messages to keep the request small
    let user_snippet = truncate_to(user_message, 500);
    let assistant_snippet = assistant_response.map(|r| truncate_to(r, 500));

    let user_content = if let Some(asst) = assistant_snippet {
        format!("User: {}\n\nAssistant: {}", user_snippet, asst)
    } else {
        user_snippet.to_string()
    };

    let messages = vec![
        AuxiliaryMessage {
            role: "system".into(),
            content: TITLE_SYSTEM_PROMPT.to_string(),
        },
        AuxiliaryMessage {
            role: "user".into(),
            content: user_content,
        },
    ];

    let result = call_auxiliary(settings, AuxiliaryTask::TitleGeneration, messages).await?;

    // Clean up: remove quotes, trailing punctuation, prefixes like "Title: "
    let title = clean_title(&result.content);
    Ok(title)
}

/// Clean up a generated title string.
fn clean_title(raw: &str) -> String {
    let mut title = raw.trim().to_string();

    // Remove surrounding quotes
    title = title.trim_matches('"').trim_matches('\'').to_string();

    // Remove "Title: " prefix
    if title.to_lowercase().starts_with("title:") {
        title = title[6..].trim().to_string();
    }

    // Enforce reasonable length
    if title.len() > 80 {
        title = truncate_to(&title, 77).to_string() + "...";
    }

    title
}

/// Truncate a string to at most `max_len` characters, respecting char boundaries.
fn truncate_to(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        return s;
    }
    let mut end = max_len;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Format conversation history as readable text for compression.
fn format_history(history: &ConversationHistory) -> String {
    // Use the to_api_messages() method to get structured data
    let messages = history.to_api_messages();
    let mut parts = Vec::new();

    for msg in &messages {
        let role = match msg.role {
            crate::api::types::Role::User => "User",
            crate::api::types::Role::Assistant => "Assistant",
        };

        for block in &msg.content {
            match block {
                crate::api::types::ContentBlock::Text { text } => {
                    parts.push(format!("[{}]: {}", role, text));
                }
                crate::api::types::ContentBlock::ToolUse { name, input, .. } => {
                    parts.push(format!("[{} -> tool {}]: {:?}", role, name, input));
                }
                crate::api::types::ContentBlock::ToolResult { content, .. } => {
                    parts.push(format!("[tool result]: {}", content));
                }
                crate::api::types::ContentBlock::Image { source_path, .. } => {
                    parts.push(format!("[{} -> image]: {}", role, source_path));
                }
            }
        }
    }

    parts.join("\n\n")
}

/// System prompt for the compression task.
const COMPRESS_SYSTEM_PROMPT: &str = r#"You are a conversation history compressor. Your job is to compress a conversation into a concise summary.

Rules:
- Preserve all important details: file paths, command names, error messages, code snippets, key facts.
- Preserve key decisions (e.g., "chose approach A over B because...").
- Preserve any solutions and their reasoning.
- Omit greetings, pleasantries, and redundant exchanges.
- Keep the summary under 500 words.
- Use bullet points for clarity."#;

/// System prompt for the title generation task.
///
/// Reference: hermes-agent `title_generator._TITLE_PROMPT`.
const TITLE_SYSTEM_PROMPT: &str = r#"Generate a short, descriptive title (3-7 words) for a conversation that starts with the following exchange. The title should capture the main topic or intent. Return ONLY the title text, nothing else. No quotes, no punctuation at the end, no prefixes."#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::ContentBlock;

    #[test]
    fn test_format_history() {
        let mut history = ConversationHistory::new();
        history.push_user("Hello, help me write a quicksort");
        history.push_assistant_blocks(vec![ContentBlock::Text {
            text: "Sure, here's a quicksort implementation...".into(),
        }]);

        let text = format_history(&history);
        assert!(text.contains("[User]"));
        assert!(text.contains("[Assistant]"));
        assert!(text.contains("quicksort"));
    }

    #[test]
    fn test_clean_title() {
        assert_eq!(clean_title("  Hello World  "), "Hello World");
        assert_eq!(clean_title(r#""Fix the bug""#), "Fix the bug");
        assert_eq!(clean_title("Title: Fix the bug"), "Fix the bug");
        assert_eq!(clean_title("TITLE: Fix the bug"), "Fix the bug");
        assert_eq!(
            clean_title(&"x".repeat(100)),
            format!("{}...", "x".repeat(77))
        );
    }

    #[test]
    fn test_truncate_to() {
        assert_eq!(truncate_to("hello", 10), "hello");
        assert_eq!(truncate_to("hello world", 5), "hello");
    }
}
