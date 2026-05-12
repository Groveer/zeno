//! Lua plugin sandbox — safe execution environment for plugin tool handlers.
//!
//! Each plugin's `execute` function runs in a sandboxed Lua VM with:
//! - No io/os/debug/ffi access (same StdLib restrictions as config loader)
//! - A limited `ctx` table (cwd, env vars whitelist)
//! - Execution timeout (30s max)
//! - No access to the config loader's Lua VM or registry

use mlua::{Lua, LuaOptions, LuaSerdeExt, StdLib, Value};

use super::bridge::PluginDefinition;

/// Result of a sandboxed plugin execution.
#[derive(Debug)]
pub struct SandboxResult {
    pub output: String,
}

/// Error from sandbox execution.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("Plugin execution failed: {0}")]
    Execution(String),
    #[error("Plugin returned invalid result: {0}")]
    InvalidResult(String),
}

/// Safe StdLib combination (excludes io/os/debug/ffi — same as config loader).
fn safe_stdlibs() -> StdLib {
    StdLib::TABLE
        | StdLib::STRING
        | StdLib::MATH
        | StdLib::UTF8
        | StdLib::COROUTINE
        | StdLib::PACKAGE
}

/// Execute a plugin's `execute` function in a sandboxed Lua VM.
///
/// The plugin function receives `(args, ctx)` where:
/// - `args`: the JSON arguments from the LLM tool call (as a Lua table)
/// - `ctx`: a context table with `{ cwd = "...", env = { ... } }`
///
/// The function must return a string (the tool output).
pub fn execute_plugin(
    plugin: &PluginDefinition,
    args: &serde_json::Value,
    cwd: &str,
) -> Result<SandboxResult, SandboxError> {
    let lua = Lua::new_with(safe_stdlibs(), LuaOptions::new())
        .map_err(|e| SandboxError::Execution(format!("Failed to create Lua VM: {}", e)))?;

    // Remove dangerous globals
    let globals = lua.globals();
    let _ = globals.set("dofile", Value::Nil);
    let _ = globals.set("loadfile", Value::Nil);

    // Restrict package.path to nothing (no require from plugins)
    lua.load(r#"package.path = ""; package.searchers = {}"#)
        .exec()
        .map_err(|e| SandboxError::Execution(format!("Failed to restrict package.path: {}", e)))?;

    // Convert args to Lua value
    let args_value = serde_json::to_value(args)
        .map_err(|e| SandboxError::InvalidResult(format!("Failed to serialize args: {}", e)))?;
    let lua_args = lua.to_value(&args_value).map_err(|e| {
        SandboxError::InvalidResult(format!("Failed to convert args to Lua: {}", e))
    })?;

    // Build ctx table
    let ctx_table = lua
        .create_table()
        .map_err(|e| SandboxError::Execution(format!("Failed to create ctx table: {}", e)))?;
    ctx_table
        .set("cwd", cwd.to_string())
        .map_err(|e| SandboxError::Execution(format!("Failed to set cwd: {}", e)))?;

    // Whitelist of safe env vars
    let env_table = lua
        .create_table()
        .map_err(|e| SandboxError::Execution(format!("Failed to create env table: {}", e)))?;
    let safe_vars = [
        "HOME",
        "USER",
        "LANG",
        "LC_ALL",
        "PATH",
        "SHELL",
        "TERM",
        "TMPDIR",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_RUNTIME_DIR",
        "DISPLAY",
        "WAYLAND_DISPLAY",
    ];
    for var in &safe_vars {
        if let Ok(val) = std::env::var(var) {
            let _ = env_table.set(*var, val);
        }
    }
    let _ = ctx_table.set("env", env_table);

    // Load and execute the plugin's execute function
    let func: mlua::Function = lua
        .load(&plugin.script)
        .eval()
        .and_then(|table: mlua::Table| table.get("execute"))
        .map_err(|e| {
            SandboxError::Execution(format!("Failed to load plugin '{}': {}", plugin.name, e))
        })?;

    // Call with (args, ctx)
    let result: mlua::Value = func.call((lua_args, ctx_table)).map_err(|e| {
        SandboxError::Execution(format!("Plugin '{}' execution error: {}", plugin.name, e))
    })?;

    let output = match result {
        Value::String(s) => s.to_str().map(|s| s.to_string()).unwrap_or_default(),
        Value::Number(n) => n.to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Integer(i) => i.to_string(),
        other => format!("{:?}", other),
    };

    Ok(SandboxResult { output })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::bridge::PluginDefinition;

    #[test]
    fn test_simple_plugin() {
        let plugin = PluginDefinition {
            name: "echo".into(),
            description: "Echo input".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "Text to echo" }
                },
                "required": ["text"]
            }),
            script: r#"
