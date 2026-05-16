//! Hook executor — stores and fires Lua callbacks at hook points.
//!
//! The executor takes ownership of the Lua VM from the config loader.
//! Hook functions registered via `zn.hook(event, fn)` live in this VM
//! as registry entries. The executor fires them at the appropriate
//! lifecycle points during the session.
//!
//! The `mlua` `send` feature makes `Lua` Send + Sync, so the executor
//! can be stored in `Arc<Mutex<QueryEngine>>`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mlua::{Lua, RegistryKey, Value};

use super::types::{HookEvent, HookResult, VALID_HOOK_EVENTS};

/// A stored hook callback (registry key reference).
struct StoredHook {
    /// Registry key pointing to the Lua function.
    key: RegistryKey,
    /// The event name string (for diagnostics / logging).
    event_name: &'static str,
}

/// The hook executor manages all registered Lua callbacks and fires them
/// at the appropriate lifecycle points.
///
/// It owns the Lua VM from the config loader (which holds all `zn.hook()`
/// callback functions in its registry). This means the Lua VM stays alive
/// for the entire session.
pub struct HookExecutor {
    /// The Lua VM from the config loader — holds hook functions in its registry.
    /// Shared with LuaMemoryProvider so provider tables (with functions) are
    /// directly accessible without cross-VM serialization.
    lua: Arc<Mutex<Lua>>,
    /// event → list of stored callbacks.
    hooks: HashMap<HookEvent, Vec<StoredHook>>,
}

impl HookExecutor {
    /// Create a new executor wrapping a shared Lua VM.
    ///
    /// The VM should be the one used by the config loader, which already
    /// has the hook functions registered in its `_rc_hooks` registry table.
    pub fn new(lua: Arc<Mutex<Lua>>) -> Self {
        Self {
            lua,
            hooks: HashMap::new(),
        }
    }

    /// Register a Lua function for a hook event.
    ///
    /// The function is stored in the Lua registry and the `RegistryKey`
    /// is kept in the executor for later invocation.
    pub fn register(&mut self, event: HookEvent, func: mlua::Function) -> Result<(), mlua::Error> {
        let key = self.lua.lock().unwrap().create_registry_value(&func)?;
        let event_name = event_name_for(event);
        self.hooks
            .entry(event)
            .or_default()
            .push(StoredHook { key, event_name });
        Ok(())
    }

    /// Fire all hooks for the given event with the provided context table.
    ///
    /// Returns a list of non-`Continue` results in registration order.
    /// Errors from individual hooks are logged but do not abort the
    /// remaining hooks.
    pub async fn execute(&self, event: HookEvent, context: &mlua::Table) -> Vec<HookResult> {
        let callbacks = match self.hooks.get(&event) {
            Some(cbs) => cbs,
            None => return Vec::new(),
        };

        let mut results = Vec::new();
        for stored in callbacks {
            let lua = self.lua.lock().unwrap();
            match lua.registry_value::<mlua::Function>(&stored.key) {
                Ok(func) => match func.call::<Value>(context.clone()) {
                    Ok(val) => {
                        if let Some(result) = parse_hook_result(&lua, event, &val) {
                            results.push(result);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            event = stored.event_name,
                            error = %e,
                            "Hook callback error"
                        );
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        event = stored.event_name,
                        error = %e,
                        "Failed to retrieve hook function from registry"
                    );
                }
            }
        }
        results
    }

    /// Fire hooks and return the first `Block` result, if any.
    /// Convenience for `PreToolUse` — the first block wins.
    pub async fn execute_first_block(
        &self,
        event: HookEvent,
        context: &mlua::Table,
    ) -> Option<String> {
        let results = self.execute(event, context).await;
        for r in results {
            if let HookResult::Block { reason } = r {
                return Some(reason);
            }
        }
        None
    }

    /// Fire hooks for `UserMessage` and return the modified input if any
    /// hook returned `ModifiedInput`. Returns None if no modification.
    pub async fn execute_user_message(&self, context: &mlua::Table) -> Option<String> {
        let results = self.execute(HookEvent::UserMessage, context).await;
        for r in results {
            if let HookResult::ModifiedInput(text) = r {
                return Some(text);
            }
        }
        None
    }

