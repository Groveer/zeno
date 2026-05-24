//! Tool trait, context, registry, and error types.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, RwLock};

/// Priority ordering for tool schemas/names: MCP first, then delegate_task, then others.
pub(crate) fn tool_priority(name: &str) -> u8 {
    if name.starts_with("mcp_") {
        0
    } else if name == "delegate_task" {
        1
    } else {
        2
    }
}

/// Categorize a tool by its "kind" for visual grouping in the system prompt.
/// Delegates to `tool_priority` for single source of truth.
pub(crate) fn tool_kind(name: &str) -> &'static str {
    match tool_priority(name) {
        0 => "mcp",
        1 => "delegate",
        _ => "other",
    }
}

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use zeno_tools::{ToolDefinition, ToolOutput};

use crate::config::settings::{DelegationConfig, ProviderConfig, Settings};
use crate::engine::cost_tracker::CostTracker;
use crate::engine::sub_agent::SubAgentEvent;
use crate::permissions::execpolicy::ExecPolicy;

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
    /// Write origin for skill_manage provenance tracking.
    /// "foreground" (default) = user-directed, "background_review" = agent-autonomous.
    pub write_origin: String,
    /// TUI event sender for routing permission prompts (and other UI events)
    /// from sub-agents to the terminal UI. When `None`, permission prompts
    /// that require user confirmation will be auto-denied (safe default).
    pub tui_event_sender:
        Option<tokio::sync::mpsc::UnboundedSender<crate::engine::tui_events::EngineEvent>>,
    /// Shared "allow all" flag from the parent engine — when the user answers
    /// "allow all" (y/a) to a permission prompt, this flag is set so subsequent
    /// tools in both the main agent AND sub-agents are auto-approved.
    pub permission_allow_all: Option<Arc<AtomicBool>>,
    /// Execution policy for rule-based command authorization.
    /// Shared with sub-agents so they respect the same exec_policy rules.
    pub exec_policy: Option<Arc<ExecPolicy>>,
}

impl std::fmt::Debug for SubAgentDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubAgentDeps")
            .field("settings", &self.settings)
            .field("write_origin", &self.write_origin)
            .field("has_tui_sender", &self.tui_event_sender.is_some())
            .field(
                "has_permission_allow_all",
                &self.permission_allow_all.is_some(),
            )
            .field("has_exec_policy", &self.exec_policy.is_some())
            .finish()
    }
}

impl SubAgentDeps {
    /// Create a new `SubAgentDeps` with the standard foreground write origin.
    pub fn new(
        client_factory: Arc<
            dyn Fn(&str, &ProviderConfig) -> Box<dyn crate::api::client::SupportsStreamingMessages>
                + Send
                + Sync,
        >,
        tool_registry: Arc<ToolRegistry>,
        settings: Arc<Settings>,
        progress_tx: tokio::sync::mpsc::UnboundedSender<SubAgentEvent>,
        delegation_config: DelegationConfig,
        cost_tracker: Arc<Mutex<CostTracker>>,
    ) -> Self {
        Self {
            client_factory,
            tool_registry,
            settings,
            progress_tx,
            delegation_config,
            cost_tracker,
            write_origin: String::from("foreground"),
            tui_event_sender: None,
            permission_allow_all: None,
            exec_policy: None,
        }
    }

    /// Set the write origin for skill_manage provenance tracking.
    /// Use `"background_review"` for autonomous background tasks.
    pub fn with_write_origin(mut self, origin: &str) -> Self {
        self.write_origin = origin.to_string();
        self
    }

    /// Attach a TUI event sender so sub-agents can prompt the user for
    /// permission confirmation. Without this, permission-required operations
    /// are auto-denied.
    pub fn with_tui_event_sender(
        mut self,
        sender: tokio::sync::mpsc::UnboundedSender<crate::engine::tui_events::EngineEvent>,
    ) -> Self {
        self.tui_event_sender = Some(sender);
        self
    }

