//! Prompt submission handlers (prompt.submit).
//!
//! Handles user prompt submissions. Unlike slash commands (which are handled
//! by `handle_input()` → `dispatch_slash()`), the `prompt.submit` handler
//! is designed for JSON-RPC-style dispatch from external TUI frontends.
//!
//! ## Design
//!
//! When a prompt arrives via the `prompt.submit` method:
//! 1. The handler extracts the text and optional images
//! 2. Calls `Gateway::submit_query()` to start an engine query
//! 3. Returns immediately with a query ID

use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::engine::query_engine::QueryEngine;
use crate::gateway::dispatch::{MethodError, MethodHandler};

/// Handler for `prompt.submit` — submits a user prompt to the engine.
#[allow(dead_code)]
pub struct PromptSubmitHandler {
    engine: Arc<Mutex<QueryEngine>>,
}

impl PromptSubmitHandler {
    pub fn new(engine: Arc<Mutex<QueryEngine>>) -> Self {
        Self { engine }
    }
}

impl MethodHandler for PromptSubmitHandler {
    fn handle(&mut self, params: &Value) -> Result<Value, MethodError> {
        let text = params
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MethodError::InvalidParams("missing 'text' field".into()))?;

        let images: Vec<String> = params
            .get("images")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        // Note: In JSON-RPC mode (future StdioTransport), the engine query
        // is launched by the RPC server thread, not the sync main loop.
        // For now, this handler validates the input and returns a query receipt.
        Ok(json!({
            "status": "accepted",
            "query": {
                "text": text,
                "image_count": images.len(),
                "id": format!("q-{}", std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()),
            }
        }))
    }
}
