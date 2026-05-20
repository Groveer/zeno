//! Configuration file watcher — monitors `init.lua` for changes.
//!
//! When the config file changes, the watcher sends a notification to the TUI
//! so the user knows to restart. Full automatic hot-reload is not implemented
//! because it would require rebuilding the system prompt, re-registering tools,
//! reloading skills, and reconnecting MCP servers — operations that are safer
//! to do on a clean restart.
//!
//! # Design
//!
//! Uses `notify` with a debounce window (500ms) to avoid triggering multiple
//! notifications during a single save operation (many editors write atomically
//! or create temp files). The watcher runs as a background tokio task.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use notify::{Event, EventKind, RecursiveMode, Watcher};
use tokio::sync::Mutex;

use crate::engine::tui_events::EngineSender;

/// Debounce window: how long to wait after the last change event before
/// sending the notification. Prevents duplicate notifications from atomic
/// saves and temp-file writes.
const DEBOUNCE_MS: u64 = 500;

/// Start watching a config file for changes.
///
/// When the file is modified, a notification is sent through the TUI event
/// channel. The watcher runs in a background thread (notify requires a
/// thread-based watcher on Linux/macOS).
///
/// Returns immediately. The watcher lives until the returned handle is dropped.
pub fn watch_config(config_path: PathBuf, sender: EngineSender) -> Result<WatcherGuard, String> {
    let sender = Arc::new(Mutex::new(sender));
    let path = config_path.clone();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    // Spawn a debounce task on the tokio runtime
    tokio::spawn(async move {
        let sender = sender;
        while rx.recv().await.is_some() {
            // Debounce: wait for more events
            tokio::time::sleep(Duration::from_millis(DEBOUNCE_MS)).await;
            // Drain any queued events during debounce window
            while rx.try_recv().is_ok() {}
            // Send notification
            let s = sender.lock().await;
            let _ = s.send(crate::engine::tui_events::EngineEvent::Status(format!(
                "Config changed: {} — restart to apply",
                path.display()
            )));
            tracing::info!(
                config = %path.display(),
                event = "config_changed",
                "Config file changed, user notified"
            );
        }
    });

    // Create a thread-based watcher (notify requires OS threads for inotify/FSEvents)
    let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        if let Ok(event) = res {
            match event.kind {
                EventKind::Modify(_) | EventKind::Create(_) => {
                    let _ = tx.send(());
                }
                _ => {}
            }
        }
    })
    .map_err(|e| format!("Failed to create file watcher: {}", e))?;

    watcher
        .watch(&config_path, RecursiveMode::NonRecursive)
        .map_err(|e| {
            format!(
                "Failed to watch config file '{}': {}",
                config_path.display(),
                e
            )
        })?;

    // Return a guard that drops the watcher when the session ends
    let guard = WatcherGuard(watcher);
    Ok(guard)
}

/// RAII guard that drops the file watcher on exit.
#[allow(dead_code, reason = "field held for RAII lifetime")]
pub struct WatcherGuard(notify::RecommendedWatcher);

impl Drop for WatcherGuard {
    fn drop(&mut self) {
        tracing::debug!("Config file watcher stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_watcher_creation() {
        let mut tmp = NamedTempFile::new().unwrap();
        writeln!(tmp, "return {{}}").unwrap();
        let path = tmp.path().to_path_buf();

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let guard = tokio::task::spawn_blocking(move || watch_config(path, tx))
            .await
            .unwrap();
        assert!(guard.is_ok(), "watcher should be created successfully");
    }
}