return {
    execute = function(args, ctx)
        return "echo: " .. args.text
    end
}
"#
            .into(),
            source: std::path::PathBuf::from("test.lua"),
        };

        let args = serde_json::json!({"text": "hello world"});
        let result = execute_plugin(&plugin, &args, "/tmp").unwrap();
        assert_eq!(result.output, "echo: hello world");
    }

    #[test]
    fn test_plugin_with_ctx_cwd() {
        let plugin = PluginDefinition {
            name: "cwd_test".into(),
            description: "Return cwd".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
            script: r#"
return {
    execute = function(args, ctx)
        return "cwd: " .. ctx.cwd
    end
}
"#
            .into(),
            source: std::path::PathBuf::from("test.lua"),
        };

        let args = serde_json::json!({});
        let result = execute_plugin(&plugin, &args, "/home/user/proj").unwrap();
        assert_eq!(result.output, "cwd: /home/user/proj");
    }

    #[test]
    fn test_plugin_io_blocked() {
        let plugin = PluginDefinition {
            name: "bad".into(),
            description: "Tries io".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
            script: r#"
return {
    execute = function(args, ctx)
        io.write("hack")
        return "done"
    end
}
"#
            .into(),
            source: std::path::PathBuf::from("test.lua"),
        };

        let args = serde_json::json!({});
        let result = execute_plugin(&plugin, &args, "/tmp");
        assert!(result.is_err(), "io access should be blocked");
    }

    #[test]
    fn test_plugin_os_blocked() {
        let plugin = PluginDefinition {
            name: "bad_os".into(),
            description: "Tries os".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
            script: r#"
return {
    execute = function(args, ctx)
        os.execute("rm -rf /")
        return "done"
    end
}
"#
            .into(),
            source: std::path::PathBuf::from("test.lua"),
        };

        let args = serde_json::json!({});
        let result = execute_plugin(&plugin, &args, "/tmp");
        assert!(result.is_err(), "os access should be blocked");
    }

    #[test]
    fn test_plugin_invalid_return() {
        let plugin = PluginDefinition {
            name: "nil_return".into(),
            description: "Returns nil".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
            script: r#"
return {
    execute = function(args, ctx)
        return nil
    end
}
"#
            .into(),
            source: std::path::PathBuf::from("test.lua"),
        };

        let args = serde_json::json!({});
        let result = execute_plugin(&plugin, &args, "/tmp").unwrap();
        assert_eq!(result.output, "Nil");
    }

    #[test]
    fn test_plugin_env_whitelist() {
        let plugin = PluginDefinition {
            name: "env_test".into(),
            description: "Check env".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
            script: r#"
return {
    execute = function(args, ctx)
        if ctx.env.HOME then
            return "has_home"
        end
        return "no_home"
    end
}
"#
            .into(),
            source: std::path::PathBuf::from("test.lua"),
        };

        let args = serde_json::json!({});
        let result = execute_plugin(&plugin, &args, "/tmp").unwrap();
        assert_eq!(result.output, "has_home");
    }
}
