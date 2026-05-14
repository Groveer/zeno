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

    /// Get stats for a specific tool.
    #[allow(dead_code, reason = "public API for future TUI status bar")]
    pub fn get(&self, tool_name: &str) -> Option<&ToolStatEntry> {
        self.tools.get(tool_name)
    }

    /// Get all tool stats.
    #[allow(dead_code, reason = "public API for future TUI status bar")]
    pub fn all(&self) -> &HashMap<String, ToolStatEntry> {
        &self.tools
    }

    /// Total calls across all tools.
    #[allow(dead_code, reason = "public API for future TUI status bar")]
    pub fn total_calls(&self) -> u64 {
        self.tools.values().map(|e| e.total_calls).sum()
    }

    /// Total failures across all tools.
    #[allow(dead_code, reason = "public API for future TUI status bar")]
    pub fn total_failures(&self) -> u64 {
        self.tools.values().map(|e| e.failed_calls).sum()
    }

    /// Generate a summary string for display.
    #[allow(dead_code, reason = "public API for future TUI status bar")]
    pub fn summary(&self) -> String {
        let total = self.total_calls();
        if total == 0 {
            return "No tool calls yet.".to_string();
        }
        let failures = self.total_failures();
        let mut lines: Vec<String> = Vec::new();
        lines.push(format!(
            " Tool usage: {} calls, {} failures",
            total, failures
        ));
        let mut tools: Vec<(&String, &ToolStatEntry)> = self.tools.iter().collect();
        tools.sort_by(|a, b| b.1.total_calls.cmp(&a.1.total_calls));
        for (name, stats) in tools.iter().take(10) {
            let avg_duration = if stats.total_calls > 0 {
                stats.total_duration_secs / stats.total_calls as f64
            } else {
                0.0
            };
            let success_rate = if stats.total_calls > 0 {
                stats.success_calls as f64 / stats.total_calls as f64 * 100.0
            } else {
                0.0
            };
            lines.push(format!(
                "  {}: {} calls, {:.0}% success, avg {:.1}s",
                name, stats.total_calls, success_rate, avg_duration
            ));
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_and_query() {
        let mut stats = ToolStats::default();
        stats.record("bash", 1.5, true);
        stats.record("bash", 0.8, true);
        stats.record("read", 0.2, true);
        stats.record("bash", 2.0, false);

        assert_eq!(stats.total_calls(), 4);
        assert_eq!(stats.total_failures(), 1);

        let bash = stats.get("bash").unwrap();
        assert_eq!(bash.total_calls, 3);
        assert_eq!(bash.success_calls, 2);
        assert_eq!(bash.failed_calls, 1);
        assert!((bash.total_duration_secs - 4.3).abs() < 0.01);
    }

    #[test]
    fn test_summary_empty() {
        let stats = ToolStats::default();
        assert_eq!(stats.summary(), "No tool calls yet.");
    }

    #[test]
    fn test_summary_non_empty() {
        let mut stats = ToolStats::default();
        stats.record("bash", 1.0, true);
        stats.record("read", 0.1, true);
        let summary = stats.summary();
        assert!(summary.contains("2 calls"));
        assert!(summary.contains("0 failures"));
    }
}
