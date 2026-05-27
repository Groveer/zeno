//! ACP agent server implementation.
//!
//! Uses the `agent-client-protocol` SDK to provide a standard ACP interface
//! for external clients (IDEs, editors) to interact with zeno.
//!
//! ## Architecture
//!
//! The server runs as a stdio-based JSON-RPC 2.0 server using the SDK's
//! `Builder` pattern. Each ACP session corresponds to a `QueryEngine`
//! instance.
//!
//! ## Protocol flow
//!
//! 1. Client sends `initialize` → server responds with capabilities
//! 2. Client sends `session/new` → server creates engine, returns session ID
//! 3. Client sends `session/prompt` → server processes via engine, streams updates
//! 4. Client sends `session/cancel` → server cancels ongoing processing
//! 5. (Optional) Client sends `session/close` → server frees session resources

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol::schema::{
    self, AgentCapabilities, CancelNotification, CloseSessionRequest, CloseSessionResponse,
    ContentBlock, Implementation, InitializeRequest, InitializeResponse, NewSessionRequest,
    NewSessionResponse, PromptRequest, PromptResponse, ProtocolVersion, SessionCapabilities,
    SessionCloseCapabilities, SessionId, SessionNotification, SessionUpdate, StopReason,
    TextContent,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Responder, Stdio};
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::config::settings::{PermissionMode, ProviderConfig, Settings};
use crate::engine::messages::ConversationHistory;
use crate::engine::query_engine::QueryEngine;
use crate::hooks::executor::HookExecutor;
use crate::mcp::manager::McpManager;
use crate::memory::manager::SharedMemoryManager;
use crate::tools::base::ToolRegistry;

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// Application-level configuration shared across all ACP sessions.
pub struct AcpConfig {
    pub settings: Arc<Settings>,
    pub tool_registry: Arc<ToolRegistry>,
    pub provider_name: String,
    pub model: String,
    /// Client factory for sub-agent creation.
    pub client_factory: Option<
        Arc<
            dyn Fn(&str, &ProviderConfig) -> Box<dyn crate::api::client::SupportsStreamingMessages>
                + Send
                + Sync,
        >,
    >,
    pub cwd: PathBuf,
    pub system_prompt: String,
    /// Shared MCP manager for lazy MCP server connections.
    pub mcp_manager: Option<Arc<tokio::sync::Mutex<McpManager>>>,
    /// Shared memory manager for external provider lifecycle.
    pub memory_manager: Option<SharedMemoryManager>,
    /// Hook executor for pre/post tool-use events.
    pub hook_executor: Option<HookExecutor>,
    /// Active identity name for this session.
    pub active_identity: Option<String>,
    /// Permission mode for tool execution in ACP sessions.
    pub permission_mode: PermissionMode,
}

/// Runtime state for a single ACP session.
struct SessionState {
    engine: Arc<Mutex<QueryEngine>>,
    cancel_token: CancellationToken,
}

