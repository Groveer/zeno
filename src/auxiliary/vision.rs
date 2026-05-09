//! Image analysis via auxiliary vision model.
//!
//! When the main model doesn't support vision (e.g. OpenAI-compatible providers),
//! this module sends images to the auxiliary vision model to get a text description.
//! The description replaces the raw Image block so the main model can reason about
//! the image content.

use crate::config::settings::Settings;

use super::client::{AuxiliaryResult, call_auxiliary_raw};
use super::router::AuxiliaryError;
use super::router::AuxiliaryTask;

/// System prompt for the vision analysis task.
const VISION_SYSTEM_PROMPT: &str = r#"You are an image analyzer. Describe the content of the image in detail.

Rules:
- Describe what you see: objects, text, UI elements, code, diagrams, error messages.
- If the image contains code or terminal output, transcribe it accurately.
- If the image contains a screenshot of an error or UI, describe the relevant details.
- Be thorough but concise — the description will be used by another AI to understand the image.
- Use markdown formatting for clarity."#;

/// Analyze an image using the auxiliary vision model.
///
/// Takes base64-encoded image data and returns a text description.
/// The description is suitable for injecting into the main model's context.
pub async fn analyze_image(
    settings: &Settings,
    base64_data: &str,
    media_type: &str,
    source_path: &str,
) -> Result<AuxiliaryResult, AuxiliaryError> {
    let data_url = format!("data:{};base64,{}", media_type, base64_data);

    let raw_messages = vec![
        serde_json::json!({
            "role": "system",
            "content": VISION_SYSTEM_PROMPT,
        }),
        serde_json::json!({
            "role": "user",
            "content": [
                {
                    "type": "text",
                    "text": format!(
                        "Describe this image in detail so another AI can understand it.\nSource: {}",
                        if source_path.is_empty() { "clipboard" } else { source_path }
                    ),
                },
                {
                    "type": "image_url",
                    "image_url": {
                        "url": data_url,
                    },
                },
            ],
        }),
    ];

    call_auxiliary_raw(settings, AuxiliaryTask::Vision, raw_messages).await
}
