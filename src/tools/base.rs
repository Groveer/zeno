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

    #[error("Permission denied: {0}")]
    PermissionDenied(String),

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
    /// Environment variables (subset, e.g. PATH, HOME).
    pub env: HashMap<String, String>,
}

impl ToolContext {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            env: HashMap::new(),
        }
    }

    /// Resolve a path relative to cwd.
    pub fn resolve_path(&self, path: &str) -> PathBuf {
        let p = PathBuf::from(path);
        if p.is_absolute() {
            p
        } else {
            self.cwd.join(p)
        }
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
    async fn execute(
        &self,
        arguments: Value,
        ctx: &ToolContext,
    ) -> Result<String, ToolError>;
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

    /// Register a tool. Panics if a tool with the same name is already registered.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        let name = tool.name().to_string();
        if self.tools.contains_key(&name) {
            panic!("Tool '{}' already registered", name);
        }
        self.tools.insert(name, tool);
    }

    /// Get all tool schemas for the LLM.
    pub fn schemas(&self) -> Vec<Value> {
        self.tools.values().map(|t| t.schema()).collect()
    }

    /// Execute a tool by name.
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
        tool.execute(args, ctx).await
    }

    /// Check if a tool is registered.
    pub fn has(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// List registered tool names.
    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(|s| s.as_str()).collect()
    }
}