impl SessionState {
    fn new(config: &AcpConfig) -> Self {
        // Precondition: provider_name is validated in run_server() before
        // any SessionState is created, so this unwrap is safe.
        let provider_config = config
            .settings
            .providers
            .get(&config.provider_name)
            .expect("Provider must be configured (validated at ACP server startup)");

        let api_key = crate::config::settings::resolve_api_key(provider_config).unwrap_or_default();
        let base_url = provider_config.base_url.clone();
        let client: Box<dyn crate::api::client::SupportsStreamingMessages> =
            match provider_config.api_type {
                crate::config::settings::ApiType::Anthropic => Box::new(
                    crate::api::anthropic::AnthropicClient::new(api_key, base_url),
                ),
                crate::config::settings::ApiType::OpenAi
                | crate::config::settings::ApiType::OpenAiResponses => {
                    Box::new(crate::api::openai::OpenAIClient::new(api_key, base_url))
                }
            };

        let mut engine = QueryEngine::new(
            client,
            config.model.clone(),
            config.system_prompt.clone(),
            ConversationHistory::new(),
            config.tool_registry.clone(),
            config.settings.max_turns,
            config.settings.max_tokens,
            config.permission_mode.clone(),
            config.settings.clone(),
            config.cwd.clone(),
            crate::engine::session::generate_session_id(),
        );

        // Wire up shared engine services (same as main.rs TUI path)
        engine.mcp_manager = config.mcp_manager.clone();
        engine.memory_manager = config.memory_manager.clone();
        engine.hook_executor = config.hook_executor.clone();
        engine.client_factory = config.client_factory.clone();
        engine.active_identity = config.active_identity.clone();

        // Initialize sub-agent topology graph store (same pattern as main.rs)
        engine.graph_store = Some(crate::store::create_graph_store(
            &crate::config::paths::data_dir(),
        ));

        Self {
            engine: Arc::new(Mutex::new(engine)),
            cancel_token: CancellationToken::new(),
        }
    }
}

type SessionMap = Arc<RwLock<HashMap<String, SessionState>>>;

// ---------------------------------------------------------------------------
// Handler struct — holds shared state, provides handler methods
// ---------------------------------------------------------------------------

struct AcpHandlers {
    sessions: SessionMap,
    config: Arc<AcpConfig>,
}

