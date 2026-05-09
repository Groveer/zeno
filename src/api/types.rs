//! Core API types — messages, content blocks, stream events, errors.
//!
//! This module defines the common type system shared by all API providers.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Role
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

// ---------------------------------------------------------------------------
// Content blocks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },

    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },

    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },

    #[serde(rename = "image")]
    Image {
        media_type: String,
        data: String,
        #[serde(skip_serializing_if = "String::is_empty", default)]
        source_path: String,
    },
}

// ---------------------------------------------------------------------------
// Message
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

// ---------------------------------------------------------------------------
// Stop reason & Usage
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence(String),
}

/// Token usage from a single API response.
///
/// Mirrors the structure returned by both Anthropic and OpenAI-compatible APIs.
/// All fields store the **raw API-reported values** (no splitting), except
/// `input_tokens` which is derived as `prompt_total - cache_read - cache_write`
/// for OpenAI-compatible APIs to avoid double-counting cached tokens in the
/// non-cached input field.
///
/// | Provider   | input_tokens (non-cached) | cache_read             | cache_write              | output_tokens    | reasoning   |
/// |------------|---------------------------|------------------------|--------------------------|------------------|-------------|
/// | Anthropic  | input_tokens              | cache_read_input_tokens | cache_creation_input_tokens | output_tokens | —         |
/// | OpenAI     | prompt - cache*           | prompt_tokens_details.cached_tokens | prompt_tokens_details.cache_write_tokens | completion_tokens | completion_tokens_details.reasoning_tokens |
///
/// *On OpenAI, `input_tokens = prompt_tokens - cached_tokens - cache_write_tokens`.
///  `output_tokens` is the raw `completion_tokens` (reasoning tokens are included
///   in output — `reasoning_tokens` is a subset for display purposes only).
///
/// `total()` = prompt_tokens() + output_tokens (the canonical total matching the
/// provider dashboard).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Non-cached input tokens (prompt minus cache hits and cache writes).
    pub input_tokens: u64,
    /// Generated output tokens (raw API value — includes reasoning tokens).
    pub output_tokens: u64,
    /// Tokens served from prompt cache (Anthropic: `cache_read_input_tokens`,
    /// OpenAI: `prompt_tokens_details.cached_tokens`).
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    /// Tokens written to prompt cache (Anthropic: `cache_creation_input_tokens`).
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    /// Tokens used for reasoning / thinking (subset of `output_tokens`,
    /// OpenAI: `completion_tokens_details.reasoning_tokens`).
    #[serde(default)]
    pub reasoning_tokens: u64,
}

impl Usage {
    /// Total prompt-side tokens: non-cached input + cache reads + cache writes.
    /// Equivalent to the API's `prompt_tokens` / `input_tokens`.
    pub fn prompt_tokens(&self) -> u64 {
        self.input_tokens + self.cache_read_input_tokens + self.cache_creation_input_tokens
    }
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    TextDelta(String),
    ToolUseStart {
        id: String,
        name: String,
        input_json: Option<String>,
    },
    ToolUseDelta {
        id: String,
        delta_json: String,
    },
    /// Carries per-request token usage from an early stream event
    /// (e.g. Anthropic `message_start`). The engine uses this to seed
    /// the final `MessageComplete` usage.
    UsageUpdate(Usage),
    MessageComplete {
        stop_reason: StopReason,
        usage: Usage,
    },
}

// ---------------------------------------------------------------------------
// API Error
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("HTTP error {status}: {body}")]
    Http {
        status: u16,
        body: String,
        /// Response headers — used for Retry-After parsing on 429 responses.
        headers: Option<std::collections::HashMap<String, String>>,
    },

    #[error("Request failed: {0}")]
    Request(#[from] reqwest::Error),

    #[error("Stream error: {0}")]
    Stream(String),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Payment required (HTTP 402) — provider balance exhausted")]
    PaymentRequired,
}
