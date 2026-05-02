#![allow(dead_code)]
//! MCP tool proxy — delegates tool calls to MCP servers.

//!

//! When an MCP server provides a tool, this module creates a Tool

//! implementation that forwards the call to the server via JSON-RPC.

//! Currently stubbed — actual MCP calls require the rmcp crate.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::base::{Tool, ToolContext, ToolError};

/// A tool that proxies calls to an MCP server.
pub struct McpToolProxy {
    pub server_name: String,
    pub tool_name: String,
    pub description: String,
    pub parameters: Value,
}

impl McpToolProxy {
    pub fn new(
        server_name: String,
        tool_name: String,
        description: String,
        parameters: Value,
    ) -> Self {
        Self {
            server_name,
            tool_name,
            description,
            parameters,
        }
    }
}

#[async_trait]
impl Tool for McpToolProxy {
    fn name(&self) -> &str {
        // Use server_tool naming to avoid collisions
        &self.tool_name
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.tool_name,
                "description": self.description,
                "parameters": self.parameters,
            }
        })
    }

    async fn execute(&self, _arguments: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        // TODO: Implement actual MCP tool call via rmcp
        // 1. Find the MCP server connection in McpManager
        // 2. Send tools/call JSON-RPC request
        // 3. Return the result
        Err(ToolError::Execution(format!(
            "MCP tool '{}' on server '{}' is not yet connected. \
             MCP server startup is required first.",
            self.tool_name, self.server_name
        )))
    }
}
