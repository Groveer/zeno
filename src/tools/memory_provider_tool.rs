//! Memory provider tool — a bridge tool that delegates to the external memory
//! provider's tool handling.
//!
//! Each tool schema from an external memory provider gets one of these bridge
//! tools registered in the tool registry. When the LLM calls one of these tools,
//! this bridge forwards the call to the MemoryManager, which dispatches it to
//! the active external provider.

use std::sync::Arc;
use tokio::sync::Mutex;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::base::{Tool, ToolContext, ToolError};
use crate::memory::manager::MemoryManager;

/// A tool that delegates execution to the external memory provider.
pub struct MemoryProviderTool {
    tool_name: String,
    schema: Value,
    manager: Arc<Mutex<MemoryManager>>,
}

impl MemoryProviderTool {
    pub fn new(tool_name: String, schema: Value, manager: Arc<Mutex<MemoryManager>>) -> Self {
        Self {
            tool_name,
            schema,
            manager,
        }
    }
}

#[async_trait]
impl Tool for MemoryProviderTool {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn schema(&self) -> Value {
        self.schema.clone()
    }

    async fn execute(&self, arguments: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
        let manager = self.manager.lock().await;
        match manager
            .handle_external_tool_call(&self.tool_name, &arguments)
            .await
        {
            Ok(result) => Ok(result),
            Err(e) => Ok(json!({
                "success": false,
                "error": e.to_string()
            })
            .to_string()),
        }
    }
}
