//! Cross-agent file state coordination — staleness detection & per-path locking.
//!
//! Prevents mangled edits when concurrent sub-agents (same process, same
//! filesystem) touch the same file.  Complements the single-agent path-overlap
//! check — this module catches the case where sub-agent B writes a file that
//! sub-agent A already read, so A's next write would overwrite B's changes
//! with stale content.
//!
//! Design
//! ------
//! A process-wide singleton `FileStateRegistry` tracks, per resolved path:
//!
//! * per-agent read stamps: `{task_id: {path: (mtime, read_ts)}}`
//! * last writer globally: `{path: (task_id, write_ts)}`
//! * per-path `tokio::sync::Mutex` for read→modify→write critical sections
//!
//! Three public hooks are used by the file tools:
//!
//! * `record_read(task_id, path)` — called by read_file
//! * `note_write(task_id, path)` — called after write_file / edit / patch
//! * `check_stale(task_id, path)` — called BEFORE write_file / edit / patch

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

/// A per-path lock guard.  Keeps the Arc alive and holds the lock.
/// Drops the path lock when it goes out of scope.
pub struct PathLock {
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl PathLock {
    async fn new(arc: Arc<Mutex<()>>) -> Self {
        let guard = arc.lock_owned().await;
        Self { _guard: guard }
    }
}

// ---------------------------------------------------------------------------
// Read stamp: (mtime, read_timestamp)
// ---------------------------------------------------------------------------
pub struct FileStateRegistry {
    /// Per-task reads: task_id -> (resolved_path -> ReadStamp)
    reads: Mutex<HashMap<String, HashMap<String, ReadStamp>>>,
    /// Global last-writer map: resolved_path -> (task_id, write_timestamp)
    last_writer: Mutex<HashMap<String, (String, u64)>>,
    /// Per-path locks for read→modify→write serialization
    path_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl FileStateRegistry {
    pub fn new() -> Self {
        Self {
            reads: Mutex::new(HashMap::new()),
            last_writer: Mutex::new(HashMap::new()),
            path_locks: Mutex::new(HashMap::new()),
        }
    }

    /// Record that `task_id` read `resolved_path`.
    pub async fn record_read(&self, task_id: &str, resolved: &str) {
        let mtime = mtime_secs(resolved).unwrap_or(0);
        let now = monotonic_ms();
        let mut reads = self.reads.lock().await;
        reads
            .entry(task_id.to_string())
            .or_default()
            .insert(resolved.to_string(), (mtime, now));
    }

    /// Record a successful write by `task_id` to `resolved_path`.
    /// Updates the global last-writer map AND the writer's own read stamp
    /// (a write is an implicit read — the agent now knows the current content).
    pub async fn note_write(&self, task_id: &str, resolved: &str) {
        let mtime = mtime_secs(resolved).unwrap_or(0);
        let now = monotonic_ms();
        let mut last_writer = self.last_writer.lock().await;
        last_writer.insert(resolved.to_string(), (task_id.to_string(), now));
        drop(last_writer);

        // Writer's own view is now up-to-date.
        let mut reads = self.reads.lock().await;
        reads
            .entry(task_id.to_string())
            .or_default()
            .insert(resolved.to_string(), (mtime, now));
    }

    /// Check whether a write to `resolved_path` by `task_id` would be stale.
    ///
    /// Returns `Some(warning_message)` if the file was modified since the
    /// agent last read it.  Returns `None` if the write is safe.
    ///
    /// Three staleness classes, in order of severity:
    ///   1. Sibling sub-agent wrote this file after this agent's last read.
    ///   2. External / unknown change (mtime differs from our last read).
    ///   3. Agent never read the file (write-without-read).
    pub async fn check_stale(&self, task_id: &str, resolved: &str) -> Option<String> {
        let reads = self.reads.lock().await;
        let stamp = reads.get(task_id).and_then(|m| m.get(resolved)).copied();
        let last_writer = self.last_writer.lock().await;
        let writer_info = last_writer.get(resolved).cloned();
        drop(reads);
        drop(last_writer);

        let current_mtime = mtime_secs(resolved);

        // Case 1: sibling sub-agent modified after our last read.
        if let Some((writer_tid, writer_ts)) = writer_info
            && writer_tid != task_id
        {
            if let Some((_read_mtime, read_ts)) = stamp {
                if writer_ts > read_ts {
                    return Some(format!(
                        "'{}' was modified by sibling agent '{}' after this agent last read it. \
                             Re-read the file before writing to avoid overwriting changes.",
                        resolved, writer_tid
                    ));
                }
            } else {
                return Some(format!(
                    "'{}' was modified by sibling agent '{}' but this agent never read it. \
                         Read the file before writing.",
                    resolved, writer_tid
                ));
            }
        }

        // Case 2: external / unknown modification (mtime drifted).
        if let Some((read_mtime, _read_ts)) = stamp
            && let Some(current) = current_mtime
            && current != read_mtime
        {
            return Some(format!(
                "'{}' was modified on disk since you last read it (external edit \
                         or unrecorded writer). Re-read the file before writing.",
                resolved
            ));
        }

        // Case 3: agent never read the file.
        if stamp.is_none() {
            return Some(format!(
                "'{}' was not read by this agent. Read the file first so you can \
                 write an informed edit.",
                resolved
            ));
        }

        None
    }

    /// Acquire the per-path lock for `resolved`.
    ///
    /// Returns a guard that releases the lock when dropped.  Same process,
    /// same filesystem — threads/tasks on the same path serialize.  Different
    /// paths proceed in parallel.
    pub async fn lock_path(&self, resolved: &str) -> PathLock {
        let mut locks = self.path_locks.lock().await;
        let arc = locks
            .entry(resolved.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        drop(locks);
        PathLock::new(arc).await
    }
}

// ---------------------------------------------------------------------------
// Singleton
// ---------------------------------------------------------------------------

use std::sync::LazyLock;

static REGISTRY: LazyLock<FileStateRegistry> = LazyLock::new(FileStateRegistry::new);

pub fn registry() -> &'static FileStateRegistry {
    &REGISTRY
}

// ---------------------------------------------------------------------------
// Read stamp: (mtime, read_timestamp)
// ---------------------------------------------------------------------------
type ReadStamp = (u64, u64); // (mtime_secs, monotonic_read_ts)

/// Get file mtime in seconds since epoch, or `None` if the file doesn't exist.
fn mtime_secs(path: &str) -> Option<u64> {
    std::fs::metadata(path).ok().and_then(|m| {
        m.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
    })
}

/// Monotonic timestamp in milliseconds (for ordering, not absolute time).
fn monotonic_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ---------------------------------------------------------------------------
// Convenience wrappers
// ---------------------------------------------------------------------------

/// Record a file read for staleness tracking.
pub async fn record_read(task_id: &str, path: &Path) {
    registry()
        .record_read(task_id, &path.to_string_lossy())
        .await
}

/// Record a file write for staleness tracking.
pub async fn note_write(task_id: &str, path: &Path) {
    registry()
        .note_write(task_id, &path.to_string_lossy())
        .await
}

/// Check whether a write would be stale.  Returns a warning string or `None`.
pub async fn check_stale(task_id: &str, path: &Path) -> Option<String> {
    registry()
        .check_stale(task_id, &path.to_string_lossy())
        .await
}

/// Acquire the per-path lock for `path`.
pub async fn lock_path(path: &Path) -> PathLock {
    registry().lock_path(&path.to_string_lossy()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::LazyLock;

    #[tokio::test]
    async fn test_record_read_and_stale() {
        let reg = FileStateRegistry::new();
        let p = "/nonexistent-file-for-test";

        // Never read → stale
        assert!(reg.check_stale("agent_a", p).await.is_some());

        // After read → not stale
        reg.record_read("agent_a", p).await;
        assert!(reg.check_stale("agent_a", p).await.is_none());
    }

    #[tokio::test]
    async fn test_sibling_write_stale() {
        let reg = FileStateRegistry::new();
        let p = "/nonexistent-file-for-test";

        reg.record_read("agent_a", p).await;
        // Small delay to ensure write timestamp > read timestamp
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        reg.note_write("agent_b", p).await;

        // agent_a's view is now stale because agent_b wrote after agent_a read
        let warning = reg.check_stale("agent_a", p).await;
        assert!(warning.is_some(), "should detect sibling write staleness");
        assert!(warning.unwrap().contains("sibling agent"));
    }

    #[tokio::test]
    async fn test_own_write_not_stale() {
        let reg = FileStateRegistry::new();
        let p = "/nonexistent-file-for-test";

        reg.record_read("agent_a", p).await;
        reg.note_write("agent_a", p).await;

        // Own write should not trigger staleness
        assert!(reg.check_stale("agent_a", p).await.is_none());
    }

    #[tokio::test]
    async fn test_path_lock_serializes() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let counter = Arc::new(AtomicUsize::new(0));
        let counter2 = counter.clone();
        let p = "/test-lock";

        let lock1 = registry().lock_path(p).await;
        let handle = tokio::spawn(async move {
            let _lock2 = registry().lock_path(p).await;
            counter2.fetch_add(1, Ordering::SeqCst);
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(counter.load(Ordering::SeqCst), 0, "task should be blocked");
        drop(lock1);
        handle.await.unwrap();
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "task should have run after lock release"
        );
    }
}