    /// Fire hooks for `PreLlmCall` and collect all injected context strings.
    pub async fn execute_pre_llm(&self, context: &mlua::Table) -> Vec<String> {
        let results = self.execute(HookEvent::PreLlmCall, context).await;
        results
            .into_iter()
            .filter_map(|r| {
                if let HookResult::InjectContext(text) = r {
                    Some(text)
                } else {
                    None
                }
            })
            .collect()
    }

    /// Fire hooks for `PostLlmCall` (observe-only).
    pub async fn execute_post_llm(&self, context: &mlua::Table) {
        self.execute(HookEvent::PostLlmCall, context).await;
    }

    /// Fire session lifecycle hooks (observe-only).
    pub async fn execute_session_event(&self, event: HookEvent, context: &mlua::Table) {
        self.execute(event, context).await;
    }

    /// Build a context table in the executor's Lua VM.
    ///
    /// Convenience method to create the context table that is passed to
    /// hook callbacks.
    pub fn build_context(&self) -> Result<mlua::Table, mlua::Error> {
        self.lua.lock().unwrap().create_table()
    }

    /// Get a locked handle to the underlying Lua VM.
    ///
    /// Used by callers that need to convert `serde_json::Value` to native
    /// Lua values via `json_to_lua_value()`.
    pub fn lua(&self) -> std::sync::MutexGuard<'_, Lua> {
        self.lua.lock().unwrap()
    }

    /// Number of registered hooks total (for diagnostics).
    pub fn hook_count(&self) -> usize {
        self.hooks.values().map(|v| v.len()).sum()
    }

    /// Check if any hooks are registered for a given event.
    pub fn has_hooks_for(&self, event: HookEvent) -> bool {
        self.hooks.get(&event).map_or(false, |v| !v.is_empty())
    }

    /// List all registered event names with counts (for `/hooks` command).
    pub fn registered_events(&self) -> Vec<(&'static str, usize)> {
        self.hooks
            .iter()
            .map(|(event, hooks)| (event_name_for(*event), hooks.len()))
            .collect()
    }
}

