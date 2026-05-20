//! Path utilities for zeno config, data, and log directories.
//!
//! `config_dir()`, `config_path()`, `data_dir()`, `cache_dir()`, `log_dir()`,
//! `ensure_log_dir()`, and `cleanup_old_logs()` are actively used. The remaining
//! functions are reserved for future migration/setup commands.

use std::path::PathBuf;

/// Cross-platform fallback: home directory, or `/tmp` if unavailable.
fn home_fallback() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// Returns the zeno config directory following XDG spec.
pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| home_fallback().join(".config"))
        .join("zeno")
}

/// Returns the path to the main config file (Lua).
pub fn config_path() -> PathBuf {
    config_dir().join("init.lua")
}

/// Returns the global memory directory (`~/.config/zeno/memory`).
/// MEMORY.md is stored here. Shared across all projects.
pub fn memory_dir() -> PathBuf {
    config_dir().join("memory")
}

/// Returns the memory directory for a specific identity.
/// When `identity` is Some, returns `~/.config/zeno/memory/{identity}/`.
/// When None, returns the global `~/.config/zeno/memory/` (backward compatible).
pub fn memory_dir_for_identity(identity: Option<&str>) -> PathBuf {
    let base = memory_dir();
    match identity {
        Some(id) if !id.is_empty() => base.join(id),
        _ => base,
    }
}

/// Returns the data directory for zeno (memory, sessions, etc.).
pub fn data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| home_fallback().join(".local").join("share"))
        .join("zeno")
}

/// Returns the cache directory for zeno (logs, temporary data).
pub fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| home_fallback().join(".cache"))
        .join("zeno")
}

/// Returns the path to the input history file.
/// Stores the last N submitted inputs for persistence across sessions.
/// When identity is Some, uses `~/.local/share/zeno/input_history/{identity}.json`.
/// When None, uses `~/.local/share/zeno/input_history.json`.
///
/// On first access after migration, automatically copies old files from
/// `~/.config/zeno/input_history*` to the new data dir location, then removes
/// the old originals.
pub fn input_history_path(identity: Option<&str>) -> PathBuf {
    let dir = data_dir();
    let old_dir = config_dir();
    match identity {
        Some(id) if !id.is_empty() => {
            let sub = dir.join("input_history");
            let new_path = sub.join(format!("{}.json", id));
            migrate_input_history_file(&new_path, identity, &old_dir);
            new_path
        }
        _ => {
            let path = dir.join("input_history.json");
            migrate_input_history_file(&path, identity, &old_dir);
            path
        }
    }
}

/// One-shot migration: move old config-dir input_history to data dir.
/// Tries `rename` first (atomic on same filesystem), falls back to
/// copy + delete (cross-device). Only runs if the new path doesn't exist
/// yet but the old path does.
///
/// `old_config_dir` is injected for testability (e.g. [`config_dir`] in production).
fn migrate_input_history_file(
    new_path: &std::path::Path,
    identity: Option<&str>,
    old_config_dir: &std::path::Path,
) {
    if new_path.exists() {
        return;
    }
    let old_path = match identity {
        Some(id) if !id.is_empty() => old_config_dir
            .join("input_history")
            .join(format!("{}.json", id)),
        _ => old_config_dir.join("input_history.json"),
    };
    if !old_path.exists() {
        return;
    }
    tracing::info!(
        from = %old_path.display(),
        to = %new_path.display(),
        "Migrating input_history to data directory"
    );
    if let Err(e) = std::fs::create_dir_all(new_path.parent().unwrap()) {
        tracing::warn!(error = %e, "Failed to create input_history data dir");
        return;
    }
    if let Err(_e) = std::fs::rename(&old_path, new_path) {
        // Fallback to copy + delete if rename fails (cross-device)
        if let Err(e) = std::fs::copy(&old_path, new_path) {
            tracing::warn!(error = %e, "Failed to copy input_history");
            return;
        }
        if let Err(e) = std::fs::remove_file(&old_path) {
            tracing::warn!(error = %e, "Failed to remove old input_history");
        }
    }
    // Clean up empty old subdirectory if applicable
    // - Identity case: old_path = config/zeno/input_history/{id}.json,
    //   parent = config/zeno/input_history/ → try remove_dir(parent) (succeeds only if empty)
    // - Non-identity case: old_path = config/zeno/input_history.json,
    //   parent = config/zeno/ → try remove_dir(input_history/ subdir) (succeeds only if empty)
    // remove_dir silently no-ops on non-empty directories.
    if let Some(parent) = old_path.parent() {
        let sub = parent.join("input_history");
        if sub.exists() {
            let _ = std::fs::remove_dir(&sub);
        }
        let _ = std::fs::remove_dir(parent);
    }
}

