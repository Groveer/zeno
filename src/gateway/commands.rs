//! Command router — handles all `/` slash commands.
//!
//! Extracted from Gateway to separate concerns:
//! - Gateway: pure event routing (EngineEvent → UiCommand)
//! - CommandRouter: slash command execution (business logic)
//!
//! CommandRouter owns the service dependencies needed by slash commands
//! (memory, skills, mcp, settings) while Gateway only keeps what it
//! needs for event routing.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::engine::messages::ConversationHistory;
use crate::engine::query_engine::QueryEngine;
use crate::engine::tui_events::EngineEvent;
use crate::gateway::UiCommand;
use crate::ui::status_bar::{AppMode, StatusInfo};

/// Result of a slash command dispatch.
///
/// This replaces the implicit "empty vec = passthrough" convention with
/// an explicit enum, preventing accidental query submission when a handler
/// returns an empty command list.
pub enum CommandResult {
    /// Command was handled — emit these UiCommands.
    Handled(Vec<UiCommand>),
    /// Unknown command — the caller should submit the input as an LLM query.
    PassThrough,
}

/// Command router — executes slash commands.
///
/// Owns the service dependencies that slash commands need:
/// - Engine (for /cost, /clear, /compact, /model, etc.)
/// - Memory manager (for /memory, /identity)
/// - Skill registry (for /skills, /identity)
/// - MCP manager (for /mcp)
/// - Settings (for /model, /identity, /search)
///
/// Communicates with the UI through the engine event channel.
pub struct CommandRouter {
    /// Engine reference — shared with Gateway for query submission.
    pub(crate) engine: Arc<tokio::sync::Mutex<QueryEngine>>,
    /// Shared memory manager.
    pub(crate) memory_manager: crate::memory::manager::SharedMemoryManager,
    /// Shared skill registry.
    pub(crate) skill_registry: Arc<tokio::sync::Mutex<crate::skills::registry::SkillRegistry>>,
    /// Shared MCP manager.
    pub(crate) mcp_manager: Arc<tokio::sync::Mutex<crate::mcp::manager::McpManager>>,
    /// Application settings.
    pub(crate) settings: Arc<crate::config::settings::Settings>,
    /// Engine event sender — for sending responses to the UI.
    pub(crate) engine_event_tx: mpsc::UnboundedSender<EngineEvent>,
    /// Provider display name (for /model, /cost).
    pub(crate) provider_name: String,
    /// Model display name (for /model, /cost).
    pub(crate) model_name: String,
    /// Built-in tool names (for /tools).
    pub(crate) builtin_tool_names: Vec<String>,
    /// Built-in tool count.
    pub(crate) builtin_tool_count: usize,
}

impl CommandRouter {
    /// Create a new command router.
    pub fn new(
        engine: Arc<tokio::sync::Mutex<QueryEngine>>,
        settings: Arc<crate::config::settings::Settings>,
        memory_manager: crate::memory::manager::SharedMemoryManager,
        skill_registry: Arc<tokio::sync::Mutex<crate::skills::registry::SkillRegistry>>,
        mcp_manager: Arc<tokio::sync::Mutex<crate::mcp::manager::McpManager>>,
        engine_event_tx: mpsc::UnboundedSender<EngineEvent>,
        provider_name: String,
        model_name: String,
        builtin_tool_names: Vec<String>,
    ) -> Self {
        let builtin_tool_count = builtin_tool_names.len();
        Self {
            engine,
            memory_manager,
            skill_registry,
            mcp_manager,
            settings,
            engine_event_tx,
            provider_name,
            model_name,
            builtin_tool_names,
            builtin_tool_count,
        }
    }