/// Convert a `serde_json::Value` to a `mlua::Value`.
///
/// Used by query.rs to pass tool input/output as native Lua tables
/// in hook context tables.
pub fn json_to_lua_value(lua: &Lua, val: &serde_json::Value) -> mlua::Value {
    match val {
        serde_json::Value::Null => mlua::Value::Nil,
        serde_json::Value::Bool(b) => mlua::Value::Boolean(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                mlua::Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                mlua::Value::Number(f)
            } else {
                mlua::Value::Nil
            }
        }
        serde_json::Value::String(s) => mlua::Value::String(lua.create_string(s).unwrap()),
        serde_json::Value::Array(arr) => {
            let table = lua.create_table().unwrap();
            for (i, v) in arr.iter().enumerate() {
                table.set(i + 1, json_to_lua_value(lua, v)).unwrap();
            }
            mlua::Value::Table(table)
        }
        serde_json::Value::Object(map) => {
            let table = lua.create_table().unwrap();
            for (k, v) in map {
                table.set(k.as_str(), json_to_lua_value(lua, v)).unwrap();
            }
            mlua::Value::Table(table)
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Get the canonical event name string for a `HookEvent`.
fn event_name_for(event: HookEvent) -> &'static str {
    for (name, e) in VALID_HOOK_EVENTS {
        if *e == event {
            return name;
        }
    }
    "unknown"
}

/// Parse a Lua return value into a `HookResult`.
///
/// Accepted shapes:
///   - `nil` → no result (skip)
///   - `true` → no result (skip)
///   - `false` → `Block` (for PreToolUse) or ignore
///   - `{ block = "reason" }` → `Block { reason }`
///   - `{ inject_context = "text" }` → `InjectContext(text)`
///   - `{ modified_input = "text" }` → `ModifiedInput(text)`
///   - bare string → context-dependent (block for PreToolUse, inject for PreLlmCall, etc.)
fn parse_hook_result(_lua: &Lua, event: HookEvent, val: &Value) -> Option<HookResult> {
    match val {
        Value::Nil | Value::Boolean(true) => None,
        Value::Boolean(false) => {
            if event == HookEvent::PreToolUse {
                Some(HookResult::Block {
                    reason: "Blocked by hook".into(),
                })
            } else {
                None
            }
        }
        Value::Table(t) => {
            if let Ok(reason) = t.get::<String>("block") {
                return Some(HookResult::Block { reason });
            }
            if let Ok(text) = t.get::<String>("inject_context") {
                return Some(HookResult::InjectContext(text));
            }
            if let Ok(text) = t.get::<String>("modified_input") {
                return Some(HookResult::ModifiedInput(text));
            }
            None
        }
        Value::String(s) => {
            let text = s.to_string_lossy();
            match event {
                HookEvent::PreToolUse => Some(HookResult::Block {
                    reason: text.to_string(),
                }),
                HookEvent::PreLlmCall => Some(HookResult::InjectContext(text.to_string())),
                HookEvent::UserMessage => Some(HookResult::ModifiedInput(text.to_string())),
                _ => None,
            }
        }
        _ => {
            tracing::warn!(
                event = event_name_for(event),
                "Hook returned unexpected type, ignoring"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::types::VALID_HOOK_EVENTS;
    use mlua::Lua;

    /// Create a test executor with a fresh Lua VM (no hooks pre-registered).
    fn test_executor() -> HookExecutor {
        let lua = Lua::new();
        HookExecutor::new(Arc::new(Mutex::new(lua)))
    }

    #[test]
    fn test_event_name_for_all_variants() {
        for (name, event) in VALID_HOOK_EVENTS {
            assert_eq!(event_name_for(*event), *name);
        }
    }

    #[test]
    fn test_executor_default_is_empty() {
        let exec = test_executor();
        assert_eq!(exec.hook_count(), 0);
        assert!(exec.registered_events().is_empty());
    }

    #[test]
    fn test_register_and_count() {
        let lua = Lua::new();
        let func = lua.create_function(|_, ()| Ok(())).unwrap();

        let mut exec = HookExecutor::new(Arc::new(Mutex::new(lua)));
        exec.register(HookEvent::PreToolUse, func).unwrap();
        assert_eq!(exec.hook_count(), 1);
        assert!(exec.has_hooks_for(HookEvent::PreToolUse));
        assert!(!exec.has_hooks_for(HookEvent::PostToolUse));
    }

    #[tokio::test]
    async fn test_execute_nil_returns_empty() {
        let lua = Lua::new();
        let func = lua.create_function(|_, ()| Ok(Value::Nil)).unwrap();

        let mut exec = HookExecutor::new(Arc::new(Mutex::new(lua)));
        exec.register(HookEvent::PreToolUse, func).unwrap();

        let ctx = exec.build_context().unwrap();
        let results = exec.execute(HookEvent::PreToolUse, &ctx).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_execute_block_from_table() {
        let lua = Lua::new();
        let func = lua
            .create_function(|lua, ()| -> Result<Value, mlua::Error> {
                let t = lua.create_table()?;
                t.set("block", "no entry")?;
                Ok(Value::Table(t))
            })
            .unwrap();

        let mut exec = HookExecutor::new(Arc::new(Mutex::new(lua)));
        exec.register(HookEvent::PreToolUse, func).unwrap();

        let ctx = exec.build_context().unwrap();
        let results = exec.execute(HookEvent::PreToolUse, &ctx).await;
        assert_eq!(results.len(), 1);
        match &results[0] {
            HookResult::Block { reason } => assert_eq!(reason, "no entry"),
            _ => panic!("expected Block"),
        }
    }

    #[tokio::test]
    async fn test_execute_block_from_string() {
        let lua = Lua::new();
        let func = lua
            .create_function(|_, ()| -> Result<String, mlua::Error> { Ok("forbidden".into()) })
            .unwrap();

        let mut exec = HookExecutor::new(Arc::new(Mutex::new(lua)));
        exec.register(HookEvent::PreToolUse, func).unwrap();

        let ctx = exec.build_context().unwrap();
        let results = exec.execute(HookEvent::PreToolUse, &ctx).await;
        assert_eq!(results.len(), 1);
        match &results[0] {
            HookResult::Block { reason } => assert_eq!(reason, "forbidden"),
            _ => panic!("expected Block"),
        }
    }

    #[tokio::test]
    async fn test_execute_first_block() {
        let lua = Lua::new();
        let func1 = lua
            .create_function(|_, ()| -> Result<Value, mlua::Error> { Ok(Value::Nil) })
            .unwrap();
        let func2 = lua
            .create_function(|_, ()| -> Result<String, mlua::Error> { Ok("blocked!".into()) })
            .unwrap();

        let mut exec = HookExecutor::new(Arc::new(Mutex::new(lua)));
        exec.register(HookEvent::PreToolUse, func1).unwrap();
        exec.register(HookEvent::PreToolUse, func2).unwrap();

        let ctx = exec.build_context().unwrap();
        let result = exec.execute_first_block(HookEvent::PreToolUse, &ctx).await;
        assert_eq!(result, Some("blocked!".into()));
    }

    #[tokio::test]
    async fn test_execute_user_message_modified() {
        let lua = Lua::new();
        let func = lua
            .create_function(|_, ()| -> Result<String, mlua::Error> { Ok("modified input".into()) })
            .unwrap();

        let mut exec = HookExecutor::new(Arc::new(Mutex::new(lua)));
        exec.register(HookEvent::UserMessage, func).unwrap();

        let ctx = exec.build_context().unwrap();
        let result = exec.execute_user_message(&ctx).await;
        assert_eq!(result, Some("modified input".into()));
    }

    #[tokio::test]
    async fn test_execute_pre_llm_inject_context() {
        let lua = Lua::new();
        let func = lua
            .create_function(|_, ()| -> Result<String, mlua::Error> { Ok("extra context".into()) })
            .unwrap();

        let mut exec = HookExecutor::new(Arc::new(Mutex::new(lua)));
        exec.register(HookEvent::PreLlmCall, func).unwrap();

        let ctx = exec.build_context().unwrap();
        let results = exec.execute_pre_llm(&ctx).await;
        assert_eq!(results, vec!["extra context".to_string()]);
    }

    #[tokio::test]
    async fn test_context_table_passed_to_hook() {
        let lua = Lua::new();
        let func = lua
            .create_function(|_, ctx: mlua::Table| -> Result<String, mlua::Error> {
                let name: String = ctx.get("tool_name")?;
                Ok(format!("blocked: {}", name))
            })
            .unwrap();

        let mut exec = HookExecutor::new(Arc::new(Mutex::new(lua)));
        exec.register(HookEvent::PreToolUse, func).unwrap();

        let ctx = exec.build_context().unwrap();
        ctx.set("tool_name", "bash").unwrap();
        let results = exec.execute(HookEvent::PreToolUse, &ctx).await;
        assert_eq!(results.len(), 1);
        match &results[0] {
            HookResult::Block { reason } => assert_eq!(reason, "blocked: bash"),
            _ => panic!("expected Block"),
        }
    }

    #[test]
    fn test_parse_hook_result_from_table_block() {
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        t.set("block", "nope").unwrap();

        let result = parse_hook_result(&lua, HookEvent::PreToolUse, &Value::Table(t));
        match result {
            Some(HookResult::Block { reason }) => assert_eq!(reason, "nope"),
            _ => panic!("expected Block"),
        }
    }

    #[test]
    fn test_parse_hook_result_from_table_inject() {
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        t.set("inject_context", "hello").unwrap();

        let result = parse_hook_result(&lua, HookEvent::PreLlmCall, &Value::Table(t));
        match result {
            Some(HookResult::InjectContext(text)) => assert_eq!(text, "hello"),
            _ => panic!("expected InjectContext"),
        }
    }

    #[test]
    fn test_parse_hook_result_from_table_modified_input() {
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        t.set("modified_input", "rewritten").unwrap();

        let result = parse_hook_result(&lua, HookEvent::UserMessage, &Value::Table(t));
        match result {
            Some(HookResult::ModifiedInput(text)) => assert_eq!(text, "rewritten"),
            _ => panic!("expected ModifiedInput"),
        }
    }

    #[test]
    fn test_parse_hook_result_false_blocks_pre_tool() {
        let lua = Lua::new();
        let result = parse_hook_result(&lua, HookEvent::PreToolUse, &Value::Boolean(false));
        match result {
            Some(HookResult::Block { reason }) => assert_eq!(reason, "Blocked by hook"),
            _ => panic!("expected Block"),
        }
    }

    #[test]
    fn test_parse_hook_result_false_ignored_for_other_events() {
        let lua = Lua::new();
        let result = parse_hook_result(&lua, HookEvent::SessionStart, &Value::Boolean(false));
        assert!(result.is_none());
    }
}