    /// Attach the parent engine's "allow all" flag so sub-agents respect
    /// the user's session-wide blanket permission.
    pub fn with_permission_allow_all(mut self, flag: Arc<AtomicBool>) -> Self {
        self.permission_allow_all = Some(flag);
        self
    }

    /// Attach the execution policy so sub-agents respect the same
    /// rule-based command authorization as the main engine.
    pub fn with_exec_policy(mut self, policy: Arc<ExecPolicy>) -> Self {
        self.exec_policy = Some(policy);
        self
    }
}

// ---------------------------------------------------------------------------
// Tool Context
// ---------------------------------------------------------------------------

/// Execution context passed to every tool invocation.
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// Current working directory (wrapped in Arc<RwLock> so bash `cd` updates it).
    pub cwd: Arc<RwLock<PathBuf>>,
    /// Agent identifier for file-staleness tracking.
    /// "main" for the primary query, task-specific ID for sub-agents.
    pub task_id: String,
    /// For ask_user tool: channel to send the question to the TUI and receive the answer.
    pub ask_sender:
        Option<tokio::sync::mpsc::UnboundedSender<crate::engine::tui_events::EngineEvent>>,
    /// Shared MCP manager for lazy MCP server connections.
    pub mcp_manager: Option<std::sync::Arc<tokio::sync::Mutex<crate::mcp::manager::McpManager>>>,
    /// Dependencies for sub-agent delegation (set when the engine supports it).
    pub sub_agent_deps: Option<SubAgentDeps>,
    /// Cancellation token from the parent engine — tools that spawn background
    /// work (e.g. delegate_task) should link this to their own cancellation.
    pub cancel_token: Option<CancellationToken>,
    /// Shared rate limiter for tool execution (e.g. bash commands).
    /// When set, tools check this before executing to prevent runaway agents.
    pub rate_limiter: Option<crate::tools::rate_limiter::SharedRateLimiter>,
    /// Tool usage statistics collector (shared across the session).
    pub tool_stats: Option<crate::tools::tool_stats::SharedToolStats>,
    /// Shared file content pool for avoiding redundant disk reads.
    pub file_content_pool: Option<crate::tools::file_content_pool::SharedFileContentPool>,
    /// Tool registry for runtime tool discovery (used by tool_search).
    pub tool_registry: Option<std::sync::Arc<ToolRegistry>>,
}

