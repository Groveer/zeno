mod api;
mod auxiliary;
mod config;
mod engine;
mod gateway;
mod hooks;
mod mcp;
mod memory;
mod permissions;
mod plugin;
mod prompts;
mod sandbox;
mod skills;
mod tools;
mod ui;
mod utils;

use api::types::ContentBlock;
use base64::Engine;
use config::load as config_load;
use config::settings;
use config::settings::ProviderConfig;
use engine::messages::ConversationHistory;
use engine::query_engine::QueryEngine;
use gateway::transport::ChannelTransport;
use memory::provider::MemoryProvider;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::sync::Arc;
use std::time::SystemTime;
use tools::base::ToolRegistry;
use ui::status_bar::AppMode;

use ui::component::Component;

/// Regex for matching image file paths in query text.
/// Matches paths ending in common image extensions (png, jpg, jpeg, gif, webp, bmp).
static IMAGE_PATH_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:^|\s)([\w.\-/\\]+\.(?:png|jpe?g|gif|webp|bmp))(?:\s|$)").unwrap()
});

/// Auto-detect image file paths in query text, read the files, and
/// convert them to base64 image blocks. Returns (cleaned_text, image_blocks).
fn extract_image_paths(query_text: &str) -> (String, Vec<(String, String)>) {
    let re = &*IMAGE_PATH_REGEX;
    let mut text = query_text.to_string();
    let mut image_blocks = Vec::new();
    let mut found_any = false;

    for cap in re.captures_iter(query_text) {
        let path_str = cap.get(1).unwrap().as_str();
        let path = std::path::Path::new(path_str);
        if let Ok(bytes) = std::fs::read(path) {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("png");
            let media_type = match ext.to_lowercase().as_str() {
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "gif" => "image/gif",
                "webp" => "image/webp",
                "bmp" => "image/bmp",
                _ => "image/png",
            };
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            let size_kb = bytes.len() / 1024;
            if size_kb <= 10240 {
                image_blocks.push((media_type.to_string(), b64));
                found_any = true;
                text = text.replace(path_str, "");
            }
        }
    }

    if found_any {
        let cleaned = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if cleaned.is_empty() {
            ("[Attached image(s)]".to_string(), image_blocks)
        } else {
            (cleaned, image_blocks)
        }
    } else {
        (text, image_blocks)
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize structured JSON logging to file (avoids corrupting the TUI).
    // JSON format enables machine-readable log analysis — filter by field
    // values like `tool_name`, `permission_decision`, `compact_method`, etc.
    config::paths::ensure_log_dir()?;
    let log_dir = config::paths::log_dir();
    let file_appender = tracing_appender::rolling::daily(&log_dir, "zeno.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::fmt()
        .json() // structured JSON output
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_current_span(false)
        .with_span_list(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "zeno=warn".into()),
        )
        .init();

    let (settings, hook_executor, lua_vm) = config_load()?;
    config::paths::cleanup_old_logs(settings.log_retention_days);
    let settings = Arc::new(settings);

    let provider_name = settings.active_provider.clone();
    let permission_mode = settings.permissions.clone();

    let provider_config = settings.providers.get(&provider_name).ok_or_else(|| {
        anyhow::anyhow!(
            "Provider '{}' not configured. Add it to ~/.config/zeno/init.lua\n\
             \n\
             local zn = require 'zeno'\n\
             zn.provider(\"anthropic\", {{ api_key = \"ANTHROPIC_API_KEY\", base_url = \"https://api.anthropic.com\", default_model = \"claude-sonnet-4-20250514\", api_type = \"anthropic\" }})\n\
             zn.provider(\"openai\", {{ api_key = \"OPENAI_API_KEY\", base_url = \"https://api.openai.com/v1\", default_model = \"gpt-4o\" }})\n\
             zn.set_provider(\"anthropic\")",
            provider_name
        )
    })?;

    // Use explicitly configured model, or fall back to provider's default_model
    let model = if !settings.model.is_empty() {
        settings.model.clone()
    } else {
        provider_config.default_model.clone()
    };

    // Build API client
    let api_key = settings::resolve_api_key(provider_config)?;
    let base_url = provider_config.base_url.clone();
    let client: Box<dyn api::client::SupportsStreamingMessages> = match provider_config.api_type {
        settings::ApiType::Anthropic => {
            Box::new(api::anthropic::AnthropicClient::new(api_key, base_url))
        }
        settings::ApiType::OpenAi | settings::ApiType::OpenAiResponses => {
            Box::new(api::openai::OpenAIClient::new(api_key, base_url))
        }
    };

    // Build tool registry
    let mut registry = ToolRegistry::new();
    let tc = &settings.tools;
    if tc.bash {
        let bash_sandbox = crate::sandbox::create_sandbox(&settings.sandbox);
        registry.register(Box::new(tools::bash::BashTool::new(
            tc.use_rtk,
            tc.bash_env.clone(),
            tc.allowed_commands.clone(),
            tc.ask_commands.clone(),
            tc.denied_commands.clone(),
            bash_sandbox,
        )))?;
    }
    if tc.read {
        registry.register(Box::new(tools::read::ReadTool::new()))?;
    }
    if tc.write {
        registry.register(Box::new(tools::write::WriteTool::new()))?;
    }
    if tc.edit {
        registry.register(Box::new(tools::edit::EditTool::new()))?;
    }
    if tc.glob {
        registry.register(Box::new(tools::glob::GlobTool::new(tc.skip_dirs.clone())))?;
    }
    if tc.grep {
        registry.register(Box::new(tools::grep::GrepTool::new(tc.skip_dirs.clone())))?;
    }
    if tc.web_search {
        registry.register(Box::new(tools::web_search::WebSearchTool::with_config(
            settings.web_search_config.clone(),
        )))?;
    }
    if tc.web_fetch {
        registry.register(Box::new(tools::web_fetch::WebFetchTool::new(
            settings.clone(),
        )))?;
    }

    registry.register(Box::new(tools::ask_user::AskUserTool::new()))?;

    // Register todo tool (in-memory task list) — always available
    let todo_state =
        std::sync::Arc::new(tokio::sync::Mutex::new(tools::todo::TodoState::default()));
    registry.register(Box::new(tools::todo::TodoTool::from_state(
        todo_state.clone(),
    )))?;

    // Register delegate_task tool (sub-agent support) — always available
    registry.register(Box::new(tools::delegate_task::DelegateTaskTool::new()))?;

    // Create client factory for sub-agents
    let client_factory: Arc<
        dyn Fn(&str, &ProviderConfig) -> Box<dyn api::client::SupportsStreamingMessages>
            + Send
            + Sync,
    > = Arc::new({
        move |_name: &str, config: &ProviderConfig| {
            let api_key = settings::resolve_api_key(config).unwrap_or_default();
            let base_url = config.base_url.clone();
            match config.api_type {
                settings::ApiType::Anthropic => {
                    Box::new(api::anthropic::AnthropicClient::new(api_key, base_url))
                        as Box<dyn api::client::SupportsStreamingMessages>
                }
                settings::ApiType::OpenAi | settings::ApiType::OpenAiResponses => {
                    Box::new(api::openai::OpenAIClient::new(api_key, base_url))
                        as Box<dyn api::client::SupportsStreamingMessages>
                }
            }
        }
    });

    // Resolve working directory early (needed for memory dir + skills + system prompt)
    let cwd = std::env::current_dir().unwrap_or_default();

    // Initialize memory store — identity-scoped paths when active
    let active_identity_name = settings.active_identity.as_deref();
    let memory_dir = config::paths::memory_dir_for_identity(active_identity_name);
    let user_profile_path = memory_dir.join("USER.md");
    let mut memory_store = memory::store::MemoryStore::new(
        memory_dir.join("MEMORY.md"),
        user_profile_path,
        settings.memory.memory_char_limit,
        settings.memory.user_char_limit,
    );
    memory_store.load_from_disk();
    let (mem_count, usr_count) = memory_store.counts();
    tracing::info!(
        memory_entries = mem_count,
        user_entries = usr_count,
        memory_dir = %memory_store.dir().display(),
        "Memory loaded from disk"
    );
    let memory_store = Arc::new(tokio::sync::Mutex::new(memory_store));

    // Initialize memory manager (orchestrates built-in + external providers).
    // Created before MemoryTool so it can receive `on_memory_change` notifications.
    let mut memory_manager = memory::manager::MemoryManager::new(memory_store.clone());

    // Load and activate the configured external memory provider (if any)
    if !settings.memory.provider.is_empty() {
        let lua_config = memory::lua_provider::LuaProviderConfig {
            name: settings.memory.provider.clone(),
        };
        match memory::lua_provider::LuaMemoryProvider::new(lua_config, lua_vm.clone()) {
            Ok(provider) => {
                let prov_name = provider.name().to_string();
                let prov_available = provider.is_available();
                if prov_available {
                    memory_manager.set_external(Box::new(provider)).await;
                    tracing::info!(provider = %prov_name, event = "external_memory_activated", "External memory provider activated");
                } else {
                    tracing::warn!(
                        provider = %prov_name,
                        "External memory provider is not available (missing credentials or deps), skipping"
                    );
                }
            }
            Err(e) => {
                tracing::error!(
                    provider = %settings.memory.provider,
                    error = %e,
                    "Failed to load external memory provider"
                );
            }
        }
    }

    // Initialize the memory manager for this session
    let session_id = format!("session-{}", std::process::id());
    memory_manager.initialize(&session_id).await;

    // Wrap memory manager for shared access
    let memory_manager: memory::manager::SharedMemoryManager = Arc::new(Mutex::new(memory_manager));

    registry.register(Box::new(tools::memory::MemoryTool::new(
        memory_store.clone(),
        memory_manager.clone(),
    )))?;

    // Register external provider's tools (if any)
    let external_schemas = memory_manager.lock().await.get_external_tool_schemas();
    for schema in &external_schemas {
        if let Some(tool_name) = schema
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
        {
            let tool = tools::memory_provider_tool::MemoryProviderTool::new(
                tool_name.to_string(),
                schema.clone(),
                memory_manager.clone(),
            );
            registry.register(Box::new(tool))?;
        }
    }

    let builtin_tool_names: Vec<String> = registry.names().into_iter().map(String::from).collect();
    tracing::info!(tools = ?builtin_tool_names, "Registered tools");

    // Release built-in skills to user config dir if needed.
    // Uses spawn_blocking since it involves synchronous filesystem I/O
    // (directory traversal + file comparisons).
    match tokio::task::spawn_blocking(skills::builtin::release_if_needed).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "Failed to release built-in skills"),
        Err(e) => tracing::warn!(error = %e, "Built-in skills release task panicked"),
    }

    // Load skills (user + project directories) — with disk cache acceleration
    let skill_dirs = {
        let mut dirs = vec![skills::loader::get_user_skills_dir()];
        dirs.extend(skills::loader::get_project_skills_dirs(&cwd));
        dirs
    };
    let (loaded_skills, loaded_categories) =
        if let Some((skills, categories)) = skills::index_cache::load_cache(&skill_dirs) {
            (skills, categories)
        } else {
            let result = skills::loader::load_skills_from_dirs(&skill_dirs, "user");
            if let Err(e) = skills::index_cache::write_cache(&skill_dirs, &result.0, &result.1) {
                tracing::debug!(error = %e, "Failed to write skills cache");
            }
            result
        };
    for skill in &loaded_skills {
        tracing::info!(skill_name = %skill.name, source = %skill.source, "Loaded skill");
    }
    let skill_registry =
        skills::registry::SkillRegistry::from_parts(loaded_skills, loaded_categories);

    // Wrap in Arc<Mutex> so skill tools (including skill_manage) share a live registry.
    let skill_registry = std::sync::Arc::new(tokio::sync::Mutex::new(skill_registry));

    // Register skill tools (needs shared skill registry)
    registry.register(Box::new(tools::skill_list::SkillListTool::new(
        skill_registry.clone(),
    )))?;
    registry.register(Box::new(tools::skill_view::SkillViewTool::new(
        skill_registry.clone(),
    )))?;
    registry.register(Box::new(tools::skill_manage::SkillManageTool::new(
        skill_registry.clone(),
        skill_dirs.clone(),
    )))?;

    // Load Lua plugins from default directory and register as tools
    let plugin_dir = crate::plugin::bridge::default_plugins_dir();
    let plugins = crate::plugin::bridge::load_plugins_from_dir(&plugin_dir);
    let plugin_count = plugins.len();
    for plugin_def in plugins {
        let tool = crate::plugin::bridge::PluginTool::new(plugin_def);
        registry.register(Box::new(tool))?;
    }
    if plugin_count > 0 {
        tracing::info!(count = plugin_count, dir = %plugin_dir.display(), "Lua plugins loaded and registered as tools");
    }
    // Initialize MCP manager (lazy — no servers started yet)
    let mcp_manager = std::sync::Arc::new(tokio::sync::Mutex::new(
        mcp::manager::McpManager::from_config(&settings.mcp.servers),
    ));
    registry.register(Box::new(mcp::tools::McpListServersTool::new()))?;
    registry.register(Box::new(mcp::tools::McpListToolsTool::new()))?;
    registry.register(Box::new(mcp::tools::McpDescribeToolTool::new()))?;
    registry.register(Box::new(mcp::tools::McpCallToolTool::new()))?;
    let mcp_server_count = settings.mcp.servers.len();
    let skill_count = skill_registry.lock().await.len();

    let skill_registry_guard = skill_registry.lock().await;
    let active_identity_config = settings
        .active_identity
        .as_deref()
        .and_then(|name| settings.identities.get(name));
    let system_prompt = crate::prompts::system_prompt::build(
        &cwd,
        &registry,
        &skill_registry_guard,
        None::<&str>,
        &settings.role,
        active_identity_config,
    );
    drop(skill_registry_guard);
    tracing::debug!(prompt_len = system_prompt.len(), "System prompt assembled");

    let registry = Arc::new(registry); // wrap for shared access by sub-agents

    let mut engine = QueryEngine::new(
        client,
        model.to_string(),
        system_prompt,
        ConversationHistory::new(),
        registry.clone(),
        settings.max_turns,
        settings.max_tokens,
        permission_mode.clone(),
        settings.clone(),
        cwd.clone(),
    );
    engine.mcp_manager = Some(mcp_manager.clone());
    engine.memory_manager = Some(memory_manager.clone());
    engine.hook_executor = hook_executor;
    engine.client_factory = Some(client_factory.clone());
    engine.active_identity = settings.active_identity.clone();

    // Fire session_start hook
    if let Some(he) = &engine.hook_executor
        && he.has_hooks_for(crate::hooks::types::HookEvent::SessionStart)
        && let Ok(ctx) = he.build_context()
    {
        let _ = ctx.set("cwd", cwd.to_string_lossy().to_string());
        let _ = ctx.set("model", model.as_str());
        let _ = ctx.set("provider", provider_name.as_str());
        he.execute_session_event(crate::hooks::types::HookEvent::SessionStart, &ctx)
            .await;
    }

    // TUI setup
    use std::time::Duration;
    use tokio::sync::Mutex;

    let engine = Arc::new(Mutex::new(engine));

    // Create Gateway command channel (cmd_rx goes to App, cmd_tx to Gateway transport)
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();

    // Create Gateway — event routing + command dispatch
    let transport = Box::new(ChannelTransport::new(cmd_tx));
    let mut gateway = gateway::Gateway::new(
        engine.clone(),
        settings.clone(),
        transport,
        memory_manager.clone(),
        skill_registry.clone(),
        mcp_manager.clone(),
        todo_state.clone(),
        provider_name.to_string(),
        model.to_string(),
        builtin_tool_names,
        cwd.clone(),
    );

    // Register method handlers (forward-looking API for JSON-RPC dispatch).
    // These enable future StdioTransport integration (external TUI frontends).
    gateway.register_handler(
        "session.create",
        Box::new(gateway::handlers::session::SessionCreateHandler),
    );
    gateway.register_handler(
        "session.list",
        Box::new(gateway::handlers::session::SessionListHandler),
    );
    gateway.register_handler(
        "config.get",
        Box::new(gateway::handlers::config::ConfigGetHandler::new(
            settings.clone(),
        )),
    );
    gateway.register_handler(
        "prompt.submit",
        Box::new(gateway::handlers::prompt::PromptSubmitHandler::new(
            engine.clone(),
        )),
    );

    // Create App — pass Gateway's engine event sender for image paste, etc.
    let mut app = ui::app::App::with_identity(
        cmd_rx,
        gateway.engine_event_sender(),
        settings.active_identity.clone(),
    );
    // Populate identity names for /identity argument completion
    app.input.identity_names = settings.identities.keys().cloned().collect();
    // Share the todo state so the TUI can render the side panel
    app.set_todo_state(todo_state.clone());
    // Share the sub-agent progress sender with the engine so delegate_task
    // can report sub-agent progress to the TUI.
    {
        let eng = engine.lock().await;
        app.set_steer_slot(eng.pending_steer.clone());
    }
    // Wire sub-agent progress channel — Gateway owns the channel,
    // the engine clones the tx into ToolContext for delegate_task.
    {
        let mut eng = engine.lock().await;
        eng.sub_agent_tx = Some(gateway.sub_agent_sender());
    }
    // Share the background cancellation token so background review tasks
    // can be cancelled on shutdown.
    {
        let mut eng = engine.lock().await;
        eng.background_cancel = Some(app.background_cancel_token());
    }
    app.set_status(ui::status_bar::StatusInfo {
        model: model.to_string(),
        provider: provider_name.to_string(),
        total_tokens: 0,
        context_window: 0,
        turn_count: 0,
        mcp_server_count,
        skill_count,
        mode: ui::status_bar::AppMode::Idle,
        steer_count: 0,
        active_identity: settings.active_identity.clone(),
        tick: 0,
    });

    // Start config file watcher for hot-reload notification
    let config_path = config::paths::config_path();
    if config_path.exists() {
        match config::watcher::watch_config(config_path, gateway.engine_event_sender()) {
            Ok(guard) => {
                app.set_watcher_guard(guard);
                tracing::info!("Config file watcher started");
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to start config file watcher");
            }
        }
    }

    // ── Global panic hook: protect terminal from raw mode residue ──
    // If any code panics after init_terminal() (raw mode, alternate screen),
    // this hook restores the terminal before the default panic behavior.
    //
    // ⚠️  The hook runs in a signal-safe context — use only `std::io::stdout()`
    //     directly (not ratatui's terminal handle). Crossterm's execute! and
    //     disable_raw_mode are safe to call from the hook.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        // Best-effort terminal recovery — if something panicked, we still want
        // the user to see the panic message, not a blank alternate screen.
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::event::DisableMouseCapture,
            crossterm::event::DisableBracketedPaste,
        );
        // Chain to the original hook for proper panic reporting
        prev_hook(panic_info);
    }));

    let mut terminal = ui::app::init_terminal()?;

    // Auto-detect saved session on startup (filtered by active identity)
    if let Some(saved) =
        engine::session::load_latest_session_for_identity(settings.active_identity.as_deref())
    {
        let one_liner = engine::session::build_one_liner(&saved);
        app.output.push(ui::output::OutputSegment::Status(
            "󰄘 Previous session found. Type `/restore` to restore it.".to_string(),
        ));
        app.output.push(ui::output::OutputSegment::Status(format!(
            "   {}",
            one_liner
        )));
        app.mark_dirty();
    }

    // Pre-generate session title in the background after the first query
    // completes, so it's ready when the user quits (no blocking exit).
    let (title_tx, mut title_rx) = tokio::sync::oneshot::channel::<String>();
    let mut title_tx = Some(title_tx);
    let mut was_running = false;

    // Main TUI event loop
    // Use longer poll timeout when idle (100ms ≈ 10fps) vs short when running (16ms ≈ 60fps).
    // This dramatically reduces CPU when the user isn't interacting.
    let mut idle_frames = 0u32;
    loop {
        // 1. Process events: Gateway maps EngineEvents → UiCommands, App processes UiCommands
        {
            gateway.drain_engine_events();
            gateway.drain_sub_agent_events();
            app.drain_commands();
            app.poll_engine_status();
            if let Ok(eng) = engine.try_lock() {
                let ct = &eng.cost_tracker;
                // Context pressure = last API call's full prompt + output
                let ctx_tokens = ct.last_prompt_tokens + ct.last_output_tokens;
                let turns = ct.turn_count;
                let cw = eng.effective_context_window();
                if app.status.total_tokens != ctx_tokens
                    || app.status.turn_count != turns
                    || app.status.context_window != cw
                    || app.status.model != eng.model
                {
                    app.status
                        .update(gateway::UiCommand::UpdateTokens(ctx_tokens));
                    app.status
                        .update(gateway::UiCommand::UpdateTurnCount(turns));
                    app.status.context_window = cw;
                    app.status
                        .update(gateway::UiCommand::SetModel(eng.model.clone()));
                    app.mark_dirty();
                }
            }

            // Detect transition from Running → Idle: fire background title
            // generation on the first completed query.
            if was_running
                && !app.is_running()
                && title_tx.is_some()
                && let Ok(eng) = engine.try_lock()
            {
                // Extract the first user message for title generation
                let first_msg = eng
                    .history
                    .entries_raw()
                    .iter()
                    .find(|e| {
                        e.role == crate::api::types::Role::User
                            && e.content
                                .iter()
                                .any(|b| matches!(b, ContentBlock::Text { .. }))
                            && !e
                                .content
                                .iter()
                                .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
                    })
                    .and_then(|e| {
                        e.content.iter().find_map(|b| {
                            if let ContentBlock::Text { text } = b {
                                Some(text.clone())
                            } else {
                                None
                            }
                        })
                    })
                    .unwrap_or_default();
                if !first_msg.is_empty() {
                    let settings = settings.clone();
                    let tx = title_tx.take().unwrap();
                    tokio::spawn(async move {
                        let title = match tokio::time::timeout(
                            std::time::Duration::from_secs(8),
                            auxiliary::compressor::generate_title(&settings, &first_msg),
                        )
                        .await
                        {
                            Ok(t) => t.unwrap_or_default(),
                            Err(_) => String::new(),
                        };
                        let _ = tx.send(title);
                    });
                }
            }
            was_running = app.is_running();
        }

        // 2. Dispatch user input via Gateway
        if let Some(query_text) = app.take_pending_query() {
            let cancel = app.reset_cancel_token();
            let image_blocks = app.take_pending_image_blocks();
            let (cleaned_text, path_images) = extract_image_paths(&query_text);
            let mut all_images = image_blocks;
            all_images.extend(path_images);
            gateway
                .handle_input(&cleaned_text, all_images, cancel)
                .await;
        }

        // 3. Adaptive frame gating: skip render when nothing changed (idle).
        //    Still draw every 100 frames even when idle to handle terminal resize
        //    and other invisible state changes.
        let is_active = app.is_running() || app.mode() == AppMode::WaitingInput;

        if app.needs_render() || is_active || idle_frames >= 100 {
            terminal.draw(|f| app.render(f))?;
            app.clear_dirty();
            idle_frames = 0;
        } else {
            idle_frames += 1;
        }

        // Curator: background skill maintenance (idle-only)
        // Runs periodically when the system is idle. Lock order: engine → skill_registry
        // (consistent across all code paths to avoid deadlock).
        if !is_active && crate::engine::curator::should_run_now(&settings.skills) {
            let background_cancel = app.background_cancel_token();

            // Build deps first (engine lock), then pass to curator (skill_registry lock).
            let deps = {
                let eng = engine.lock().await;
                eng.client_factory.as_ref().map(|factory| {
                    crate::tools::base::SubAgentDeps::new(
                        factory.clone(),
                        eng.tools.clone(),
                        settings.clone(),
                        eng.sub_agent_tx.clone().unwrap_or_else(|| {
                            let (tx, _) = tokio::sync::mpsc::unbounded_channel();
                            tx
                        }),
                        settings.delegation.clone(),
                        eng.sub_agent_cost_tracker.clone(),
                    )
                    .with_write_origin(crate::skills::provenance::BACKGROUND_REVIEW)
                })
            };

            let cwd_option = deps.as_ref().map(|_| cwd.clone());
            let registry = skill_registry.lock().await;
            let summary = crate::engine::curator::run_curator_pass(
                &registry,
                deps,
                cwd_option,
                &settings.skills,
                Some(background_cancel),
            );
            drop(registry);

            if summary != "No lifecycle transitions needed." {
                tracing::info!(summary = %summary, "Curator lifecycle maintenance");

                // ⚠️ Curator may have archived/moved skill directories on disk,
                // but the in-memory registry still holds entries for them.
                // Reload the registry to keep counts accurate.
                let user_skills_dir = skills::loader::get_user_skills_dir();
                let (new_skills, new_categories) =
                    skills::loader::load_skills_from_dirs(&[user_skills_dir], "user");

                let mut reg = skill_registry.lock().await;

                // Keep bundled skills (non-user source) from current registry
                let bundled: Vec<_> = reg
                    .list_skills()
                    .into_iter()
                    .filter(|s| s.source != "user")
                    .cloned()
                    .collect();

                // Rebuild: bundled skills + fresh user skills
                let mut all_skills = bundled.clone();
                all_skills.extend(new_skills);

                // Start with fresh user categories, then merge bundled categories
                let mut all_categories = new_categories.clone();
                for (cat, info) in reg.categories() {
                    // Only include bundled skill names from old registry categories
                    let bundled_names: Vec<String> = info
                        .skill_names
                        .iter()
                        .filter(|n| bundled.iter().any(|s| &s.name == *n))
                        .cloned()
                        .collect();
                    if bundled_names.is_empty() {
                        continue;
                    }
                    let entry = all_categories.entry(cat.clone()).or_insert_with(|| {
                        skills::types::CategoryInfo {
                            description: info.description.clone(),
                            skill_names: Vec::new(),
                        }
                    });
                    for name in bundled_names {
                        if !entry.skill_names.contains(&name) {
                            entry.skill_names.push(name);
                        }
                    }
                }

                *reg = skills::registry::SkillRegistry::from_parts(all_skills, all_categories);

                // Update disk cache to match
                if let Err(e) = skills::index_cache::write_cache(
                    &skill_dirs,
                    &reg.list_skills().into_iter().cloned().collect::<Vec<_>>(),
                    reg.categories(),
                ) {
                    tracing::warn!(error = %e, "Failed to write skills cache after curator lifecycle");
                }

                // Update status bar with corrected count
                let new_count = reg.len();
                drop(reg);
                app.set_status(ui::status_bar::StatusInfo {
                    model: model.to_string(),
                    provider: provider_name.to_string(),
                    total_tokens: 0,
                    context_window: 0,
                    turn_count: 0,
                    mcp_server_count,
                    skill_count: new_count,
                    mode: ui::status_bar::AppMode::Idle,
                    steer_count: 0,
                    active_identity: settings.active_identity.clone(),
                    tick: 0,
                });
            }
        }

        // 4. Handle keyboard/mouse input with adaptive poll timeout.
        //    Idle: 100ms (10fps), Active: 16ms (60fps) for responsive input.
        let poll_ms = if is_active { 16u64 } else { 100u64 };
        if crossterm::event::poll(Duration::from_millis(poll_ms))? {
            match crossterm::event::read()? {
                crossterm::event::Event::Key(key) => {
                    app.handle_key(key);
                }
                crossterm::event::Event::Mouse(mouse) => {
                    use crossterm::event::MouseEventKind;
                    match mouse.kind {
                        MouseEventKind::ScrollUp => {
                            app.scroll_up(3, mouse.column, mouse.row);
                        }
                        MouseEventKind::ScrollDown => {
                            app.scroll_down(3, mouse.column, mouse.row);
                        }
                        _ => {
                            // Forward other mouse events (drag, press, release)
                            // for side panel resize handling.
                            if let Ok((w, _)) = crossterm::terminal::size() {
                                app.handle_mouse(mouse, w);
                            }
                        }
                    }
                }
                // Bracketed paste: insert the entire pasted text at once,
                // keeping newlines as-is instead of treating each as Enter.
                crossterm::event::Event::Paste(text) => {
                    app.handle_paste(text);
                }
                _ => {}
            }
        }

        // 5. Check quit — restore terminal ASAP so the user gets their
        //    shell back without delay.  Slow session-save work is done
        //    *after* the terminal is restored (below the loop).
        if app.should_quit() {
            // Cancel any running LLM task so the engine lock is released quickly.
            app.cancel_running();
            // Cancel background tasks (curator, review) so they stop promptly.
            app.cancel_background();
            // Unmount child components — signals lifecycle end for cleanup.
            app.unmount_components();
            break;
        }
    }

    // Restore terminal
    // Drain any buffered stdin events (mouse sequences etc.) before restoring
    // the terminal so they don't leak as garbage characters to the shell.
    while crossterm::event::poll(std::time::Duration::from_millis(0))? {
        let _ = crossterm::event::read();
    }

    ui::app::restore_terminal(&mut terminal)?;

    // Session persistence (after terminal restore)
    // Capture engine state for session persistence (with timeout).
    // If the engine is still busy, we proceed with whatever we can get.
    let (entries, total_tokens, current_model, saved_identity) =
        match tokio::time::timeout(std::time::Duration::from_secs(3), engine.lock()).await {
            Ok(eng) => {
                let entries = eng.history.entries_raw().to_vec();
                let tokens = eng.cost_tracker.total_tokens();
                let m = eng.model.clone();
                let id = eng.active_identity.clone();
                (entries, tokens, m, id)
            }
            Err(_) => (Vec::new(), 0, String::new(), None),
        };

    // Notify memory provider of session end (with timeout)
    {
        let json_entries: Vec<Value> = entries
            .iter()
            .filter_map(|e| serde_json::to_value(e).ok())
            .collect();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let mgr = memory_manager.lock().await;
            mgr.on_session_end(&json_entries).await;
        })
        .await;
    }

    if !entries.is_empty() {
        let now = SystemTime::now();
        let summary = engine::session::build_summary(&entries);
        let final_response = engine::session::extract_final_response(&entries).unwrap_or_default();

        // Use the pre-generated title if it's ready (fired earlier in
        // the background when the first query completed).  If it's not
        // ready yet, poll briefly then fall back to empty.
        let title = match title_rx.try_recv() {
            Ok(t) => t,
            Err(_) => {
                // Title still in-flight — wait up to 2s then give up.
                match tokio::time::timeout(std::time::Duration::from_secs(2), &mut title_rx).await {
                    Ok(Ok(t)) => t,
                    _ => String::new(),
                }
            }
        };

        let data = engine::session::SessionData {
            id: engine::session::generate_session_id(),
            saved_at: engine::session::format_timestamp(now),
            model: current_model,
            provider: provider_name.to_string(),
            cwd: cwd.to_string_lossy().to_string(),
            entries,
            total_tokens,
            summary,
            final_response,
            title,
            identity: saved_identity,
        };
        engine::session::save_session(&data);
    } else {
        // Empty session — remove stale session index if it exists
        let idx = config::paths::session_index_path();
        if idx.exists() {
            let _ = std::fs::remove_file(&idx);
        }
    }

    // Fire session_end hook
    if let Some(he) = &engine.lock().await.hook_executor
        && he.has_hooks_for(crate::hooks::types::HookEvent::SessionEnd)
        && let Ok(ctx) = he.build_context()
    {
        let _ = ctx.set("cwd", cwd.to_string_lossy().to_string());
        let _ = ctx.set("model", model.as_str());
        let _ = ctx.set("provider", provider_name.as_str());
        he.execute_session_event(crate::hooks::types::HookEvent::SessionEnd, &ctx)
            .await;
    }

    // Shut down memory manager (flush external provider)
    {
        let mut mgr = memory_manager.lock().await;
        mgr.shutdown().await;
    }

    // Gracefully shut down all connected MCP servers
    {
        let mut mgr = mcp_manager.lock().await;
        mgr.shutdown();
    }

    Ok(())
}
