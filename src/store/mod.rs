//! Sub-agent topology graph store — persist parent/child spawn relationships.
//!
//! See `docs/agent-graph-store-absorption.md` for design rationale.

pub mod agent_graph;
pub mod json_graph;

pub use agent_graph::*;
pub use json_graph::JsonAgentGraphStore;

use std::sync::Arc;

/// Create a shared sub-agent graph store with `NoopGraphStore` fallback.
///
/// Returns `Some(Arc<dyn SubAgentGraphStore>)` — either a real JSON-backed store
/// or a no-op implementation on failure. This ensures the engine never has to
/// carry an `Option` that might be `None`.
///
/// # Usage
///
/// ```ignore
/// let store = crate::store::create_graph_store(&crate::config::paths::data_dir());
/// engine.graph_store = Some(store);
/// ```
pub fn create_graph_store(zeno_home: &std::path::Path) -> Arc<dyn SubAgentGraphStore> {
    JsonAgentGraphStore::new(zeno_home)
        .map(|s| Arc::new(s) as Arc<dyn SubAgentGraphStore>)
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "Failed to initialize sub-agent graph store");
            Arc::new(agent_graph::NoopGraphStore)
        })
}