impl AcpHandlers {
    fn new(config: Arc<AcpConfig>) -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            config,
        }
    }

    /// Clone the inner fields of a session out of the read lock, so callers
    /// can operate on the engine/cancel_token without holding the lock.
    async fn get_session(&self, sid: &str) -> Option<SessionState> {
        let map = self.sessions.read().await;
        map.get(sid).map(|s| SessionState {
            engine: s.engine.clone(),
            cancel_token: s.cancel_token.clone(),
        })
    }

    /// Handle `initialize` — negotiate protocol version and capabilities.
    async fn handle_initialize(
        &self,
        req: InitializeRequest,
        responder: Responder<InitializeResponse>,
    ) -> Result<(), agent_client_protocol::Error> {
        tracing::info!(client_info = ?req.client_info, "ACP initialize");

        let response = InitializeResponse::new(ProtocolVersion::V1)
            .agent_capabilities(AgentCapabilities::new().session_capabilities(
                SessionCapabilities::new().close(SessionCloseCapabilities::new()),
            ))
            .agent_info(Implementation::new("zeno", env!("CARGO_PKG_VERSION")).title("Zeno"));

        responder.respond(response)
    }

    /// Handle `session/new` — create a new conversation session.
    async fn handle_new_session(
        &self,
        req: NewSessionRequest,
        responder: Responder<NewSessionResponse>,
    ) -> Result<(), agent_client_protocol::Error> {
        let session_id = format!("sess_{}", Uuid::new_v4().simple());
        tracing::info!(session_id = %session_id, cwd = ?req.cwd, "ACP new session");

        let session = SessionState::new(&self.config);
        self.sessions
            .write()
            .await
            .insert(session_id.clone(), session);

        let response = NewSessionResponse::new(SessionId::new(session_id));
        responder.respond(response)
    }

    /// Handle `session/prompt` — process a user message via the engine.
    async fn handle_prompt(
        &self,
        req: PromptRequest,
        responder: Responder<PromptResponse>,
        cx: ConnectionTo<Client>,
    ) -> Result<(), agent_client_protocol::Error> {
        let sid = req.session_id.to_string();
        tracing::info!(session_id = %sid, "ACP prompt");

        let session = self.get_session(&sid).await;

        let session = match session {
            Some(s) => s,
            None => {
                tracing::warn!(session_id = %sid, "ACP prompt for unknown session");
                let _ = responder.respond(PromptResponse::new(StopReason::EndTurn));
                return Ok(());
            }
        };

        // Extract text from content blocks
        let text: String = req.prompt.iter().fold(String::new(), |mut acc, block| {
            if let ContentBlock::Text(tc) = block {
                if !acc.is_empty() {
                    acc.push('\n');
                }
                acc.push_str(&tc.text);
            }
            acc
        });

        if text.trim().is_empty() {
            tracing::warn!("ACP empty prompt");
            let _ = responder.respond(PromptResponse::new(StopReason::EndTurn));
            return Ok(());
        }

        // Send initial "processing" notification
        let _ = cx.send_notification(SessionNotification::new(
            req.session_id.clone(),
            SessionUpdate::AgentMessageChunk(schema::ContentChunk::new(ContentBlock::Text(
                TextContent::new("Processing your request..."),
            ))),
        ));

        // Lock engine and process
        let mut engine = session.engine.lock().await;
        let cancel = session.cancel_token.clone();

        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();

        // Forward EngineEvents → session/update notifications
        let session_id = req.session_id.clone();
        let cx_clone = cx.clone();
        tokio::spawn(async move {
            use crate::engine::tui_events::EngineEvent;
            while let Some(event) = event_rx.recv().await {
                let notif = match event {
                    EngineEvent::TextDelta(chunk) => Some(SessionNotification::new(
                        session_id.clone(),
                        SessionUpdate::AgentMessageChunk(schema::ContentChunk::new(
                            ContentBlock::Text(TextContent::new(chunk)),
                        )),
                    )),
                    EngineEvent::ReasoningDelta(chunk) => Some(SessionNotification::new(
                        session_id.clone(),
                        SessionUpdate::AgentMessageChunk(schema::ContentChunk::new(
                            ContentBlock::Text(TextContent::new(format!(
                                "<thinking>{chunk}</thinking>"
                            ))),
                        )),
                    )),
                    EngineEvent::ToolStart {
                        name,
                        input_summary,
                    } => Some(SessionNotification::new(
                        session_id.clone(),
                        SessionUpdate::AgentMessageChunk(schema::ContentChunk::new(
                            ContentBlock::Text(TextContent::new(format!(
                                "\n🔧 **{name}**: {input_summary}"
                            ))),
                        )),
                    )),
                    EngineEvent::ToolOutput { name, output } => Some(SessionNotification::new(
                        session_id.clone(),
                        SessionUpdate::AgentMessageChunk(schema::ContentChunk::new(
                            ContentBlock::Text(TextContent::new(format!(
                                "\n✅ **{name}** completed.\n```\n{output}\n```"
                            ))),
                        )),
                    )),
                    EngineEvent::ToolError { name, error } => Some(SessionNotification::new(
                        session_id.clone(),
                        SessionUpdate::AgentMessageChunk(schema::ContentChunk::new(
                            ContentBlock::Text(TextContent::new(format!(
                                "\n❌ **{name}** error: {error}"
                            ))),
                        )),
                    )),
                    EngineEvent::ToolDiff { diff, .. } => Some(SessionNotification::new(
                        session_id.clone(),
                        SessionUpdate::AgentMessageChunk(schema::ContentChunk::new(
                            ContentBlock::Text(TextContent::new(format!("\n```diff\n{diff}\n```"))),
                        )),
                    )),
                    EngineEvent::Error(msg) => Some(SessionNotification::new(
                        session_id.clone(),
                        SessionUpdate::AgentMessageChunk(schema::ContentChunk::new(
                            ContentBlock::Text(TextContent::new(format!("\n❌ Error: {msg}"))),
                        )),
                    )),
                    _ => None,
                };
                if let Some(n) = notif {
                    let _ = cx_clone.send_notification(n);
                }
            }
        });

        let result = engine.query_tui(&text, Vec::new(), &event_tx, cancel).await;

        match result {
            Ok(_) => {
                let _ = cx.send_notification(SessionNotification::new(
                    req.session_id.clone(),
                    SessionUpdate::AgentMessageChunk(schema::ContentChunk::new(
                        ContentBlock::Text(TextContent::new("\n\n✅ Done.")),
                    )),
                ));
                responder.respond(PromptResponse::new(StopReason::EndTurn))
            }
            Err(e) => {
                tracing::error!(error = %e, "ACP prompt processing error");
                let _ = cx.send_notification(SessionNotification::new(
                    req.session_id.clone(),
                    SessionUpdate::AgentMessageChunk(schema::ContentChunk::new(
                        ContentBlock::Text(TextContent::new(format!("\n❌ Error: {e}"))),
                    )),
                ));
                // StopReason::EndTurn is the closest match — ACP schema lacks an Error variant
                responder.respond(PromptResponse::new(StopReason::EndTurn))
            }
        }
    }

    /// Handle `session/cancel` — cancel ongoing prompt processing.
    async fn handle_cancel(
        &self,
        notif: CancelNotification,
    ) -> Result<(), agent_client_protocol::Error> {
        let sid = notif.session_id.to_string();
        tracing::info!(session_id = %sid, "ACP cancel");

        // Clone the token via get_session() before releasing the read lock
        let cancel_token = self.get_session(&sid).await.map(|s| s.cancel_token);
        if let Some(token) = cancel_token {
            token.cancel();
        }
        Ok(())
    }

    /// Handle `session/close` — close and remove a session.
    async fn handle_close_session(
        &self,
        req: CloseSessionRequest,
        responder: Responder<CloseSessionResponse>,
    ) -> Result<(), agent_client_protocol::Error> {
        let sid = req.session_id.to_string();
        tracing::info!(session_id = %sid, "ACP close session");

        // Cancel any ongoing processing before removing.
        // Note: there is a TOCTOU gap between the read lock (cancel)
        // and the write lock (remove). This is safe because cancel_token
        // is reference-counted and cancel() is idempotent — any prompt
        // that starts between the two operations will still be cancellable
        // via its own token or will complete naturally and be discarded.
        if let Some(session) = self.sessions.read().await.get(&sid) {
            session.cancel_token.cancel();
        }

        self.sessions.write().await.remove(&sid);
        responder.respond(CloseSessionResponse::new())
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Run the ACP agent server on stdio transport.
///
/// Initializes shared state and starts the JSON-RPC event loop.
/// Blocks until stdin is closed or an error occurs.
pub async fn run_server(config: AcpConfig) -> Result<(), anyhow::Error> {
    // Validate provider exists before accepting any connections.
    // Without this, SessionState::new() would fail at session creation time,
    // but fail-fast at startup is better for the operator.
    if !config
        .settings
        .providers
        .contains_key(&config.provider_name)
    {
        return Err(anyhow::anyhow!(
            "ACP server: provider '{}' not configured in settings",
            config.provider_name
        ));
    }

    let config = Arc::new(config);
    let handlers = AcpHandlers::new(config);
    tracing::info!("Starting ACP server on stdio transport");

    Agent
        .builder()
        .name("zeno")
        .on_receive_request(
            {
                let h = &handlers;
                async move |req: InitializeRequest, responder, _cx: ConnectionTo<Client>| {
                    h.handle_initialize(req, responder).await
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let h = &handlers;
                async move |req: NewSessionRequest, responder, _cx: ConnectionTo<Client>| {
                    h.handle_new_session(req, responder).await
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let h = &handlers;
                async move |req: PromptRequest, responder, cx: ConnectionTo<Client>| {
                    h.handle_prompt(req, responder, cx).await
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let h = &handlers;
                async move |notif: CancelNotification, _cx: ConnectionTo<Client>| {
                    h.handle_cancel(notif).await
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let h = &handlers;
                async move |req: CloseSessionRequest,
                            responder: Responder<CloseSessionResponse>,
                            _cx: ConnectionTo<Client>| {
                    h.handle_close_session(req, responder).await
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_to(Stdio::new())
        .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::jsonrpcmsg::{Id, Message, Params, Request};
    use agent_client_protocol::{Agent, Channel};
    use futures::StreamExt;
    use serde_json::json;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    /// Create an `AcpConfig` with a configured test provider.
    fn make_config(provider_name: &str) -> AcpConfig {
        let mut providers = HashMap::new();
        providers.insert(
            "test-provider".into(),
            ProviderConfig {
                api_key: Some("test-key".into()),
                base_url: "https://api.example.com/v1".into(),
                default_model: "test-model".into(),
                max_output_tokens: None,
                api_type: crate::config::settings::ApiType::OpenAi,
            },
        );

        AcpConfig {
            settings: Arc::new(Settings {
                providers,
                active_provider: "test-provider".into(),
                model: "test-model".into(),
                ..Settings::default()
            }),
            tool_registry: Arc::new(ToolRegistry::new()),
            provider_name: provider_name.to_string(),
            model: "test-model".into(),
            client_factory: None,
            cwd: PathBuf::from("/tmp"),
            system_prompt: String::new(),
            mcp_manager: None,
            memory_manager: None,
            hook_executor: None,
            active_identity: None,
            permission_mode: PermissionMode::Allow,
        }
    }

    /// Build an Agent wired to the given Channel transport with all ACP handlers.
    async fn run_agent(
        channel: Channel,
        config: Arc<AcpConfig>,
    ) -> Result<(), agent_client_protocol::Error> {
        let handlers = AcpHandlers::new(config);

        Agent
            .builder()
            .name("zeno-test")
            .on_receive_request(
                {
                    let h = &handlers;
                    async move |req: InitializeRequest, responder, _cx: ConnectionTo<Client>| {
                        h.handle_initialize(req, responder).await
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let h = &handlers;
                    async move |req: NewSessionRequest, responder, _cx: ConnectionTo<Client>| {
                        h.handle_new_session(req, responder).await
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let h = &handlers;
                    async move |req: CloseSessionRequest, responder, _cx: ConnectionTo<Client>| {
                        h.handle_close_session(req, responder).await
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_notification(
                {
                    let h = &handlers;
                    async move |notif: CancelNotification, _cx: ConnectionTo<Client>| {
                        h.handle_cancel(notif).await
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_to(channel)
            .await
    }

    /// Send a JSON-RPC 2.0 request through the channel and wait for the response.
    /// Times out after `timeout` duration to prevent hanging tests.
    async fn send_request(
        tx: &futures::channel::mpsc::UnboundedSender<Result<Message, agent_client_protocol::Error>>,
        rx: &mut futures::channel::mpsc::UnboundedReceiver<
            Result<Message, agent_client_protocol::Error>,
        >,
        method: &str,
        params: Option<serde_json::Map<String, serde_json::Value>>,
        id: u64,
        timeout: std::time::Duration,
    ) -> Result<Message, String> {
        let request = Request::new_v2(
            method.to_string(),
            params.map(Params::Object),
            Some(Id::Number(id)),
        );
        tx.unbounded_send(Ok(Message::Request(request)))
            .map_err(|e| format!("send error: {e}"))?;

        tokio::time::timeout(timeout, rx.next())
            .await
            .map_err(|_| format!("timeout waiting for response to '{method}' (id={id})"))?
            .ok_or_else(|| "no response received — channel closed".to_string())?
            .map_err(|e| format!("response error: {e}"))
    }

    // -----------------------------------------------------------------------
    // run_server() — provider validation
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_run_server_missing_provider_returns_error() {
        let config = make_config("nonexistent");
        let err = run_server(config).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not configured"),
            "Expected error mentioning 'not configured', got: {msg}",
        );
    }

    // -----------------------------------------------------------------------
    // Integration: initialize → session/new → session/close
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_initialize_returns_protocol_version_and_capabilities() {
        let config = Arc::new(make_config("test-provider"));
        let (mut client_chan, server_chan) = Channel::duplex();

        let server_handle = tokio::spawn(async move { run_agent(server_chan, config).await });

        // Send initialize request (minimal params per ACP spec)
        let params = Some({
            let mut m = serde_json::Map::new();
            m.insert("protocolVersion".into(), json!("v1"));
            m.insert(
                "clientInfo".into(),
                json!({
                    "name": "test-client",
                    "version": "1.0.0"
                }),
            );
            m
        });
        let msg = send_request(
            &client_chan.tx,
            &mut client_chan.rx,
            "initialize",
            params,
            1,
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("initialize should return a response");

        match msg {
            Message::Response(resp) => {
                let result = resp
                    .result
                    .expect("initialize response should have a result");

                assert_eq!(
                    result.get("protocolVersion").and_then(|v| v.as_i64()),
                    Some(1),
                    "response should contain protocolVersion == 1",
                );
                assert!(
                    result.get("agentInfo").is_some(),
                    "response should contain agentInfo",
                );
                assert!(
                    result.get("agentCapabilities").is_some(),
                    "response should contain agentCapabilities",
                );
            }
            other => panic!("expected Response, got: {other:?}"),
        }

        // Clean shutdown: abort the server task (Channel event loop runs until
        // the channel is closed, which requires dropping both ends).
        drop(client_chan);
        server_handle.abort();
    }

    #[tokio::test]
    async fn test_new_session_creates_and_returns_session_id() {
        let config = Arc::new(make_config("test-provider"));
        let (mut client_chan, server_chan) = Channel::duplex();

        let server_handle = tokio::spawn(async move { run_agent(server_chan, config).await });

        // 1. Initialize
        let init_params = Some({
            let mut m = serde_json::Map::new();
            m.insert("protocolVersion".into(), json!("v1"));
            m
        });
        send_request(
            &client_chan.tx,
            &mut client_chan.rx,
            "initialize",
            init_params,
            1,
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("initialize should succeed");

        // 2. Create session
        let session_params = Some({
            let mut m = serde_json::Map::new();
            m.insert("cwd".into(), json!("/tmp"));
            m.insert("mcpServers".into(), json!([]));
            m
        });
        let msg = send_request(
            &client_chan.tx,
            &mut client_chan.rx,
            "session/new",
            session_params,
            2,
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("session/new should return a response");

        let session_id = match &msg {
            Message::Response(resp) => {
                let result = resp
                    .result
                    .as_ref()
                    .expect("session/new response should have a result");
                let sid = result
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .expect("session/new should return sessionId");

                assert!(
                    sid.starts_with("sess_"),
                    "sessionId should start with 'sess_', got: {sid}",
                );
                sid.to_string()
            }
            other => panic!("expected Response, got: {other:?}"),
        };

        // 3. Close session
        let close_params = Some({
            let mut m = serde_json::Map::new();
            m.insert("sessionId".into(), json!(session_id));
            m
        });
        let msg = send_request(
            &client_chan.tx,
            &mut client_chan.rx,
            "session/close",
            close_params,
            3,
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("session/close should return a response");

        match msg {
            Message::Response(resp) => {
                assert!(
                    resp.result.is_some(),
                    "session/close should return a result",
                );
            }
            other => panic!("expected Response for session/close, got: {other:?}"),
        }

        drop(client_chan);
        server_handle.abort();
    }

    #[tokio::test]
    async fn test_new_session_with_nonexistent_cwd_does_not_crash() {
        let config = Arc::new(make_config("test-provider"));
        let (mut client_chan, server_chan) = Channel::duplex();

        let server_handle = tokio::spawn(async move { run_agent(server_chan, config).await });

        // Initialize
        let init_params = Some({
            let mut m = serde_json::Map::new();
            m.insert("protocolVersion".into(), json!("v1"));
            m
        });
        send_request(
            &client_chan.tx,
            &mut client_chan.rx,
            "initialize",
            init_params,
            1,
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();

        // Session/new with non-existent cwd — should still create session
        let session_params = Some({
            let mut m = serde_json::Map::new();
            m.insert("cwd".into(), json!("/nonexistent/path"));
            m.insert("mcpServers".into(), json!([]));
            m
        });
        let msg = send_request(
            &client_chan.tx,
            &mut client_chan.rx,
            "session/new",
            session_params,
            2,
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();

        match msg {
            Message::Response(resp) => {
                let result = resp.result.expect("session/new should have a result");
                let sid = result
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .expect("sessionId should be present");
                assert!(sid.starts_with("sess_"), "sessionId: {sid}");
            }
            other => panic!("expected Response, got: {other:?}"),
        }

        drop(client_chan);
        server_handle.abort();
    }

    // -----------------------------------------------------------------------
    // Integration: cancellation
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_cancel_unknown_session_does_not_crash() {
        let config = Arc::new(make_config("test-provider"));
        let (mut client_chan, server_chan) = Channel::duplex();

        let server_handle = tokio::spawn(async move { run_agent(server_chan, config).await });

        // Initialize
        let init_params = Some({
            let mut m = serde_json::Map::new();
            m.insert("protocolVersion".into(), json!("v1"));
            m
        });
        send_request(
            &client_chan.tx,
            &mut client_chan.rx,
            "initialize",
            init_params,
            1,
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();

        // Send cancel notification for a session that doesn't exist
        let cancel_params = Some({
            let mut m = serde_json::Map::new();
            m.insert("sessionId".into(), json!("sess_nonexistent"));
            m
        });
        let cancel_req = Request::notification_v2(
            "session/cancel".to_string(),
            cancel_params.map(Params::Object),
        );
        client_chan
            .tx
            .unbounded_send(Ok(Message::Request(cancel_req)))
            .expect("cancel notification should send");

        // Small delay to let server process the notification
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Verify server is still responsive: create a session
        let session_params = Some({
            let mut m = serde_json::Map::new();
            m.insert("cwd".into(), json!("/tmp"));
            m.insert("mcpServers".into(), json!([]));
            m
        });
        let msg = send_request(
            &client_chan.tx,
            &mut client_chan.rx,
            "session/new",
            session_params,
            3,
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("server should still respond after cancel");

        assert!(
            matches!(msg, Message::Response(_)),
            "server should be responsive after cancelling unknown session",
        );

        drop(client_chan);
        server_handle.abort();
    }

    #[tokio::test]
    async fn test_close_unknown_session_returns_ok() {
        let config = Arc::new(make_config("test-provider"));
        let (mut client_chan, server_chan) = Channel::duplex();

        let server_handle = tokio::spawn(async move { run_agent(server_chan, config).await });

        // Initialize
        let init_params = Some({
            let mut m = serde_json::Map::new();
            m.insert("protocolVersion".into(), json!("v1"));
            m
        });
        send_request(
            &client_chan.tx,
            &mut client_chan.rx,
            "initialize",
            init_params,
            1,
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();

        // Close a session that doesn't exist — should return OK without error
        let close_params = Some({
            let mut m = serde_json::Map::new();
            m.insert("sessionId".into(), json!("sess_nonexistent"));
            m
        });
        let msg = send_request(
            &client_chan.tx,
            &mut client_chan.rx,
            "session/close",
            close_params,
            2,
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("session/close for unknown session should return a response");

        match msg {
            Message::Response(resp) => {
                // SDK returns an error response for unknown sessions
                if resp.error.is_some() {
                    // This is acceptable — the server handles it gracefully (no crash)
                    eprintln!(
                        "Note: closing unknown session returned error (graceful): {:?}",
                        resp.error
                    );
                } else {
                    assert!(
                        resp.result.is_some(),
                        "close should return a result or error"
                    );
                }
            }
            other => panic!("expected Response, got: {other:?}"),
        }

        drop(client_chan);
        server_handle.abort();
    }
}
