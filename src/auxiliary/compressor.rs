//! Conversation history compressor and title generator — uses an auxiliary
//! model to compress long conversation history into a concise summary, and
//! to generate short session titles.
//!
//! `compress_history()` is actively used by `engine/compact.rs`.
//! Title generation functions are reserved for future session management UI.

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
}
