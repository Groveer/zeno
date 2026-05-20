//! Slash command handler.
//!
//! Handles all `/` commands (e.g., /help, /model, /cost, /clear, etc.)
//! that were previously scattered in main.rs.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::engine::messages::ConversationHistory;
use crate::engine::query_engine::QueryEngine;
use crate::engine::tui_events::EngineEvent;
use crate::gateway::{Gateway, UiCommand};
use crate::ui::status_bar::{AppMode, StatusInfo};

impl Gateway {
    /// Dispatch a slash command.
    ///
    /// Called from `handle_input()` when input starts with '/'.
    /// Handles all slash commands that were previously in main.rs.
    pub async fn dispatch_slash(&mut self, input: &str) {
        match input {
            "/help" => {
                emit_text_response(self, HELP_TEXT);
            }
            "/exit" | "/quit" => {
                // These are handled by App.handle_key() before reaching Gateway
            }
            "/tools" => {
                emit_text_response(
                    self,
                    &format!(
                        "Tools ({})\n{}",
                        self.builtin_tool_count,
                        self.builtin_tool_names.join(", ")
                    ),
                );
            }
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
                emit_text_response(self, &summary);
            }
            "/mcp" => {
                let summary = {
                    let mgr = self.mcp_manager.lock().await;
                    mgr.summary()
                };
                emit_text_response(self, &summary);
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
                emit_text_response(self, &lines.join("\n"));
            }
            "/cost" => {
                self.handle_cost_cmd().await;
            }
            "/clear" => {
                self.handle_clear_cmd().await;
            }
            "/compact" => {
                self.handle_compact_cmd().await;
            }
            s if s.starts_with("/restore") => {
                let sender = self.engine_event_tx.clone();
                let arg = s.strip_prefix("/restore").unwrap_or("").trim();
                self.handle_restore_cmd(arg, &sender).await;
            }
            s if s.starts_with("/model") => {
                let arg = s.strip_prefix("/model").unwrap_or("").trim();
                if !arg.is_empty() {
                    let mut eng = self.engine.lock().await;
                    eng.set_model(arg.to_string());
                    drop(eng);
                    self.model_name = arg.to_string();
                    self.emit(UiCommand::SetModel(arg.to_string()));
                    emit_text_response(self, &format!("Model temporarily set to: {}", arg));
                } else {
                    emit_text_response(
                        self,
                        &format!(
                            "Model: {} (provider: {})",
                            self.model_name, self.provider_name
                        ),
                    );
                }
            }
            "/hooks" => {
                self.handle_hooks_cmd().await;
            }
            s if s.starts_with("/search") => {
                let sender = self.engine_event_tx.clone();
                let arg = s.strip_prefix("/search").unwrap_or("").trim();
                self.handle_search_cmd(arg, &sender).await;
            }
            s if s.starts_with("/goal") => {
                let arg = s.strip_prefix("/goal").unwrap_or("").trim();
                let mut eng = self.engine.lock().await;
                let msg = handle_goal(&mut eng, arg);
                drop(eng);
                emit_text_response(self, &msg);
            }
            s if s.starts_with("/identity") => {
                let arg = s.strip_prefix("/identity").unwrap_or("").trim();
                self.handle_identity_cmd(arg).await;
            }
            _ => {
                // Unknown slash command — treat as LLM query
                self.submit_query(input, vec![], tokio_util::sync::CancellationToken::new())
                    .await;
            }
        }
    }

    /// Handle /cost command.
    async fn handle_cost_cmd(&self) {
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
        emit_text_response(self, &msg);
    }

    /// Handle /clear command.
    async fn handle_clear_cmd(&self) {
        let mut eng = self.engine.lock().await;
        let count = eng.history.len();
        eng.history.clear();
        drop(eng);
        self.emit(UiCommand::ClearOutput);
        emit_text_response(self, &format!("Cleared {} entries.", count));
    }

    /// Handle /compact command.
    async fn handle_compact_cmd(&self) {
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
    }

    /// Handle /restore command.
    async fn handle_restore_cmd(&self, arg: &str, sender: &mpsc::UnboundedSender<EngineEvent>) {
        // Get active identity for session filtering
        let eng = self.engine.lock().await;
        let active_identity = eng.active_identity.as_deref().map(|s| s.to_string());
        drop(eng);

        let session_data = {
            if arg.trim().is_empty() {
                // Restore latest session matching current identity
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
                    emit_text_response(
                        self,
                        &format!(
                            "Invalid session number: {}. {}\n\n{}",
                            n,
                            if filtered.is_empty() {
                                "No sessions available."
                            } else {
                                ""
                            },
                            list,
                        ),
                    );
                    None
                } else {
                    crate::engine::session::load_session_by_id(&filtered[n - 1].id)
                }
            } else {
                let index = crate::engine::session::load_session_index();
                let filtered = crate::engine::session::filter_index_by_identity(
                    &index,
                    active_identity.as_deref(),
                );
                let list = crate::engine::session::format_session_list(&filtered);
                emit_text_response(self, &list);
                None
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

            // Notify external memory provider of session switch
            if let Some(ref mm) = eng.memory_manager {
                let mm = mm.lock().await;
                mm.on_session_switch(&new_session_id, "", false);
            }
            drop(eng);

            let _ = sender.send(EngineEvent::ClearOutput);
            let _ = sender.send(EngineEvent::Status(format!(" Resumed: {}", one_liner)));
            let _ = sender.send(EngineEvent::TextDelta(format!(
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
            let _ = sender.send(EngineEvent::QueryDone {
                text: String::new(),
                tool_calls: 0,
                tokens: data.total_tokens,
            });
        } else if arg.trim().is_empty() {
            // No session data and no argument
            emit_text_response(self, "No saved session to resume.");
        }
    }

    /// Handle /hooks command.
    async fn handle_hooks_cmd(&self) {
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
        emit_text_response(self, &msg);
    }

    /// Handle /search command.
    async fn handle_search_cmd(&self, query: &str, sender: &mpsc::UnboundedSender<EngineEvent>) {
        let query = query.trim();
        let all_index = crate::engine::session::load_session_index();
        // Filter by active identity
        let eng = self.engine.lock().await;
        let active_identity = eng.active_identity.as_deref().map(|s| s.to_string());
        drop(eng);
        let index = crate::engine::session::filter_index_by_identity(
            &all_index,
            active_identity.as_deref(),
        );

        if index.is_empty() {
            emit_text_response(self, "No saved sessions to search.");
        } else if query.is_empty() {
            let list = crate::engine::session::format_session_list(&index);
            emit_text_response(self, &format!("Usage: `/search [query]`\n\n{}", list));
        } else {
            let settings = self.settings.clone();
            let sender2 = sender.clone();
            let query_owned = query.to_string();
            let index_owned = index; // already filtered
            tokio::spawn(async move {
                let _ = sender2.send(EngineEvent::Status("Searching sessions...".into()));
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
                        let _ = sender2.send(EngineEvent::TextDelta(output));
                        let _ = sender2.send(EngineEvent::QueryDone {
                            text: String::new(),
                            tool_calls: 0,
                            tokens: 0,
                        });
                    }
                    Err(e) => {
                        let _ =
                            sender2.send(EngineEvent::TextDelta(format!("Search failed: {}", e)));
                        let _ = sender2.send(EngineEvent::QueryDone {
                            text: String::new(),
                            tool_calls: 0,
                            tokens: 0,
                        });
                    }
                }
            });
        }
    }

    /// Handle /identity command.
    async fn handle_identity_cmd(&mut self, arg: &str) {
        if arg.is_empty() || arg == "status" {
            let eng = self.engine.lock().await;
            let msg = match &eng.active_identity {
                Some(name) => format!("Identity: {} (active)", name),
                None => "No active identity (using default role).".to_string(),
            };
            drop(eng);
            emit_text_response(self, &msg);
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
            self.emit(UiCommand::UpdateStatus(StatusInfo {
                active_identity: None,
                mcp_server_count: self.settings.mcp.servers.len(),
                skill_count,
                ..self.default_status_info()
            }));
            self.emit(UiCommand::SetInputIdentity(None));
            emit_text_response(self, "Identity cleared — using default role.");
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
                self.emit(UiCommand::UpdateStatus(StatusInfo {
                    active_identity: Some(arg.to_string()),
                    mcp_server_count: self.settings.mcp.servers.len(),
                    skill_count: self.skill_registry.lock().await.len(),
                    ..self.default_status_info()
                }));
                self.emit(UiCommand::SetInputIdentity(Some(arg.to_string())));
                emit_text_response(self, &format!("Switched to identity: {}", arg));
            } else {
                emit_text_response(
                    self,
                    &format!(
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
                    ),
                );
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

/// Send a simple text response directly through the transport (no engine roundtrip).
fn emit_text_response(gw: &Gateway, text: &str) {
    gw.emit(UiCommand::AppendText(text.to_string()));
    gw.emit(UiCommand::SetMode(AppMode::Idle));
}

/// Handle /goal command — requires &mut QueryEngine.
fn handle_goal(eng: &mut QueryEngine, arg: &str) -> String {
    if arg.is_empty() || arg == "status" {
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
    }
}

/// Rebuild system prompt and reload memory store after identity change.
async fn rebuild_after_identity_change(
    eng: &mut QueryEngine,
    skill_registry: &Arc<tokio::sync::Mutex<crate::skills::registry::SkillRegistry>>,
    memory_store: &Arc<tokio::sync::Mutex<crate::memory::store::MemoryStore>>,
    settings: &crate::config::settings::Settings,
    identity_config: Option<&crate::config::settings::IdentityConfig>,
    identity_name: Option<&str>,
) {
    // 1. Build memory prompt
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

    // 2. Rebuild system prompt with optional identity override
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

    // 3. Reload memory store from identity-scoped (or global) paths
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
