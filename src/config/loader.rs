//! Lua-based configuration loader.
//!
//! Loads `~/.config/zeno/init.lua`, executes it in a sandboxed Lua VM,
//! and converts the returned table to a `Settings` struct via mlua serde.
//!
//! If `init.lua` does not exist, returns `Settings::default()` with a
//! log message nudging the user to create one.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use mlua::{Lua, LuaOptions, LuaSerdeExt, StdLib, Value};

use super::paths;
use super::settings::{
    AuxiliaryConfig, DelegationConfig, EngineConfig, McpServerConfig, PermissionMode,
    ProviderConfig, Settings, SkillsConfig, ToolsConfig, WebSearchConfig,
};

// ---------------------------------------------------------------------------
// Safe StdLib combination (excludes io/os/debug/ffi)
// ---------------------------------------------------------------------------

fn safe_stdlibs() -> StdLib {
    StdLib::TABLE
        | StdLib::STRING
        | StdLib::MATH
        | StdLib::UTF8
        | StdLib::COROUTINE
        | StdLib::PACKAGE
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Load settings from `~/.config/zeno/init.lua`.
///
/// If the file does not exist, returns `Settings::default()`, `None` hooks,
/// and a minimal Lua VM.
pub fn load() -> anyhow::Result<(
    Settings,
    Option<crate::hooks::executor::HookExecutor>,
    Arc<Mutex<Lua>>,
)> {
    let init_path = paths::config_path();
    if !init_path.exists() {
        tracing::info!(path = %init_path.display(), event = "no_init_lua", "No init.lua found, using defaults");
        let lua = Lua::new_with(safe_stdlibs(), LuaOptions::new()).context("creating Lua VM")?;
        return Ok((Settings::default(), None, Arc::new(Mutex::new(lua))));
    }
    load_lua(&init_path, &paths::config_dir())
}

/// Load settings from a custom config directory (for testing).
#[cfg(test)]
pub fn load_from_dir(
    config_dir: &std::path::Path,
) -> anyhow::Result<(
    Settings,
    Option<crate::hooks::executor::HookExecutor>,
    Arc<Mutex<Lua>>,
)> {
    let init_path = config_dir.join("init.lua");
    if !init_path.exists() {
        tracing::info!(path = %init_path.display(), event = "no_init_lua", "No init.lua found, using defaults");
        let lua = Lua::new_with(safe_stdlibs(), LuaOptions::new()).context("creating Lua VM")?;
        return Ok((Settings::default(), None, Arc::new(Mutex::new(lua))));
    }
    load_lua(&init_path, config_dir)
}

fn load_lua(
    path: &Path,
    config_dir: &Path,
) -> anyhow::Result<(
    Settings,
    Option<crate::hooks::executor::HookExecutor>,
    Arc<Mutex<Lua>>,
)> {
    // 1. Create sandboxed Lua VM
    let lua = Lua::new_with(safe_stdlibs(), LuaOptions::new()).context("creating Lua VM")?;

    // 2. Remove dangerous global functions
    let globals = lua.globals();
    globals.set("dofile", Value::Nil)?;
    globals.set("loadfile", Value::Nil)?;

    // 3. Restrict package.path to <config_dir>/lua/ only
    let lua_dir = config_dir.join("lua");
    let pattern = format!(
        "{}/?.lua;{}/?/init.lua",
        lua_dir.display(),
        lua_dir.display()
    );
    lua.load(format!("package.path = [====[{}]====]", pattern))
        .exec()
        .context("setting package.path")?;

    // Override package.searchers: add preload check + path traversal protection.
    // The default searchers are: preload, lua path, C, all-in-one.
    // We replace with: preload, safe path (no traversal).
    lua.load(
        r#"
        local preload_searcher = package.searchers[1]
        local path_searcher = package.searchers[2]
        package.searchers = {
            preload_searcher,
            function(name)
                if name:find("%.%.") then
                    return nil, "module name contains path traversal: " .. name
                end
                return path_searcher(name)
            end
        }
        "#,
    )
    .exec()
    .context("setting up safe require")?;

    // 3b. Add provider extensions to the config VM (shared with LuaMemoryProvider)
    // os.getenv (safe, read-only access to env vars)
    let os_table = lua.create_table()?;
    let getenv_fn =
        lua.create_function(|_, name: String| -> Result<Option<String>, mlua::Error> {
            Ok(std::env::var(&name).ok())
        })?;
    os_table.set("getenv", getenv_fn)?;
    globals.set("os", os_table)?;

    // json library (json.encode / json.decode)
    let json_table = lua.create_table()?;
    let encode_fn = lua.create_function(|_, val: mlua::Value| {
        let json_val = crate::memory::lua_provider::lua_value_to_json(&val);
        serde_json::to_string(&json_val)
            .map_err(|e| mlua::Error::external(format!("json.encode failed: {}", e)))
    })?;
    let decode_fn = lua.create_function(|lua, s: String| {
        match serde_json::from_str::<serde_json::Value>(&s) {
            Ok(v) => Ok(crate::memory::lua_provider::json_to_lua_value(lua, &v)),
            Err(e) => Err(mlua::Error::external(format!("json.decode failed: {}", e))),
        }
    })?;
    json_table.set("encode", encode_fn)?;
    json_table.set("decode", decode_fn)?;
    globals.set("json", json_table)?;

    // http library for memory providers that need HTTP calls
    // http.request(method, url, body, headers) -> response_body, status_code, error
    let http_table = lua.create_table()?;
    let request_fn = lua.create_function(
        |_, (method, url, body, headers): (String, String, Option<String>, Option<mlua::Table>)| {
            let client = reqwest::blocking::Client::new();
            let method_upper = method.to_uppercase();
            let mut req = match method_upper.as_str() {
                "GET" => client.get(&url),
                "POST" => client.post(&url),
                "PUT" => client.put(&url),
                "DELETE" => client.delete(&url),
                "PATCH" => client.patch(&url),
                _ => {
                    return Err(mlua::Error::external(format!(
                        "Unsupported HTTP method: {}",
                        method
                    )));
                }
            };

            // Add headers
            if let Some(headers_table) = headers {
                for pair in headers_table.pairs::<String, String>() {
                    if let Ok((key, value)) = pair {
                        req = req.header(&key, &value);
                    }
                }
            }

            // Add body
            if let Some(body_str) = body {
                req = req
                    .header("Content-Type", "application/json")
                    .body(body_str);
            }

            match req.send() {
                Ok(response) => {
                    let status = response.status().as_u16();
                    match response.text() {
                        Ok(text) => Ok((text, status, None::<String>)),
                        Err(e) => Ok((
                            String::new(),
                            status,
                            Some(format!("Failed to read response: {}", e)),
                        )),
                    }
                }
                Err(e) => Ok((
                    String::new(),
                    0u16,
                    Some(format!("HTTP request failed: {}", e)),
                )),
            }
        },
    )?;
    http_table.set("request", request_fn)?;
    globals.set("http", http_table)?;

    // 4. Build the `zeno` Lua module.
    // Strategy: use an "overrides" table in the registry.
    // Only keys explicitly set by the user are stored here.
    // When building Settings, we start from Settings::default() and
    // apply overrides — this ensures unset fields keep their defaults.
    let overrides = lua.create_table()?;
    lua.set_named_registry_value("_rc_overrides", overrides)?;

    // Initialize tools defaults so that `zn.tools({ web_fetch = false })`
    // only overrides the specific key, not all tools.
    let tools_defaults = lua.create_table()?;
    tools_defaults.set("bash", true)?;
    tools_defaults.set("use_rtk", true)?;
    tools_defaults.set("bash_env", lua.create_table()?)?;
    tools_defaults.set("read", true)?;
    tools_defaults.set("write", true)?;
    tools_defaults.set("edit", true)?;
    tools_defaults.set("glob", true)?;
    tools_defaults.set("grep", true)?;
    tools_defaults.set("web_search", true)?;
    tools_defaults.set("web_fetch", true)?;
    lua.set_named_registry_value("_rc_tools_defaults", tools_defaults)?;

    let zeno_table = lua.create_table()?;
    register_zeno_api(&lua, &zeno_table)?;

    // Make `require 'zeno'` work via package.preload
    let preload: mlua::Table = lua
        .load("return package.preload or {}")
        .eval::<Value>()
        .ok()
        .and_then(|v| {
            if let Value::Table(t) = v {
                Some(t)
            } else {
                None
            }
        })
        .unwrap_or_else(|| lua.create_table().unwrap());
    let zn_clone = zeno_table.clone();
    preload.set(
        "zeno",
        lua.create_function(move |_, ()| Ok(zn_clone.clone()))?,
    )?;

    // Also set as global for convenience
    lua.globals().set("zeno", zeno_table)?;

    // 5. Execute init.lua
    let source = std::fs::read_to_string(path).context(format!("reading {}", path.display()))?;

    let eval_result: Result<Value, mlua::Error> =
        lua.load(&source).set_name(path.to_string_lossy()).eval();
    if let Err(e) = eval_result {
        anyhow::bail!("{}", format_lua_error(&e, &source, path));
    }

    // 6. Build Settings from defaults + overrides
    let settings = build_settings(&lua)?;

    // 7. Validate
    validate(&settings)?;

    // 8. Wrap VM in Arc<Mutex<>> for shared ownership between hooks and provider
    let lua = Arc::new(Mutex::new(lua));

    // 9. Load hook registrations from Lua
    let hooks = crate::hooks::loader::load_hooks(lua.clone())?;

    Ok((settings, hooks, lua))
}

// ---------------------------------------------------------------------------
// Build Settings from defaults + user overrides
// ---------------------------------------------------------------------------

fn build_settings(lua: &Lua) -> anyhow::Result<Settings> {
    let mut settings = Settings::default();

    let overrides: mlua::Table = lua.named_registry_value("_rc_overrides")?;

    // --- Providers ---
    if let Ok(providers_table) = overrides.get::<mlua::Table>("providers") {
        match lua.from_value::<HashMap<String, ProviderConfig>>(Value::Table(providers_table)) {
            Ok(val) => settings.providers = val,
            Err(e) => tracing::warn!(error = %e, "Failed to parse providers from Lua config"),
        }
    }

    // --- active_provider ---
    if let Ok(v) = overrides.get::<String>("active_provider")
        && !v.is_empty()
    {
        settings.active_provider = v;
    }

    // --- model ---
    if let Ok(v) = overrides.get::<String>("model")
        && !v.is_empty()
    {
        settings.model = v;
    }

    // --- tools ---
    if let Ok(user_tools) = overrides.get::<mlua::Table>("tools") {
        // Merge: start from defaults, override user-set keys
        let defaults: mlua::Table = lua.named_registry_value("_rc_tools_defaults")?;
        let merged = lua.create_table()?;
        for result in defaults.pairs::<String, mlua::Value>() {
            let (k, v) = result?;
            merged.set(k, v)?;
        }
        for result in user_tools.pairs::<String, mlua::Value>() {
            let (k, v) = result?;
            merged.set(k, v)?;
        }
        match lua.from_value::<ToolsConfig>(Value::Table(merged)) {
            Ok(val) => settings.tools = val,
            Err(e) => tracing::warn!(error = %e, "Failed to parse tools from Lua config"),
        }
    }

    // --- commands ---
    // zn.commands({ allow = {...}, ask = {...}, deny = {...} })
    // Merged into settings.tools.{allowed_commands, ask_commands, denied_commands}
    if let Ok(cmd_table) = overrides.get::<mlua::Table>("commands") {
        if let Ok(cmds) = cmd_table.get::<Vec<String>>("allow") {
            settings.tools.allowed_commands = cmds;
        }
        if let Ok(cmds) = cmd_table.get::<Vec<String>>("ask") {
            settings.tools.ask_commands = cmds;
        }
        if let Ok(cmds) = cmd_table.get::<Vec<String>>("deny") {
            settings.tools.denied_commands = cmds;
        }
    }

    // --- role ---
    if let Ok(role_table) = overrides.get::<mlua::Table>("role") {
        if let Ok(v) = role_table.get::<String>("identity")
            && !v.is_empty()
        {
            settings.role.identity = Some(v);
        }
        if let Ok(v) = role_table.get::<String>("guidelines")
            && !v.is_empty()
        {
            settings.role.guidelines = Some(v);
        }
    }

    // --- web_search_config ---
    if let Ok(ws) = overrides.get::<mlua::Table>("web_search_config") {
        match lua.from_value::<WebSearchConfig>(Value::Table(ws)) {
            Ok(val) => settings.web_search_config = val,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to parse web_search_config from Lua config")
            }
        }
    }

    // --- mcp ---
    if let Ok(mcp_servers) = overrides.get::<mlua::Table>("mcp_servers") {
        match lua.from_value::<HashMap<String, McpServerConfig>>(Value::Table(mcp_servers)) {
            Ok(val) => settings.mcp.servers = val,
            Err(e) => tracing::warn!(error = %e, "Failed to parse MCP servers from Lua config"),
        }
    }

    // --- permissions ---
    if let Ok(v) = overrides.get::<String>("permissions") {
        match v.parse::<PermissionMode>() {
            Ok(mode) => settings.permissions = mode,
            Err(e) => tracing::warn!(value = %v, error = %e, "Invalid permissions value"),
        }
    }

    // --- trusted paths ---
    if let Ok(paths) = overrides.get::<Vec<String>>("trusted_paths") {
        settings.trusted_paths = paths;
    }

    // --- numeric fields ---
    if let Ok(v) = overrides.get::<u32>("max_turns") {
        settings.max_turns = v;
    }
    if let Ok(v) = overrides.get::<u32>("max_tokens") {
        settings.max_tokens = v;
    }

    // --- model context table ---
    if let Ok(model_contexts) = overrides.get::<mlua::Table>("model_contexts") {
        match lua.from_value::<HashMap<String, u32>>(Value::Table(model_contexts)) {
            Ok(val) => settings.model_contexts = val,
            Err(e) => tracing::warn!(error = %e, "Failed to parse model_contexts from Lua config"),
        }
    }

    // --- theme ---
    if let Ok(v) = overrides.get::<String>("theme")
        && !v.is_empty()
    {
        settings.theme = v;
    }

    // --- plugins_dir ---
    if let Ok(v) = overrides.get::<String>("plugins_dir")
        && !v.is_empty()
    {
        settings.plugins.dir = v;
    }

    // --- memory char limits ---
    if let Ok(v) = overrides.get::<usize>("memory_char_limit") {
        settings.memory.memory_char_limit = v;
    }
    if let Ok(v) = overrides.get::<usize>("user_char_limit") {
        settings.memory.user_char_limit = v;
    }

    // --- memory provider (active name only — table lives in Lua registry) ---
    if let Ok(v) = overrides.get::<String>("memory_provider_active")
        && !v.is_empty()
    {
        settings.memory.provider = v;
    }

    // --- auxiliary ---
    if let Ok(aux) = overrides.get::<mlua::Table>("auxiliary") {
        // Coerce integer timeouts to float before serde deserialization.
        // Lua stores `timeout = 30` as integer, but AuxiliaryTaskConfig.timeout is f64.
        match coerce_auxiliary_timeouts(lua, &aux) {
            Ok(coerced) => match lua.from_value::<AuxiliaryConfig>(Value::Table(coerced)) {
                Ok(val) => settings.auxiliary = val,
                Err(e) => tracing::warn!(error = %e, "Failed to parse auxiliary config from Lua"),
            },
            Err(e) => tracing::warn!(error = %e, "Failed to coerce auxiliary timeouts"),
        }
    }

    // --- log_retention_days ---
    if let Ok(v) = overrides.get::<u64>("log_retention_days") {
        settings.log_retention_days = v;
    }

    // --- llm_max_retries ---
    if let Ok(v) = overrides.get::<u32>("llm_max_retries") {
        settings.llm.max_retries = v;
    }

    // --- compact_threshold ---
    if let Ok(v) = overrides.get::<f64>("compact_threshold") {
        settings.llm.compact_threshold = v.clamp(0.0, 1.0);
    }

    // --- skills (background review + curator) ---
    if let Ok(skills) = overrides.get::<mlua::Table>("skills") {
        match lua.from_value::<SkillsConfig>(Value::Table(skills)) {
            Ok(val) => settings.skills = val,
            Err(e) => tracing::warn!(error = %e, "Failed to parse skills config from Lua"),
        }
    }

    // --- engine ---
    if let Ok(engine) = overrides.get::<mlua::Table>("engine") {
        match lua.from_value::<EngineConfig>(Value::Table(engine)) {
            Ok(val) => settings.engine = val,
            Err(e) => tracing::warn!(error = %e, "Failed to parse engine config from Lua"),
        }
    }

    // --- delegation ---
    if let Ok(delegation) = overrides.get::<mlua::Table>("delegation") {
        match lua.from_value::<DelegationConfig>(Value::Table(delegation)) {
            Ok(val) => settings.delegation = val,
            Err(e) => tracing::warn!(error = %e, "Failed to parse delegation config from Lua"),
        }
    }

    // --- safe_paths ---
    if let Ok(paths) = overrides.get::<Vec<String>>("safe_paths") {
        settings.safe_paths = paths;
    }

    Ok(settings)
}

