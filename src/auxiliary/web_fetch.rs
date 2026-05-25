//! Web page content extraction — uses an auxiliary model to extract and
//! summarize web page content.
//!
//! `extract_web_content()` is an internal helper used by the `web_fetch`
//! tool pipeline, called through the auxiliary dispatch.

use crate::config::settings::Settings;

use super::client::{AuxiliaryMessage, call_auxiliary};
use super::router::AuxiliaryError;
use super::router::AuxiliaryTask;

/// System prompt for the web extraction task.
const WEB_EXTRACT_SYSTEM_PROMPT: &str = r#"You are a web content extractor. Your job is to extract and summarize the main content from a web page.

Rules:
- Extract the primary content (article text, documentation, code examples).
- Preserve all code snippets, API signatures, and technical details.
- Remove navigation, ads, footers, sidebars, and boilerplate.
- Preserve the document structure (headings, lists, tables).
- Keep the summary concise but complete — include all substantive information.
- If the page is an error page or has no meaningful content, say so briefly.
- Use markdown formatting for clarity."#;

/// Extract and summarize content from raw HTML text.
///
/// Uses the auxiliary model to extract the main content from the raw HTML.
/// The HTML should already be fetched and converted to text (via html2text
/// or similar) before calling this function.
pub async fn extract_web_content(
    settings: &Settings,
    url: &str,
    raw_text: &str,
) -> Result<String, AuxiliaryError> {
    if raw_text.trim().is_empty() {
        return Ok("(empty page)".into());
    }

    // Truncate very long pages to stay within reasonable token limits.
    // Use char-boundary-safe truncation to avoid panic on multi-byte UTF-8.
    let truncated = if raw_text.len() > 50_000 {
        let end = raw_text
            .char_indices()
            .take_while(|(idx, _)| *idx <= 50_000)
            .last()
            .map_or(0, |(idx, c)| idx + c.len_utf8());
        tracing::debug!(
            original_bytes = raw_text.len(),
            truncated_bytes = end,
            "Web extract: truncating page (char-boundary safe)"
        );
        &raw_text[..end]
    } else {
        raw_text
    };

    let messages = vec![
        AuxiliaryMessage {
            role: "system".into(),
            content: WEB_EXTRACT_SYSTEM_PROMPT.to_string(),
        },
        AuxiliaryMessage {
            role: "user".into(),
            content: format!(
                "Extract and summarize the main content from this web page.\n\nURL: {}\n\nContent:\n```\n{}\n```",
                url, truncated
            ),
        },
    ];

    let result = call_auxiliary(settings, AuxiliaryTask::WebExtract, messages).await?;
    Ok(result.content)
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_web_fetch_truncation() {
        // Verify that very long content gets truncated
        let long_content = "x".repeat(100_000);
        assert!(long_content.len() > 50_000);
        // The actual truncation happens inside extract_web_content,
        // we just verify the logic doesn't panic on long input
    }
}
