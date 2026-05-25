//! JSON-file-backed implementation of [`SubAgentGraphStore`].
//!
//! Writes to `~/.local/share/zeno/graph/edges.json` with cross-process file
//! locking via `fs2`.  No user setup required — the file and directory are
//! created automatically on first use.
//!
//! ## Concurrency notes
//!
//! All write operations use `tokio::sync::RwLock` which yields to the runtime
//! when the lock is contended, avoiding worker-thread stalls.  The lock is held
//! during serialization + file I/O (write + atomic rename), but these are fast
//! operations for the expected data sizes (hundreds of edges).  If this store
//! is ever used at very high concurrency or with very large edge files, the I/O
//! phase could be moved to `tokio::task::spawn_blocking` with a generation counter
//! to prevent stale-write races.

use async_trait::async_trait;
use chrono::Utc;
use fs2::FileExt;
use std::fs;
use std::path::{Path, PathBuf};
use tokio::sync::RwLock;

use crate::store::agent_graph::{
    EdgeRecord, EdgeStatus, StoreError, StoreResult, SubAgentGraphStore,
};

// ---------------------------------------------------------------------------
// In-memory store
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct JsonEdgeStore {
    edges: Vec<EdgeRecord>,
    path: PathBuf,
}

// ---------------------------------------------------------------------------
// Public type
// ---------------------------------------------------------------------------

/// JSON-file-backed graph store.
///
/// Thread-safe via `RwLock` and cross-process-safe via `fs2::FileExt`.
#[derive(Debug)]
pub struct JsonAgentGraphStore {
    inner: RwLock<JsonEdgeStore>,
}

impl JsonAgentGraphStore {
    /// Create (or open) an existing graph store rooted at `zeno_home`.
    ///
    /// Creates `{zeno_home}/graph/edges.json` if it does not exist.
    pub fn new(zeno_home: &Path) -> StoreResult<Self> {
        let graph_dir = zeno_home.join("graph");
        let path = graph_dir.join("edges.json");

        fs::create_dir_all(&graph_dir).map_err(|e| StoreError::Internal {
            message: format!(
                "failed to create graph dir '{}': {}",
                graph_dir.display(),
                e
            ),
        })?;

        // Acquire cross-process lock for the entire read-cleanup-write cycle,
        // preventing races when two zeno processes start simultaneously.
        let lock_path = path.with_extension("json.lock");
        let _lock = {
            let f = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .open(&lock_path)
                .map_err(|e| StoreError::Internal {
                    message: format!("failed to open lock '{}': {}", lock_path.display(), e),
                })?;
            f.lock_exclusive().map_err(|e| StoreError::Internal {
                message: format!("failed to lock '{}': {}", lock_path.display(), e),
            })?;
            f // held until end of scope
        };

        let mut edges = if path.exists() {
            let content = fs::read_to_string(&path).map_err(|e| StoreError::Internal {
                message: format!("failed to read '{}': {}", path.display(), e),
            })?;
            serde_json::from_str::<Vec<EdgeRecord>>(&content).map_err(|e| StoreError::Internal {
                message: format!("failed to parse '{}': {}", path.display(), e),
            })?
        } else {
            // Write an empty array to seed the file (lock already held).
            if let Ok(json) = serde_json::to_string_pretty(&Vec::<EdgeRecord>::new()) {
                let tmp_path = path.with_extension("json.tmp");
                let _ = std::fs::write(&tmp_path, &json)
                    .and_then(|_| std::fs::rename(&tmp_path, &path));
            }
            Vec::new()
        };

        // Cleanup: remove closed edges older than 7 days to prevent unbounded growth.
        let cutoff = Utc::now() - chrono::TimeDelta::days(7);
        let before = edges.len();
        edges.retain(|e| {
            match e.status {
                EdgeStatus::Closed => e.closed_at.map_or(true, |ts| ts >= cutoff),
                EdgeStatus::Open => true, // keep open edges regardless of age
            }
        });
        if edges.len() < before {
            tracing::info!(
                removed = before - edges.len(),
                remaining = edges.len(),
                "Cleaned up stale closed edges from graph store"
            );
            // Persist the cleaned set immediately (lock still held by _lock).
            if let Ok(json) = serde_json::to_string_pretty(&edges) {
                let tmp_path = path.with_extension("json.tmp");
                let _ = std::fs::write(&tmp_path, &json)
                    .and_then(|_| std::fs::rename(&tmp_path, &path));
            }
        }

        Ok(Self {
            inner: RwLock::new(JsonEdgeStore { edges, path }),
        })
    }

