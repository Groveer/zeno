//! Input history persistence — load/save from disk.
//!
//! History entries are persisted as a JSON array to `~/.config/zeno/session_history.json`.
//! Uses atomic write (temp file + rename) to prevent partial reads by concurrent instances.

use crate::config::paths;

/// Maximum number of history entries to persist to disk.
pub const MAX_PERSISTED_HISTORY: usize = 2000;

/// Load input history from disk. Returns an empty Vec if the file doesn't
/// exist or is corrupted, so the user never loses the ability to type.
pub fn load_history() -> Vec<String> {
    let path = paths::session_history_path();
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
                    "Failed to parse session history, starting fresh"
                );
                Vec::new()
            }
        },
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "Failed to read session history, starting fresh"
            );
            Vec::new()
        }
    }
}

/// Save input history to disk. Truncates to MAX_PERSISTED_HISTORY entries.
/// Uses atomic write (temp file + rename) to prevent partial reads by
/// other concurrent Zeno instances.
pub fn save_history(history: &[String]) {
    let path = paths::session_history_path();
    let truncated: Vec<&str> = history
        .iter()
        .take(MAX_PERSISTED_HISTORY)
        .map(|s| s.as_str())
        .collect();
    match serde_json::to_string(&truncated) {
        Ok(json) => {
            // Atomic write: write to temp file, then rename to final path.
            // This prevents other instances from reading a partially-written file.
            let tmp_path = path.with_extension("json.tmp");
            if let Err(e) = std::fs::write(&tmp_path, &json) {
                tracing::warn!(
                    error = %e,
                    path = %tmp_path.display(),
                    "Failed to save session history"
                );
                return;
            }
            if let Err(e) = std::fs::rename(&tmp_path, &path) {
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "Failed to atomically save session history"
                );
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to serialize session history");
        }
    }
}
