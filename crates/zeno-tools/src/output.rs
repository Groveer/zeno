//! Tool output types — structured results from tool execution.

use serde_json::Value;

/// Trait for structured tool output.
///
/// Implementations provide multiple views of the same result:
/// - `content()`: The full content sent back to the LLM
/// - `log_preview()`: A short preview for logging/tracing
/// - `is_error()`: Whether this represents an error
///
/// This trait enables consistent formatting across all tools while
/// allowing each tool to customize its output presentation.
pub trait ToolOutput: Send + Sync {
    /// The full content string to send back to the LLM.
    fn content(&self) -> &str;

    /// A short preview for logging and TUI display (typically ≤ 80 chars).
    fn log_preview(&self) -> String {
        let c = self.content();
        if c.len() <= 80 {
            c.to_string()
        } else {
            format!("{}…", &c[..77])
        }
    }

    /// Whether this output represents an error.
    fn is_error(&self) -> bool {
        false
    }

    /// Convert to a plain string (backward compatibility).
    fn into_string(self: Box<Self>) -> String {
        self.content().to_string()
    }
}

/// JSON-structured tool output.
#[derive(Debug, Clone)]
pub struct JsonToolOutput {
    /// The full content (JSON string or plain text).
    pub content: String,
    /// Whether this represents an error.
    pub error: bool,
}

impl JsonToolOutput {
    /// Create a successful output.
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            error: false,
        }
    }

    /// Create an error output.
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            error: true,
        }
    }

    /// Create from a JSON value.
    pub fn from_json(value: &Value) -> Self {
        Self {
            content: serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
            error: false,
        }
    }
}

impl ToolOutput for JsonToolOutput {
    fn content(&self) -> &str {
        &self.content
    }

    fn is_error(&self) -> bool {
        self.error
    }
}

/// Implement ToolOutput for plain strings (backward compatibility).
impl ToolOutput for String {
    fn content(&self) -> &str {
        self
    }
}

impl ToolOutput for &str {
    fn content(&self) -> &str {
        self
    }
}