/// Returns the sessions directory for multi-session storage.
/// Each session is saved as `{id}.json` inside this directory.
pub fn sessions_dir() -> PathBuf {
    let dir = data_dir().join("sessions");
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// Returns the path to the session index file.
/// A JSON array of `SessionIndexEntry` for quick listing without parsing full sessions.
pub fn session_index_path() -> PathBuf {
    sessions_dir().join("index.json")
}

/// Returns the cache directory for zeno log files.
/// Uses `~/.cache/zeno/logs/` per XDG cache spec.
///
/// On first access after migration, automatically copies old log files from
/// `~/.config/zeno/logs/` to the new cache dir location, then removes the old originals.
pub fn log_dir() -> PathBuf {
    let dir = cache_dir().join("logs");
    migrate_logs(&dir, &config_dir());
    dir
}

/// Ensures the log directory exists, returns its path.
pub fn ensure_log_dir() -> anyhow::Result<PathBuf> {
    let dir = log_dir();
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Remove log files older than `retention_days` days from the log directory.
/// `tracing_appender::rolling::daily` produces files named `zeno.log.YYYY-MM-DD`.
pub fn cleanup_old_logs(retention_days: u64) {
    let dir = log_dir();
    if !dir.exists() {
        return;
    }
    let cutoff = std::time::SystemTime::now()
        - std::time::Duration::from_secs(retention_days * 24 * 60 * 60);

    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none() && path.file_name().is_some() {
                // Current active log file (zeno.log), skip
                continue;
            }
            if let Ok(metadata) = path.metadata()
                && let Ok(modified) = metadata.modified()
                && modified < cutoff
            {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// One-shot migration: copy old config-dir logs to cache dir, then delete originals.
/// Only runs if the new cache log dir doesn't exist but the old config log dir does.
///
/// `old_config_dir` is injected for testability (e.g. [`config_dir`] in production).
fn migrate_logs(new_dir: &std::path::Path, old_config_dir: &std::path::Path) {
    if new_dir.exists() {
        return;
    }
    let old_dir = old_config_dir.join("logs");
    if !old_dir.exists() {
        return;
    }
    tracing::info!(
        from = %old_dir.display(),
        to = %new_dir.display(),
        "Migrating logs to cache directory"
    );
    // Create the new log directory
    if let Err(e) = std::fs::create_dir_all(new_dir) {
        tracing::warn!(error = %e, "Failed to create log cache dir");
        return;
    }
    // Move all files from old logs dir to new logs dir
    if let Ok(entries) = std::fs::read_dir(&old_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() || path.is_symlink() {
                let file_name = path.file_name().unwrap();
                let dest = new_dir.join(file_name);
                if let Err(_e) = std::fs::rename(&path, &dest) {
                    // Fallback to copy + delete if rename fails (cross-device)
                    if let Err(e2) = std::fs::copy(&path, &dest) {
                        tracing::warn!(error = %e2, file = %file_name.to_string_lossy(), "Failed to copy log file");
                        continue;
                    }
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
    // Remove old directory after successful migration
    if let Err(e) = std::fs::remove_dir(&old_dir) {
        tracing::warn!(error = %e, "Failed to remove old log directory");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_dir_for_identity_none() {
        let result = memory_dir_for_identity(None);
        let expected = memory_dir();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_memory_dir_for_identity_empty() {
        let result = memory_dir_for_identity(Some(""));
        let expected = memory_dir();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_memory_dir_for_identity_some() {
        let result = memory_dir_for_identity(Some("dev"));
        let expected = memory_dir().join("dev");
        assert_eq!(result, expected);
    }

    #[test]
    fn test_memory_dir_for_identity_with_special_chars() {
        let result = memory_dir_for_identity(Some("my-identity_v2"));
        let expected = memory_dir().join("my-identity_v2");
        assert_eq!(result, expected);
    }

    #[test]
    fn test_memory_dir_for_identity_with_spaces() {
        // Spaces in identity names should be allowed
        let result = memory_dir_for_identity(Some("my identity"));
        let expected = memory_dir().join("my identity");
        assert_eq!(result, expected);
    }

    #[test]
    fn test_config_dir_is_absolute() {
        let dir = config_dir();
        assert!(
            dir.is_absolute(),
            "config_dir should return an absolute path"
        );
    }

    #[test]
    fn test_memory_dir_is_absolute() {
        let dir = memory_dir();
        assert!(
            dir.is_absolute(),
            "memory_dir should return an absolute path"
        );
    }

    #[test]
    fn test_data_dir_is_absolute() {
        let dir = data_dir();
        assert!(dir.is_absolute(), "data_dir should return an absolute path");
    }

    #[test]
    fn test_cache_dir_is_absolute() {
        let dir = cache_dir();
        assert!(
            dir.is_absolute(),
            "cache_dir should return an absolute path"
        );
    }

    #[test]
    fn test_log_dir_is_absolute() {
        let dir = log_dir();
        assert!(dir.is_absolute(), "log_dir should return an absolute path");
    }

    // ---------------------------------------------------------------------------
    // Migration tests — use tempfile to avoid touching real user directories
    // ---------------------------------------------------------------------------

    #[test]
    fn test_migrate_input_history_non_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let old_cfg = tmp.path().join("config/zeno");
        let new_data = tmp.path().join("data/zeno");

        // Create old-style file
        std::fs::create_dir_all(&old_cfg).unwrap();
        std::fs::write(old_cfg.join("input_history.json"), "[\"hello\"]").unwrap();

        let new_path = new_data.join("input_history.json");
        migrate_input_history_file(&new_path, None, &old_cfg);

        assert!(new_path.exists(), "non-identity file should be migrated");
        assert!(
            !old_cfg.join("input_history.json").exists(),
            "old file should be removed"
        );
        assert_eq!(
            std::fs::read_to_string(&new_path).unwrap(),
            "[\"hello\"]",
            "content preserved"
        );
    }

    #[test]
    fn test_migrate_input_history_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let old_cfg = tmp.path().join("config/zeno");
        let new_data = tmp.path().join("data/zeno");

        // Create old identity-scoped file
        std::fs::create_dir_all(old_cfg.join("input_history")).unwrap();
        std::fs::write(old_cfg.join("input_history/dev.json"), "[\"dev data\"]").unwrap();

        let new_path = new_data.join("input_history/dev.json");
        migrate_input_history_file(&new_path, Some("dev"), &old_cfg);

        assert!(new_path.exists(), "identity file should be migrated");
        assert!(
            !old_cfg.join("input_history/dev.json").exists(),
            "old identity file should be removed"
        );
        // Empty subdirectory should be cleaned up
        assert!(
            !old_cfg.join("input_history").exists(),
            "empty input_history subdirectory should be removed"
        );
    }

    #[test]
    fn test_migrate_input_history_skips_if_new_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let old_cfg = tmp.path().join("config/zeno");
        let new_data = tmp.path().join("data/zeno");

        std::fs::create_dir_all(&old_cfg).unwrap();
        std::fs::write(old_cfg.join("input_history.json"), "[\"old\"]").unwrap();
        std::fs::create_dir_all(&new_data).unwrap();
        std::fs::write(new_data.join("input_history.json"), "[\"newer\"]").unwrap();

        migrate_input_history_file(&new_data.join("input_history.json"), None, &old_cfg);

        // Should NOT overwrite existing new file
        assert_eq!(
            std::fs::read_to_string(new_data.join("input_history.json")).unwrap(),
            "[\"newer\"]",
            "existing file should not be overwritten"
        );
        // Old file should remain since migration was skipped
        assert!(
            old_cfg.join("input_history.json").exists(),
            "old file should remain when migration is skipped"
        );
    }

    #[test]
    fn test_migrate_input_history_skips_if_no_old() {
        let tmp = tempfile::tempdir().unwrap();
        let old_cfg = tmp.path().join("config/zeno");
        let new_data = tmp.path().join("data/zeno");

        // No old file created at all
        let new_path = new_data.join("input_history.json");
        migrate_input_history_file(&new_path, None, &old_cfg);

        assert!(
            !new_path.exists(),
            "should not create new file when no old file exists"
        );
    }

    #[test]
    fn test_migrate_input_history_keeps_subdir_if_other_identities_remain() {
        let tmp = tempfile::tempdir().unwrap();
        let old_cfg = tmp.path().join("config/zeno");

        // Two identity files in the same subdirectory
        std::fs::create_dir_all(old_cfg.join("input_history")).unwrap();
        std::fs::write(old_cfg.join("input_history/dev.json"), "[\"dev\"]").unwrap();
        std::fs::write(old_cfg.join("input_history/prod.json"), "[\"prod\"]").unwrap();

        let new_data = tmp.path().join("data/zeno");
        let new_path = new_data.join("input_history/dev.json");

        // Migrate only dev
        migrate_input_history_file(&new_path, Some("dev"), &old_cfg);

        // dev should be migrated and removed from old
        assert!(!old_cfg.join("input_history/dev.json").exists());
        // prod should remain
        assert!(old_cfg.join("input_history/prod.json").exists());
        // Subdirectory should still exist (not empty)
        assert!(
            old_cfg.join("input_history").exists(),
            "subdirectory with remaining files should persist"
        );
    }

    #[test]
    fn test_migrate_logs_moves_files() {
        let tmp = tempfile::tempdir().unwrap();
        let old_cfg = tmp.path().join("config/zeno");
        let new_cache = tmp.path().join("cache/zeno");

        // Create old log dir with log files
        std::fs::create_dir_all(old_cfg.join("logs")).unwrap();
        std::fs::write(old_cfg.join("logs/zeno.log.2025-01-01"), "log content 1").unwrap();
        std::fs::write(old_cfg.join("logs/zeno.log.2025-01-02"), "log content 2").unwrap();

        let new_dir = new_cache.join("logs");
        migrate_logs(&new_dir, &old_cfg);

        assert!(
            new_dir.join("zeno.log.2025-01-01").exists(),
            "first log should be migrated"
        );
        assert!(
            new_dir.join("zeno.log.2025-01-02").exists(),
            "second log should be migrated"
        );
        assert!(
            !old_cfg.join("logs").exists(),
            "old log dir should be removed"
        );
        assert_eq!(
            std::fs::read_to_string(new_dir.join("zeno.log.2025-01-01")).unwrap(),
            "log content 1",
            "content preserved"
        );
    }

    #[test]
    fn test_migrate_logs_skips_if_new_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let old_cfg = tmp.path().join("config/zeno");
        let new_cache = tmp.path().join("cache/zeno");

        std::fs::create_dir_all(old_cfg.join("logs")).unwrap();
        std::fs::write(old_cfg.join("logs/zeno.log.2025-01-01"), "old").unwrap();

        let new_dir = new_cache.join("logs");
        std::fs::create_dir_all(&new_dir).unwrap();
        std::fs::write(new_dir.join("zeno.log.2025-01-01"), "newer").unwrap();

        migrate_logs(&new_dir, &old_cfg);

        assert_eq!(
            std::fs::read_to_string(new_dir.join("zeno.log.2025-01-01")).unwrap(),
            "newer",
            "existing file should not be overwritten"
        );
    }

    #[test]
    fn test_migrate_logs_skips_if_no_old() {
        let tmp = tempfile::tempdir().unwrap();
        let old_cfg = tmp.path().join("config/zeno");
        let new_dir = tmp.path().join("cache/zeno/logs");

        migrate_logs(&new_dir, &old_cfg);

        assert!(
            !new_dir.exists(),
            "should not create new dir when no old log dir exists"
        );
    }
}
