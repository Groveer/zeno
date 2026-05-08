#![allow(dead_code)]
//! MCP gateway tools — 4 meta-tools that proxy calls to MCP servers.

use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;

use super::manager::McpManager;
use crate::tools::base::{Tool, ToolContext, ToolError};

// ---------------------------------------------------------------------------
// Shared helper
// ---------------------------------------------------------------------------

fn get_manager(ctx: &ToolContext) -> Result<Arc<Mutex<McpManager>>, ToolError> {
    ctx.mcp_manager
        .clone()
        .ok_or_else(|| ToolError::Execution("MCP manager not available in this context".into()))
}

// ---------------------------------------------------------------------------
// mcp_list_servers
// ---------------------------------------------------------------------------

pub struct McpListServersTool;

impl McpListServersTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for McpListServersTool {
    fn name(&self) -> &str {
        "mcp_list_servers"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "mcp_list_servers",
                "description": "List all configured MCP servers and their current status (stopped, starting, connected, failed).",
                "parameters": {
                    "type": "object",
                    "properties": {},
                    "required": []
                }
            }
        })
    }

    async fn execute(&self, _arguments: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let mgr = get_manager(ctx)?;
        let mgr = mgr.lock().await;
        Ok(mgr.summary())
    }

    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// mcp_list_tools
// ---------------------------------------------------------------------------

pub struct McpListToolsTool;

impl McpListToolsTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for McpListToolsTool {
    fn name(&self) -> &str {
        "mcp_list_tools"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "mcp_list_tools",
                "description": "Discover available tools on a specific MCP server. Connects to the server if not already connected (on-demand startup).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "server_name": {
                            "type": "string",
                            "description": "Name of the MCP server to query"
                        }
                    },
                    "required": ["server_name"]
                }
            }
        })
    }

    async fn execute(&self, arguments: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let server_name = arguments
            .get("server_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("server_name is required".into()))?;

        let mgr = get_manager(ctx)?;
        let mut mgr = mgr.lock().await;
        mgr.discover_tools(server_name).await
    }

    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// mcp_describe_tool
// ---------------------------------------------------------------------------

pub struct McpDescribeToolTool;

impl McpDescribeToolTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for McpDescribeToolTool {
    fn name(&self) -> &str {
        "mcp_describe_tool"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "mcp_describe_tool",
                "description": "Get detailed schema for a specific tool on an MCP server. Returns the tool's parameter schema.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "server_name": {
                            "type": "string",
                            "description": "Name of the MCP server"
                        },
                        "tool_name": {
                            "type": "string",
                            "description": "Name of the tool to describe"
                        }
                    },
                    "required": ["server_name", "tool_name"]
                }
            }
        })
    }

    async fn execute(&self, arguments: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let server_name = arguments
            .get("server_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("server_name is required".into()))?;
        let tool_name = arguments
            .get("tool_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("tool_name is required".into()))?;

        let mgr = get_manager(ctx)?;
        let mut mgr = mgr.lock().await;

        // Ensure connected
        mgr.ensure_connected(server_name).await?;

        let state = mgr
            .get_mut(server_name)
            .ok_or_else(|| ToolError::Execution(format!("Server '{}' not found", server_name)))?;

        let tool = state
            .tools_cache
            .iter()
            .find(|t| t.name.as_str() == tool_name);
        match tool {
            Some(t) => Ok(serde_json::to_string_pretty(&json!({
                "server": server_name,
                "tool": t.name,
                "description": t.description,
                "parameters": t.input_schema,
            }))
            .map_err(|e| ToolError::Execution(e.to_string()))?),
            None => Err(ToolError::Execution(format!(
                "Tool '{}' not found on server '{}'. Available: {}",
                tool_name,
                server_name,
                state
                    .tools_cache
                    .iter()
                    .map(|t| t.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }

    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// mcp_call_tool
// ---------------------------------------------------------------------------

pub struct McpCallToolTool;

impl McpCallToolTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for McpCallToolTool {
    fn name(&self) -> &str {
        "mcp_call_tool"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "mcp_call_tool",
                "description": "Execute a tool on a specific MCP server. Connects to the server if not already connected (on-demand startup). Use mcp_list_tools first to discover available tools.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "server_name": {
                            "type": "string",
                            "description": "Name of the MCP server"
                        },
                        "tool_name": {
                            "type": "string",
                            "description": "Name of the tool to call"
                        },
                        "arguments": {
                            "type": "object",
                            "description": "Arguments to pass to the tool (optional)"
                        }
                    },
                    "required": ["server_name", "tool_name"]
                }
            }
        })
    }

    async fn execute(&self, arguments: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let server_name = arguments
            .get("server_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("server_name is required".into()))?;
        let tool_name = arguments
            .get("tool_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("tool_name is required".into()))?;
        let tool_args = arguments
            .get("arguments")
            .cloned()
            .unwrap_or(Value::Object(Default::default()));

        let mgr = get_manager(ctx)?;
        let mgr_clone = mgr.clone();

        // ensure_connected + call_tool need separate lock scopes
        // because call_tool is async and we can't hold MutexGuard across .await
        {
            let mut mgr = mgr_clone.lock().await;
            mgr.ensure_connected(server_name).await?;
        }

        McpManager::call_tool_static(&mgr_clone, server_name, tool_name, tool_args).await
    }
}