    /// Dispatch a slash command and return the result.
    ///
    /// This is the main entry point. The Gateway calls this and emits
    /// the returned commands through its transport, or submits the input
    /// as an LLM query if the command is unknown (`PassThrough`).
    pub async fn dispatch(&mut self, input: &str) -> CommandResult {
        match input {
            "/help" => CommandResult::Handled(vec![
                UiCommand::AppendText(HELP_TEXT.to_string()),
                UiCommand::SetMode(AppMode::Idle),
            ]),
            "/exit" | "/quit" => {
                // These are handled by App.handle_key() before reaching Gateway
                CommandResult::Handled(vec![])
            }
            "/tools" => CommandResult::Handled(vec![
                UiCommand::AppendText(format!(
                    "Tools ({})\n{}",
                    self.builtin_tool_count,
                    self.builtin_tool_names.join(", ")
                )),
                UiCommand::SetMode(AppMode::Idle),
            ]),
            "/memory" => {
                let summary = {
                    let mgr = self.memory_manager.lock().await;
                    let store = mgr.memory_store().lock().await;
                    let mut s = store.summary();
                    if let Some(name) = mgr.external_name() {
                        s.push_str(&format!("\n\nExternal provider: {} (active)", name));
                    }
                    s
                };
                CommandResult::Handled(vec![
                    UiCommand::AppendText(summary),
                    UiCommand::SetMode(AppMode::Idle),
                ])
            }
            "/mcp" => {
                let summary = {
                    let mgr = self.mcp_manager.lock().await;
                    mgr.summary()
                };
                CommandResult::Handled(vec![
                    UiCommand::AppendText(summary),
                    UiCommand::SetMode(AppMode::Idle),
                ])
            }
            "/skills" => {
                let mut lines = Vec::new();
                let reg = self.skill_registry.lock().await;
                let categories = reg.categories();
                if categories.is_empty() {
                    let skills = reg.list_skills();
                    lines.push(format!("Skills ({})\n", skills.len()));
                    for s in &skills {
                        lines.push(format!("- {}: {}", s.name, s.description));
                    }
                } else {
                    lines.push(format!(
                        "Skills ({} skills in {} categories)\n",
                        reg.len(),
                        categories.len()
                    ));
                    for (cat, info) in categories {
                        lines.push(format!(
                            "## {} — {}",
                            cat,
                            if info.description.is_empty() {
                                String::new()
                            } else {
                                info.description.clone()
                            }
                        ));
                        for name in &info.skill_names {
                            if let Some(skill) = reg.get(name) {
                                lines.push(format!("- {}: {}", skill.name, skill.description));
                            }
                        }
                        lines.push(String::new());
                    }
                }
                drop(reg);
                CommandResult::Handled(vec![
                    UiCommand::AppendText(lines.join("\n")),
                    UiCommand::SetMode(AppMode::Idle),
                ])
            }
            "/cost" => self.handle_cost_cmd().await,
            "/clear" => self.handle_clear_cmd().await,
            "/compact" => self.handle_compact_cmd().await,
            s if s.starts_with("/restore") => {
                let arg = s.strip_prefix("/restore").unwrap_or("").trim();
                self.handle_restore_cmd(arg).await
            }
            s if s.starts_with("/model") => {
                let arg = s.strip_prefix("/model").unwrap_or("").trim();
                self.handle_model_cmd(arg).await
            }
            "/hooks" => self.handle_hooks_cmd().await,
            s if s.starts_with("/search") => {
                let arg = s.strip_prefix("/search").unwrap_or("").trim();
                self.handle_search_cmd(arg).await
            }
            s if s.starts_with("/goal") => {
                let arg = s.strip_prefix("/goal").unwrap_or("").trim();
                self.handle_goal_cmd(arg).await
            }
            s if s.starts_with("/identity") => {
                let arg = s.strip_prefix("/identity").unwrap_or("").trim();
                self.handle_identity_cmd(arg).await
            }
            _ => {
                // Unknown slash command — let caller submit as LLM query
                CommandResult::PassThrough
            }
        }
    }

    // ── Command handlers ──────────────────────────────────────────────────

