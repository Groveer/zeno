//! Tool usage statistics — tracks call frequency, success rates, and latency.
//!
//! # Design
//!
//! Thread-safe via `Arc<Mutex<>>`. Each tool call records the tool name,
//! whether it succeeded, and the duration. The data can be queried by the
//! engine to display in status bar or diagnostic output.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Shared tool statistics collector.
pub type SharedToolStats = Arc<Mutex<ToolStats>>;

/// Create a new shared tool stats collector.
pub fn new_shared() -> SharedToolStats {
    Arc::new(Mutex::new(ToolStats::default()))
}

/// Per-tool statistics.
#[derive(Debug, Clone, Default)]
pub struct ToolStatEntry {
    /// Total number of calls.
    pub total_calls: u64,
    /// Number of successful calls.
    pub success_calls: u64,
    /// Number of failed calls.
    pub failed_calls: u64,
    /// Total duration in seconds (for average latency).
    pub total_duration_secs: f64,
}

/// Thread-safe tool statistics collector.
#[derive(Debug, Default)]
pub struct ToolStats {
    tools: HashMap<String, ToolStatEntry>,
}

impl ToolStats {
    /// Record a tool call result.
    pub fn record(&mut self, tool_name: &str, duration_secs: f64, success: bool) {
        let entry = self.tools.entry(tool_name.to_string()).or_default();
        entry.total_calls += 1;
        entry.total_duration_secs += duration_secs;
        if success {
            entry.success_calls += 1;
        } else {
            entry.failed_calls += 1;
        }
    }
}
