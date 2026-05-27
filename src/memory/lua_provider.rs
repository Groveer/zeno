//! Lua-configured memory provider — a generic provider whose behavior is
//! defined by a Lua script.
//!
//! This allows users to integrate any memory backend (Mem0, Honcho, custom
//! HTTP API, etc.) by writing a Lua script instead of Rust.
//!
//! The Lua script must return a table with these fields:
//!
//! ```lua
//! return {
//!     name = "mem0",
//!
//!     -- Check if provider is available (no network calls)
//!     is_available = function()
//!         return os.getenv("MEM0_API_KEY") ~= nil
//!     end,
//!
//!     -- Initialize the provider (called once at startup)
//!     initialize = function(session_id)
//!         _api_key = os.getenv("MEM0_API_KEY")
//!         _base_url = "https://api.mem0.ai/v1"
//!     end,
//!
//!     -- Static system prompt text (optional, string)
//!     system_prompt = "Mem0 memory provider is active.",
//!
//!     -- Tool schemas (array of tables, optional)
//!     tool_schemas = {
//!         {
//!             name = "mem0_search",
//!             description = "Search memories by meaning.",
//!             parameters = {
//!                 type = "object",
//!                 properties = {
//!                     query = { type = "string", description = "What to search for." },
//!                 },
//!                 required = { "query" },
//!             },
//!         },
//!     },
//!
//!     -- Handle tool calls (required if tool_schemas is set)
//!     -- args_json is a JSON string of the tool arguments
//!     handle_tool_call = function(tool_name, args_json)
//!         if tool_name == "mem0_search" then
//!             local args = json.decode(args_json)
//!             -- do something with args.query
//!             return '{"success": true}'
//!         end
//!         return '{"error": "unknown tool"}'
//!     end,
//!
//!     -- Prefetch context before each turn (optional)
//!     prefetch = function(query) return "" end,
//!
//!     -- Queue background prefetch for the next turn (optional)
//!     -- Called after each turn. Pre-fetches context for the next turn.
//!     queue_prefetch = function(query) end,
//!
//!     -- Sync turn after each response (optional)
//!     sync_turn = function(user_content, assistant_content) end,
//!
//!     -- Mirror built-in memory writes (optional)
//!     on_memory_change = function(action, target, content) end,
//!
//!     -- Per-turn tick with turn number and user message (optional)
//!     on_turn_start = function(turn_number, message) end,
//!
//!     -- End-of-session extraction (optional)
//!     -- messages_json is a JSON string of the full conversation history.
//!     -- Use to extract facts, update summaries, flush buffers, etc.
//!     on_session_end = function(messages_json) end,
//!
//!     -- Mid-process session_id rotation (optional)
//!     -- Fires on /restore, /branch, /reset, /new, and context compression.
//!     on_session_switch = function(new_id, parent_id, reset) end,
//!
//!     -- Called before context compression discards old messages (optional)
//!     -- messages_json is a JSON string of messages about to be discarded.
//!     -- Return a string with insights to preserve, or "" for no contribution.
//!     on_pre_compress = function(messages_json) return "" end,
//!
//!     -- Called when a sub-agent completes on the parent agent (optional)
//!     -- Use to observe delegation outcomes, extract facts, etc.
//!     on_delegation = function(task, result, child_session_id) end,
//!
//!     -- Shutdown (optional)
//!     shutdown = function() end,
//! }
//! ```
//!
//! Configuration in init.lua:
//! ```lua
//! -- From a Lua module file (copy hindsight.lua to ~/.config/zeno/lua/):
//! zn.memory_provider("hindsight", require("hindsight"))
//!
//! -- Or inline (for simple providers):
//! zn.memory_provider("simple", {
//!     name = "simple",
//!     system_prompt = "Custom provider active.",
//!     is_available = function() return true end,
//!     initialize = function(sid) end,
//!     handle_tool_call = function(name, args_json)
//!         return '{"success": true}'
//!     end,
//!     tool_schemas = {
//!         {
//!             name = "my_search",
//!             description = "Search my backend.",
//!             parameters = {
//!                 type = "object",
//!                 properties = { query = { type = "string" } },
//!                 required = { "query" },
//!             },
//!         },
//!     },
//! })
//! ```