    /// Handle /cost command.
    async fn handle_cost_cmd(&self) -> CommandResult {
        let eng = self.engine.lock().await;
        let ct = &eng.cost_tracker;
        let total = ct.total_tokens();
        let msg = if ct.model_breakdown().len() > 1 {
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
                if mc.cache_read_input_tokens > 0 || mc.cache_creation_input_tokens > 0 {
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
        CommandResult::Handled(vec![
            UiCommand::AppendText(msg),
            UiCommand::SetMode(AppMode::Idle),
        ])
    }

    /// Handle /clear command.
    async fn handle_clear_cmd(&self) -> CommandResult {
        let mut eng = self.engine.lock().await;
        let count = eng.history.len();
        eng.history.clear();
        drop(eng);
        CommandResult::Handled(vec![
            UiCommand::ClearOutput,
            UiCommand::AppendText(format!("Cleared {} entries.", count)),
            UiCommand::SetMode(AppMode::Idle),
        ])
    }

    /// Handle /compact command.
    async fn handle_compact_cmd(&self) -> CommandResult {
        let engine = self.engine.clone();
        let settings = self.settings.clone();
        let sender = self.engine_event_tx.clone();
        tokio::spawn(async move {
            let (snapshot, orig_len) = {
                let eng = engine.lock().await;
                (eng.history.clone(), eng.history.len())
            };
            match crate::auxiliary::compressor::compress_history(&settings, &snapshot, None).await {
                Ok(summary) => {
                    let mut eng = engine.lock().await;
                    if eng.history.len() >= orig_len / 2 {
                        eng.history.clear();
                        eng.history.push_user(&format!(
                            "[Compressed conversation history ({} entries)]\n\n{}",
                            orig_len, summary,
                        ));
                    }
                    let _ = sender.send(EngineEvent::Status(format!(
                        "Compressed {} entries → {} chars",
                        orig_len,
                        summary.len()
                    )));
                    let _ = sender.send(EngineEvent::QueryDone {
                        text: String::new(),
                        tool_calls: 0,
                        tokens: eng.cost_tracker.last_prompt_tokens
                            + eng.cost_tracker.last_output_tokens,
                    });
                }
                Err(e) => {
                    let _ = sender.send(EngineEvent::Error(format!("Compression failed: {}", e)));
                }
            }
        });
        CommandResult::Handled(vec![]) // Async task sends events directly
    }

    /// Handle /restore command.
    async fn handle_restore_cmd(&self, arg: &str) -> CommandResult {
        let active_identity = {
            let eng = self.engine.lock().await;
            eng.active_identity.as_deref().map(|s| s.to_string())
        };

        let session_data = {
            if arg.trim().is_empty() {
                let index = crate::engine::session::load_session_index();
                let filtered = crate::engine::session::filter_index_by_identity(
                    &index,
                    active_identity.as_deref(),
                );
                filtered
                    .first()
                    .and_then(|e| crate::engine::session::load_session_by_id(&e.id))
            } else if let Ok(n) = arg.trim().parse::<usize>() {
                let index = crate::engine::session::load_session_index();
                let filtered = crate::engine::session::filter_index_by_identity(
                    &index,
                    active_identity.as_deref(),
                );
                if n == 0 || n > filtered.len() {
                    let list = crate::engine::session::format_session_list(&filtered);
                    return CommandResult::Handled(vec![
                        UiCommand::AppendText(format!(
                            "Invalid session number: {}. {}\n\n{}",
                            n,
                            if filtered.is_empty() {
                                "No sessions available."
                            } else {
                                ""
                            },
                            list,
                        )),
                        UiCommand::SetMode(AppMode::Idle),
                    ]);
                }
                crate::engine::session::load_session_by_id(&filtered[n - 1].id)
            } else {
                let index = crate::engine::session::load_session_index();
                let filtered = crate::engine::session::filter_index_by_identity(
                    &index,
                    active_identity.as_deref(),
                );
                let list = crate::engine::session::format_session_list(&filtered);
                return CommandResult::Handled(vec![
                    UiCommand::AppendText(list),
                    UiCommand::SetMode(AppMode::Idle),
                ]);
            }
        };

        if let Some(data) = session_data {
            let new_session_id = data.id.clone();
            let mut eng = self.engine.lock().await;
            let hist = ConversationHistory::from_entries(data.entries.clone());
            eng.history = hist;
            eng.cost_tracker = crate::engine::cost_tracker::CostTracker::default();
            let one_liner = crate::engine::session::build_one_liner(&data);
            let summary = data.summary.clone();

            if let Some(ref mm) = eng.memory_manager {
                let mm = mm.lock().await;
                mm.on_session_switch(&new_session_id, "", false);
            }
            drop(eng);

            let cmds = vec![
                UiCommand::ClearOutput,
                UiCommand::ShowStatus(format!(" Resumed: {}", one_liner)),
                UiCommand::AppendText(format!(
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
                )),
                UiCommand::ClearSteerQueue,
                UiCommand::SetMode(AppMode::Idle),
                UiCommand::QueryDone {
                    tokens: data.total_tokens,
                },
            ];
            CommandResult::Handled(cmds)
        } else if arg.trim().is_empty() {
            CommandResult::Handled(vec![
                UiCommand::AppendText("No saved session to resume.".to_string()),
                UiCommand::SetMode(AppMode::Idle),
            ])
        } else {
            CommandResult::Handled(vec![])
        }
    }

    /// Handle /model command.
    async fn handle_model_cmd(&mut self, arg: &str) -> CommandResult {
        if !arg.is_empty() {
            let mut eng = self.engine.lock().await;
            eng.set_model(arg.to_string());
            drop(eng);
            self.model_name = arg.to_string();
            CommandResult::Handled(vec![
                UiCommand::SetModel(arg.to_string()),
                UiCommand::AppendText(format!("Model temporarily set to: {}", arg)),
                UiCommand::SetMode(AppMode::Idle),
            ])
        } else {
            CommandResult::Handled(vec![
                UiCommand::AppendText(format!(
                    "Model: {} (provider: {})",
                    self.model_name, self.provider_name
                )),
                UiCommand::SetMode(AppMode::Idle),
            ])
        }
    }

    /// Handle /hooks command.
    async fn handle_hooks_cmd(&self) -> CommandResult {
        let eng = self.engine.lock().await;
        let msg = if let Some(he) = &eng.hook_executor {
            let events = he.registered_events();
            if events.is_empty() {
                "No hooks registered. Use zn.hook(event, fn) in init.lua to register hooks."
                    .to_string()
            } else {
                let mut lines = vec![format!("Registered hooks ({} total):", he.hook_count())];
                for (event, count) in &events {
                    lines.push(format!("  {} — {} handler(s)", event, count));
                }
                lines.join("\n")
            }
        } else {
            "No hooks registered. Use zn.hook(event, fn) in init.lua to register hooks.".to_string()
        };
        drop(eng);
        CommandResult::Handled(vec![
            UiCommand::AppendText(msg),
            UiCommand::SetMode(AppMode::Idle),
        ])
    }

    /// Handle /search command.
    async fn handle_search_cmd(&self, query: &str) -> CommandResult {
        let query = query.trim();
        let all_index = crate::engine::session::load_session_index();
        let active_identity = {
            let eng = self.engine.lock().await;
            eng.active_identity.as_deref().map(|s| s.to_string())
        };
        let index = crate::engine::session::filter_index_by_identity(
            &all_index,
            active_identity.as_deref(),
        );

        if index.is_empty() {
            CommandResult::Handled(vec![
                UiCommand::AppendText("No saved sessions to search.".to_string()),
                UiCommand::SetMode(AppMode::Idle),
            ])
        } else if query.is_empty() {
            let list = crate::engine::session::format_session_list(&index);
            CommandResult::Handled(vec![
                UiCommand::AppendText(format!("Usage: `/search [query]`\n\n{}", list)),
                UiCommand::SetMode(AppMode::Idle),
            ])
        } else {
            let settings = self.settings.clone();
            let sender = self.engine_event_tx.clone();
            let query_owned = query.to_string();
            let index_owned = index;
            tokio::spawn(async move {
                let _ = sender.send(EngineEvent::Status("Searching sessions...".into()));
                match crate::auxiliary::session_search::search_sessions(
                    &settings,
                    &query_owned,
                    &index_owned,
                )
                .await
                {
                    Ok(result) => {
                        let mut output =
                            format!("### Session Search: {}\n\n{}", query_owned, result);
                        output.push_str("\n\nUse `/restore N` to load a session.");
                        let _ = sender.send(EngineEvent::TextDelta(output));
                        let _ = sender.send(EngineEvent::QueryDone {
                            text: String::new(),
                            tool_calls: 0,
                            tokens: 0,
                        });
                    }
                    Err(e) => {
                        let _ =
                            sender.send(EngineEvent::TextDelta(format!("Search failed: {}", e)));
                        let _ = sender.send(EngineEvent::QueryDone {
                            text: String::new(),
                            tool_calls: 0,
                            tokens: 0,
                        });
                    }
                }
            });
            CommandResult::Handled(vec![]) // Async task sends events directly
        }
    }

    /// Handle /goal command.
    async fn handle_goal_cmd(&self, arg: &str) -> CommandResult {
        let mut eng = self.engine.lock().await;
        let msg = if arg.is_empty() || arg == "status" {
            if eng.carryover.has_pending_goal() {
                format!(" Goal (active): {}", eng.carryover.task_focus.goal)
            } else {
                "No active goal. Use /goal <text> to set one.".to_string()
            }
        } else if arg == "clear" || arg == "stop" {
            let had = eng.carryover.has_pending_goal();
            eng.carryover.clear_goal();
            if had {
                " Goal cleared.".to_string()
            } else {
                "No active goal.".to_string()
            }
        } else if arg == "pause" {
            if eng.carryover.has_pending_goal() {
                let goal_text = eng.carryover.task_focus.goal.clone();
                eng.carryover.clear_goal();
                format!("⏸ Goal paused: {}", goal_text)
            } else {
                "No active goal to pause.".to_string()
            }
        } else if arg == "resume" {
            let last_goal = eng.carryover.task_focus.recent_goals.last().cloned();
            if let Some(last) = last_goal {
                eng.carryover.set_goal(&last);
                format!("▶ Goal resumed: {}", last)
            } else {
                "No recent goal to resume.".to_string()
            }
        } else {
            eng.carryover.set_goal(arg);
            format!(" Goal set: {}", arg)
        };
        drop(eng);
        CommandResult::Handled(vec![
            UiCommand::AppendText(msg),
            UiCommand::SetMode(AppMode::Idle),
        ])
    }

    /// Handle /identity command.
    async fn handle_identity_cmd(&mut self, arg: &str) -> CommandResult {
        if arg.is_empty() || arg == "status" {
            let eng = self.engine.lock().await;
            let msg = match &eng.active_identity {
                Some(name) => format!("Identity: {} (active)", name),
                None => "No active identity (using default role).".to_string(),
            };
            drop(eng);
            CommandResult::Handled(vec![
                UiCommand::AppendText(msg),
                UiCommand::SetMode(AppMode::Idle),
            ])
        } else if arg == "none" || arg == "clear" {
            let memory_store = {
                let mgr = self.memory_manager.lock().await;
                mgr.memory_store().clone()
            };
            let mut eng = self.engine.lock().await;
            eng.active_identity = None;
            rebuild_after_identity_change(
                &mut eng,
                &self.skill_registry,
                &memory_store,
                &self.settings,
                None,
                None,
            )
            .await;
            let skill_count = self.skill_registry.lock().await.len();
            drop(eng);
            CommandResult::Handled(vec![
                UiCommand::UpdateStatus(StatusInfo {
                    active_identity: None,
                    mcp_server_count: self.settings.mcp.servers.len(),
                    skill_count,
                    ..self.default_status_info()
                }),
                UiCommand::SetInputIdentity(None),
                UiCommand::AppendText("Identity cleared — using default role.".to_string()),
                UiCommand::SetMode(AppMode::Idle),
            ])
        } else {
            let identity_config = self.settings.identities.get(arg).cloned();
            if let Some(identity_config) = identity_config {
                let memory_store = {
                    let mgr = self.memory_manager.lock().await;
                    mgr.memory_store().clone()
                };
                let mut eng = self.engine.lock().await;
                eng.active_identity = Some(arg.to_string());
                rebuild_after_identity_change(
                    &mut eng,
                    &self.skill_registry,
                    &memory_store,
                    &self.settings,
                    Some(&identity_config),
                    Some(arg),
                )
                .await;
                drop(eng);
                CommandResult::Handled(vec![
                    UiCommand::UpdateStatus(StatusInfo {
                        active_identity: Some(arg.to_string()),
                        mcp_server_count: self.settings.mcp.servers.len(),
                        skill_count: self.skill_registry.lock().await.len(),
                        ..self.default_status_info()
                    }),
                    UiCommand::SetInputIdentity(Some(arg.to_string())),
                    UiCommand::AppendText(format!("Switched to identity: {}", arg)),
                    UiCommand::SetMode(AppMode::Idle),
                ])
            } else {
                CommandResult::Handled(vec![
                    UiCommand::AppendText(format!(
                        "Unknown identity: '{}'. Available: {}",
                        arg,
                        if self.settings.identities.is_empty() {
                            "(none defined — use zn.def_identity() in init.lua)".to_string()
                        } else {
                            self.settings
                                .identities
                                .keys()
                                .cloned()
                                .collect::<Vec<_>>()
                                .join(", ")
                        }
                    )),
                    UiCommand::SetMode(AppMode::Idle),
                ])
            }
        }
    }

    /// Build a default StatusInfo for UiCommand::UpdateStatus.
    fn default_status_info(&self) -> StatusInfo {
        StatusInfo {
            model: self.model_name.clone(),
            provider: self.provider_name.clone(),
            total_tokens: 0,
            context_window: 0,
            turn_count: 0,
            mcp_server_count: 0,
            skill_count: 0,
            mode: AppMode::Idle,
            steer_count: 0,
            active_identity: None,
            tick: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Help text (constant to avoid re-allocation every call).
pub const HELP_TEXT: &str = "\
## Available Commands

**Session**
- `/help` — Show this help
- `/exit` `/quit` — Exit
- `/clear` — Clear history
- `/compact` — Compress history
- `/cost` — Token usage
- `/model` — Current model

**Inspect**
- `/tools` — List builtin tools
- `/mcp` — List MCP servers and tools
- `/skills` — List loaded skills
- `/memory` — Memory files
- `/hooks` — List hooks

**History**
- `/restore` — Restore the last session
- `/restore N` — Restore session #N
- `/search` — Search past sessions
- `/search [query]` — Search by topic

**Goal**
- `/goal [text]` — Set goal
- `/goal clear` — Clear goal
- `/goal pause` — Pause goal
- `/goal resume` — Resume goal

**Identity**
- `/identity` — Show current identity
- `/identity [name]` — Switch to named identity
- `/identity clear` — Clear identity (use default role)
- `/identity none` — Same as clear

## Navigation

- `/` — History (input) or scroll (output)
- `PgUp` `PgDn` — Scroll output
- `Mouse wheel` — Scroll output
- `Shift+drag` — Select and copy text
- `Ctrl+C` — Clear input / Interrupt
- `Ctrl+D` — Hard quit (immediate)
";

/// Rebuild system prompt and reload memory store after identity change.
async fn rebuild_after_identity_change(
    eng: &mut QueryEngine,
    skill_registry: &Arc<tokio::sync::Mutex<crate::skills::registry::SkillRegistry>>,
    memory_store: &Arc<tokio::sync::Mutex<crate::memory::store::MemoryStore>>,
    settings: &crate::config::settings::Settings,
    identity_config: Option<&crate::config::settings::IdentityConfig>,
    identity_name: Option<&str>,
) {
    let memory_prompt = eng
        .memory_manager
        .as_ref()
        .map(|mm| {
            let mm_guard = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(mm.lock())
            });
            mm_guard.build_system_prompt()
        })
        .unwrap_or_default();

    let skill_reg = skill_registry.lock().await;
    let new_prompt = crate::prompts::system_prompt::build(
        &eng.cwd,
        &eng.tools,
        &skill_reg,
        Some(&memory_prompt),
        &eng.settings.role,
        identity_config,
    );
    drop(skill_reg);
    eng.system_prompt = new_prompt;

    let mem_dir = crate::config::paths::memory_dir_for_identity(identity_name);
    let mut store = memory_store.lock().await;
    *store = crate::memory::store::MemoryStore::new(
        mem_dir.join("MEMORY.md"),
        mem_dir.join("USER.md"),
        settings.memory.memory_char_limit,
        settings.memory.user_char_limit,
    );
    store.load_from_disk();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use std::pin::Pin;

    use async_trait::async_trait;
    use futures::Stream;
    use indexmap::IndexMap;
    use tempfile::TempDir;

    use super::*;
    use crate::api::client::SupportsStreamingMessages;
    use crate::api::types::{ApiError, Message, StreamEvent};
    use crate::config::settings::Settings;
    use crate::engine::messages::ConversationHistory;
    use crate::engine::query_engine::QueryEngine;
    use crate::hooks::executor::HookExecutor;
    use crate::mcp::manager::McpManager;
    use crate::memory::manager::MemoryManager;
    use crate::memory::store::MemoryStore;
    use crate::skills::registry::SkillRegistry;
    use crate::skills::types::{CategoryInfo, SkillDefinition};
    use crate::tools::base::ToolRegistry;

    /// A client that panics if called — CommandRouter handlers never stream.
    struct DummyClient;

    #[async_trait]
    impl SupportsStreamingMessages for DummyClient {
        async fn stream_messages(
            &self,
            _model: &str,
            _system: &str,
            _messages: &[Message],
            _tools: &[serde_json::Value],
            _max_tokens: Option<u32>,
            _response_format: Option<&serde_json::Value>,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ApiError>> + Send>>, ApiError>
        {
            panic!("DummyClient should not be called in CommandRouter tests");
        }
    }

    /// Build a minimal CommandRouter for testing.
    async fn test_router() -> (CommandRouter, TempDir) {
        let tmp = TempDir::new().unwrap();
        let settings = Arc::new(Settings::default());

        // Engine
        let engine = Arc::new(tokio::sync::Mutex::new(QueryEngine::new(
            Box::new(DummyClient),
            "claude-sonnet-4".into(),
            "You are a helpful assistant.".into(),
            ConversationHistory::new(),
            Arc::new(ToolRegistry::new()),
            100,
            4096,
            Default::default(),
            settings.clone(),
            tmp.path().to_path_buf(),
        )));

        // Memory store + manager
        let mem_store = Arc::new(tokio::sync::Mutex::new(MemoryStore::new(
            tmp.path().join("MEMORY.md"),
            tmp.path().join("USER.md"),
            2000,
            1000,
        )));
        let memory_manager = Arc::new(tokio::sync::Mutex::new(MemoryManager::new(mem_store)));

        // Skill registry
        let skill_registry = Arc::new(tokio::sync::Mutex::new(SkillRegistry::new()));

        // MCP manager
        let mcp_manager = Arc::new(tokio::sync::Mutex::new(McpManager::from_config(
            &Default::default(),
        )));

        // Event channel
        let (tx, _rx) = mpsc::unbounded_channel();

        let router = CommandRouter::new(
            engine,
            settings,
            memory_manager,
            skill_registry,
            mcp_manager,
            tx,
            "TestProvider".into(),
            "claude-sonnet-4".into(),
            vec!["read".into(), "write".into(), "edit".into(), "bash".into()],
        );

        (router, tmp)
    }

    // ── Static commands ────────────────────────────────────────────────

    #[tokio::test]
    async fn test_help() {
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("/help").await;
        match result {
            CommandResult::Handled(cmds) => {
                assert_eq!(cmds.len(), 2);
                assert!(matches!(cmds[0], UiCommand::AppendText(_)));
                assert!(matches!(cmds[1], UiCommand::SetMode(AppMode::Idle)));
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert_eq!(text, HELP_TEXT);
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    #[tokio::test]
    async fn test_tools() {
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("/tools").await;
        match result {
            CommandResult::Handled(cmds) => {
                assert_eq!(cmds.len(), 2);
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert!(text.starts_with("Tools (4)"));
                    assert!(text.contains("read"));
                    assert!(text.contains("bash"));
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    #[tokio::test]
    async fn test_exit_and_quit() {
        let (mut r, _tmp) = test_router().await;
        for cmd in &["/exit", "/quit"] {
            let result = r.dispatch(cmd).await;
            match result {
                CommandResult::Handled(cmds) => assert!(cmds.is_empty()),
                _ => panic!("expected Handled for {cmd}"),
            }
        }
    }

    #[tokio::test]
    async fn test_unknown_command() {
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("/foobar").await;
        assert!(matches!(result, CommandResult::PassThrough));
    }

    #[tokio::test]
    async fn test_not_a_slash() {
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("hello world").await;
        // Unknown commands (non-slash go through handle_input, not dispatch)
        // But dispatch itself only matches on '/'
        assert!(matches!(result, CommandResult::PassThrough));
    }

    // ── /clear ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_clear_empty() {
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("/clear").await;
        match result {
            CommandResult::Handled(cmds) => {
                assert!(cmds.len() >= 2);
                assert!(matches!(cmds[0], UiCommand::ClearOutput));
                if let UiCommand::AppendText(ref text) = cmds[1] {
                    assert_eq!(text, "Cleared 0 entries.");
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    #[tokio::test]
    async fn test_clear_with_history() {
        let (mut r, _tmp) = test_router().await;
        // Push some history
        {
            let mut eng = r.engine.lock().await;
            eng.history.push_user("Hello");
            eng.history
                .push_assistant_blocks(vec![crate::api::types::ContentBlock::Text {
                    text: "Hi there!".into(),
                }]);
            eng.history.push_user("How are you?");
        }
        let result = r.dispatch("/clear").await;
        match result {
            CommandResult::Handled(cmds) => {
                if let UiCommand::AppendText(ref text) = cmds[1] {
                    assert_eq!(text, "Cleared 3 entries.");
                }
            }
            _ => panic!("expected Handled"),
        }
        // Verify engine history is empty
        let eng = r.engine.lock().await;
        assert_eq!(eng.history.len(), 0);
    }

    // ── /cost ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_cost_empty() {
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("/cost").await;
        match result {
            CommandResult::Handled(cmds) => {
                assert_eq!(cmds.len(), 2);
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert!(text.contains("Token usage"));
                    assert!(text.contains("0 total"));
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    #[tokio::test]
    async fn test_cost_with_tokens() {
        let (mut r, _tmp) = test_router().await;
        {
            let mut eng = r.engine.lock().await;
            eng.cost_tracker.record(
                "claude-sonnet-4",
                &crate::api::types::Usage {
                    input_tokens: 100,
                    output_tokens: 50,
                    cache_read_input_tokens: 5,
                    cache_creation_input_tokens: 0,
                    reasoning_tokens: 0,
                },
            );
        }
        let result = r.dispatch("/cost").await;
        match result {
            CommandResult::Handled(cmds) => {
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert!(text.contains("155 total"));
                    assert!(text.contains("100 input"));
                    assert!(text.contains("50 output"));
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    // ── /model ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_model_get() {
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("/model").await;
        match result {
            CommandResult::Handled(cmds) => {
                assert_eq!(cmds.len(), 2);
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert!(text.contains("claude-sonnet-4"));
                    assert!(text.contains("TestProvider"));
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    #[tokio::test]
    async fn test_model_set() {
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("/model claude-3-haiku").await;
        match result {
            CommandResult::Handled(cmds) => {
                assert_eq!(cmds.len(), 3);
                assert!(matches!(cmds[0], UiCommand::SetModel(ref m) if m == "claude-3-haiku"));
                if let UiCommand::AppendText(ref text) = cmds[1] {
                    assert!(text.contains("claude-3-haiku"));
                }
            }
            _ => panic!("expected Handled"),
        }
        // Verify model_name was updated on router
        assert_eq!(r.model_name, "claude-3-haiku");
        // Verify engine model was updated
        let eng = r.engine.lock().await;
        assert_eq!(eng.model, "claude-3-haiku");
    }

    // ── /goal ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_goal_status_no_goal() {
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("/goal").await;
        match result {
            CommandResult::Handled(cmds) => {
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert!(text.contains("No active goal"));
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    #[tokio::test]
    async fn test_goal_set_and_status() {
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("/goal implement login").await;
        match result {
            CommandResult::Handled(cmds) => {
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert!(text.contains("Goal set"));
                    assert!(text.contains("implement login"));
                }
            }
            _ => panic!("expected Handled"),
        }

        // Check status now shows active goal
        let result = r.dispatch("/goal status").await;
        match result {
            CommandResult::Handled(cmds) => {
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert!(text.contains("implement login"));
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    #[tokio::test]
    async fn test_goal_clear() {
        let (mut r, _tmp) = test_router().await;
        r.dispatch("/goal implement login").await;
        let result = r.dispatch("/goal clear").await;
        match result {
            CommandResult::Handled(cmds) => {
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert!(text.contains("cleared") || text.contains("Cleared"));
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    #[tokio::test]
    async fn test_goal_clear_when_none() {
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("/goal clear").await;
        match result {
            CommandResult::Handled(cmds) => {
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert!(text.contains("No active goal"));
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    #[tokio::test]
    async fn test_goal_pause_resume() {
        let (mut r, _tmp) = test_router().await;
        r.dispatch("/goal implement login").await;

        // Pause
        let result = r.dispatch("/goal pause").await;
        match result {
            CommandResult::Handled(cmds) => {
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert!(text.contains("paused") || text.contains("Paused"));
                    assert!(text.contains("implement login"));
                }
            }
            _ => panic!("expected Handled"),
        }

        // Resume
        let result = r.dispatch("/goal resume").await;
        match result {
            CommandResult::Handled(cmds) => {
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert!(text.contains("resumed") || text.contains("Resumed"));
                    assert!(text.contains("implement login"));
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    #[tokio::test]
    async fn test_goal_pause_when_none() {
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("/goal pause").await;
        match result {
            CommandResult::Handled(cmds) => {
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert!(text.contains("No active goal to pause"));
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    #[tokio::test]
    async fn test_goal_resume_no_recent() {
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("/goal resume").await;
        match result {
            CommandResult::Handled(cmds) => {
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert!(text.contains("No recent goal to resume"));
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    // ── /hooks ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_hooks_no_executor() {
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("/hooks").await;
        match result {
            CommandResult::Handled(cmds) => {
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert!(text.contains("No hooks registered"));
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    #[tokio::test]
    async fn test_hooks_with_executor() {
        let (mut r, _tmp) = test_router().await;
        // Wire up a hook executor with a registered event.
        {
            let mut eng = r.engine.lock().await;
            let lua = Arc::new(std::sync::Mutex::new(mlua::Lua::new()));
            let func = lua.lock().unwrap().create_function(|_, ()| Ok(())).unwrap();
            let mut executor = HookExecutor::new(lua);
            executor
                .register(crate::hooks::types::HookEvent::PreToolUse, func)
                .unwrap();
            eng.hook_executor = Some(executor);
        }
        let result = r.dispatch("/hooks").await;
        match result {
            CommandResult::Handled(cmds) => {
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert!(text.contains("Registered hooks"));
                    assert!(text.contains("pre_tool_use"));
                    assert!(text.contains("1 handler(s)"));
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    // ── /memory ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_memory() {
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("/memory").await;
        match result {
            CommandResult::Handled(cmds) => {
                assert_eq!(cmds.len(), 2);
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert!(text.contains("MEMORY.md"));
                    assert!(text.contains("USER.md"));
                    assert!(text.contains("0 entries"));
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    // ── /mcp ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_mcp_empty() {
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("/mcp").await;
        match result {
            CommandResult::Handled(cmds) => {
                assert_eq!(cmds.len(), 2);
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    // Empty MCP manager should produce some summary
                    assert!(!text.is_empty());
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    // ── /skills ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_skills_empty() {
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("/skills").await;
        match result {
            CommandResult::Handled(cmds) => {
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert!(text.contains("Skills (0)"));
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    #[tokio::test]
    async fn test_skills_with_entries() {
        let (mut r, _tmp) = test_router().await;
        // Register some skills with categories
        {
            let mut reg = r.skill_registry.lock().await;
            let categories = IndexMap::from([
                (
                    "software-development".into(),
                    CategoryInfo {
                        description: "Coding skills".into(),
                        skill_names: vec!["code-review".into(), "git-commit".into()],
                    },
                ),
                (
                    "research".into(),
                    CategoryInfo {
                        description: "Research skills".into(),
                        skill_names: vec!["llm-wiki".into()],
                    },
                ),
            ]);
            let skills = vec![
                SkillDefinition {
                    name: "code-review".into(),
                    description: "Review code rigorously".into(),
                    content: "content".into(),
                    source: "bundled".into(),
                    path: None,
                    category: "software-development".into(),
                },
                SkillDefinition {
                    name: "git-commit".into(),
                    description: "Write commit messages".into(),
                    content: "content".into(),
                    source: "bundled".into(),
                    path: None,
                    category: "software-development".into(),
                },
                SkillDefinition {
                    name: "llm-wiki".into(),
                    description: "LLM knowledge base".into(),
                    content: "content".into(),
                    source: "bundled".into(),
                    path: None,
                    category: "research".into(),
                },
            ];
            *reg = SkillRegistry::from_parts(skills, categories);
        }
        let result = r.dispatch("/skills").await;
        match result {
            CommandResult::Handled(cmds) => {
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert!(text.contains("3 skills"));
                    assert!(text.contains("software-development"));
                    assert!(text.contains("code-review"));
                    assert!(text.contains("git-commit"));
                    assert!(text.contains("research"));
                    assert!(text.contains("llm-wiki"));
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    // ── /identity ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_identity_no_identity() {
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("/identity").await;
        match result {
            CommandResult::Handled(cmds) => {
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert!(text.contains("No active identity"));
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    #[tokio::test]
    async fn test_identity_status_with_active() {
        let (mut r, _tmp) = test_router().await;
        {
            let mut eng = r.engine.lock().await;
            eng.active_identity = Some("work".into());
        }
        let result = r.dispatch("/identity").await;
        match result {
            CommandResult::Handled(cmds) => {
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert!(text.contains("work"));
                    assert!(text.contains("active"));
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    #[tokio::test]
    async fn test_identity_unknown() {
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("/identity nonexistent").await;
        match result {
            CommandResult::Handled(cmds) => {
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert!(text.contains("Unknown identity"));
                    assert!(text.contains("nonexistent"));
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    // ── /restore — basic edge cases ────────────────────────────────────

    #[tokio::test]
    async fn test_restore_no_session() {
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("/restore").await;
        match result {
            CommandResult::Handled(cmds) => {
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert!(text.contains("No saved session"));
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    #[tokio::test]
    async fn test_restore_out_of_range() {
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("/restore 999").await;
        match result {
            CommandResult::Handled(cmds) => {
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert!(text.contains("Invalid session number"));
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    #[tokio::test]
    async fn test_restore_zero_invalid() {
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("/restore 0").await;
        match result {
            CommandResult::Handled(cmds) => {
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert!(text.contains("Invalid session number"));
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    #[tokio::test]
    async fn test_restore_list_format() {
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("/restore list").await;
        match result {
            CommandResult::Handled(cmds) => {
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert!(!text.is_empty());
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    // ── /search — basic edge cases ─────────────────────────────────────

    #[tokio::test]
    async fn test_search_no_sessions() {
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("/search").await;
        match result {
            CommandResult::Handled(cmds) => {
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    // If no sessions exist: shows "No saved sessions to search."
                    // If sessions exist: shows "Usage: /search [query]" with session list
                    // Accept both — test depends on the real environment.
                    assert!(
                        text.contains("No saved sessions") || text.contains("Usage:"),
                        "unexpected text: {text}"
                    );
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    #[tokio::test]
    async fn test_search_with_query_empty_index() {
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("/search hello").await;
        match result {
            CommandResult::Handled(cmds) => {
                // If index is empty: returns text "No saved sessions to search."
                // If index has entries: spawns async search, returns empty vec initially
                if cmds.is_empty() {
                    // Spawned async search — sessions exist on disk
                    return;
                }
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert!(text.contains("No saved sessions"));
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    // ── /compact ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_compact_spawns_async() {
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("/compact").await;
        match result {
            CommandResult::Handled(cmds) => {
                // /compact spawns an async compression task and returns
                // an empty vec immediately. The task sends events via channel.
                assert!(cmds.is_empty());
            }
            _ => panic!("expected Handled"),
        }
    }

    // ── /model edge cases ────────────────────────────────────────────────

    #[tokio::test]
    async fn test_model_get_with_whitespace() {
        let (mut r, _tmp) = test_router().await;
        // "/model " with trailing space should behave as get, not set
        let result = r.dispatch("/model ").await;
        match result {
            CommandResult::Handled(cmds) => {
                assert_eq!(cmds.len(), 2);
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert!(
                        text.contains("claude-sonnet-4"),
                        "should show current model, got: {text}"
                    );
                }
            }
            _ => panic!("expected Handled"),
        }
        // Model name should NOT have changed
        assert_eq!(r.model_name, "claude-sonnet-4");
    }

    // ── /tools structural ────────────────────────────────────────────────

    #[tokio::test]
    async fn test_tools_format() {
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("/tools").await;
        match result {
            CommandResult::Handled(cmds) => {
                assert_eq!(cmds.len(), 2);
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert!(text.contains("Tools"));
                    assert!(text.contains("read"));
                    assert!(text.contains("write"));
                    assert!(text.contains("bash"));
                    // Should show the count, e.g. "Tools (4)"
                    let first_line = text.lines().next().unwrap_or("");
                    assert!(
                        first_line.contains('(') && first_line.contains(')'),
                        "should show tool count in parens: {first_line}"
                    );
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    // ── /help structural ─────────────────────────────────────────────────

    #[tokio::test]
    async fn test_help_includes_sections() {
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("/help").await;
        match result {
            CommandResult::Handled(cmds) => {
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert!(text.contains("/help"));
                    assert!(text.contains("/search"));
                    assert!(text.contains("/restore"));
                    assert!(text.contains("/model"));
                    // Should contain section headers
                    let lower = text.to_lowercase();
                    assert!(lower.contains("commands"), "help should list commands");
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    // ── /search edge cases ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_search_returns_handled() {
        // Verify /search always returns Handled regardless of index state
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("/search").await;
        assert!(
            matches!(result, CommandResult::Handled(_)),
            "expected Handled"
        );

        let result2 = r.dispatch("/search something").await;
        assert!(
            matches!(result2, CommandResult::Handled(_)),
            "expected Handled"
        );
    }

    // ── /restore edge cases ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_restore_non_numeric_arg() {
        let (mut r, _tmp) = test_router().await;
        // Non-numeric arg like "help" should list sessions
        let result = r.dispatch("/restore help").await;
        match result {
            CommandResult::Handled(cmds) => {
                assert!(!cmds.is_empty(), "should return session list or message");
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    let lower = text.to_lowercase();
                    // Either shows session list or "No saved session"
                    assert!(
                        lower.contains("saved") || lower.contains("invalid"),
                        "unexpected: {text}"
                    );
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    #[tokio::test]
    async fn test_restore_zero_returns_error() {
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("/restore 0").await;
        match result {
            CommandResult::Handled(cmds) => {
                if let UiCommand::AppendText(ref text) = cmds[0] {
                    assert!(text.contains("Invalid session number: 0"), "got: {text}");
                }
            }
            _ => panic!("expected Handled"),
        }
    }

    // ── Dispatch passthrough ─────────────────────────────────────────────

    #[tokio::test]
    async fn test_dispatch_unknown_is_passthrough() {
        let (mut r, _tmp) = test_router().await;
        let result = r.dispatch("/nonexistent").await;
        assert!(
            matches!(result, CommandResult::PassThrough),
            "expected PassThrough for unknown command"
        );
    }

    #[tokio::test]
    async fn test_dispatch_passthrough_for_plain_text() {
        let (mut r, _tmp) = test_router().await;
        // Non-slash text should pass through
        let result = r.dispatch("hello world").await;
        assert!(
            matches!(result, CommandResult::PassThrough),
            "expected PassThrough for plain text"
        );
    }
}
