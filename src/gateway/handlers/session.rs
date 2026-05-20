//! Session lifecycle handlers (session.create, session.list).
//!
//! Manages conversation sessions: creating new sessions, listing recent ones,
//! and switching between sessions.
//!
//! These handlers are registered via `Gateway::register_handler()` and follow
//! the `MethodHandler` trait pattern for JSON-RPC-style dispatch.
//!
//! ## Future
//!
//! - `session.restore(id)` — restore a previous session from disk
//! - `session.delete(id)` — delete a saved session

use serde_json::{Value, json};

use crate::gateway::dispatch::{MethodError, MethodHandler};

/// Handler for `session.create` — creates a new conversation session.
#[allow(dead_code)]
pub struct SessionCreateHandler;

impl MethodHandler for SessionCreateHandler {
    fn handle(&mut self, params: &Value) -> Result<Value, MethodError> {
        let title = params
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("Untitled Session");
        Ok(json!({
            "status": "ok",
            "session": {
                "id": format!("session-{}", std::process::id()),
                "title": title,
            }
        }))
    }
}

/// Handler for `session.list` — lists recent sessions.
#[allow(dead_code)]
pub struct SessionListHandler;

impl MethodHandler for SessionListHandler {
    fn handle(&mut self, _params: &Value) -> Result<Value, MethodError> {
        // Placeholder: in Phase 4, this will query the session index on disk.
        Ok(json!({
            "status": "ok",
            "sessions": [],
            "total": 0,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_session_create() {
        let mut handler = SessionCreateHandler;
        let result = handler.handle(&json!({"title": "Test"})).unwrap();
        assert_eq!(result["status"], "ok");
        assert!(
            result["session"]["id"]
                .as_str()
                .unwrap()
                .contains("session-")
        );
        assert_eq!(result["session"]["title"], "Test");
    }

    #[test]
    fn test_session_create_default_title() {
        let mut handler = SessionCreateHandler;
        let result = handler.handle(&json!({})).unwrap();
        assert_eq!(result["session"]["title"], "Untitled Session");
    }

    #[test]
    fn test_session_list() {
        let mut handler = SessionListHandler;
        let result = handler.handle(&json!({})).unwrap();
        assert_eq!(result["status"], "ok");
        assert_eq!(result["total"], 0);
    }
}
