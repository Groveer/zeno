//! Input history persistence — identity-scoped load/save from disk.
//!
//! History entries are persisted as a JSON array to:
//! - `~/.local/share/zeno/input_history.json` (no identity / default)
//! - `~/.local/share/zeno/input_history/{identity}.json` (per-identity)
//!
//! Uses atomic write (temp file + rename) to prevent partial reads by concurrent instances.

use std::path::Path;

use crate::config::paths;

/// Maximum number of history entries to persist to disk.
pub const MAX_PERSISTED_HISTORY: usize = 2000;

/// Ensure the parent directory of `path` exists.
/// Uses `create_dir_all` which is a no-op if the directory already exists.
fn ensure_parent(path: &Path) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
}

/// Load input history from disk for an optional identity.
///
/// When `identity` is Some and non-empty, loads per-identity history from
/// `input_history/{identity}.json` under the data directory. Otherwise loads the
/// default `input_history.json` from the data directory.
///
/// Returns an empty Vec if the file doesn't exist or is corrupted, so the user
/// never loses the ability to type.
pub fn load_history(identity: Option<&str>) -> Vec<String> {
    let path = paths::input_history_path(identity);
    if !path.exists() {
        return Vec::new();
    }
    match std::fs::read_to_string(&path) {
        Ok(json) => match serde_json::from_str::<Vec<String>>(&json) {
            Ok(history) => history,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "Failed to parse input history, starting fresh"
                );
                Vec::new()
            }
        },
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "Failed to read input history, starting fresh"
            );
            Vec::new()
        }
    }
}

/// Save input history to disk for an optional identity.
///
/// When `identity` is Some and non-empty, saves to `input_history/{identity}.json`
/// under the data directory. Otherwise saves to `input_history.json` in the data
/// directory.
///
/// Truncates to MAX_PERSISTED_HISTORY entries. Uses atomic write (temp file + rename)
/// to prevent partial reads by other concurrent Zeno instances.
pub fn save_history(history: &[String], identity: Option<&str>) {
    let path = paths::input_history_path(identity);
    let truncated: Vec<&str> = history
        .iter()
        .take(MAX_PERSISTED_HISTORY)
        .map(|s| s.as_str())
        .collect();
    match serde_json::to_string(&truncated) {
        Ok(json) => {
            // Ensure the parent directory exists (e.g. `input_history/` for per-identity files).
            // This is critical — without it, `std::fs::write` silently fails on first use
            // of a per-identity history, and the old history is lost when switching back.
            ensure_parent(&path);

            // Atomic write: write to temp file, then rename to final path.
            // This prevents other instances from reading a partially-written file.
            let tmp_path = path.with_extension("json.tmp");
            if let Err(e) = std::fs::write(&tmp_path, &json) {
                tracing::warn!(
                    error = %e,
                    path = %tmp_path.display(),
                    "Failed to save input history"
                );
                return;
            }
            if let Err(e) = std::fs::rename(&tmp_path, &path) {
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "Failed to atomically save input history"
                );
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to serialize input history");
        }
    }
}
