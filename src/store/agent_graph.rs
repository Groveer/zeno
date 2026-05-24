//! Storage-neutral trait for persisted sub-agent parent/child topology.
//!
//! Records `delegate_task` invocations as directional edges so the engine and
//! TUI can navigate the session tree — who spawned whom, which sub-agents are
//! still open vs. closed, and full descendant walks.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Edge status
// ---------------------------------------------------------------------------

/// Lifecycle status attached to a directional sub-agent spawn edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeStatus {
    /// The sub-agent is still running (or resumable).
    Open,
    /// The sub-agent has completed / been closed.
    Closed,
}

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Result type for graph store operations.
pub type StoreResult<T> = Result<T, StoreError>;

/// Errors from graph store operations.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Implementation-level failure.
    #[error("graph store internal error: {message}")]
    Internal {
        /// User-facing explanation.
        message: String,
    },
}

// ---------------------------------------------------------------------------
// Record
// ---------------------------------------------------------------------------

/// A single directional parent→child edge with metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeRecord {
    /// Identifier of the parent agent (session id).
    pub parent_id: String,
    /// Identifier of the child agent (uuid).
    pub child_id: String,
    /// Current lifecycle status.
    pub status: EdgeStatus,
    /// Index within a batch (0 for single tasks).
    pub task_index: usize,
    /// The goal / task description passed to the sub-agent.
    pub goal: String,
    /// ISO-8601 timestamp of edge creation.
    pub created_at: DateTime<Utc>,
    /// ISO-8601 timestamp of when the edge was closed (None if still open).
    pub closed_at: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Storage-neutral persistence for sub-agent spawn topology.
///
/// Implementations MUST return stable ordering so callers can merge persisted
/// graph state with live in-memory state without nondeterministic output.
#[async_trait]
pub trait SubAgentGraphStore: Send + Sync {
    /// Insert or replace a directional parent→child edge.
    ///
    /// A child has at most one persisted parent. Re-inserting the same child
    /// updates the parent, status, and metadata to the supplied values.
    async fn upsert_edge(
        &self,
        parent_id: &str,
        child_id: &str,
        status: EdgeStatus,
        task_index: usize,
        goal: &str,
    ) -> StoreResult<()>;

    /// Update the lifecycle status of an existing edge by child id.
    ///
    /// Implementations should treat missing children as a successful no-op.
    async fn set_edge_status(&self, child_id: &str, status: EdgeStatus) -> StoreResult<()>;

    /// List direct children of a parent agent.
    ///
    /// When `status_filter` is `Some`, only edges with that exact status are
    /// returned. When `None`, all children regardless of status are returned.
    ///
    /// Prefer `list_children_with_details` when full edge metadata is needed.
    #[allow(
        dead_code,
        reason = "public API — used in tests, kept for trait completeness"
    )]
    async fn list_children(
        &self,
        parent_id: &str,
        status_filter: Option<EdgeStatus>,
    ) -> StoreResult<Vec<String>>;

    /// List direct children of a parent agent, returning full edge records.
    ///
    /// Semantically identical to `list_children` but avoids the N+1 query
    /// pattern of calling `get_edge` on each returned child id.
    /// Callers that need record metadata (goal, timestamps, etc.) should
    /// prefer this method.
    async fn list_children_with_details(
        &self,
        parent_id: &str,
        status_filter: Option<EdgeStatus>,
    ) -> StoreResult<Vec<EdgeRecord>>;

    /// List descendant agent ids breadth-first by creation order.
    ///
    /// `status_filter` applies to every traversed edge. For example,
    /// `Some(Open)` walks only open edges, so descendants under a closed edge
    /// are excluded even if their own edge is open. `None` walks every edge.
    #[allow(
        dead_code,
        reason = "public API — unused currently but part of the trait contract"
    )]
    async fn list_descendants(
        &self,
        root_id: &str,
        status_filter: Option<EdgeStatus>,
    ) -> StoreResult<Vec<String>>;

    /// Look up the full edge record for a child agent.
    #[allow(
        dead_code,
        reason = "public API — unused currently but part of the trait contract"
    )]
    async fn get_edge(&self, child_id: &str) -> StoreResult<Option<EdgeRecord>>;
}

// ---------------------------------------------------------------------------
// No-op implementation (fallback when store initialization fails)
// ---------------------------------------------------------------------------

/// A [`SubAgentGraphStore`] that silently discards all data.
///
/// Used as a graceful fallback when `JsonAgentGraphStore::new()` fails
/// during engine initialization, so the engine never has to carry an
/// `Option` that might be `None`.
#[derive(Debug, Clone, Copy)]
pub struct NoopGraphStore;

#[async_trait]
impl SubAgentGraphStore for NoopGraphStore {
    async fn upsert_edge(
        &self,
        _parent_id: &str,
        _child_id: &str,
        _status: EdgeStatus,
        _task_index: usize,
        _goal: &str,
    ) -> StoreResult<()> {
        Ok(())
    }

    async fn set_edge_status(&self, _child_id: &str, _status: EdgeStatus) -> StoreResult<()> {
        Ok(())
    }

    async fn list_children(
        &self,
        _parent_id: &str,
        _status_filter: Option<EdgeStatus>,
    ) -> StoreResult<Vec<String>> {
        Ok(Vec::new())
    }

    async fn list_children_with_details(
        &self,
        _parent_id: &str,
        _status_filter: Option<EdgeStatus>,
    ) -> StoreResult<Vec<EdgeRecord>> {
        Ok(Vec::new())
    }

    async fn list_descendants(
        &self,
        _root_id: &str,
        _status_filter: Option<EdgeStatus>,
    ) -> StoreResult<Vec<String>> {
        Ok(Vec::new())
    }

    async fn get_edge(&self, _child_id: &str) -> StoreResult<Option<EdgeRecord>> {
        Ok(None)
    }
}
