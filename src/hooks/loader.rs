//! Hook loader — extracts hook registrations from the Lua config VM
//! and builds a `HookExecutor` with all user-defined callbacks.
//!
//! Flow:
//! 1. During `config::loader::load_lua()`, `zn.hook(event, fn)` stores
//!    `{event_name, lua_func}` entries in a registry table `_rc_hooks`.
//! 2. After `build_settings()`, `load_hooks()` is called to read the
//!    registry entries, validate event names, and register each
//!    callback in a `HookExecutor`.
//! 3. The executor takes ownership of the Lua VM and is returned
//!    alongside the `Settings`.

use mlua::Lua;

use super::executor::HookExecutor;
use super::types::VALID_HOOK_EVENTS;

/// Extract hook registrations from the Lua VM and build a `HookExecutor`.
///
/// Reads the `_rc_hooks` registry table (populated by `zn.hook()` calls
/// in init.lua) and registers each callback in the executor.
///
/// Takes ownership of the `Lua` VM so that the registered `Function`
/// values remain valid throughout the session.
///
/// Returns `None` if no hooks were registered (and the Lua VM is dropped).
pub fn load_hooks(lua: Lua) -> anyhow::Result<Option<HookExecutor>> {
    let hooks_table: mlua::Table = match lua.named_registry_value("_rc_hooks") {
        Ok(t) => t,
        Err(_) => return Ok(None), // no `zn.hook()` calls were made
    };

    // Build lookup: event_name → HookEvent
    let event_lookup: std::collections::HashMap<&str, super::types::HookEvent> =
        VALID_HOOK_EVENTS.iter().cloned().collect();

    let mut executor = HookExecutor::new(lua);
    let mut loaded_count = 0;

    for pair in {
        // Collect the entries first to avoid borrowing issues with the
        // sequence iterator over the table we just read.
        let mut entries = Vec::new();
        for entry in hooks_table.sequence_values::<mlua::Table>() {
            entries.push(entry?);
        }
        entries
    } {
        let event_name: String = pair.get("event")?;
        let func: mlua::Function = pair.get("fn")?;

        match event_lookup.get(event_name.as_str()) {
            Some(&event) => {
                executor.register(event, func)?;
                loaded_count += 1;
            }
            None => {
                let valid_names: Vec<&str> = VALID_HOOK_EVENTS.iter().map(|(n, _)| *n).collect();
                tracing::warn!(
                    event_name = %event_name,
                    valid_events = %valid_names.join(", "),
                    "Unknown hook event in init.lua, skipping"
                );
            }
        }
    }

    if loaded_count == 0 {
        return Ok(None);
    }

    tracing::info!(
        hooks_loaded = loaded_count,
        "Loaded Lua hooks from init.lua"
    );

    Ok(Some(executor))
}
