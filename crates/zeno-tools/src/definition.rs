//! Tool definition — declarative metadata for a tool.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::exposure::ToolExposure;

/// Declarative metadata for a tool.
///
/// Unlike the `Tool` trait (which handles execution), `ToolDefinition`
/// is a lightweight, serializable description used for:
/// - Sending tool schemas to the LLM
/// - Displaying tool info in the UI
/// - Filtering tools by exposure level
/// - Registering tools from external sources (MCP, plugins)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Tool name — must match the LLM function calling name.
    pub name: String,
    /// Human-readable description of what the tool does.
    pub description: String,
    /// JSON Schema describing the tool's input parameters.
    pub input_schema: Value,
    /// Optional JSON Schema describing the tool's output format.
    /// When set, the model knows what structure to expect back.
    pub output_schema: Option<Value>,
    /// Visibility level controlling model exposure.
    #[serde(default)]
    pub exposure: ToolExposure,
    /// Whether this tool supports being called in parallel with other tools
    /// in the same turn. Default: false (sequential only).
    #[serde(default)]
    pub supports_parallel: bool,
    /// Whether this tool is read-only (no side effects).
    /// Used for automatic permission decisions.
    #[serde(default)]
    pub read_only: bool,
    /// Tool category for UI grouping (e.g. "file", "search", "execution").
    #[serde(default)]
    pub category: Option<String>,
}

impl ToolDefinition {
    /// Create a basic tool definition with defaults.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            output_schema: None,
            exposure: ToolExposure::Explicit,
            supports_parallel: false,
            read_only: false,
            category: None,
        }
    }

    /// Set the output schema.
    pub fn with_output_schema(mut self, schema: Value) -> Self {
        self.output_schema = Some(schema);
        self
    }

    /// Set the exposure level.
    pub fn with_exposure(mut self, exposure: ToolExposure) -> Self {
        self.exposure = exposure;
        self
    }

    /// Mark this tool as supporting parallel execution.
    pub fn with_parallel(mut self, supports: bool) -> Self {
        self.supports_parallel = supports;
        self
    }

    /// Mark this tool as read-only.
    pub fn with_read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// Set the tool category.
    pub fn with_category(mut self, category: impl Into<String>) -> Self {
        self.category = Some(category.into());
        self
    }

    /// Convert to the OpenAI function-calling JSON schema format.
    pub fn to_function_schema(&self) -> Value {
        let mut func = serde_json::json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.input_schema,
            }
        });
        if let Some(ref output) = self.output_schema {
            func["function"]["output_schema"] = output.clone();
        }
        func
    }
}
