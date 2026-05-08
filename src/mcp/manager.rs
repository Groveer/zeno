//! MCP server manager — lazy on-demand connections to MCP servers.
//!
//! Servers are NOT started at boot. The first `discover_tools` or `call_tool`
//! triggers the actual connection (spawn subprocess for stdio, HTTP for url).

use std::collections::HashMap;

use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use rmcp::{Peer, ServiceExt};
use serde_json::Value;

use crate::config::settings::McpServerConfig;

// ---------------------------------------------------------------------------
// Server status
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum ServerStatus {
    Stopped,
    Starting,
    Connected,
    Failed(String),
}

// ---------------------------------------------------------------------------
// Cached tool info (lightweight, serializable)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CachedTool {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

// ---------------------------------------------------------------------------
// Per-server state
// ---------------------------------------------------------------------------

struct ServerConnection {
    peer: Peer<RoleClient>,
    /// We must hold the RunningService to keep the background task alive.
    /// For stdio this also keeps the child process alive.
    _service: RunningService<RoleClient, ()>,
}

impl std::fmt::Debug for ServerConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConnection").finish()
    }
}

pub struct McpServerState {
    pub config: McpServerConfig,
    pub status: ServerStatus,
    connection: Option<ServerConnection>,
    pub tools_cache: Vec<CachedTool>,
}

// ---------------------------------------------------------------------------
// McpManager
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct McpManager {
    servers: HashMap<String, McpServerState>,
}

impl std::fmt::Debug for McpServerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServerState")
            .field("config", &self.config)
            .field("status", &self.status)
            .field("tools_count", &self.tools_cache.len())
            .finish()
    }
}

impl McpManager {
    /// Create from config — zero I/O, servers stay in Stopped state.
    pub fn from_config(configs: &HashMap<String, McpServerConfig>) -> Self {
        let servers = configs
            .iter()
            .map(|(name, config)| {
                (
                    name.clone(),
                    McpServerState {
                        config: McpServerConfig {
                            command: config.command.clone(),
                            url: config.url.clone(),
                            headers: config.headers.clone(),
                        },
                        status: ServerStatus::Stopped,
                        connection: None,
                        tools_cache: Vec::new(),
                    },
                )
            })
            .collect();
        Self { servers }
    }

    /// Summary text for `/mcp` command.
    pub fn summary(&self) -> String {
        if self.servers.is_empty() {
            return "No MCP servers configured.".to_string();
        }
        let lines: Vec<String> = self
            .servers
            .iter()
            .map(|(name, s)| {
                let status = match &s.status {
                    ServerStatus::Stopped => "stopped".to_string(),
                    ServerStatus::Starting => "starting".to_string(),
                    ServerStatus::Connected => {
                        format!("connected ({} tools)", s.tools_cache.len())
                    }
                    ServerStatus::Failed(e) => format!("failed: {}", e),
                };
                let transport = if s.config.url.is_some() {
                    "http"
                } else {
                    "stdio"
                };
                format!("- {} [{}] ({})", name, status, transport)
            })
            .collect();
        format!(
            "MCP servers ({}):\n{}",
            self.servers.len(),
            lines.join("\n")
        )
    }

    // -----------------------------------------------------------------------
    // Lazy connection
    // -----------------------------------------------------------------------

    /// Ensure a server is connected. No-op if already connected.
    pub async fn ensure_connected(
        &mut self,
        name: &str,
    ) -> Result<(), crate::tools::base::ToolError> {
        // Check if already connected (immutable borrow first)
        if let Some(state) = self.servers.get(name) {
            if state.status == ServerStatus::Connected && state.connection.is_some() {
                return Ok(());
            }
        }

        // Server exists?
        if !self.servers.contains_key(name) {
            let available: Vec<&str> = self.servers.keys().map(|s| s.as_str()).collect();
            return Err(crate::tools::base::ToolError::Execution(format!(
                "MCP server '{}' not found. Available: {}",
                name,
                available.join(", ")
            )));
        }

        // Take mutable borrow
        let state = self.servers.get_mut(name).unwrap();
        state.status = ServerStatus::Starting;
        let config = state.config.clone();

        tracing::info!(server = %name, "Connecting to MCP server (lazy)");

        match connect_server(&config).await {
            Ok((peer, service)) => {
                // Discover tools
                let tools = match peer.list_all_tools().await {
                    Ok(tools) => tools,
                    Err(e) => {
                        tracing::warn!(server = %name, error = %e, "Failed to list tools");
                        Vec::new()
                    }
                };

                let cached: Vec<CachedTool> = tools
                    .into_iter()
                    .map(|t| {
                        let schema = serde_json::to_value(&t.input_schema)
                            .unwrap_or(Value::Object(Default::default()));
                        CachedTool {
                            name: t.name.to_string(),
                            description: t.description.map(|d| d.to_string()),
                            input_schema: schema,
                        }
                    })
                    .collect();

                let count = cached.len();
                let state = self.servers.get_mut(name).unwrap();
                state.status = ServerStatus::Connected;
                state.connection = Some(ServerConnection {
                    peer,
                    _service: service,
                });
                state.tools_cache = cached;

                tracing::info!(server = %name, tools = count, "MCP server connected");
                Ok(())
            }
            Err(e) => {
                let msg = e.to_string();
                tracing::warn!(server = %name, error = %msg, "MCP server connection failed");
                let state = self.servers.get_mut(name).unwrap();
                state.status = ServerStatus::Failed(msg.clone());
                Err(crate::tools::base::ToolError::Execution(format!(
                    "Failed to connect to MCP server '{}': {}",
                    name, msg
                )))
            }
        }
    }

