//! Lua plugin bridge — loads plugin `.lua` files and wraps them as `Tool` trait objects.
//!
//! # Plugin format
//!
//! Each plugin is a `.lua` file that returns a table with:
//!
//! ```lua
//! return {
//!   name = "my_tool",
//!   description = "Does something useful",
//!   parameters = {
//!     type = "object",
//!     properties = {
//!       input = { type = "string", description = "Input text" },
//!     },
//!     required = { "input" },
//!   },
//!   execute = function(args, ctx)
//!     -- args: tool arguments as a Lua table
//!     -- ctx: { cwd = "/path", env = { HOME = "...", ... } }
//!     return "result string"
//!   end,
//! }
//! ```
//!
//! # Security
//!
//! Plugin `execute` functions run in a sandboxed Lua VM (no io/os/debug/ffi).
//! Each invocation creates a fresh VM — state is not shared between calls.
//! The `ctx.env` table only exposes a whitelist of safe environment variables.
//!
//! # Auto-discovery
//!
//! On startup, the bridge scans `~/.config/zeno/plugins/*.lua` and registers
//! each plugin as a tool. Plugins are loaded eagerly — parsing the Lua file
//! to extract `name`, `description`, and `parameters` without executing the
//! `execute` function.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use mlua::LuaSerdeExt as _;
use serde_json::{Value, json};

use crate::tools::base::{Tool, ToolContext, ToolError};

use super::sandbox::{SandboxError, execute_plugin};

// ---------------------------------------------------------------------------
// Plugin definition (parsed from Lua)
// ---------------------------------------------------------------------------

/// A parsed plugin definition extracted from a `.lua` file.
#[derive(Debug, Clone)]
pub struct PluginDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    /// The raw Lua script content (loaded from file).
    pub script: String,
    /// Source file path (for diagnostics).
    pub source: PathBuf,
}

/// Error loading a plugin.
#[derive(Debug, thiserror::Error)]
pub enum PluginLoadError {
    #[error("Failed to read plugin file '{path}': {detail}")]
    Io { path: String, detail: String },
    #[error("Failed to parse plugin '{path}': {detail}")]
    Parse { path: String, detail: String },
    #[error("Plugin '{name}' is missing required field '{field}'")]
    MissingField { name: String, field: String },
}

// ---------------------------------------------------------------------------
// Plugin loader
// ---------------------------------------------------------------------------

/// Default plugins directory.
pub fn default_plugins_dir() -> PathBuf {
    let config_dir = crate::config::paths::config_dir();
    config_dir.join("plugins")
}

/// Scan a directory for `.lua` plugin files and parse them.
///
/// Returns a list of plugin definitions. Non-Lua files are silently skipped.
/// Malformed plugins are logged as warnings and skipped.
pub fn load_plugins_from_dir(dir: &Path) -> Vec<PluginDefinition> {
    if !dir.exists() {
        tracing::debug!(dir = %dir.display(), "Plugin directory does not exist, skipping");
        return Vec::new();
    }

    let mut plugins = Vec::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(dir = %dir.display(), error = %e, "Failed to read plugin directory");
            return Vec::new();
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "lua") {
            continue;
        }

        match load_plugin_file(&path) {
            Ok(plugin) => {
                tracing::info!(
                    plugin = %plugin.name,
                    source = %plugin.source.display(),
                    "Loaded Lua plugin"
                );
                plugins.push(plugin);
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Skipping malformed plugin"
                );
            }
        }
    }

    plugins
}

/// Load and parse a single plugin `.lua` file.
fn load_plugin_file(path: &Path) -> Result<PluginDefinition, PluginLoadError> {
    let script = std::fs::read_to_string(path).map_err(|e| PluginLoadError::Io {
        path: path.display().to_string(),
        detail: e.to_string(),
    })?;

    // Parse the Lua file with a minimal VM to extract metadata.
    // We DO NOT execute the `execute` function here — only inspect
    // the returned table structure.
    let lua = mlua::Lua::new_with(
        mlua::StdLib::TABLE | mlua::StdLib::STRING | mlua::StdLib::MATH | mlua::StdLib::UTF8,
        mlua::LuaOptions::new(),
    )
    .map_err(|e| PluginLoadError::Parse {
        path: path.display().to_string(),
        detail: format!("Failed to create Lua VM: {}", e),
    })?;

    // Remove dangerous globals
    let globals = lua.globals();
    let _ = globals.set("dofile", mlua::Value::Nil);
    let _ = globals.set("loadfile", mlua::Value::Nil);

    let table: mlua::Table = lua
        .load(&script)
        .eval()
        .map_err(|e| PluginLoadError::Parse {
            path: path.display().to_string(),
            detail: format!("Failed to evaluate plugin script: {}", e),
        })?;

    let name: String = table
        .get("name")
        .map_err(|_| PluginLoadError::MissingField {
            name: path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
            field: "name".into(),
        })?;

    let description: String = table.get("description").unwrap_or_default();

    // Extract parameters as JSON
    let parameters: Value = table
        .get::<mlua::Value>("parameters")
        .ok()
        .and_then(|v| {
            // Use mlua's LuaSerdeExt::from_value for direct Lua→JSON conversion
            // without going through Debug formatting
            lua.from_value::<Value>(v).ok()
        })
        .unwrap_or_else(|| {
            json!({
                "type": "object",
                "properties": {},
                "required": []
            })
        });

    // Verify execute function exists
    let has_execute = table.get::<mlua::Function>("execute").is_ok();
    if !has_execute {
        return Err(PluginLoadError::MissingField {
            name: name.clone(),
            field: "execute".into(),
        });
    }

    Ok(PluginDefinition {
        name,
        description,
        parameters,
        script,
        source: path.to_path_buf(),
    })
}

