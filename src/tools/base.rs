//! Tool trait, context, registry, and error types.

use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Tool Error
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("Execution failed: {0}")]
    Execution(String),

    #[error("Invalid arguments: {0}")]
    InvalidArguments(String),

    #[error("File not found: {0}")]
    NotFound(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Timeout: {0}")]
    Timeout(String),
}

// ---------------------------------------------------------------------------
// Tool Context
// ---------------------------------------------------------------------------

/// Execution context passed to every tool invocation.
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// Current working directory.
    pub cwd: PathBuf,
    /// For ask_user tool: channel to send the question to the TUI and receive the answer.
    pub ask_sender: Option<tokio::sync::mpsc::UnboundedSender<crate::engine::tui_events::UiEvent>>,
    /// Shared MCP manager for lazy MCP server connections.
    pub mcp_manager: Option<std::sync::Arc<tokio::sync::Mutex<crate::mcp::manager::McpManager>>>,
}

impl ToolContext {
    /// Create a context with an ask channel (for TUI mode).
    pub fn with_ask_sender(
        cwd: PathBuf,
        sender: tokio::sync::mpsc::UnboundedSender<crate::engine::tui_events::UiEvent>,
        mcp_manager: Option<std::sync::Arc<tokio::sync::Mutex<crate::mcp::manager::McpManager>>>,
    ) -> Self {
        Self {
            cwd,
            ask_sender: Some(sender),
            mcp_manager,
        }
    }

    /// Resolve a path relative to cwd.
    pub fn resolve_path(&self, path: &str) -> PathBuf {
        let p = PathBuf::from(path);
        if p.is_absolute() { p } else { self.cwd.join(p) }
    }
}

// ---------------------------------------------------------------------------
// Tool Trait
// ---------------------------------------------------------------------------

/// A tool that can be invoked by the LLM via function calling.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Tool name, must match the LLM function calling name.
    fn name(&self) -> &str;

    /// JSON Schema describing the tool's parameters.
    fn schema(&self) -> Value;

    /// Execute the tool with the given arguments.
    async fn execute(&self, arguments: Value, ctx: &ToolContext) -> Result<String, ToolError>;

    /// Whether this tool is read-only (no side effects).
    /// Default implementation returns false.
    fn is_read_only(&self, _input: &Value) -> bool {
        false
    }

    /// Validate tool input against the schema's required fields.
    /// Default implementation checks that all fields listed in
    /// `function.parameters.required` exist and are non-null.
    /// Override for custom validation (type checks, enum values, etc.).
    fn validate_input(&self, arguments: &Value) -> Result<(), ToolError> {
        let schema = self.schema();
        let required: Vec<&str> = schema
            .get("function")
            .and_then(|f| f.get("parameters"))
            .and_then(|p| p.get("required"))
            .and_then(|r| r.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();

        let obj = match arguments.as_object() {
            Some(o) => o,
            None => {
                if required.is_empty() {
                    return Ok(());
                }
                return Err(ToolError::InvalidArguments(format!(
                    "{}: expected object input, got {}",
                    self.name(),
                    arguments
                )));
            }
        };

        for field in &required {
            match obj.get(*field) {
                None | Some(Value::Null) => {
                    return Err(ToolError::InvalidArguments(format!(
                        "{}: missing required field '{}'",
                        self.name(),
                        field
                    )));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tool Summary
// ---------------------------------------------------------------------------

/// Lightweight summary of a tool for system prompt injection.
pub struct ToolSummary {
    pub name: String,
    pub description: String,
}

// ---------------------------------------------------------------------------
// Tool Registry
// ---------------------------------------------------------------------------

/// Static registry of available tools.
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Register a tool. Returns an error if a tool with the same name is already registered.
    pub fn register(&mut self, tool: Box<dyn Tool>) -> Result<(), ToolError> {
        let name = tool.name().to_string();
        if self.tools.contains_key(&name) {
            return Err(ToolError::Execution(format!(
                "Tool '{}' already registered",
                name
            )));
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    /// Get all tool schemas for the LLM.
    pub fn schemas(&self) -> Vec<Value> {
        self.tools.values().map(|t| t.schema()).collect()
    }

    /// Execute a tool by name. Validates input before execution.
    pub async fn execute(
        &self,
        name: &str,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<String, ToolError> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| ToolError::Execution(format!("Unknown tool: {}", name)))?;
        // Validate input against schema before execution
        tool.validate_input(&args)?;
        tool.execute(args, ctx).await
    }

    /// Get a tool by name (for introspection: is_read_only, validate_input, etc.).
    #[allow(clippy::borrowed_box)]
    pub fn get(&self, name: &str) -> Option<&Box<dyn Tool>> {
        self.tools.get(name)
    }

    /// List registered tool names.
    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(|s| s.as_str()).collect()
    }

    /// Get summaries of all registered tools (name + description) for system prompt.
    pub fn summaries(&self) -> Vec<ToolSummary> {
        self.tools
            .values()
            .map(|t| {
                let desc = t
                    .schema()
                    .get("function")
                    .and_then(|f| f.get("description"))
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string();
                ToolSummary {
                    name: t.name().to_string(),
                    description: desc,
                }
            })
            .collect()
    }
}