    /// Discover tools on a server (connects if needed). Returns summary text.
    pub async fn discover_tools(
        &mut self,
        name: &str,
    ) -> Result<String, crate::tools::base::ToolError> {
        self.ensure_connected(name).await?;

        let state = self.servers.get(name).unwrap();
        if state.tools_cache.is_empty() {
            return Ok(format!(
                "Server '{}' connected but reported no tools.",
                name
            ));
        }

        let lines: Vec<String> = state
            .tools_cache
            .iter()
            .map(|t| {
                let desc = t.description.as_deref().unwrap_or("");
                if desc.is_empty() {
                    format!("- {}", t.name)
                } else {
                    format!("- {}: {}", t.name, desc)
                }
            })
            .collect();

        Ok(format!(
            "Server '{}' ({} tools):\n{}",
            name,
            lines.len(),
            lines.join("\n")
        ))
    }

    // -----------------------------------------------------------------------
    // Static call_tool (needs separate lock scope for async .await)
    // -----------------------------------------------------------------------

    /// Call a tool on an MCP server. The manager is behind Arc<Mutex> so
    /// callers must lock, ensure_connected, drop lock, then call this.
    pub async fn call_tool_static(
        manager: &std::sync::Arc<tokio::sync::Mutex<McpManager>>,
        server_name: &str,
        tool_name: &str,
        arguments: Value,
    ) -> Result<String, crate::tools::base::ToolError> {
        // Get a clone of the peer while holding the lock briefly
        let peer = {
            let mgr = manager.lock().await;
            let state = mgr.servers.get(server_name).ok_or_else(|| {
                crate::tools::base::ToolError::Execution(format!(
                    "MCP server '{}' not connected",
                    server_name
                ))
            })?;
            let conn = state.connection.as_ref().ok_or_else(|| {
                crate::tools::base::ToolError::Execution(format!(
                    "MCP server '{}' not connected",
                    server_name
                ))
            })?;
            conn.peer.clone()
        };

        // Build the arguments map
        let args_map = match &arguments {
            Value::Object(map) => Some(map.clone()),
            Value::Null => None,
            other => {
                // Wrap non-object in a map for servers that expect it
                let mut m = serde_json::Map::new();
                m.insert("value".to_string(), other.clone());
                Some(m)
            }
        };

        let params = CallToolRequestParams::new(tool_name.to_string())
            .with_arguments(args_map.unwrap_or_default());

        let result = peer.call_tool(params).await.map_err(|e| {
            crate::tools::base::ToolError::Execution(format!(
                "MCP tool call failed on '{}/{}': {}",
                server_name, tool_name, e
            ))
        })?;

        // Extract text from content blocks
        let mut parts = Vec::new();
        for block in &result.content {
            if let Some(text) = block.as_text() {
                parts.push(text.text.clone());
            }
        }

        if result.is_error.unwrap_or(false) {
            let error_text = parts.join("\n");
            return Err(crate::tools::base::ToolError::Execution(format!(
                "MCP tool '{}/{}' returned error: {}",
                server_name, tool_name, error_text
            )));
        }

        Ok(parts.join("\n"))
    }

    /// Get mutable reference to a server state (for mcp_describe_tool).
    pub fn get_mut(&mut self, name: &str) -> Option<&mut McpServerState> {
        self.servers.get_mut(name)
    }

    /// Whether any servers are configured.
    #[allow(dead_code)]
    pub fn has_servers(&self) -> bool {
        !self.servers.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Connection helpers
// ---------------------------------------------------------------------------

/// Connect to a single MCP server. Returns (peer, running_service).
async fn connect_server(
    config: &McpServerConfig,
) -> anyhow::Result<(Peer<RoleClient>, RunningService<RoleClient, ()>)> {
    // HTTP transport
    if let Some(ref url) = config.url {
        use reqwest::header::{HeaderName, HeaderValue};

        use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
        let mut transport_config = StreamableHttpClientTransportConfig::with_uri(url.as_str())
            .reinit_on_expired_session(true);

        // Apply custom headers from config
        if !config.headers.is_empty() {
            let mut custom_headers = std::collections::HashMap::new();
            for (name, value) in &config.headers {
                let header_name = HeaderName::from_bytes(name.as_bytes())
                    .map_err(|e| anyhow::anyhow!("Invalid header name '{}': {}", name, e))?;
                let header_value = HeaderValue::from_str(value)
                    .map_err(|e| anyhow::anyhow!("Invalid header value for '{}': {}", name, e))?;
                custom_headers.insert(header_name, header_value);
            }
            transport_config = transport_config.custom_headers(custom_headers);
        }

        let transport = rmcp::transport::StreamableHttpClientTransport::with_client(
            reqwest::Client::new(),
            transport_config,
        );
        let service = ()
            .serve(transport)
            .await
            .map_err(|e| anyhow::anyhow!("MCP HTTP connect failed: {:?}", e))?;
        let peer = service.peer().clone();
        return Ok((peer, service));
    }

    // Stdio transport
    if let Some(ref command) = config.command {
        if command.is_empty() {
            anyhow::bail!("Empty command for MCP server");
        }
        let program = &command[0];
        let args: Vec<&str> = command[1..].iter().map(|s| s.as_str()).collect();

        let mut cmd = tokio::process::Command::new(program);
        cmd.args(&args);

        let process = TokioChildProcess::new(cmd)?;
        let service = ()
            .serve(process)
            .await
            .map_err(|e| anyhow::anyhow!("MCP stdio connect failed: {:?}", e))?;
        let peer = service.peer().clone();
        return Ok((peer, service));
    }

    anyhow::bail!("MCP server config must have either 'command' or 'url'")
}