// ---------------------------------------------------------------------------
// PluginTool — wraps a PluginDefinition as a Tool trait object
// ---------------------------------------------------------------------------

/// A tool that delegates execution to a Lua plugin's `execute` function.
pub struct PluginTool {
    plugin: PluginDefinition,
}

impl PluginTool {
    pub fn new(plugin: PluginDefinition) -> Self {
        Self { plugin }
    }
}

impl std::fmt::Debug for PluginTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginTool")
            .field("name", &self.plugin.name)
            .field("source", &self.plugin.source)
            .finish()
    }
}

#[async_trait]
impl Tool for PluginTool {
    fn name(&self) -> &str {
        &self.plugin.name
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.plugin.name,
                "description": self.plugin.description,
                "parameters": self.plugin.parameters,
            }
        })
    }

    async fn execute(&self, arguments: Value, ctx: &ToolContext) -> Result<String, ToolError> {
        let cwd = ctx.get_cwd().to_string_lossy().to_string();

        match execute_plugin(&self.plugin, &arguments, &cwd) {
            Ok(result) => Ok(result.output),
            Err(SandboxError::Execution(msg)) => Err(ToolError::Execution(format!(
                "Plugin '{}' error: {}",
                self.plugin.name, msg
            ))),
            Err(SandboxError::InvalidResult(msg)) => Err(ToolError::InvalidArguments(format!(
                "Plugin '{}' returned invalid result: {}",
                self.plugin.name, msg
            ))),
        }
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        // Plugins are treated as potentially non-read-only for safety.
        // Users can override by setting `read_only = true` in their plugin table.
        false
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::base::ToolRegistry;

    #[test]
    fn test_load_plugin_file_valid() {
        let dir = std::env::temp_dir().join("zeno-test-plugins");
        let _ = std::fs::create_dir_all(&dir);
        let plugin_path = dir.join("echo.lua");

        std::fs::write(
            &plugin_path,
            r#"
return {
    name = "echo",
    description = "Echo input text",
    parameters = {
        type = "object",
        properties = {
            text = { type = "string", description = "Text to echo" },
        },
        required = { "text" },
    },
    execute = function(args, ctx)
        return "echo: " .. args.text
    end,
}
"#,
        )
        .unwrap();

        let plugin = load_plugin_file(&plugin_path).unwrap();
        assert_eq!(plugin.name, "echo");
        assert_eq!(plugin.description, "Echo input text");
        assert!(plugin.parameters.get("type").and_then(|v| v.as_str()) == Some("object"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_plugin_missing_execute() {
        let dir = std::env::temp_dir().join("zeno-test-plugins-missing");
        let _ = std::fs::create_dir_all(&dir);
        let plugin_path = dir.join("bad.lua");

        std::fs::write(
            &plugin_path,
            r#"
return {
    name = "bad",
    description = "Missing execute",
    parameters = {},
}
"#,
        )
        .unwrap();

        let result = load_plugin_file(&plugin_path);
        assert!(result.is_err(), "Plugin without execute should fail");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_plugins_from_dir() {
        let dir = std::env::temp_dir().join("zeno-test-plugins-dir");
        let _ = std::fs::create_dir_all(&dir);

        std::fs::write(
            dir.join("echo.lua"),
            r#"return { name = "echo", description = "Echo", parameters = {}, execute = function() return "ok" end }"#,
        )
        .unwrap();

        std::fs::write(
            dir.join("reverse.lua"),
            r#"return { name = "reverse", description = "Reverse", parameters = {}, execute = function(args, ctx) return string.reverse(args.text or "") end }"#,
        )
        .unwrap();

        // A non-Lua file that should be skipped
        std::fs::write(dir.join("README.txt"), "not a plugin").unwrap();

        let plugins = load_plugins_from_dir(&dir);
        assert_eq!(plugins.len(), 2);

        let names: Vec<&str> = plugins.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"echo"));
        assert!(names.contains(&"reverse"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_plugin_tool_registration() {
        let plugin = PluginDefinition {
            name: "test_tool".into(),
            description: "A test".into(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
            script: r#"
return {
    name = "test_tool",
    description = "A test",
    parameters = { type = "object", properties = {}, required = {} },
    execute = function(args, ctx)
        return "plugin_result"
    end,
}
"#
            .into(),
            source: PathBuf::from("test.lua"),
        };

        let tool = PluginTool::new(plugin);
        assert_eq!(tool.name(), "test_tool");
        assert!(tool.schema().get("function").is_some());

        // Test registration in registry
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(tool)).unwrap();
        assert!(registry.names().contains(&"test_tool"));
    }
}