use async_trait::async_trait;
use mlua::Lua;
use serde_json::Value;
use std::sync::Arc;
use std::sync::Mutex;

use super::provider::{MemoryProvider, ProviderError, ProviderResult};

/// Config for a Lua memory provider, identified by name.
/// The actual provider table lives in the shared Lua VM's registry.
#[derive(Debug, Clone)]
pub struct LuaProviderConfig {
    /// Provider name (e.g. "mem0", "hindsight").
    /// Used to look up the provider table from the registry at `_rc_provider_{name}`.
    pub name: String,
}

/// Static data extracted from the Lua provider script at load time.
#[derive(Debug, Clone, Default)]
struct ProviderStatic {
    system_prompt: String,
    tool_schemas: Vec<Value>,
    available: bool,
}

/// A memory provider backed by a Lua script.
pub struct LuaMemoryProvider {
    config: LuaProviderConfig,
    lua: Arc<Mutex<Lua>>,
    static_data: ProviderStatic,
    initialized: bool,
}

impl LuaMemoryProvider {
    /// Create a new Lua memory provider using a shared Lua VM.
    ///
    /// The provider table is looked up from the VM's registry at
    /// `_rc_provider_{name}`, which was stored there by `zn.memory_provider()`
    /// during config loading.
    pub fn new(config: LuaProviderConfig, lua: Arc<Mutex<Lua>>) -> Result<Self, ProviderError> {
        let static_data = Self::load_provider(&lua, &config)?;

        Ok(Self {
            config,
            lua,
            static_data,
            initialized: false,
        })
    }

    /// Look up the provider table from the registry and extract static data.
    fn load_provider(
        lua: &Arc<Mutex<Lua>>,
        config: &LuaProviderConfig,
    ) -> Result<ProviderStatic, ProviderError> {
        let lua = lua
            .lock()
            .map_err(|e| ProviderError::Config(format!("Failed to lock Lua VM: {}", e)))?;
        let registry_key = format!("_rc_provider_{}", config.name);
        let provider_table: mlua::Table = lua.named_registry_value(&registry_key).map_err(|e| {
            ProviderError::Config(format!(
                "Memory provider '{}' not found in registry: {}",
                config.name, e
            ))
        })?;

        let mut static_data = ProviderStatic::default();

        // Validate name matches
        if let Ok(name) = provider_table.get::<String>("name")
            && name != config.name
        {
            tracing::warn!(
                script_name = %name,
                config_name = %config.name,
                "Memory provider script name doesn't match config name"
            );
        }

        // Extract system_prompt
        if let Ok(s) = provider_table.get::<String>("system_prompt") {
            static_data.system_prompt = s;
        }

        // Extract tool_schemas
        if let Ok(schemas_table) = provider_table.get::<mlua::Table>("tool_schemas") {
            static_data.tool_schemas = extract_tool_schemas(&schemas_table);
        }

        // Check is_available
        if let Ok(is_avail_fn) = provider_table.get::<mlua::Function>("is_available") {
            match is_avail_fn.call::<bool>(()) {
                Ok(v) => static_data.available = v,
                Err(e) => {
                    tracing::warn!(
                        provider = %config.name,
                        error = %e,
                        "Memory provider is_available() failed"
                    );
                    static_data.available = false;
                }
            }
        } else if let Ok(v) = provider_table.get::<bool>("is_available") {
            static_data.available = v;
        } else {
            static_data.available = true;
        }

        // Store the provider table as a global for callback access
        lua.globals()
            .set("_provider", provider_table)
            .map_err(|e| ProviderError::Config(format!("Failed to store provider table: {}", e)))?;

        Ok(static_data)
    }
}

#[async_trait]
impl MemoryProvider for LuaMemoryProvider {
    fn name(&self) -> &str {
        &self.config.name
    }

