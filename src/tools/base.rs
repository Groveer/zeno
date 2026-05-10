//! Tool trait, context, registry, and error types.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::config::settings::{DelegationConfig, ProviderConfig, Settings};
use crate::engine::cost_tracker::CostTracker;
use crate::engine::sub_agent::SubAgentEvent;

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
// Sub-agent dependencies
// ---------------------------------------------------------------------------

/// Dependencies needed to spawn sub-agents from tools.
/// Carried in `ToolContext` so the `delegate_task` tool can create sub-agents.
#[derive(Clone)]
#[allow(dead_code, reason = "used by delegate_task tool via ToolContext")]
pub struct SubAgentDeps {
    /// Factory to create API clients for sub-agents.
    pub client_factory: Arc<
        dyn Fn(&str, &ProviderConfig) -> Box<dyn crate::api::client::SupportsStreamingMessages>
            + Send
            + Sync,
    >,
    /// The parent's tool registry (shared reference).
    pub tool_registry: Arc<ToolRegistry>,
    /// Application settings.
    pub settings: Arc<Settings>,
    /// Channel to send sub-agent progress events to the TUI.
    pub progress_tx: tokio::sync::mpsc::UnboundedSender<SubAgentEvent>,
    /// Delegation config (max_concurrent, timeout).
    pub delegation_config: DelegationConfig,
    /// Shared cost tracker — sub-agents fold their token usage into this.
    pub cost_tracker: Arc<Mutex<CostTracker>>,
}

impl std::fmt::Debug for SubAgentDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubAgentDeps")
            .field("settings", &self.settings)
            .finish()
    }
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
    /// Dependencies for sub-agent delegation (set when the engine supports it).
    pub sub_agent_deps: Option<SubAgentDeps>,
    /// Cancellation token from the parent engine — tools that spawn background
    /// work (e.g. delegate_task) should link this to their own cancellation.
    pub cancel_token: Option<CancellationToken>,
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
            sub_agent_deps: None,
            cancel_token: None,
        }
    }

    /// Attach sub-agent dependencies to this context.
    pub fn with_sub_agent_deps(mut self, deps: SubAgentDeps) -> Self {
        self.sub_agent_deps = Some(deps);
        self
    }

    /// Attach a cancellation token to this context.
    pub fn with_cancel_token(mut self, token: CancellationToken) -> Self {
        self.cancel_token = Some(token);
        self
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
// Tool Registry
// ---------------------------------------------------------------------------

/// Static registry of available tools.
///
/// After construction, wrap in `Arc` for shared ownership (e.g. sub-agent support).
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Wrap in `Arc` for shared ownership.
    #[allow(
        dead_code,
        reason = "used by sub-agent engine for ToolRegistry sharing"
    )]
    pub fn into_arc(self) -> Arc<Self> {
        Arc::new(self)
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

    /// Get schemas only for the specified tool names.
    /// Used by sub-agents so they only see their allowed tools.
    pub fn schemas_for(&self, names: &[String]) -> Vec<Value> {
        names
            .iter()
            .filter_map(|n| self.tools.get(n.as_str()))
            .map(|t| t.schema())
            .collect()
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
}