// ---------------------------------------------------------------------------
// zeno Lua module API registration
// ---------------------------------------------------------------------------

fn register_zeno_api(lua: &Lua, table: &mlua::Table) -> anyhow::Result<()> {
    fn get_overrides(lua: &Lua) -> Result<mlua::Table, mlua::Error> {
        lua.named_registry_value("_rc_overrides")
    }

    // --- Provider ---
    table.set(
        "provider",
        lua.create_function(move |lua, (name, opts): (String, mlua::Value)| {
            let providers: mlua::Table = get_overrides(lua)?
                .get::<mlua::Table>("providers")
                .unwrap_or_else(|_| lua.create_table().unwrap());
            providers.set(name, opts)?;
            get_overrides(lua)?.set("providers", providers)?;
            Ok(())
        })?,
    )?;

    table.set(
        "set_provider",
        lua.create_function(move |lua, name: String| {
            get_overrides(lua)?.set("active_provider", name)?;
            Ok(())
        })?,
    )?;

    table.set(
        "set_model",
        lua.create_function(move |lua, name: String| {
            get_overrides(lua)?.set("model", name)?;
            Ok(())
        })?,
    )?;

    // --- Tools ---
    // zn.tools({...})  → bulk tool config (booleans, skip_dirs, bash_env, etc.)
    //   zn.tools({ web_fetch = false, bash = false })
    //
    // Command permissions → zn.commands({ allow=..., ask=..., deny=... })
    table.set(
        "tools",
        lua.create_function(move |lua, opts: mlua::Table| {
            let tools: mlua::Table = get_overrides(lua)?
                .get::<mlua::Table>("tools")
                .unwrap_or_else(|_| lua.create_table().unwrap());
            for result in opts.pairs::<String, mlua::Value>() {
                let (k, v) = result?;
                tools.set(k, v)?;
            }
            get_overrides(lua)?.set("tools", tools)?;
            Ok(())
        })?,
    )?;

    // --- Commands ---
    // zn.commands({ allow = {...}, ask = {...}, deny = {...} })
    // Separated from zn.tools for cleaner semantics.
    //
    //   zn.commands({
    //     allow = { "pnpm list", "just --list" },   -- always auto-allow
    //     ask   = { "git checkout", "git restore" }, -- require confirmation
    //     deny  = { "some-dangerous-cmd" },          -- always blocked
    //   })
    table.set(
        "commands",
        lua.create_function(move |lua, opts: mlua::Table| {
            let commands: mlua::Table = get_overrides(lua)?
                .get::<mlua::Table>("commands")
                .unwrap_or_else(|_| lua.create_table().unwrap());
            for result in opts.pairs::<String, mlua::Value>() {
                let (k, v) = result?;
                commands.set(k, v)?;
            }
            get_overrides(lua)?.set("commands", commands)?;
            Ok(())
        })?,
    )?;

    // --- Role ---
    // Bulk: zn.role({ identity = "...", guidelines = "..." })
    table.set(
        "role",
        lua.create_function(move |lua, opts: mlua::Table| {
            let role: mlua::Table = get_overrides(lua)?
                .get::<mlua::Table>("role")
                .unwrap_or_else(|_| lua.create_table().unwrap());
            for result in opts.pairs::<String, String>() {
                let (k, v) = result?;
                if !v.is_empty() {
                    role.set(k, v)?;
                }
            }
            get_overrides(lua)?.set("role", role)?;
            Ok(())
        })?,
    )?;

    // Individual setters
    table.set(
        "identity",
        lua.create_function(move |lua, text: String| {
            let role: mlua::Table = get_overrides(lua)?
                .get::<mlua::Table>("role")
                .unwrap_or_else(|_| lua.create_table().unwrap());
            role.set("identity", text)?;
            get_overrides(lua)?.set("role", role)?;
            Ok(())
        })?,
    )?;

    table.set(
        "guidelines",
        lua.create_function(move |lua, text: String| {
            let role: mlua::Table = get_overrides(lua)?
                .get::<mlua::Table>("role")
                .unwrap_or_else(|_| lua.create_table().unwrap());
            role.set("guidelines", text)?;
            get_overrides(lua)?.set("role", role)?;
            Ok(())
        })?,
    )?;

    // --- Bash environment variables ---
    table.set(
        "bash_env",
        lua.create_function(move |lua, opts: mlua::Table| {
            let tools: mlua::Table = get_overrides(lua)?
                .get::<mlua::Table>("tools")
                .unwrap_or_else(|_| lua.create_table().unwrap());
            let env: mlua::Table = tools
                .get::<mlua::Table>("bash_env")
                .unwrap_or_else(|_| lua.create_table().unwrap());
            for pair in opts.pairs::<String, String>() {
                let (k, v) = pair?;
                env.set(k, v)?;
            }
            tools.set("bash_env", env)?;
            get_overrides(lua)?.set("tools", tools)?;
            Ok(())
        })?,
    )?;

    // --- Web Search ---
    // zn.web_search({ provider = "brave", api_key = "BRAVE_API_KEY" })
    table.set(
        "web_search",
        lua.create_function(move |lua, opts: mlua::Table| {
            get_overrides(lua)?.set("web_search_config", opts)?;
            Ok(())
        })?,
    )?;

    // --- MCP ---
    // zn.mcp_servers({ name1 = {...}, name2 = {...} })
    table.set(
        "mcp_servers",
        lua.create_function(move |lua, opts: mlua::Table| {
            let existing: mlua::Table = get_overrides(lua)?
                .get::<mlua::Table>("mcp_servers")
                .unwrap_or_else(|_| lua.create_table().unwrap());
            // Merge: bulk table entries are added/overridden on top of existing
            for pair in opts.pairs::<String, mlua::Value>() {
                let (name, val) = pair?;
                existing.set(name, val)?;
            }
            get_overrides(lua)?.set("mcp_servers", existing)?;
            Ok(())
        })?,
    )?;

    // --- Auxiliary (bulk) ---
    // zn.auxiliaries({ compression = {...}, vision = {...}, ... })
    table.set(
        "auxiliaries",
        lua.create_function(move |lua, opts: mlua::Table| {
            let existing: mlua::Table = get_overrides(lua)?
                .get::<mlua::Table>("auxiliary")
                .unwrap_or_else(|_| lua.create_table().unwrap());
            for pair in opts.pairs::<String, mlua::Value>() {
                let (task, val) = pair?;
                existing.set(task, val)?;
            }
            get_overrides(lua)?.set("auxiliary", existing)?;
            Ok(())
        })?,
    )?;

    // --- Global settings ---
    table.set(
        "permissions",
        lua.create_function(move |lua, mode: String| {
            get_overrides(lua)?.set("permissions", mode)?;
            Ok(())
        })?,
    )?;

    table.set(
        "trusted_paths",
        lua.create_function(move |lua, paths: Vec<String>| {
            get_overrides(lua)?.set("trusted_paths", paths)?;
            Ok(())
        })?,
    )?;

    table.set(
        "max_turns",
        lua.create_function(move |lua, n: u32| {
            get_overrides(lua)?.set("max_turns", n)?;
            Ok(())
        })?,
    )?;

    table.set(
        "max_tokens",
        lua.create_function(move |lua, n: u32| {
            get_overrides(lua)?.set("max_tokens", n)?;
            Ok(())
        })?,
    )?;

    table.set(
        "model_context",
        lua.create_function(move |lua, opts: mlua::Table| {
            let table: mlua::Table = get_overrides(lua)?
                .get::<mlua::Table>("model_contexts")
                .unwrap_or_else(|_| lua.create_table().unwrap());
            for pair in opts.pairs::<String, u32>() {
                let (k, v) = pair?;
                table.set(k, v)?;
            }
            get_overrides(lua)?.set("model_contexts", table)?;
            Ok(())
        })?,
    )?;

    table.set(
        "theme",
        lua.create_function(move |lua, name: String| {
            get_overrides(lua)?.set("theme", name)?;
            Ok(())
        })?,
    )?;

    table.set(
        "plugins_dir",
        lua.create_function(move |lua, path: String| {
            get_overrides(lua)?.set("plugins_dir", path)?;
            Ok(())
        })?,
    )?;

    table.set(
        "memory_char_limit",
        lua.create_function(move |lua, n: usize| {
            get_overrides(lua)?.set("memory_char_limit", n)?;
            Ok(())
        })?,
    )?;

    table.set(
        "user_char_limit",
        lua.create_function(move |lua, n: usize| {
            get_overrides(lua)?.set("user_char_limit", n)?;
            Ok(())
        })?,
    )?;

    // --- Memory Provider ---
    // zn.memory_provider("name", require("module")) or
    // zn.memory_provider("name", { source = [[inline code]] })
    //
    // Stores the provider table in the registry under `_rc_provider_{name}`
    // so it can be looked up later by LuaMemoryProvider in the same VM.
    table.set(
        "memory_provider",
        lua.create_function(move |lua, (name, provider_table): (String, mlua::Table)| {
            // Store the provider table in the registry for later retrieval
            let registry_key = format!("_rc_provider_{}", name);
            lua.set_named_registry_value(&registry_key, provider_table)?;
            // Record the active provider name in overrides (for Settings)
            get_overrides(lua)?.set("memory_provider_active", name)?;
            Ok(())
        })?,
    )?;

    table.set(
        "log_retention_days",
        lua.create_function(move |lua, n: u64| {
            get_overrides(lua)?.set("log_retention_days", n)?;
            Ok(())
        })?,
    )?;

    table.set(
        "llm_max_retries",
        lua.create_function(move |lua, n: u32| {
            get_overrides(lua)?.set("llm_max_retries", n)?;
            Ok(())
        })?,
    )?;

    table.set(
        "compact_threshold",
        lua.create_function(move |lua, n: f64| {
            get_overrides(lua)?.set("compact_threshold", n.clamp(0.0, 1.0))?;
            Ok(())
        })?,
    )?;

    // --- Environment queries (read-only, for conditional config) ---
    table.set(
        "cwd",
        lua.create_function(|_, ()| {
            Ok(std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string())
        })?,
    )?;

    table.set(
        "env",
        lua.create_function(|_, name: String| -> Result<Option<String>, mlua::Error> {
            Ok(std::env::var(&name).ok())
        })?,
    )?;

    table.set(
        "os",
        lua.create_function(|_, ()| {
            Ok(if cfg!(target_os = "linux") {
                "linux"
            } else if cfg!(target_os = "macos") {
                "macos"
            } else if cfg!(target_os = "windows") {
                "windows"
            } else {
                "unknown"
            })
        })?,
    )?;

    table.set(
        "hostname",
        lua.create_function(|_, ()| -> Result<String, mlua::Error> {
            Ok(hostname::get()
                .map(|h| h.to_string_lossy().to_string())
                .unwrap_or_else(|_| "unknown".into()))
        })?,
    )?;

    // --- Hooks ---
    // zn.hook("pre_tool_use", function(ctx) ... end)
    table.set(
        "hook",
        lua.create_function(move |lua, (event_name, func): (String, mlua::Function)| {
            let hooks: mlua::Table = lua
                .named_registry_value::<mlua::Table>("_rc_hooks")
                .unwrap_or_else(|_| lua.create_table().unwrap());
            let entry = lua.create_table()?;
            entry.set("event", event_name)?;
            entry.set("fn", func)?;
            hooks.set(hooks.len()? + 1, entry)?;
            lua.set_named_registry_value("_rc_hooks", hooks)?;
            Ok(())
        })?,
    )?;

    // --- Engine ---
    // zn.engine({ max_auto_continue = 3, stream_timeout_secs = 120, ... })
    table.set(
        "engine",
        lua.create_function(move |lua, opts: mlua::Table| {
            get_overrides(lua)?.set("engine", opts)?;
            Ok(())
        })?,
    )?;

    // --- Delegation ---
    // zn.delegation({ max_concurrent_children = 3, child_timeout = 300, ... })
    table.set(
        "delegation",
        lua.create_function(move |lua, opts: mlua::Table| {
            get_overrides(lua)?.set("delegation", opts)?;
            Ok(())
        })?,
    )?;

    // --- Safe paths ---
    // zn.safe_paths({ "/tmp/", "/var/tmp/", "/home/user/sandbox/" })
    table.set(
        "safe_paths",
        lua.create_function(move |lua, paths: Vec<String>| {
            get_overrides(lua)?.set("safe_paths", paths)?;
            Ok(())
        })?,
    )?;

    // --- Finalize: zn.config() marks the config as ready ---
    // Users call `return zn.config()` at the end of init.lua.
    // We don't actually use the return value — build_settings() in load_lua
    // step 6 always reads from the registry. This function exists for
    // semantic clarity and to provide a natural "end of config" marker.
    table.set(
        "config",
        lua.create_function(move |lua, ()| {
            // Return the overrides table directly for inspection.
            // The actual Settings build happens once in load_lua step 6,
            // avoiding a redundant build_settings() call here.
            let overrides: mlua::Table = lua.named_registry_value("_rc_overrides")?;
            Ok(overrides)
        })?,
    )?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Format a mlua error for user-facing display.
/// Extracts line number from mlua's `[string "name"]:LINE` format
/// and shows the offending source line with context.
fn format_lua_error(err: &mlua::Error, source: &str, path: &Path) -> String {
    let msg = err.to_string();

    // Try to extract line number from mlua's error format:
    // [string "/path/to/init.lua"]:42: attempt to index nil value
    let line_num = extract_line_number(&msg);

    if let Some(line) = line_num {
        let source_lines: Vec<&str> = source.lines().collect();
        let line_usize = line as usize;
        let line_idx = line_usize.saturating_sub(1);
        let context_start = line_idx.saturating_sub(2);
        let context_end = (line_idx + 3).min(source_lines.len());

        let mut result = format!("Error in {} (line {}):\n", path.display(), line);

        for (i, line_content) in source_lines[context_start..context_end].iter().enumerate() {
            let line_no = context_start + i + 1;
            let marker = if line_no == line_usize { ">>>" } else { "   " };
            result.push_str(&format!(" {} {:4} | {}\n", marker, line_no, line_content));
        }

        // Append the cleaned error message (strip [string...] prefix)
        let cleaned = strip_line_prefix(&msg, path);
        result.push_str(&format!("\n {}", cleaned));

        result
    } else {
        format!("Error in {}: {}", path.display(), msg)
    }
}

/// Extract line number from mlua error string like:
/// `[string "/path/init.lua"]:42: ...`
fn extract_line_number(msg: &str) -> Option<u32> {
    // Simple parse: find `]:DIGITS` after `[string`
    let start = msg.find("[string")?;
    let after_bracket = msg[start..].find("]:")?;
    let rest = &msg[start + after_bracket + 2..];
    let digits_end = rest.find(':')?; // stop at the colon after line number
    rest[..digits_end].parse().ok()
}

/// Strip the `[string "/path/init.lua"]:LINE:` prefix from error message
/// since we already display file and line separately.
fn strip_line_prefix(msg: &str, path: &Path) -> String {
    let prefix = format!("[string \"{}\"]:", path.display());
    if let Some(rest) = msg.strip_prefix(&prefix) {
        // rest starts with "LINE: actual_message"
        rest.trim_start_matches(|c: char| c.is_ascii_digit())
            .trim_start_matches(':')
            .trim()
            .to_string()
    } else {
        msg.to_string()
    }
}

/// Coerce integer timeout values to float in the auxiliary config table.
///
/// Lua stores `timeout = 30` as integer, but `AuxiliaryTaskConfig.timeout` is f64.
/// mlua serde cannot deserialize i64 → f64, so we walk the auxiliary table
/// and convert any integer "timeout" keys to float.
///
/// This is called in `build_settings` (not in `zn.auxiliaries()`) so that it
/// handles all code paths, including users constructing tables directly.
fn coerce_auxiliary_timeouts(lua: &Lua, aux: &mlua::Table) -> anyhow::Result<mlua::Table> {
    let result = lua.create_table()?;

    for pair in aux.pairs::<String, mlua::Value>() {
        let (key, value) = pair?;
        if let Value::Table(task_table) = value {
            let coerced = lua.create_table()?;
            for inner in task_table.pairs::<String, mlua::Value>() {
                let (k, v) = inner?;
                if k == "timeout"
                    && let Value::Integer(n) = v
                {
                    coerced.set(k, n as f64)?;
                    continue;
                }
                coerced.set(k, v)?;
            }
            result.set(key, coerced)?;
        } else {
            result.set(key, value)?;
        }
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

fn validate(settings: &Settings) -> anyhow::Result<()> {
    if settings.providers.is_empty() {
        anyhow::bail!(
            "No providers configured. Add at least one provider in init.lua:\n\
             \n\
             local zn = require 'zeno'\n\
             zn.provider(\"my-provider\", {{ base_url = \"...\", api_key = \"...\" }})\n\
             zn.set_provider(\"my-provider\")\n\
             return zn.config()"
        );
    }
    if !settings.providers.contains_key(&settings.active_provider) {
        anyhow::bail!(
            "active_provider '{}' not found in providers. Available: {}",
            settings.active_provider,
            settings
                .providers
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    // Validate each provider has a base_url
    for (name, config) in &settings.providers {
        if config.base_url.is_empty() {
            anyhow::bail!(
                "provider '{}' is missing base_url. Add:\n\
                 zn.provider(\"{}\", {{ base_url = \"https://...\", ... }})",
                name,
                name
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a temp config dir with an init.lua, then load from it.
    fn load_from_tmpdir(init_lua_content: &str) -> anyhow::Result<Settings> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("init.lua"), init_lua_content)?;
        let (settings, _hooks, _lua) = load_from_dir(dir.path())?;
        Ok(settings)
    }

    const MINIMAL_INIT_LUA: &str = r#"
        local zn = require 'zeno'
        zn.provider("anthropic", {
            api_key = "ANTHROPIC_API_KEY",
            base_url = "https://api.anthropic.com",
            default_model = "claude-sonnet-4-20250514",
        })
        zn.set_provider("anthropic")
        return zn.config()
    "#;

    #[test]
    fn test_load_minimal_config() {
        let settings = load_from_tmpdir(MINIMAL_INIT_LUA).unwrap();
        assert_eq!(settings.max_turns, 200);
        assert_eq!(settings.active_provider, "anthropic");
        assert!(
            settings.providers.contains_key("anthropic"),
            "anthropic provider should be configured"
        );
    }

    #[test]
    fn test_load_preserves_tool_defaults() {
        let settings = load_from_tmpdir(MINIMAL_INIT_LUA).unwrap();
        assert!(settings.tools.bash, "bash should default to true");
        assert!(settings.tools.web_fetch, "web_fetch should default to true");
    }

    #[test]
    fn test_tool_override() {
        let init_lua = r#"
            local zn = require 'zeno'
            zn.provider("anthropic", {
                api_key = "ANTHROPIC_API_KEY",
                base_url = "https://api.anthropic.com",
                default_model = "claude-sonnet-4-20250514",
            })
            zn.set_provider("anthropic")
            zn.tools({ web_fetch = false, bash = false })
            return zn.config()
        "#;
        let settings = load_from_tmpdir(init_lua).unwrap();
        assert!(
            !settings.tools.web_fetch,
            "web_fetch should be disabled via override"
        );
        assert!(!settings.tools.bash, "bash should be disabled");
        // Other tools should keep defaults
        assert!(settings.tools.read, "read should default to true");
    }

    #[test]
    fn test_validate_empty_providers() {
        let settings = Settings::default();
        assert!(validate(&settings).is_err());
    }

    #[test]
    fn test_validate_active_provider_not_found() {
        let mut settings = Settings::default();
        settings.providers.insert("test".into(), Default::default());
        settings.active_provider = "nonexistent".into();
        assert!(validate(&settings).is_err());
    }

    #[test]
    fn test_validate_ok() {
        let mut settings = Settings::default();
        let mut pc = ProviderConfig::default();
        pc.base_url = "https://api.test.com".into();
        settings.providers.insert("test".into(), pc);
        settings.active_provider = "test".into();
        assert!(validate(&settings).is_ok());
    }

    #[test]
    fn test_validate_provider_missing_base_url() {
        let mut settings = Settings::default();
        let pc = ProviderConfig::default(); // base_url is empty now
        settings.providers.insert("bad".into(), pc);
        settings.active_provider = "bad".into();
        assert!(validate(&settings).is_err());
    }

    #[test]
    fn test_lua_vm_basic() {
        let lua = Lua::new_with(safe_stdlibs(), LuaOptions::new()).unwrap();
        let result: i32 = lua.load("return 1 + 2").eval().unwrap();
        assert_eq!(result, 3);
    }

    #[test]
    fn test_lua_sandbox_blocks_io() {
        let lua = Lua::new_with(safe_stdlibs(), LuaOptions::new()).unwrap();
        let io_val: mlua::Value = lua.load("return io").eval().unwrap();
        assert!(matches!(io_val, mlua::Value::Nil), "io should be nil");
        let os_val: mlua::Value = lua.load("return os").eval().unwrap();
        assert!(matches!(os_val, mlua::Value::Nil), "os should be nil");
    }

    #[test]
    fn test_lua_sandbox_blocks_dofile() {
        let lua = Lua::new_with(safe_stdlibs(), LuaOptions::new()).unwrap();
        lua.globals().set("dofile", Value::Nil).unwrap();
        let result: Result<mlua::Value, _> = lua.load("return dofile").eval();
        assert!(result.is_err() || matches!(result.unwrap(), mlua::Value::Nil));
    }

    #[test]
    fn test_auxiliary_timeout_int_to_float() {
        let lua = Lua::new_with(safe_stdlibs(), LuaOptions::new()).unwrap();
        let aux = lua.create_table().unwrap();
        let vision = lua.create_table().unwrap();
        vision.set("timeout", 30i64).unwrap();
        vision.set("provider", "auto").unwrap();
        aux.set("vision", vision).unwrap();

        let coerced = coerce_auxiliary_timeouts(&lua, &aux).unwrap();
        // Verify the timeout was converted to float
        let vision_table: mlua::Table = coerced.get("vision").unwrap();
        let timeout: mlua::Value = vision_table.get("timeout").unwrap();
        assert!(
            matches!(timeout, Value::Number(_)),
            "timeout should be a float after coercion"
        );
    }

    #[test]
    fn test_auxiliary_config_from_lua() {
        let init_lua = r#"
            local zn = require 'zeno'
            zn.provider("anthropic", {
                api_key = "ANTHROPIC_API_KEY",
                base_url = "https://api.anthropic.com",
                default_model = "claude-sonnet-4-20250514",
            })
            zn.set_provider("anthropic")
            zn.auxiliaries({
                vision = { provider = "auto", model = "gemini-2.5-flash", timeout = 30 },
            })
            return zn.config()
        "#;
        let settings = load_from_tmpdir(init_lua).unwrap();
        assert_eq!(settings.auxiliary.vision.model, "gemini-2.5-flash");
        assert_eq!(settings.auxiliary.vision.timeout, 30.0);
    }

    #[test]
    fn test_auxiliaries_bulk_config_from_lua() {
        let init_lua = r#"
            local zn = require 'zeno'
            zn.provider("anthropic", {
                api_key = "ANTHROPIC_API_KEY",
                base_url = "https://api.anthropic.com",
                default_model = "claude-sonnet-4-20250514",
            })
            zn.set_provider("anthropic")
            zn.auxiliaries({
                vision = { provider = "auto", model = "gemini-2.5-flash", timeout = 30 },
                compression = { provider = "openai", model = "gpt-4o-mini", timeout = 15 },
                web_fetch = { provider = "auto", timeout = 45, max_tokens = 2048 },
            })
            return zn.config()
        "#;
        let settings = load_from_tmpdir(init_lua).unwrap();
        assert_eq!(settings.auxiliary.vision.model, "gemini-2.5-flash");
        assert_eq!(settings.auxiliary.vision.timeout, 30.0);
        assert_eq!(settings.auxiliary.compression.provider, "openai");
        assert_eq!(settings.auxiliary.compression.model, "gpt-4o-mini");
        assert_eq!(settings.auxiliary.compression.timeout, 15.0);
        assert_eq!(settings.auxiliary.web_fetch.timeout, 45.0);
        assert_eq!(settings.auxiliary.web_fetch.max_tokens, 2048);
    }

    #[test]
    fn test_require_blocks_path_traversal() {
        let lua = Lua::new_with(safe_stdlibs(), LuaOptions::new()).unwrap();
        lua.load(
            r#"
            package.path = "/tmp/?.lua"
            package.searchers = {
                function(name)
                    if name:find("%.%.") then
                        return nil, "blocked: " .. name
                    end
                    return nil, "module not found: " .. name
                end
            }
            "#,
        )
        .exec()
        .unwrap();
        let result: Result<mlua::Value, _> = lua.load("require '../etc/passwd'").eval();
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_line_number() {
        let msg = r#"[string "/home/user/.config/zeno/init.lua"]:5: attempt to index nil value"#;
        assert_eq!(extract_line_number(msg), Some(5));

        let msg_no_line = "some random error without line info";
        assert_eq!(extract_line_number(msg_no_line), None);
    }

    #[test]
    fn test_format_lua_error_with_context() {
        let lua = Lua::new_with(safe_stdlibs(), LuaOptions::new()).unwrap();
        let source = "local x = 1\nlocal y = nil\ny.field = 2\n";
        let result: Result<mlua::Value, _> = lua.load(source).set_name("test.lua").eval();
        let err = result.unwrap_err();
        let path = Path::new("test.lua");
        let formatted = format_lua_error(&err, source, path);
        assert!(formatted.contains("line"), "should show line number");
        assert!(formatted.contains("y.field"), "should show offending line");
    }

    #[test]
    fn test_no_init_lua_returns_defaults() {
        let dir = tempfile::tempdir().unwrap();
        // No init.lua created — should return defaults
        let (settings, _hooks, _lua) = load_from_dir(dir.path()).unwrap();
        assert!(settings.providers.is_empty());
        assert_eq!(settings.max_turns, 200);
    }

    #[test]
    fn test_provider_default_base_url_is_empty() {
        let pc = ProviderConfig::default();
        assert!(
            pc.base_url.is_empty(),
            "ProviderConfig::default() base_url should be empty"
        );
    }

    #[test]
    fn test_conditional_config_by_os() {
        let init_lua = r#"
local zn = require 'zeno'
zn.provider("anthropic", {
    api_key = "ANTHROPIC_API_KEY",
    base_url = "https://api.anthropic.com",
    default_model = "claude-sonnet-4-20250514",
})
zn.set_provider("anthropic")
if zn.os() == "linux" then
    zn.permissions("allow")
end
return zn.config()
"#;
        let settings = load_from_tmpdir(init_lua).unwrap();
        if cfg!(target_os = "linux") {
            assert_eq!(settings.permissions, PermissionMode::Allow);
        }
    }

    #[test]
    fn test_bash_env_from_lua() {
        let init_lua = r#"
local zn = require 'zeno'
zn.provider("anthropic", {
    api_key = "ANTHROPIC_API_KEY",
    base_url = "https://api.anthropic.com",
    default_model = "claude-sonnet-4-20250514",
})
zn.set_provider("anthropic")
zn.bash_env({
    NODE_ENV = "development",
    DOCKER_HOST = "unix:///var/run/docker.sock",
})
return zn.config()
"#;
        let settings = load_from_tmpdir(init_lua).unwrap();
        assert_eq!(
            settings.tools.bash_env.get("NODE_ENV").unwrap(),
            "development"
        );
        assert_eq!(
            settings.tools.bash_env.get("DOCKER_HOST").unwrap(),
            "unix:///var/run/docker.sock"
        );
        // Default: no bash_env
        let default_settings = load_from_tmpdir(MINIMAL_INIT_LUA).unwrap();
        assert!(
            default_settings.tools.bash_env.is_empty(),
            "bash_env should default to empty"
        );
    }

    #[test]
    fn test_web_search_config_default() {
        let settings = load_from_tmpdir(MINIMAL_INIT_LUA).unwrap();
        assert_eq!(settings.web_search_config.provider, "searxng");
        assert!(settings.web_search_config.url.is_empty());
        assert!(settings.web_search_config.api_key.is_none());
    }

    #[test]
    fn test_web_search_config_brave() {
        let init_lua = r#"
    local zn = require 'zeno'
    zn.provider("anthropic", {
        api_key = "ANTHROPIC_API_KEY",
        base_url = "https://api.anthropic.com",
        default_model = "claude-sonnet-4-20250514",
    })
    zn.set_provider("anthropic")
    zn.web_search({
        provider = "brave",
        api_key = "BRAVE_API_KEY",
    })
    return zn.config()
    "#;
        let settings = load_from_tmpdir(init_lua).unwrap();
        assert_eq!(settings.web_search_config.provider, "brave");
        assert_eq!(
            settings.web_search_config.api_key.as_deref(),
            Some("BRAVE_API_KEY")
        );
    }

    #[test]
    fn test_web_search_config_tavily() {
        let init_lua = r#"
    local zn = require 'zeno'
    zn.provider("anthropic", {
        api_key = "ANTHROPIC_API_KEY",
        base_url = "https://api.anthropic.com",
        default_model = "claude-sonnet-4-20250514",
    })
    zn.set_provider("anthropic")
    zn.web_search({
        provider = "tavily",
        api_key = "TAVILY_API_KEY",
    })
    return zn.config()
    "#;
        let settings = load_from_tmpdir(init_lua).unwrap();
        assert_eq!(settings.web_search_config.provider, "tavily");
        assert_eq!(
            settings.web_search_config.api_key.as_deref(),
            Some("TAVILY_API_KEY")
        );
    }
    #[test]
    fn test_web_search_config_custom_searxng() {
        let init_lua = r#"
            local zn = require 'zeno'
            zn.provider("anthropic", {
                api_key = "ANTHROPIC_API_KEY",
                base_url = "https://api.anthropic.com",
                default_model = "claude-sonnet-4-20250514",
            })
            zn.set_provider("anthropic")
            zn.web_search({
                provider = "searxng",
                url = "http://localhost:8888",
            })
            return zn.config()
        "#;
        let settings = load_from_tmpdir(init_lua).unwrap();
        assert_eq!(settings.web_search_config.provider, "searxng");
        assert_eq!(settings.web_search_config.url, "http://localhost:8888");
    }

    #[test]
    fn test_role_config_default() {
        let settings = load_from_tmpdir(MINIMAL_INIT_LUA).unwrap();
        assert!(settings.role.identity.is_none());
        assert!(settings.role.guidelines.is_none());
    }

    #[test]
    fn test_role_config_bulk_setter() {
        let init_lua = r#"
            local zn = require 'zeno'
            zn.provider("anthropic", {
                api_key = "ANTHROPIC_API_KEY",
                base_url = "https://api.anthropic.com",
            })
            zn.set_provider("anthropic")
            zn.role({
                identity = "You are Bob, a data engineer.",
                guidelines = "- Always validate data.\n- Prefer SQL.",
            })
            return zn.config()
        "#;
        let settings = load_from_tmpdir(init_lua).unwrap();
        assert_eq!(
            settings.role.identity.as_deref(),
            Some("You are Bob, a data engineer.")
        );
        assert!(
            settings
                .role
                .guidelines
                .as_ref()
                .unwrap()
                .contains("validate data")
        );
    }

    #[test]
    fn test_role_config_individual_setters() {
        let init_lua = r#"
            local zn = require 'zeno'
            zn.provider("anthropic", {
                api_key = "ANTHROPIC_API_KEY",
                base_url = "https://api.anthropic.com",
            })
            zn.set_provider("anthropic")
            zn.identity("You are a sysadmin.")
            return zn.config()
        "#;
        let settings = load_from_tmpdir(init_lua).unwrap();
        assert_eq!(
            settings.role.identity.as_deref(),
            Some("You are a sysadmin.")
        );
        assert!(settings.role.guidelines.is_none());
    }

    #[test]
    fn test_role_config_empty_string_ignored() {
        let init_lua = r#"
            local zn = require 'zeno'
            zn.provider("anthropic", {
                api_key = "ANTHROPIC_API_KEY",
                base_url = "https://api.anthropic.com",
            })
            zn.set_provider("anthropic")
            zn.identity("")
            return zn.config()
        "#;
        let settings = load_from_tmpdir(init_lua).unwrap();
        assert!(
            settings.role.identity.is_none(),
            "empty string should result in None"
        );
    }

    #[test]
    fn test_mcp_servers_bulk_table() {
        let init_lua = r#"
            local zn = require 'zeno'
            zn.provider("anthropic", {
                api_key = "ANTHROPIC_API_KEY",
                base_url = "https://api.anthropic.com",
            })
            zn.set_provider("anthropic")
            zn.mcp_servers({
                ["filesystem"] = { command = { "npx", "-y", "@modelcontextprotocol/server-filesystem", "/tmp" } },
                ["git"] = { command = { "npx", "-y", "@modelcontextprotocol/server-git" } },
            })
            return zn.config()
        "#;
        let settings = load_from_tmpdir(init_lua).unwrap();
        assert_eq!(settings.mcp.servers.len(), 2);
        assert!(settings.mcp.servers.contains_key("filesystem"));
        assert!(settings.mcp.servers.contains_key("git"));
        assert!(settings.mcp.servers["filesystem"].command.is_some());
    }

    #[test]
    fn test_mcp_servers_bulk_with_headers() {
        let init_lua = r#"
            local zn = require 'zeno'
            zn.provider("anthropic", {
                api_key = "ANTHROPIC_API_KEY",
                base_url = "https://api.anthropic.com",
            })
            zn.set_provider("anthropic")
            zn.mcp_servers({
                ["remote"] = {
                    url = "https://api.example.com/mcp",
                    headers = {
                        ["Authorization"] = "Bearer sk-test",
                        ["X-API-Key"] = "my-key",
                    },
                },
            })
            return zn.config()
        "#;
        let settings = load_from_tmpdir(init_lua).unwrap();
        let remote = settings.mcp.servers.get("remote").unwrap();
        assert_eq!(remote.url.as_deref(), Some("https://api.example.com/mcp"));
        assert_eq!(
            remote.headers.get("Authorization").unwrap(),
            "Bearer sk-test"
        );
        assert_eq!(remote.headers.get("X-API-Key").unwrap(), "my-key");
    }

    #[test]
    fn test_mcp_servers_bulk() {
        let init_lua = r#"
            local zn = require 'zeno'
            zn.provider("anthropic", {
                api_key = "ANTHROPIC_API_KEY",
                base_url = "https://api.anthropic.com",
            })
            zn.set_provider("anthropic")
            zn.mcp_servers({
                ["filesystem"] = { command = { "npx", "-y", "server-filesystem", "/tmp" } },
                ["git"] = { command = { "npx", "-y", "server-git" } },
            })
            return zn.config()
        "#;
        let settings = load_from_tmpdir(init_lua).unwrap();
        assert_eq!(settings.mcp.servers.len(), 2);
        assert!(settings.mcp.servers.contains_key("filesystem"));
        assert!(settings.mcp.servers.contains_key("git"));
    }

    // --- Hook integration tests ---

    /// Helper: load settings AND hooks from a temp dir.
    fn load_full_from_tmpdir(
        init_lua_content: &str,
    ) -> anyhow::Result<(
        Settings,
        Option<crate::hooks::executor::HookExecutor>,
        Arc<Mutex<Lua>>,
    )> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("init.lua"), init_lua_content)?;
        load_from_dir(dir.path())
    }

    #[test]
    fn test_no_hooks_when_none_registered() {
        let (_settings, hooks, _lua) = load_full_from_tmpdir(MINIMAL_INIT_LUA).unwrap();
        assert!(hooks.is_none());
    }

    #[tokio::test]
    async fn test_hook_pre_tool_use_from_lua() {
        let init_lua = r#"
            local zn = require 'zeno'
            zn.provider("anthropic", {
                api_key = "ANTHROPIC_API_KEY",
                base_url = "https://api.anthropic.com",
            })
            zn.set_provider("anthropic")
            zn.hook("pre_tool_use", function(ctx)
                if ctx.tool_name == "bash" then
                    return { block = "bash is forbidden" }
                end
            end)
            return zn.config()
        "#;
        let (settings, hooks, _lua) = load_full_from_tmpdir(init_lua).unwrap();
        assert_eq!(settings.active_provider, "anthropic");
        let he = hooks.expect("hooks should be registered");
        assert_eq!(he.hook_count(), 1);
        assert!(he.has_hooks_for(crate::hooks::types::HookEvent::PreToolUse));

        // Test firing the hook
        let ctx = he.build_context().unwrap();
        ctx.set("tool_name", "bash").unwrap();
        let result = he
            .execute_first_block(crate::hooks::types::HookEvent::PreToolUse, &ctx)
            .await;
        assert_eq!(result, Some("bash is forbidden".into()));
    }

    #[tokio::test]
    async fn test_hook_pre_llm_call_inject_context() {
        let init_lua = r#"
            local zn = require 'zeno'
            zn.provider("anthropic", {
                api_key = "ANTHROPIC_API_KEY",
                base_url = "https://api.anthropic.com",
            })
            zn.set_provider("anthropic")
            zn.hook("pre_llm_call", function(ctx)
                return { inject_context = "Extra context from hook" }
            end)
            return zn.config()
        "#;
        let (_settings, hooks, _lua) = load_full_from_tmpdir(init_lua).unwrap();
        let he = hooks.unwrap();
        let ctx = he.build_context().unwrap();
        let results = he.execute_pre_llm(&ctx).await;
        assert_eq!(results, vec!["Extra context from hook".to_string()]);
    }

    #[tokio::test]
    async fn test_hook_user_message_modify() {
        let init_lua = r#"
            local zn = require 'zeno'
            zn.provider("anthropic", {
                api_key = "ANTHROPIC_API_KEY",
                base_url = "https://api.anthropic.com",
            })
            zn.set_provider("anthropic")
            zn.hook("user_message", function(ctx)
                return { modified_input = "prefix: " .. ctx.input }
            end)
            return zn.config()
        "#;
        let (_settings, hooks, _lua) = load_full_from_tmpdir(init_lua).unwrap();
        let he = hooks.unwrap();
        let ctx = he.build_context().unwrap();
        ctx.set("input", "hello").unwrap();
        let result = he.execute_user_message(&ctx).await;
        assert_eq!(result, Some("prefix: hello".into()));
    }

    #[test]
    fn test_hook_unknown_event_warns_and_skips() {
        let init_lua = r#"
            local zn = require 'zeno'
            zn.provider("anthropic", {
                api_key = "ANTHROPIC_API_KEY",
                base_url = "https://api.anthropic.com",
            })
            zn.set_provider("anthropic")
            zn.hook("nonexistent_event", function() end)
            return zn.config()
        "#;
        let (_settings, hooks, _lua) = load_full_from_tmpdir(init_lua).unwrap();
        // Unknown events are silently skipped; no hooks should be registered
        assert!(hooks.is_none());
    }

    #[tokio::test]
    async fn test_hook_multiple_same_event() {
        let init_lua = r#"
            local zn = require 'zeno'
            zn.provider("anthropic", {
                api_key = "ANTHROPIC_API_KEY",
                base_url = "https://api.anthropic.com",
            })
            zn.set_provider("anthropic")
            -- First hook: returns nil (continue)
            zn.hook("pre_tool_use", function(ctx) end)
            -- Second hook: blocks
            zn.hook("pre_tool_use", function(ctx)
                return { block = "blocked by second hook" }
            end)
            return zn.config()
        "#;
        let (_settings, hooks, _lua) = load_full_from_tmpdir(init_lua).unwrap();
        let he = hooks.unwrap();
        assert_eq!(he.hook_count(), 2);
        let ctx = he.build_context().unwrap();
        let result = he
            .execute_first_block(crate::hooks::types::HookEvent::PreToolUse, &ctx)
            .await;
        assert_eq!(result, Some("blocked by second hook".into()));
    }

    #[test]
    fn test_hook_json_library_available() {
        let init_lua = r#"
            local zn = require 'zeno'
            zn.provider("anthropic", {
                api_key = "ANTHROPIC_API_KEY",
                base_url = "https://api.anthropic.com",
            })
            zn.set_provider("anthropic")
            zn.hook("pre_tool_use", function(ctx)
                local encoded = "ok"
                if json ~= nil then
                    encoded = json.encode({a=1})
                end
                return nil
            end)
            return zn.config()
        "#;
        let (_settings, hooks, _lua) = load_full_from_tmpdir(init_lua).unwrap();
        let he = hooks.unwrap();
        assert!(he.has_hooks_for(crate::hooks::types::HookEvent::PreToolUse));
    }
}