impl ToolContext {
    /// Create a context with an ask channel (for TUI mode).
    pub fn with_ask_sender(
        cwd: PathBuf,
        sender: tokio::sync::mpsc::UnboundedSender<crate::engine::tui_events::EngineEvent>,
        mcp_manager: Option<std::sync::Arc<tokio::sync::Mutex<crate::mcp::manager::McpManager>>>,
    ) -> Self {
        Self {
            cwd: Arc::new(RwLock::new(cwd)),
            task_id: String::from("main"),
            ask_sender: Some(sender),
            mcp_manager,
            sub_agent_deps: None,
            cancel_token: None,
            rate_limiter: None,
            tool_stats: None,
            file_content_pool: None,
            tool_registry: None,
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

    /// Attach a rate limiter to this context.
    pub fn with_rate_limiter(
        mut self,
        limiter: crate::tools::rate_limiter::SharedRateLimiter,
    ) -> Self {
        self.rate_limiter = Some(limiter);
        self
    }

    /// Attach a tool stats collector to this context.
    pub fn with_tool_stats(mut self, stats: crate::tools::tool_stats::SharedToolStats) -> Self {
        self.tool_stats = Some(stats);
        self
    }

    /// Attach a file content pool to this context.
    pub fn with_file_content_pool(
        mut self,
        pool: crate::tools::file_content_pool::SharedFileContentPool,
    ) -> Self {
        self.file_content_pool = Some(pool);
        self
    }

    /// Attach a tool registry for runtime tool discovery (used by tool_search).
    pub fn with_tool_registry(mut self, registry: std::sync::Arc<ToolRegistry>) -> Self {
        self.tool_registry = Some(registry);
        self
    }

    /// Resolve a path: expand `~`, join relative paths to cwd, then normalize.
    pub fn resolve_path(&self, path: &str) -> PathBuf {
        let expanded = if path.starts_with("~/") || path == "~" {
            let suffix = path.strip_prefix("~/").unwrap_or("");
            if let Some(home) = std::env::var_os("HOME") {
                PathBuf::from(home).join(suffix)
            } else {
                PathBuf::from(path)
            }
        } else {
            PathBuf::from(path)
        };
        let joined = if expanded.is_absolute() {
            expanded
        } else {
            self.cwd.read().unwrap().join(&expanded)
        };
        // Normalize: resolve . and .. segments without following symlinks
        let mut normalized = PathBuf::new();
        for component in joined.components() {
            match component {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    normalized.pop();
                }
                other => normalized.push(other),
            }
        }
        normalized
    }

    /// Get the current working directory (thread-safe read).
    pub fn get_cwd(&self) -> PathBuf {
        self.cwd.read().unwrap().clone()
    }

    /// Set the current working directory (e.g. after bash `cd`).
    pub fn set_cwd(&self, new_cwd: PathBuf) {
        *self.cwd.write().unwrap() = new_cwd;
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

    /// Exposure level controlling how this tool is visible to the model.
    ///
    /// - `Explicit` / `Direct`: always in the tool list (default)
    /// - `Deferred`: registered but not shown initially; discoverable via `tool_search`
    /// - `Suggested`: compact summary form
    /// - `Hidden`: not visible to model; system-only
    ///
    /// Default: `ToolExposure::Explicit`.
    fn exposure(&self) -> zeno_tools::ToolExposure {
        zeno_tools::ToolExposure::Explicit
    }

    /// Whether this tool supports parallel execution with other tools.
    ///
    /// Returns `true` for read-only tools that don't conflict (e.g. `read`,
    /// `glob`, `grep`). Returns `false` for tools with side effects or
    /// user interaction.
    ///
    /// Default: `false` (sequential-only, safest default).
    fn supports_parallel(&self) -> bool {
        false
    }

    /// Structured definition of this tool (name, description, schemas, metadata).
    ///
    /// Default implementation extracts from `schema()` and `name()`.
    /// Override to provide additional metadata like `output_schema`,
    /// `supports_parallel`, or `category`.
    fn definition(&self) -> ToolDefinition {
        let schema = self.schema();
        let name = self.name().to_string();
        let description = schema
            .get("function")
            .and_then(|f| f.get("description"))
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string();
        let input_schema = schema
            .get("function")
            .and_then(|f| f.get("parameters"))
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        ToolDefinition {
            name,
            description,
            input_schema,
            output_schema: None,
            exposure: self.exposure(),
            supports_parallel: self.supports_parallel(),
            read_only: false,
            category: None,
        }
    }

    /// Execute the tool with the given arguments.
    async fn execute(
        &self,
        arguments: Value,
        ctx: &ToolContext,
    ) -> Result<Box<dyn ToolOutput>, ToolError>;

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
        let params = schema.get("function").and_then(|f| f.get("parameters"));
        let properties = params
            .and_then(|p| p.get("properties"))
            .and_then(|p| p.as_object());
        let required: Vec<&str> = params
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

        // Type validation: check provided fields against schema property types
        if let Some(props) = properties {
            for (field_name, field_schema) in props {
                if let Some(value) = obj.get(field_name.as_str()) {
                    if value.is_null() {
                        continue; // null is handled by required check above
                    }
                    if let Some(expected_type) = field_schema.get("type").and_then(|t| t.as_str()) {
                        let type_ok = match expected_type {
                            "string" => value.is_string(),
                            "integer" | "number" => value.is_number(),
                            "boolean" => value.is_boolean(),
                            "array" => value.is_array(),
                            "object" => value.is_object(),
                            _ => true, // unknown type, skip validation
                        };
                        if !type_ok {
                            return Err(ToolError::InvalidArguments(format!(
                                "{}: field '{}' expected type '{}', got {}",
                                self.name(),
                                field_name,
                                expected_type,
                                value
                            )));
                        }
                    }
                }
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

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tool_count", &self.tools.len())
            .finish()
    }
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

    /// Get all tool schemas for the LLM — only tools with `is_visible_to_model()`.
    /// Uses `ToolDefinition::to_function_schema()` which includes `output_schema`
    /// if the tool provides one via `definition()`.
    pub fn schemas(&self) -> Vec<Value> {
        let mut result: Vec<Value> = self
            .tools
            .values()
            .filter(|t| t.exposure().is_visible_to_model())
            .map(|t| t.definition().to_function_schema())
            .collect();
        result.sort_by(|a, b| {
            let a_name = a["function"]["name"].as_str().unwrap_or("");
            let b_name = b["function"]["name"].as_str().unwrap_or("");
            tool_priority(a_name).cmp(&tool_priority(b_name))
        });
        result
    }

    /// Get schemas only for the specified tool names.
    /// Used by sub-agents so they only see their allowed tools.
    pub fn schemas_for(&self, names: &[String]) -> Vec<Value> {
        names
            .iter()
            .filter_map(|n| self.tools.get(n.as_str()))
            .map(|t| t.definition().to_function_schema())
            .collect()
    }

    /// Execute a tool by name. Validates input before execution.
    /// Returns an error if the tool is not found.
    pub async fn execute(
        &self,
        name: &str,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<Box<dyn ToolOutput>, ToolError> {
        let start = std::time::Instant::now();
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| ToolError::Execution(format!("Unknown tool: {}", name)))?;
        // Validate input against schema before execution
        tool.validate_input(&args)?;
        let result = tool.execute(args, ctx).await;

        // Record tool usage statistics
        let duration = start.elapsed().as_secs_f64();
        let success = result.is_ok();
        if let Some(ref stats) = ctx.tool_stats
            && let Ok(mut stats) = stats.lock()
        {
            stats.record(name, duration, success);
        }

        result
    }

    /// Get a tool by name (for introspection: is_read_only, validate_input, etc.).
    #[allow(clippy::borrowed_box)]
    pub fn get(&self, name: &str) -> Option<&Box<dyn Tool>> {
        self.tools.get(name)
    }

    /// List registered tool names — sorted with MCP tools first.
    pub fn names(&self) -> Vec<&str> {
        let mut result: Vec<&str> = self.tools.keys().map(|s| s.as_str()).collect();
        result.sort_by_key(|a| tool_priority(a));
        result
    }

    /// Search tools by name/description keyword (fuzzy match).
    /// Used by the tool_search mechanism to discover Deferred tools.
    pub fn search_tools(&self, query: &str) -> Vec<ToolDefinition> {
        let query_lower = query.to_lowercase();
        let mut result: Vec<ToolDefinition> = self
            .tools
            .values()
            .filter(|t| t.exposure().is_discoverable())
            .filter(|t| {
                let name = t.name().to_lowercase();
                let def = t.definition();
                name.contains(&query_lower)
                    || def.description.to_lowercase().contains(&query_lower)
                    || def
                        .category
                        .as_ref()
                        .is_some_and(|c| c.to_lowercase().contains(&query_lower))
            })
            .map(|t| t.definition())
            .collect();
        result.sort_by_key(|d| {
            let name_lower = d.name.to_lowercase();
            if name_lower == query_lower {
                0
            } else if name_lower.starts_with(&query_lower) {
                1
            } else {
                2
            }
        });
        result
    }
}