    fn is_available(&self) -> bool {
        self.static_data.available
    }

    async fn initialize(&mut self, session_id: &str) -> Result<(), ProviderError> {
        let lua = self.lua.lock().unwrap();
        let globals = lua.globals();
        if let Ok(provider) = globals.get::<mlua::Table>("_provider")
            && let Ok(func) = provider.get::<mlua::Function>("initialize")
            && let Err(e) = func.call::<()>(session_id)
        {
            tracing::warn!(
                provider = %self.config.name,
                error = %e,
                "Memory provider initialize() failed"
            );
        }
        self.initialized = true;
        Ok(())
    }

    fn system_prompt_block(&self) -> String {
        self.static_data.system_prompt.clone()
    }

    async fn prefetch(&self, query: &str) -> String {
        let lua = self.lua.lock().unwrap();
        let globals = lua.globals();
        if let Ok(provider) = globals.get::<mlua::Table>("_provider")
            && let Ok(func) = provider.get::<mlua::Function>("prefetch")
        {
            match func.call::<mlua::String>(query) {
                Ok(s) => return s.to_string_lossy().to_string(),
                Err(e) => {
                    tracing::debug!(
                        provider = %self.config.name,
                        error = %e,
                        "Memory provider prefetch failed"
                    );
                }
            }
        }
        String::new()
    }

    async fn sync_turn(&self, user_content: &str, assistant_content: &str) {
        let lua = self.lua.lock().unwrap();
        let globals = lua.globals();
        if let Ok(provider) = globals.get::<mlua::Table>("_provider")
            && let Ok(func) = provider.get::<mlua::Function>("sync_turn")
            && let Err(e) = func.call::<()>((user_content, assistant_content))
        {
            tracing::debug!(
                provider = %self.config.name,
                error = %e,
                "Memory provider sync_turn failed"
            );
        }
    }

    async fn on_session_end(&self, messages: &[Value]) {
        let messages_json = serde_json::to_string(messages).unwrap_or_default();
        let lua = self.lua.lock().unwrap();
        let globals = lua.globals();
        if let Ok(provider) = globals.get::<mlua::Table>("_provider")
            && let Ok(func) = provider.get::<mlua::Function>("on_session_end")
            && let Err(e) = func.call::<()>(messages_json)
        {
            tracing::debug!(
                provider = %self.config.name,
                error = %e,
                "Memory provider on_session_end failed"
            );
        }
    }

    fn get_tool_schemas(&self) -> Vec<Value> {
        self.static_data.tool_schemas.clone()
    }

    async fn handle_tool_call(&self, tool_name: &str, args: &Value) -> ProviderResult {
        let args_json = serde_json::to_string(args).unwrap_or_default();

        let lua = self.lua.lock().unwrap();
        let globals = lua.globals();
        let provider: mlua::Table = globals
            .get("_provider")
            .map_err(|_| ProviderError::Execution("Provider table not found".into()))?;

        let func: mlua::Function = provider.get("handle_tool_call").map_err(|_| {
            ProviderError::ToolNotFound(format!(
                "No handle_tool_call in provider '{}'",
                self.config.name
            ))
        })?;

        match func.call::<mlua::String>((tool_name.to_string(), args_json.clone())) {
            Ok(s) => Ok(s.to_string_lossy().to_string()),
            Err(e) => Err(ProviderError::Execution(format!(
                "Provider '{}' handle_tool_call('{}') failed: {}",
                self.config.name, tool_name, e
            ))),
        }
    }

    async fn shutdown(&mut self) {
        let lua = self.lua.lock().unwrap();
        let globals = lua.globals();
        if let Ok(provider) = globals.get::<mlua::Table>("_provider")
            && let Ok(func) = provider.get::<mlua::Function>("shutdown")
            && let Err(e) = func.call::<()>(())
        {
            tracing::debug!(
                provider = %self.config.name,
                error = %e,
                "Memory provider shutdown failed"
            );
        }
        self.initialized = false;
    }

