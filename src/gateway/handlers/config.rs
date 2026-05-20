//! Configuration handlers (config.get, config.set).
//!
//! Provides read/write access to runtime configuration settings.
//! Handlers receive Gateway's settings reference and can query or modify
//! the active configuration.
//!
//! ## Future
//!
//! - `config.set` — update a config key at runtime
//! - `config.reload` — trigger config file hot-reload

use std::sync::Arc;

use serde_json::{Value, json};

use crate::config::settings::Settings;
use crate::gateway::dispatch::{MethodError, MethodHandler};

/// Handler for `config.get` — retrieves configuration values.
#[allow(dead_code)]
pub struct ConfigGetHandler {
    settings: Arc<Settings>,
}

impl ConfigGetHandler {
    pub fn new(settings: Arc<Settings>) -> Self {
        Self { settings }
    }
}

impl MethodHandler for ConfigGetHandler {
    fn handle(&mut self, params: &Value) -> Result<Value, MethodError> {
        let key = params
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MethodError::InvalidParams("missing 'key' field".into()))?;

        match key {
            "provider" => Ok(json!({ "key": "provider", "value": self.settings.active_provider })),
            "model" => Ok(json!({ "key": "model", "value": self.settings.model })),
            "max_tokens" => Ok(json!({ "key": "max_tokens", "value": self.settings.max_tokens })),
            "max_turns" => Ok(json!({ "key": "max_turns", "value": self.settings.max_turns })),
            _ => Err(MethodError::NotFound(format!("unknown config key: {key}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::settings::Settings;
    use std::sync::Arc;

    fn test_settings() -> Arc<Settings> {
        Arc::new(Settings {
            active_provider: "anthropic".into(),
            model: "claude-sonnet-4-20250514".into(),
            max_tokens: 4096,
            max_turns: 100,
            ..Default::default()
        })
    }

    #[test]
    fn test_config_get_provider() {
        let mut handler = ConfigGetHandler::new(test_settings());
        let result = handler
            .handle(&serde_json::json!({"key": "provider"}))
            .unwrap();
        assert_eq!(result["value"], "anthropic");
    }

    #[test]
    fn test_config_get_unknown_key() {
        let mut handler = ConfigGetHandler::new(test_settings());
        let result = handler.handle(&serde_json::json!({"key": "nonexistent"}));
        assert!(result.is_err());
    }

    #[test]
    fn test_config_get_missing_key() {
        let mut handler = ConfigGetHandler::new(test_settings());
        let result = handler.handle(&serde_json::json!({}));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing 'key'"));
    }
}