    /// Write pre-serialized JSON to the edges file with cross-process locking.
    ///
    /// Uses a dedicated lock file (`edges.json.lock`) so the exclusive lock
    /// survives the atomic rename — locking the data file directly would be
    /// ineffective because the rename creates a new inode that a concurrent
    /// writer could lock immediately.
    ///
    /// **Must only be called while holding the write lock** to prevent the
    /// in-process interleaving race: two concurrent writers could serialize
    /// their in-memory state, then one's `write_json` could overwrite the
    /// other's output. Serializing inside the write lock ensures the on-disk
    /// file always reflects the latest in-memory state.
    fn write_json(path: &Path, json: &str) -> StoreResult<()> {
        // Lock a dedicated lock file (never renamed) so the lock is visible
        // cross-process even after the data file is atomically replaced.
        let lock_path = path.with_extension("json.lock");
        let lock_file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(&lock_path)
            .map_err(|e| StoreError::Internal {
                message: format!("failed to open lock '{}': {}", lock_path.display(), e),
            })?;
        lock_file
            .lock_exclusive()
            .map_err(|e| StoreError::Internal {
                message: format!("failed to lock '{}': {}", lock_path.display(), e),
            })?;

        // Write to a temp file first, then atomically rename.
        let tmp_path = path.with_extension("json.tmp");
        {
            let file = fs::File::create(&tmp_path).map_err(|e| StoreError::Internal {
                message: format!("failed to create '{}': {}", tmp_path.display(), e),
            })?;
            use std::io::Write;
            let mut writer = std::io::BufWriter::new(file);
            writer
                .write_all(json.as_bytes())
                .map_err(|e| StoreError::Internal {
                    message: format!("failed to write '{}': {}", tmp_path.display(), e),
                })?;
        }
        fs::rename(&tmp_path, path).map_err(|e| StoreError::Internal {
            message: format!("failed to rename to '{}': {}", path.display(), e),
        })?;

        // Lock is released when `lock_file` is dropped (end of scope).
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Trait impl
// ---------------------------------------------------------------------------

#[async_trait]
impl SubAgentGraphStore for JsonAgentGraphStore {
    async fn upsert_edge(
        &self,
        parent_id: &str,
        child_id: &str,
        status: EdgeStatus,
        task_index: usize,
        goal: &str,
    ) -> StoreResult<()> {
        let now = Utc::now();
        let mut guard = self.inner.write().await;

        // Replace existing edge for this child, or push a new one.
        if let Some(existing) = guard.edges.iter_mut().find(|e| e.child_id == child_id) {
            existing.parent_id = parent_id.to_string();
            existing.status = status;
            existing.task_index = task_index;
            existing.goal = goal.to_string();
            if status == EdgeStatus::Closed {
                existing.closed_at = Some(now);
            } else {
                // Clear closed_at when re-opening a previously closed edge.
                existing.closed_at = None;
            }
        } else {
            guard.edges.push(EdgeRecord {
                parent_id: parent_id.to_string(),
                child_id: child_id.to_string(),
                status,
                task_index,
                goal: goal.to_string(),
                created_at: now,
                closed_at: None,
            });
        }

        // Serialize + write to disk inside the write lock to prevent
        // interleaving with another writer (see write_json docstring).
        let json =
            serde_json::to_string_pretty(&guard.edges).map_err(|e| StoreError::Internal {
                message: format!("serialization error: {}", e),
            })?;
        Self::write_json(&guard.path, &json)
    }

    async fn set_edge_status(&self, child_id: &str, status: EdgeStatus) -> StoreResult<()> {
        let mut guard = self.inner.write().await;

        if let Some(edge) = guard.edges.iter_mut().find(|e| e.child_id == child_id) {
            edge.status = status;
            if status == EdgeStatus::Closed {
                edge.closed_at = Some(Utc::now());
            }
        }
        // Missing child = no-op (matching codex convention)

        // Serialize + write to disk inside the write lock.
        let json =
            serde_json::to_string_pretty(&guard.edges).map_err(|e| StoreError::Internal {
                message: format!("serialization error: {}", e),
            })?;
        Self::write_json(&guard.path, &json)
    }

    async fn list_children_with_details(
        &self,
        parent_id: &str,
        status_filter: Option<EdgeStatus>,
    ) -> StoreResult<Vec<EdgeRecord>> {
        let guard = self.inner.read().await;

        let records: Vec<EdgeRecord> = guard
            .edges
            .iter()
            .filter(|e| e.parent_id == parent_id)
            .filter(|e| match status_filter {
                Some(s) => e.status == s,
                None => true,
            })
            .cloned()
            .collect();

        Ok(records)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn graph_store() -> (JsonAgentGraphStore, TempDir) {
        let tmp = TempDir::new().expect("tempdir should be created");
        let store = JsonAgentGraphStore::new(tmp.path()).expect("store should init");
        (store, tmp)
    }

    #[tokio::test]
    async fn missing_child_set_status_is_noop() {
        let (store, _tmp) = graph_store();

        // Should not error
        store
            .set_edge_status("nonexistent", EdgeStatus::Closed)
            .await
            .expect("should be noop");
    }
}