    fn on_memory_change(&self, action: &str, target: &str, content: &str) {
        // Best-effort synchronous call — skip if VM is busy
        if let Ok(lua) = self.lua.try_lock() {
            let globals = lua.globals();
            if let Ok(provider) = globals.get::<mlua::Table>("_provider")
                && let Ok(func) = provider.get::<mlua::Function>("on_memory_change")
                && let Err(e) = func.call::<()>((action, target, content))
            {
                tracing::debug!(
                    provider = %self.config.name,
                    error = %e,
                    "Memory provider on_memory_change failed"
                );
            }
        }
    }

    fn on_delegation(&self, task: &str, result: &str, child_session_id: &str) {
        // Best-effort synchronous call — skip if VM is busy
        if let Ok(lua) = self.lua.try_lock() {
            let globals = lua.globals();
            if let Ok(provider) = globals.get::<mlua::Table>("_provider")
                && let Ok(func) = provider.get::<mlua::Function>("on_delegation")
                && let Err(e) = func.call::<()>((task, result, child_session_id))
            {
                tracing::debug!(
                    provider = %self.config.name,
                    error = %e,
                    "Memory provider on_delegation failed"
                );
            }
        }
    }

    fn queue_prefetch(&self, query: &str) {
        if let Ok(lua) = self.lua.try_lock() {
            let globals = lua.globals();
            if let Ok(provider) = globals.get::<mlua::Table>("_provider")
                && let Ok(func) = provider.get::<mlua::Function>("queue_prefetch")
                && let Err(e) = func.call::<()>(query)
            {
                tracing::debug!(
                    provider = %self.config.name,
                    error = %e,
                    "Memory provider queue_prefetch failed"
                );
            }
        }
    }

    fn on_turn_start(&self, turn_number: u32, message: &str) {
        if let Ok(lua) = self.lua.try_lock() {
            let globals = lua.globals();
            if let Ok(provider) = globals.get::<mlua::Table>("_provider")
                && let Ok(func) = provider.get::<mlua::Function>("on_turn_start")
                && let Err(e) = func.call::<()>((turn_number as i64, message))
            {
                tracing::debug!(
                    provider = %self.config.name,
                    error = %e,
                    "Memory provider on_turn_start failed"
                );
            }
        }
    }

    fn on_session_switch(&self, new_session_id: &str, parent_session_id: &str, reset: bool) {
        if let Ok(lua) = self.lua.try_lock() {
            let globals = lua.globals();
            if let Ok(provider) = globals.get::<mlua::Table>("_provider")
                && let Ok(func) = provider.get::<mlua::Function>("on_session_switch")
                && let Err(e) = func.call::<()>((new_session_id, parent_session_id, reset))
            {
                tracing::debug!(
                    provider = %self.config.name,
                    error = %e,
                    "Memory provider on_session_switch failed"
                );
            }
        }
    }

    fn on_pre_compress(&self, messages: &[Value]) -> String {
        let messages_json = serde_json::to_string(messages).unwrap_or_default();
        if let Ok(lua) = self.lua.try_lock() {
            let globals = lua.globals();
            if let Ok(provider) = globals.get::<mlua::Table>("_provider")
                && let Ok(func) = provider.get::<mlua::Function>("on_pre_compress")
            {
                match func.call::<mlua::String>(messages_json) {
                    Ok(s) => return s.to_string_lossy().to_string(),
                    Err(e) => {
                        tracing::debug!(
                            provider = %self.config.name,
                            error = %e,
                            "Memory provider on_pre_compress failed"
                        );
                    }
                }
            }
        }
        String::new()
    }
}

// ---------------------------------------------------------------------------
// Lua ↔ JSON conversion helpers
// ---------------------------------------------------------------------------

