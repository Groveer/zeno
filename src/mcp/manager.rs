#![allow(dead_code)]
//! MCP server manager — manage MCP server connections and tool proxying.

//!

//! MCP (Model Context Protocol) allows external tools to be registered

//! via stdio or HTTP transport. This manager handles server lifecycle

//! and tool proxy registration.

//!

//! Note: Full MCP integration requires the `rmcp` crate. This implementation

//! provides the configuration and management scaffolding; actual MCP tool

//! calls will be wired up when rmcp is integrated.

use std::collections::HashMap;

use crate::config::settings::McpServerConfig;

/// Status of an MCP server.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerStatus {
    /// Server has not been started yet.
    Stopped,
    /// Server is starting up.
    Starting,
    /// Server is connected and ready.
    Connected,
    /// Server failed to start or connect.
    Failed(String),
}

/// Information about an MCP server.
#[derive(Debug)]
pub struct McpServer {
    pub name: String,
    pub config: McpServerConfig,
    pub status: ServerStatus,
    pub tools: Vec<String>,
}

/// Manager for MCP server connections.
pub struct McpManager {
    servers: HashMap<String, McpServer>,
}

impl McpManager {
    /// Create a new MCP manager from configuration.
    pub fn from_config(configs: &HashMap<String, McpServerConfig>) -> Self {
        let servers: HashMap<String, McpServer> = configs
            .iter()
            .map(|(name, config)| {
                (
                    name.clone(),
                    McpServer {
                        name: name.clone(),
                        config: McpServerConfig {
                            command: config.command.clone(),
                            url: config.url.clone(),
                        },
                        status: ServerStatus::Stopped,
                        tools: Vec::new(),
                    },
                )
            })
            .collect();

        Self { servers }
    }

    /// List all configured server names.
    pub fn server_names(&self) -> Vec<&str> {
        self.servers.keys().map(|s| s.as_str()).collect()
    }

    /// Get a server by name.
    pub fn get(&self, name: &str) -> Option<&McpServer> {
        self.servers.get(name)
    }

    /// Get a mutable reference to a server.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut McpServer> {
        self.servers.get_mut(name)
    }

    /// Get all tools from all connected servers.
    pub fn all_tools(&self) -> Vec<(&str, &str)> {
        let mut tools = Vec::new();
        for server in self.servers.values() {
            if server.status == ServerStatus::Connected {
                for tool in &server.tools {
                    tools.push((server.name.as_str(), tool.as_str()));
                }
            }
        }
        tools
    }

    /// Start a server by name (currently stubbed — real implementation needs rmcp).
    pub async fn start(&mut self, name: &str) -> anyhow::Result<()> {
        let server = self
            .servers
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("MCP server '{}' not found", name))?;

        server.status = ServerStatus::Starting;
        tracing::info!(server_name = %name, "Starting MCP server");

        // TODO: Implement actual MCP server startup using rmcp crate.
        // For stdio servers: spawn the command, establish JSON-RPC connection.
        // For HTTP servers: connect to the URL, discover tools.
        //
        // Stub: mark as connected with no tools
        server.status = ServerStatus::Connected;
        server.tools = Vec::new();

        Ok(())
    }

    /// Stop a server by name.
    pub async fn stop(&mut self, name: &str) -> anyhow::Result<()> {
        let server = self
            .servers
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("MCP server '{}' not found", name))?;

        if server.status == ServerStatus::Stopped {
            return Ok(());
        }

        tracing::info!(server_name = %name, "Stopping MCP server");
        server.status = ServerStatus::Stopped;
        server.tools.clear();
        Ok(())
    }

    /// Start all configured servers.
    pub async fn start_all(&mut self) {
        let names: Vec<String> = self.servers.keys().cloned().collect();
        for name in names {
            if let Err(e) = self.start(&name).await {
                if let Some(server) = self.servers.get_mut(&name) {
                    server.status = ServerStatus::Failed(e.to_string());
                }
                tracing::warn!(server_name = %name, error = %e, "Failed to start MCP server");
            }
        }
    }

    /// Stop all servers.
    pub async fn stop_all(&mut self) {
        let names: Vec<String> = self.servers.keys().cloned().collect();
        for name in names {
            if let Err(e) = self.stop(&name).await {
                tracing::warn!(server_name = %name, error = %e, "Failed to stop MCP server");
            }
        }
    }

    /// Get a summary of all servers and their status.
    pub fn summary(&self) -> String {
        if self.servers.is_empty() {
            return "No MCP servers configured.".to_string();
        }

        let lines: Vec<String> = self
            .servers
            .values()
            .map(|s| {
                let status = match &s.status {
                    ServerStatus::Stopped => "stopped",
                    ServerStatus::Starting => "starting",
                    ServerStatus::Connected => &format!("connected ({} tools)", s.tools.len()),
                    ServerStatus::Failed(e) => &format!("failed: {}", e),
                };
                format!("- {} [{}]", s.name, status)
            })
            .collect();

        format!(
            "MCP servers ({}):\n{}",
            self.servers.len(),
            lines.join("\n")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config() -> HashMap<String, McpServerConfig> {
        let mut configs = HashMap::new();
        configs.insert(
            "test-server".into(),
            McpServerConfig {
                command: Some(vec!["npx".into(), "-y".into(), "test-mcp".into()]),
                url: None,
            },
        );
        configs.insert(
            "http-server".into(),
            McpServerConfig {
                command: None,
                url: Some("http://localhost:3000".into()),
            },
        );
        configs
    }

    #[test]
    fn test_from_config() {
        let manager = McpManager::from_config(&make_config());
        assert_eq!(manager.server_names().len(), 2);
    }

    #[test]
    fn test_server_status_initial() {
        let manager = McpManager::from_config(&make_config());
        let server = manager.get("test-server").unwrap();
        assert_eq!(server.status, ServerStatus::Stopped);
    }

    #[test]
    fn test_summary() {
        let manager = McpManager::from_config(&make_config());
        let summary = manager.summary();
        assert!(summary.contains("test-server"));
        assert!(summary.contains("http-server"));
    }

    #[tokio::test]
    async fn test_start_stop() {
        let mut manager = McpManager::from_config(&make_config());
        manager.start("test-server").await.unwrap();
        let server = manager.get("test-server").unwrap();
        assert_eq!(server.status, ServerStatus::Connected);

        manager.stop("test-server").await.unwrap();
        let server = manager.get("test-server").unwrap();
        assert_eq!(server.status, ServerStatus::Stopped);
    }
}
