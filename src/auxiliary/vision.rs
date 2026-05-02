//! Vision-specific auxiliary routing and image content support.
//!
//! Vision backend not yet wired into the agent loop. All types and functions
//! here are scaffolding for when vision mode is activated.
#![allow(
    dead_code,
    reason = "vision backend not yet integrated into agent loop"
)]

use std::path::Path;

use base64::Engine;

use crate::config::settings::Settings;

use super::router::{AuxiliaryError, AuxiliaryTask, ResolvedProvider, resolve_provider};

// ---------------------------------------------------------------------------
// Image encoding
// ---------------------------------------------------------------------------

/// Guess the MIME type from a file extension.
fn guess_mime(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_lowercase())
        .as_deref()
    {
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        Some("svg") => "image/svg+xml",
        _ => "image/jpeg", // default fallback
    }
}

/// Encode a local image file as a base64 data URL.
///
/// Returns `None` if the file cannot be read.
pub fn file_to_data_url(path: &Path) -> Option<String> {
    let raw = std::fs::read(path).ok()?;
    let mime = guess_mime(path);
    let b64 = base64::engine::general_purpose::STANDARD.encode(&raw);
    Some(format!("data:{mime};base64,{b64}"))
}

/// Build an OpenAI-style content part for an image.
///
/// Shape: `{"type": "image_url", "image_url": {"url": "data:image/png;base64,..."}}`
pub fn build_image_content_part(data_url: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "image_url",
        "image_url": {
            "url": data_url
        }
    })
}

// ---------------------------------------------------------------------------
// Image input mode (mirrors hermes-agent `decide_image_input_mode`)
// ---------------------------------------------------------------------------

/// How user-attached images should be presented to the main model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageInputMode {
    /// Attach images as native `image_url` content parts (model sees pixels).
    Native,
    /// Run vision_analyze first and prepend description text (lossy).
    Text,
}

impl ImageInputMode {
    /// Decide the image input mode for the current turn.
    ///
    /// In "auto" mode:
    /// - If the user explicitly configured `auxiliary.vision.provider` (not "auto"),
    ///   use Text mode (they opted into a specific vision backend).
    /// - Otherwise, use Native mode (let the main model see images directly).
    ///
    /// The config `agent.image_input_mode` can force "native" or "text".
    pub fn decide(settings: &Settings) -> Self {
        let vision_config = &settings.auxiliary.vision;

        // If user explicitly configured a vision provider (not "auto"), use text mode
        let provider_normalized =
            super::router::normalize_provider(&vision_config.provider, &settings.active_provider);
        if provider_normalized != "auto" {
            return Self::Text;
        }

        // Default: native — let the main model handle images directly
        Self::Native
    }
}

// ---------------------------------------------------------------------------
// Vision provider resolution
// ---------------------------------------------------------------------------

/// Providers known to support vision / multimodal input.
const VISION_CAPABLE_PROVIDERS: &[&str] = &[
    "anthropic",
    "openai",
    "openai-codex",
    "openrouter",
    "gemini",
    "xai",
];

/// Check if a provider is known to support vision.
pub fn provider_supports_vision(provider_name: &str) -> bool {
    VISION_CAPABLE_PROVIDERS.contains(&provider_name)
}

/// Resolve the provider for a vision task, filtering out providers
/// that are known NOT to support multimodal input.
///
/// In auto mode, only vision-capable providers are tried. If an explicit
/// non-vision provider is configured, it's still used (the user explicitly
/// chose it), but a warning is logged.
pub fn resolve_vision_provider(settings: &Settings) -> Result<ResolvedProvider, AuxiliaryError> {
    let task_config = AuxiliaryTask::Vision.config(settings);
    let normalized =
        super::router::normalize_provider(&task_config.provider, &settings.active_provider);

    // Explicit non-auto provider — use it directly (user chose it)
    if normalized != "auto" {
        if !provider_supports_vision(&normalized) {
            tracing::warn!(
                provider = %normalized,
                "Vision auxiliary: provider may not support multimodal input"
            );
        }
        return resolve_provider(AuxiliaryTask::Vision, settings);
    }

    // Auto: build chain filtered to vision-capable providers
    let chain = super::router::build_provider_chain(settings);
    let vision_chain: Vec<String> = chain
        .into_iter()
        .filter(|p| provider_supports_vision(p))
        .collect();

    for candidate in &vision_chain {
        match super::router::try_resolve_candidate(candidate, task_config, settings) {
            Ok(resolved) => {
                tracing::info!(
                    provider = %resolved.provider_name,
                    model = %resolved.model,
                    "Vision auto-detect: using provider"
                );
                return Ok(resolved);
            }
            Err(AuxiliaryError::NoApiKey(_)) => continue,
            Err(e) => return Err(e),
        }
    }

    // Fallback: try all providers (maybe the user has a custom endpoint with vision)
    tracing::debug!(
        event = "vision_fallback",
        "Vision auto-detect: no vision-known provider available, trying all providers"
    );
    resolve_provider(AuxiliaryTask::Vision, settings)
}

