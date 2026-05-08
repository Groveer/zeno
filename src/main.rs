mod api;
mod auth;
mod auxiliary;
mod config;
mod engine;
mod hooks;
mod mcp;
mod memory;
mod permissions;
mod prompts;
mod skills;
mod tools;
mod ui;
mod utils;

use config::load as config_load;
use config::settings;
use engine::messages::ConversationHistory;
use engine::query_engine::QueryEngine;
use memory::provider::MemoryProvider;
use std::sync::Arc;
use std::time::SystemTime;
use tools::base::ToolRegistry;
use ui::status_bar::AppMode;

// ---------------------------------------------------------------------------
// Slash command dispatch
// ---------------------------------------------------------------------------

/// What the TUI main loop should do after evaluating a slash command.
enum CommandAction {
    /// The command was handled synchronously; events already sent.
    Done,
    /// /compact — needs async engine task.
    Compact,
    /// /cost, /clear, /goal — need engine lock.
    NeedEngine(&'static str, String), // (command_name, arg)
    /// Not a slash command — send to LLM.
    Query,
}

/// Try to handle a slash command. Returns what action the caller should take.
fn dispatch_command(input: &str) -> CommandAction {
    if !input.starts_with('/') {
        return CommandAction::Query;
    }

    match input {
        "/help" => CommandAction::Done, // handled inline in caller
        "/cost" => CommandAction::NeedEngine("cost", String::new()),
        "/clear" => CommandAction::NeedEngine("clear", String::new()),
        "/compact" => CommandAction::Compact,
        "/resume" => CommandAction::NeedEngine("resume", String::new()),
        s if s.starts_with("/resume") => {
            let arg = s.strip_prefix("/resume").unwrap_or("").trim().to_string();
            CommandAction::NeedEngine("resume", arg)
        }
        s if s.starts_with("/model") => {
            let arg = s.strip_prefix("/model").unwrap_or("").trim().to_string();
            CommandAction::NeedEngine("model", arg)
        }
        "/tools" => CommandAction::Done,
        "/config" => CommandAction::Done,
        "/memory" => CommandAction::Done,
        "/mcp" => CommandAction::Done,
        s if s.starts_with("/goal") => {
            let arg = s.strip_prefix("/goal").unwrap_or("").trim().to_string();
            CommandAction::NeedEngine("goal", arg)
        }
        // Unknown slash command  send to LLM
        _ => CommandAction::Query,
    }
}

/// Help text (constant to avoid re-allocation every call).
const HELP_TEXT: &str = "\
Available commands:
/help — Show this help
/exit, /quit — Exit
/clear — Clear history
/compact — Compress history
/cost — Token usage
/model — Current model
/tools — List tools
/config — Show config
/memory — Memory files
/mcp — MCP servers
/resume — Restore the last session (conversation history + output)
/resume N — Restore session #N (use /resume to list all)
/goal [text] — Set/show/clear auto-continue goal
/goal clear — Clear goal
/goal pause — Pause goal
/goal resume — Resume goal

Navigation:
/ — History (input) or scroll (output)
PgUp/PgDn — Scroll output
Mouse wheel — Scroll output
Shift+drag — Select & copy text
 Ctrl+C — Interrupt (when running) / Quit (when idle)
 Ctrl+D — Hard quit (immediate, any mode)
";

/// Send a simple text response + QueryDone through the channel.
fn send_text_response(sender: &engine::tui_events::UiSender, text: &str) {
    let _ = sender.send(engine::tui_events::UiEvent::TextDelta(text.to_string()));
    let _ = sender.send(engine::tui_events::UiEvent::QueryDone {
        text: String::new(),
        tool_calls: 0,
        tokens: 0,
    });
}

/// Handle /goal command — requires &mut QueryEngine.
fn handle_goal(eng: &mut QueryEngine, arg: &str) -> String {
    if arg.is_empty() || arg == "status" {
        if eng.carryover.has_pending_goal() {
            format!(" Goal (active): {}", eng.carryover.task_focus.goal)
        } else {
            "No active goal. Use /goal <text> to set one.".to_string()
        }
    } else if arg == "clear" || arg == "stop" {
        let had = eng.carryover.has_pending_goal();
        eng.carryover.clear_goal();
        if had {
            " Goal cleared.".to_string()
        } else {
            "No active goal.".to_string()
        }
    } else if arg == "pause" {
        if eng.carryover.has_pending_goal() {
            let goal_text = eng.carryover.task_focus.goal.clone();
            eng.carryover.clear_goal();
            format!(" Goal paused: {}", goal_text)
        } else {
            "No active goal to pause.".to_string()
        }
    } else if arg == "resume" {
        let last_goal = eng.carryover.task_focus.recent_goals.last().cloned();
        if let Some(last) = last_goal {
            eng.carryover.set_goal(&last);
            format!(" Goal resumed: {}", last)
        } else {
            "No recent goal to resume.".to_string()
        }
    } else {
        eng.carryover.set_goal(arg);
        format!(" Goal set: {}", arg)
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

    let settings = config_load()?;
    config::paths::cleanup_old_logs(settings.log_retention_days);
    let settings = Arc::new(settings);

    let provider_name = settings.active_provider.clone();
    let model = settings.model.clone();
    let permission_mode = settings.permissions.clone();

    let provider_config = settings.providers.get(&provider_name).ok_or_else(|| {
        anyhow::anyhow!(
            "Provider '{}' not configured. Add it to ~/.config/zeno/init.lua\n\
             \n\
             local zn = require 'zeno'\n\
             zn.provider(\"anthropic\", {{ api_key_env = \"ANTHROPIC_API_KEY\", base_url = \"https://api.anthropic.com\", default_model = \"claude-sonnet-4-20250514\" }})\n\
             zn.provider(\"openai\", {{ api_key_env = \"OPENAI_API_KEY\", base_url = \"https://api.openai.com/v1\", default_model = \"gpt-4o\" }})\n\
             zn.set_provider(\"anthropic\")",
            provider_name
        )
    })?;

    // Build API client
    let api_key = settings::resolve_api_key(provider_config)?;
    let base_url = provider_config.base_url.clone();
    let client: Box<dyn api::client::SupportsStreamingMessages> = match provider_name.as_str() {
        "anthropic" => Box::new(api::anthropic::AnthropicClient::new(api_key, base_url)),
        _ => Box::new(api::openai::OpenAIClient::new(api_key, base_url)),
    };

    // Build tool registry
    let mut registry = ToolRegistry::new();
    let tc = &settings.tools;
    if tc.bash {
        registry.register(Box::new(tools::bash::BashTool::new(
            tc.use_rtk,
            tc.bash_env.clone(),
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
        registry.register(Box::new(tools::glob::GlobTool::new()))?;
    }
    if tc.grep {
        registry.register(Box::new(tools::grep::GrepTool::new()))?;
    }
    if tc.web_search {
        registry.register(Box::new(tools::web_search::WebSearchTool::with_config(
            settings.web_search_config.clone(),
        )))?;
    }
    if tc.web_fetch {
        registry.register(Box::new(tools::web_fetch::WebFetchTool::new()))?;
    }
    registry.register(Box::new(tools::config_tool::ConfigTool::new()))?;
    registry.register(Box::new(tools::ask_user::AskUserTool::new()))?;

    // Resolve working directory early (needed for memory dir + skills + system prompt)
    let cwd = std::env::current_dir().unwrap_or_default();

    // Initialize memory store — global paths only
    let memory_dir = config::paths::memory_dir();
    let user_profile_path = config::paths::user_profile_path();
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
    registry.register(Box::new(tools::memory::MemoryTool::new(
        memory_store.clone(),
    )))?;

    // Initialize memory manager (orchestrates built-in + external providers)
    let mut memory_manager = memory::manager::MemoryManager::new(memory_store.clone());

    // Load and activate the configured external memory provider (if any)
    if !settings.memory.provider.is_empty() {
        if let Some(provider_entry) = settings.memory.providers.get(&settings.memory.provider) {
            let config_dir = config::paths::config_dir();
            let lua_config = memory::lua_provider::LuaProviderConfig {
                name: settings.memory.provider.clone(),
                script: provider_entry.script.clone(),
                inline: provider_entry.inline,
            };
            match memory::lua_provider::LuaMemoryProvider::new(lua_config, config_dir) {
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
        } else {
            tracing::warn!(
                provider = %settings.memory.provider,
                "Memory provider referenced in config but not registered"
            );
        }
    }

    // Initialize the memory manager for this session
    let session_id = format!("session-{}", std::process::id());
    memory_manager.initialize(&session_id).await;

    // Wrap memory manager for shared access
    let memory_manager: memory::manager::SharedMemoryManager = Arc::new(Mutex::new(memory_manager));

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

    let tool_names: Vec<String> = registry.names().into_iter().map(String::from).collect();
    tracing::info!(tools = ?tool_names, "Registered tools");

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

    // Register skill tools (needs skill registry)
    registry.register(Box::new(tools::skill_list::SkillListTool::new(
        skill_registry.clone(),
    )))?;
    registry.register(Box::new(tools::skill_view::SkillViewTool::new(
        skill_registry.clone(),
    )))?;

    // Initialize MCP manager (lazy — no servers started yet)
    let mcp_manager = std::sync::Arc::new(tokio::sync::Mutex::new(
        mcp::manager::McpManager::from_config(&settings.mcp.servers),
    ));
    registry.register(Box::new(mcp::tools::McpListServersTool::new()))?;
    registry.register(Box::new(mcp::tools::McpListToolsTool::new()))?;
    registry.register(Box::new(mcp::tools::McpDescribeToolTool::new()))?;
    registry.register(Box::new(mcp::tools::McpCallToolTool::new()))?;

    // Build system prompt — use memory manager for built-in + external provider content
    let memory_prompt = memory_manager.lock().await.build_system_prompt();
    let system_prompt = crate::prompts::system_prompt::build(
        &cwd,
        &registry,
        &skill_registry,
        Some(&memory_prompt),
        &settings.role,
    );
    drop(memory_prompt);
    tracing::debug!(prompt_len = system_prompt.len(), "System prompt assembled");

    let mut engine = QueryEngine::new(
        client,
        model.to_string(),
        system_prompt,
        ConversationHistory::new(),
        registry,
        settings.max_turns,
        settings.max_tokens,
        permission_mode.clone(),
        settings.clone(),
        cwd.clone(),
    );
    engine.mcp_manager = Some(mcp_manager.clone());

    // TUI setup
    use std::time::Duration;
    use tokio::sync::Mutex;

    let engine = Arc::new(Mutex::new(engine));

    let mut app = ui::app::App::new();
    // Share the engine's steer slot so the TUI can inject mid-run user input
    // without needing the engine lock. This enables the "type while running"
    // feature — the user's text goes into the slot, and the engine drains it
    // after tool results are appended.
    {
        let eng = engine.lock().await;
        app.set_steer_slot(eng.pending_steer.clone());
    }
    app.set_status(ui::status_bar::StatusInfo {
        model: model.to_string(),
        provider: provider_name.to_string(),
        total_tokens: 0,
        context_window: 0,
        turn_count: 0,
        tool_count: tool_names.len(),
        mode: ui::status_bar::AppMode::Idle,
        steer_count: 0,
    });

    let mut terminal = ui::app::init_terminal()?;

    // Auto-detect saved session on startup
    if let Some(saved) = engine::session::load_latest_session() {
        let one_liner = engine::session::build_one_liner(&saved);
        app.output.push(ui::output::OutputSegment::Status(
            "󰄘 Previous session found. Type `/resume` to restore it.".to_string(),
        ));
        app.output.push(ui::output::OutputSegment::Status(format!(
            "   {}",
            one_liner
        )));
        app.mark_dirty();
    }

    // Main TUI event loop
    // Use longer poll timeout when idle (100ms ≈ 10fps) vs short when running (16ms ≈ 60fps).
    // This dramatically reduces CPU when the user isn't interacting.
    let mut idle_frames = 0u32;
    loop {
        // 1. Process engine events (non-blocking drain)
        {
            app.process_events();
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
                    app.status.total_tokens = ctx_tokens;
                    app.status.turn_count = turns;
                    app.status.context_window = cw;
                    app.status.model = eng.model.clone();
                    app.mark_dirty();
                }
            }
        }

        // 2. Dispatch user input
        if let Some(query_text) = app.take_pending_query() {
            let engine = engine.clone();
            let sender = app.event_sender();

            match dispatch_command(&query_text) {
                CommandAction::Done => {
                    // Handle synchronous commands that don't need engine lock
                    match query_text.as_str() {
                        "/help" => send_text_response(&sender, HELP_TEXT),
                        "/model" => send_text_response(
                            &sender,
                            &format!("Model: {} (provider: {})", model, provider_name),
                        ),
                        "/tools" => send_text_response(
                            &sender,
                            &format!("Tools ({}): {}", tool_names.len(), tool_names.join(", ")),
                        ),
                        "/config" => send_text_response(
                            &sender,
                            &format!(
                                "Config: ~/.config/zeno/init.lua\nProvider: {}\nModel: {}",
                                provider_name, model
                            ),
                        ),
                        "/memory" => {
                            let summary = {
                                let store = memory_store.lock().await;
                                let mut s = store.summary();
                                // Append external provider info if active
                                let mgr = memory_manager.lock().await;
                                if let Some(name) = mgr.external_name() {
                                    s.push_str(&format!(
                                        "\n\nExternal provider: {} (active)",
                                        name
                                    ));
                                }
                                s
                            };
                            send_text_response(&sender, &summary);
                        }
                        "/mcp" => {
                            let summary = {
                                let mgr = mcp_manager.lock().await;
                                mgr.summary()
                            };
                            send_text_response(&sender, &summary);
                        }
                        _ => {} // shouldn't reach here
                    }
                }
                CommandAction::NeedEngine(cmd, arg) => match cmd {
                    "cost" => {
                        let eng = engine.lock().await;
                        let ct = &eng.cost_tracker;
                        let total = ct.total_tokens();
                        let msg = if ct.model_breakdown().len() > 1 {
                            // Multi-model breakdown
                            let mut lines = vec![format!(
                                "Token usage: {} total ({} input + {} output + {} cache) — {} API calls",
                                total,
                                ct.total_input_tokens,
                                ct.total_output_tokens,
                                ct.total_cached_tokens(),
                                ct.turn_count,
                            )];
                            lines.push(format!(
                                "Context pressure: {} prompt + {} output = {} tokens",
                                ct.last_prompt_tokens,
                                ct.last_output_tokens,
                                ct.last_prompt_tokens + ct.last_output_tokens,
                            ));
                            for (model, mc) in ct.model_breakdown() {
                                lines.push(format!(
                                    "  {model}: {} input + {} output ({} calls)",
                                    mc.input_tokens, mc.output_tokens, mc.calls,
                                ));
                                let mut sub_parts = Vec::new();
                                if mc.cache_read_input_tokens > 0
                                    || mc.cache_creation_input_tokens > 0
                                {
                                    sub_parts.push(format!(
                                        "cache: {} read + {} write",
                                        mc.cache_read_input_tokens, mc.cache_creation_input_tokens,
                                    ));
                                }
                                if mc.reasoning_tokens > 0 {
                                    sub_parts.push(format!(
                                        "reasoning: {} (included in output)",
                                        mc.reasoning_tokens,
                                    ));
                                }
                                for part in &sub_parts {
                                    lines.push(format!("    {part}"));
                                }
                            }
                            lines.join("\n")
                        } else {
                            let mut msg = format!(
                                "Token usage: {} total ({} input + {} output",
                                total, ct.total_input_tokens, ct.total_output_tokens,
                            );
                            if ct.total_cached_tokens() > 0 {
                                msg.push_str(&format!(" + {} cached", ct.total_cached_tokens()));
                            }
                            msg.push(')');
                            if ct.total_reasoning_tokens > 0 {
                                msg.push_str(&format!(
                                    " (reasoning: {}, included in output)",
                                    ct.total_reasoning_tokens
                                ));
                            }
                            let calls = if ct.turn_count == 1 { "call" } else { "calls" };
                            msg.push_str(&format!(" — {} {}", ct.turn_count, calls));
                            msg.push_str(&format!(
                                "\nContext pressure: {} prompt + {} output = {} tokens",
                                ct.last_prompt_tokens,
                                ct.last_output_tokens,
                                ct.last_prompt_tokens + ct.last_output_tokens,
                            ));
                            msg
                        };
                        drop(eng);
                        send_text_response(&sender, &msg);
                    }
                    "clear" => {
                        let mut eng = engine.lock().await;
                        let count = eng.history.len();
                        eng.history.clear();
                        drop(eng);
                        let _ = sender.send(engine::tui_events::UiEvent::ClearOutput);
                        send_text_response(&sender, &format!("Cleared {} entries.", count));
                    }
                    "goal" => {
                        let mut eng = engine.lock().await;
                        let msg = handle_goal(&mut eng, &arg);
                        drop(eng);
                        send_text_response(&sender, &msg);
                    }
                    "model" => {
                        if arg.is_empty() {
                            send_text_response(
                                &sender,
                                &format!("Model: {} (provider: {})", model, provider_name),
                            );
                        } else {
                            let mut eng = engine.lock().await;
                            eng.set_model(arg.clone());
                            drop(eng);
                            app.status.model = arg.clone();
                            app.mark_dirty();
                            send_text_response(
                                &sender,
                                &format!("Model temporarily set to: {}", arg),
                            );
                        }
                    }
                    "resume" => {
                        // Parse optional index argument: "/resume" or "/resume 2"
                        let session_data = {
                            if arg.trim().is_empty() {
                                engine::session::load_latest_session()
                            } else if let Ok(n) = arg.trim().parse::<usize>() {
                                let index = engine::session::load_session_index();
                                if n == 0 || n > index.len() {
                                    let list = engine::session::format_session_list(&index);
                                    send_text_response(
                                        &sender,
                                        &format!(
                                            "Invalid session number: {}. {}\n\n{}",
                                            n,
                                            if index.is_empty() {
                                                "No sessions available."
                                            } else {
                                                ""
                                            },
                                            list,
                                        ),
                                    );
                                    None // handled
                                } else {
                                    engine::session::load_session_by_id(&index[n - 1].id)
                                }
                            } else {
                                let index = engine::session::load_session_index();
                                let list = engine::session::format_session_list(&index);
                                send_text_response(&sender, &list);
                                None // handled
                            }
                        };

                        if let Some(data) = session_data {
                            let mut eng = engine.lock().await;
                            // Rebuild conversation history from saved entries
                            let hist = ConversationHistory::from_entries(data.entries.clone());
                            eng.history = hist;
                            eng.cost_tracker = crate::engine::cost_tracker::CostTracker::default();
                            let one_liner = engine::session::build_one_liner(&data);
                            let summary = data.summary.clone();
                            drop(eng);

                            // Also rebuild the TUI output area with the saved summary
                            let _ = sender.send(engine::tui_events::UiEvent::ClearOutput);
                            let _ = sender.send(engine::tui_events::UiEvent::Status(format!(
                                "󰄘 Resumed: {}",
                                one_liner
                            )));
                            let _ = sender.send(engine::tui_events::UiEvent::TextDelta(format!(
                                "━━━ Session Resume ━━━\n\
                                     Saved: {}\n\
                                     Model: {} | Provider: {}\n\
                                     Total tokens: {}\n\n\
                                     {}\n\
                                     ━━━ End of Session ━━━\n\n\
                                     Ready to continue — type your next message.",
                                &data
                                    .saved_at
                                    .get(..19)
                                    .unwrap_or(&data.saved_at)
                                    .replace('T', " "),
                                data.model,
                                data.provider,
                                data.total_tokens,
                                summary,
                            )));
                            let _ = sender.send(engine::tui_events::UiEvent::QueryDone {
                                text: String::new(),
                                tool_calls: 0,
                                tokens: data.total_tokens,
                            });
                        } else {
                            // No session data available — the handler above already sent
                            // the appropriate message, or there was nothing to resume.
                            // Ensure we always return to Idle.
                            send_text_response(&sender, "No saved session to resume.");
                        }
                    }
                    _ => {}
                },
                CommandAction::Compact => {
                    let settings = settings.clone();
                    let sender2 = sender.clone();
                    tokio::spawn(async move {
                        // Take a snapshot while holding the lock, then release for compression.
                        let (snapshot, orig_len) = {
                            let eng = engine.lock().await;
                            (eng.history.clone(), eng.history.len())
                        };
                        match auxiliary::compressor::compress_history(&settings, &snapshot, None)
                            .await
                        {
                            Ok(summary) => {
                                let mut eng = engine.lock().await;
                                // Replace history only if it wasn't already compacted
                                // (another task may have run compression between our
                                // snapshot and this lock acquisition).
                                if eng.history.len() >= orig_len / 2 {
                                    eng.history.clear();
                                    eng.history.push_user(&format!(
                                        "[Compressed conversation history ({} entries)]\n\n{}",
                                        orig_len, summary,
                                    ));
                                }
                                let _ = sender2.send(engine::tui_events::UiEvent::Status(format!(
                                    "Compressed {} entries  {} chars",
                                    orig_len,
                                    summary.len()
                                )));
                                let _ = sender2.send(engine::tui_events::UiEvent::QueryDone {
                                    text: String::new(),
                                    tool_calls: 0,
                                    tokens: eng.cost_tracker.last_prompt_tokens
                                        + eng.cost_tracker.last_output_tokens,
                                });
                            }
                            Err(e) => {
                                let _ = sender2.send(engine::tui_events::UiEvent::Error(format!(
                                    "Compression failed: {}",
                                    e
                                )));
                            }
                        }
                    });
                }
                CommandAction::Query => {
                    let cancel = app.reset_cancel_token();
                    tokio::spawn(async move {
                        let mut eng = engine.lock().await;
                        if let Err(e) = eng.query_tui(&query_text, &sender, cancel).await {
                            let _ = sender.send(engine::tui_events::UiEvent::Error(e.to_string()));
                        }
                    });
                }
            }
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
                        MouseEventKind::ScrollUp => app.scroll_up(3),
                        MouseEventKind::ScrollDown => app.scroll_down(3),
                        _ => {}
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

        // 5. Check quit — auto-save session before exiting
        if app.should_quit() {
            // Capture engine state for session persistence
            let (entries, total_tokens, current_model) = {
                let eng = engine.lock().await;
                let entries = eng.history.entries_raw().to_vec();
                let tokens = eng.cost_tracker.total_tokens();
                let m = eng.model.clone();
                (entries, tokens, m)
            };

            if !entries.is_empty() {
                let now = SystemTime::now();
                let summary = engine::session::build_summary(&entries);
                let final_response =
                    engine::session::extract_final_response(&entries).unwrap_or_default();
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
                };
                engine::session::save_session(&data);
            } else {
                // Empty session — remove stale session index if it exists
                let idx = config::paths::session_index_path();
                if idx.exists() {
                    let _ = std::fs::remove_file(&idx);
                }
            }
            break;
        }
    }

    ui::app::restore_terminal(&mut terminal)?;
    Ok(())
}
