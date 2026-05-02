//! Path utilities for zeno config, data, and log directories.
//!
//! `config_dir()`, `config_path()`, `data_dir()`, `log_dir()`, `ensure_log_dir()`,
//! and `cleanup_old_logs()` are actively used. The remaining functions are
//! reserved for future migration/setup commands.
#![allow(
    dead_code,
    reason = "legacy_config_path/ensure_config_dir/log_path reserved for future CLI"
)]

use std::path::PathBuf;

/// Returns the zeno config directory following XDG spec.
pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("zeno")
}

/// Returns the path to the main config file (Lua).
pub fn config_path() -> PathBuf {
    config_dir().join("init.lua")
}

/// Returns the path to the legacy YAML config (for /migrate only).
pub fn legacy_config_path() -> PathBuf {
    config_dir().join("config.yaml")
}

/// Returns the global memory directory (`~/.config/zeno/memory`).
/// MEMORY.md is stored here. Shared across all projects.
pub fn memory_dir() -> PathBuf {
    config_dir().join("memory")
}

/// Returns the path to the user profile file (`~/.config/zeno/USER.md`).
/// USER.md is stored alongside init.lua in the config directory.
pub fn user_profile_path() -> PathBuf {
    config_dir().join("USER.md")
}

/// Returns the data directory for zeno (memory, sessions, etc.).
pub fn data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("~/.local/share"))
        .join("zeno")
}

/// Ensures the config directory exists, returns its path.
pub fn ensure_config_dir() -> anyhow::Result<PathBuf> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Returns the log directory inside the config directory.
pub fn log_dir() -> PathBuf {
    config_dir().join("logs")
}

/// Returns the path to the main log file.
pub fn log_path() -> PathBuf {
    log_dir().join("zeno.log")
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