/// Convert a Lua value to a serde_json::Value.
pub fn lua_value_to_json(val: &mlua::Value) -> Value {
    match val {
        mlua::Value::Nil => Value::Null,
        mlua::Value::Boolean(b) => Value::Bool(*b),
        mlua::Value::Integer(i) => Value::Number((*i).into()),
        mlua::Value::Number(n) => serde_json::Number::from_f64(*n)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        mlua::Value::String(s) => Value::String(s.to_string_lossy().to_string()),
        mlua::Value::Table(t) => {
            // Heuristic: if all keys are sequential integers starting from 1, it's an array
            let mut pairs: Vec<(i64, Value)> = Vec::new();
            let mut map = serde_json::Map::new();
            let mut has_string_keys = false;

            for (k, v) in t.pairs::<mlua::Value, mlua::Value>().flatten() {
                match k {
                    mlua::Value::Integer(i) => {
                        pairs.push((i, lua_value_to_json(&v)));
                    }
                    mlua::Value::String(s) => {
                        has_string_keys = true;
                        map.insert(s.to_string_lossy().to_string(), lua_value_to_json(&v));
                    }
                    _ => {
                        has_string_keys = true;
                    }
                }
            }

            if !has_string_keys && !pairs.is_empty() {
                pairs.sort_by_key(|(k, _)| *k);
                // Check if it's a sequential array starting from 1
                let is_sequential = pairs
                    .iter()
                    .enumerate()
                    .all(|(i, (k, _))| *k == (i as i64 + 1));
                if is_sequential {
                    Value::Array(pairs.into_iter().map(|(_, v)| v).collect())
                } else {
                    // Sparse array — treat as object
                    let mut m = serde_json::Map::new();
                    for (k, v) in pairs {
                        m.insert(k.to_string(), v);
                    }
                    Value::Object(m)
                }
            } else if !map.is_empty() {
                Value::Object(map)
            } else {
                Value::Array(Vec::new())
            }
        }
        _ => Value::Null,
    }
}

/// Convert a serde_json::Value to a Lua value.
pub fn json_to_lua_value(lua: &Lua, val: &Value) -> mlua::Value {
    match val {
        Value::Null => mlua::Value::Nil,
        Value::Bool(b) => mlua::Value::Boolean(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                mlua::Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                mlua::Value::Number(f)
            } else {
                mlua::Value::Nil
            }
        }
        Value::String(s) => lua
            .create_string(s)
            .map(mlua::Value::String)
            .unwrap_or(mlua::Value::Nil),
        Value::Array(arr) => match lua.create_table() {
            Ok(table) => {
                for (i, v) in arr.iter().enumerate() {
                    let _ = table.set(i + 1, json_to_lua_value(lua, v));
                }
                mlua::Value::Table(table)
            }
            Err(_) => mlua::Value::Nil,
        },
        Value::Object(map) => match lua.create_table() {
            Ok(table) => {
                for (k, v) in map {
                    let _ = table.set(k.as_str(), json_to_lua_value(lua, v));
                }
                mlua::Value::Table(table)
            }
            Err(_) => mlua::Value::Nil,
        },
    }
}

/// Extract tool schemas from a Lua table into OpenAI function calling format.
fn extract_tool_schemas(schemas_table: &mlua::Table) -> Vec<Value> {
    let mut schemas = Vec::new();

    for pair in schemas_table.sequence_values::<mlua::Table>() {
        match pair {
            Ok(t) => {
                let name = match t.get::<String>("name") {
                    Ok(n) => n,
                    Err(_) => continue,
                };
                let description = t.get::<String>("description").unwrap_or_default();

                let mut function = serde_json::Map::new();
                function.insert("name".into(), Value::String(name));
                function.insert("description".into(), Value::String(description));

                if let Ok(params) = t.get::<mlua::Table>("parameters") {
                    function.insert(
                        "parameters".into(),
                        lua_value_to_json(&mlua::Value::Table(params)),
                    );
                }

                schemas.push(Value::Object(serde_json::Map::from_iter([
                    ("type".into(), Value::String("function".into())),
                    ("function".into(), Value::Object(function)),
                ])));
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to read tool schema from Lua");
            }
        }
    }

    schemas
}
