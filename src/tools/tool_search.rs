//! Tool search — discover tools by keyword at runtime.
//!
//! This tool is registered with `Exposure::Deferred` so it's not included in
//! the initial tool list sent to the LLM. The model can discover it at runtime
//! via the `tool_search` mechanism, enabling on-demand discovery of niche or
//! rarely-used tools without wasting tokens on their schemas.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::base::{Tool, ToolContext, ToolError};
use zeno_tools::{JsonToolOutput, ToolOutput};

pub struct ToolSearchTool;

impl ToolSearchTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn name(&self) -> &str {
        "tool_search"
    }

    fn exposure(&self) -> zeno_tools::ToolExposure {
        zeno_tools::ToolExposure::Deferred
    }

    fn supports_parallel(&self) -> bool {
        true
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "tool_search",
                "description": "Search registered tools by name, description, or category. \
                    Discover tools that are not shown in the initial tool list. \
                    Use this when you need a tool that might exist but wasn't offered directly.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Search keyword to match against tool names, descriptions, or categories."
                        }
                    },
                    "required": ["query"]
                }
            }
        })
    }

    async fn execute(
        &self,
        arguments: Value,
        ctx: &ToolContext,
    ) -> Result<Box<dyn ToolOutput>, ToolError> {
        let query = arguments
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("missing 'query'".into()))?;

        let registry = ctx
            .tool_registry
            .as_ref()
            .ok_or_else(|| ToolError::Execution("Tool registry not available".into()))?;

        let results = registry.search_tools(query);
        let summary = if results.is_empty() {
            format!("No tools found matching '{}'.", query)
        } else {
            let mut lines: Vec<String> = results
                .iter()
                .map(|d| format!("- **{}**: {}", d.name, d.description))
                .collect();
            lines.insert(
                0,
                format!("Found {} tool(s) matching '{}':", results.len(), query),
            );
            lines.join("\n")
        };

        Ok(Box::new(JsonToolOutput::success(summary)))
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }
}