// ---------------------------------------------------------------------------
// Multimodal message construction
// ---------------------------------------------------------------------------

/// A message that can contain both text and image content parts.
#[derive(Debug, Clone)]
pub struct VisionMessage {
    pub role: String,
    pub text: String,
    /// Optional image data URLs attached to this message.
    pub image_urls: Vec<String>,
}

impl VisionMessage {
    /// Build the OpenAI-compatible messages array including image content parts.
    ///
    /// For a message with images, the content becomes an array of parts:
    /// ```json
    /// [{"type": "text", "text": "..."}, {"type": "image_url", "image_url": {"url": "data:..."}}]
    /// ```
    pub fn to_api_value(&self) -> serde_json::Value {
        if self.image_urls.is_empty() {
            // Pure text message
            serde_json::json!({
                "role": self.role,
                "content": self.text,
            })
        } else {
            // Multimodal message with content parts
            let mut parts = Vec::new();

            // Text part (use a neutral prompt if no text provided)
            let text = if self.text.is_empty() && !self.image_urls.is_empty() {
                "What do you see in this image?".to_string()
            } else {
                self.text.clone()
            };
            parts.push(serde_json::json!({
                "type": "text",
                "text": text,
            }));

            // Image parts
            for url in &self.image_urls {
                parts.push(build_image_content_part(url));
            }

            serde_json::json!({
                "role": self.role,
                "content": parts,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_guess_mime() {
        assert_eq!(guess_mime(Path::new("test.png")), "image/png");
        assert_eq!(guess_mime(Path::new("test.jpg")), "image/jpeg");
        assert_eq!(guess_mime(Path::new("test.JPEG")), "image/jpeg");
        assert_eq!(guess_mime(Path::new("test.unknown")), "image/jpeg");
    }

    #[test]
    fn test_vision_message_pure_text() {
        let msg = VisionMessage {
            role: "user".into(),
            text: "Hello".into(),
            image_urls: vec![],
        };
        let val = msg.to_api_value();
        assert_eq!(val["role"], "user");
        assert_eq!(val["content"], "Hello");
    }

    #[test]
    fn test_vision_message_with_image() {
        let msg = VisionMessage {
            role: "user".into(),
            text: "Analyze this".into(),
            image_urls: vec!["data:image/png;base64,abc123".into()],
        };
        let val = msg.to_api_value();
        assert_eq!(val["role"], "user");
        let content = val["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image_url");
    }

    #[test]
    fn test_vision_message_no_text_default_prompt() {
        let msg = VisionMessage {
            role: "user".into(),
            text: "".into(),
            image_urls: vec!["data:image/png;base64,abc123".into()],
        };
        let val = msg.to_api_value();
        let content = val["content"].as_array().unwrap();
        assert_eq!(content[0]["text"], "What do you see in this image?");
    }

    #[test]
    fn test_provider_supports_vision() {
        assert!(provider_supports_vision("anthropic"));
        assert!(provider_supports_vision("openai"));
        assert!(provider_supports_vision("gemini"));
        assert!(!provider_supports_vision("deepseek"));
        assert!(!provider_supports_vision("custom"));
    }

    #[test]
    fn test_build_image_content_part() {
        let part = build_image_content_part("data:image/png;base64,abc");
        assert_eq!(part["type"], "image_url");
        assert_eq!(part["image_url"]["url"], "data:image/png;base64,abc");
    }
}
