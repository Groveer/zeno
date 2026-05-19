//! Tool Carryover — lightweight working memory that survives across turns.
//!
//! Tracks what the agent has *read*, *written*, and *done* so that
//! after context compression the LLM can still "remember" key facts.
//!
//! Design reference: OpenHarness `_record_tool_carryover()` (query.py L337-452).
//! Simplified: we keep only the most impactful buckets (read files,
//! active artifacts, work log, user goals) and skip async-agent tracking
//! since zeno does not have a swarm/agent system.

// ---------------------------------------------------------------------------
// Constants — bucket size caps
// ---------------------------------------------------------------------------

const MAX_READ_FILES: usize = 6;
const MAX_ACTIVE_ARTIFACTS: usize = 8;
const MAX_WORK_LOG: usize = 10;
const MAX_USER_GOALS: usize = 5;

/// Files with fewer lines than this have their full content preserved
/// in carryover, so the LLM doesn't need to re-read after context compression.
const FULL_CONTENT_LINE_THRESHOLD: usize = 200;

/// Maximum characters for a single full-content carryover entry.
const MAX_FULL_CONTENT_CHARS: usize = 4096;

/// Total character budget for all full-content entries combined.
/// Prevents carryover from ballooning when many small files are read.
/// ~12,000 chars ≈ 3,000 tokens — a reasonable cap for post-compaction context.
const FULL_CONTENT_TOTAL_BUDGET: usize = 12_000;

// ---------------------------------------------------------------------------
// Bucket types
// ---------------------------------------------------------------------------

/// Content mode for a read-file entry in carryover.
#[derive(Debug, Clone)]
pub enum ReadContent {
    /// Summary only — first few lines, truncated. Used for large files.
    Preview(String),
    /// Full file content preserved. Used for small files so the LLM
    /// doesn't need to re-read after context compression.
    Full(String),
}

/// A snapshot of a file that was read.
#[derive(Debug, Clone)]
pub struct ReadFileEntry {
    pub path: String,
    pub span: String, // e.g. "lines 1-50"
    /// Content: either a short preview or the full file content.
    pub content: ReadContent,
}

/// A single work-log entry (max 320 chars).
pub type WorkLogEntry = String;

// ---------------------------------------------------------------------------
// Carryover state
// ---------------------------------------------------------------------------

/// Task focus state — tracks the agent's current objective and next step.
/// Reference: OpenHarness `_task_focus_state` in query.py L108-136.
#[derive(Debug, Clone, Default)]
pub struct TaskFocusState {
    /// The agent's current primary goal (latest user intent).
    pub goal: String,
    /// Recent goals history (most recent at the end, capped).
    pub recent_goals: Vec<String>,
    /// Files/URLs the agent is actively working with.
    pub active_artifacts: Vec<String>,
    /// Confirmed completed work entries.
    pub verified_state: Vec<String>,
    /// What the agent should do next (updated each turn).
    pub next_step: String,
}

/// Working memory that accumulates across tool invocations within a query.
#[derive(Debug, Clone, Default)]
pub struct Carryover {
    /// Files the agent has read (most recent first, capped).
    pub read_files: Vec<ReadFileEntry>,
    /// Files/URLs the agent has created or modified.
    pub active_artifacts: Vec<String>,
    /// Short descriptions of what the agent has done.
    pub work_log: Vec<WorkLogEntry>,
    /// User's stated goals (latest at the end).
    pub user_goals: Vec<String>,
    /// Confirmed completed work — prevents the LLM from redoing
    /// tasks after context compression.
    pub verified_work: Vec<String>,
    /// Task focus state — tracks current goal and next step.
    /// This is the key mechanism to keep the agent on-track when
    /// the model returns an empty or tool-less response prematurely.
    pub task_focus: TaskFocusState,
}

impl Carryover {
    /// Strip line numbers from formatted tool output.
    ///
    /// The read tool output format is: "{line_num:>6} | {line}\n"
    /// This function extracts just the line content, stripping the
    /// line-number prefix and metadata/footer lines.
    fn strip_line_numbers(output: &str) -> String {
        let mut lines = Vec::new();
        for line in output.lines() {
            // Skip metadata lines (lines X-Y of Z) and overlap hints
            if line.starts_with("(lines ") || line.starts_with("[Note:") {
                continue;
            }
            // The read tool outputs: format!("{:>6} | {}", line_num, line)
            // So " | " always appears at column 6 (0-indexed) when the line
            // follows the expected format. Check for this exact position to
            // avoid false positives on content like "2024 | some data".
            if line.len() > 9 && line.as_bytes().get(6) == Some(&b' ') && &line[6..9] == " | " {
                let num_part = &line[..6];
                if num_part.trim().parse::<usize>().is_ok() {
                    let content = &line[9..]; // skip " | " (positions 6-8)
                    lines.push(content);
                } else {
                    lines.push(line);
                }
            } else if line.trim().is_empty() {
                lines.push("");
            } else {
                lines.push(line);
            }
        }
        lines.join("\n")
    }

    /// Record that a file was read.
    /// For small files (≤200 lines), preserves the full content in carryover
    /// so the LLM doesn't need to re-read after context compression.
    /// Respects `FULL_CONTENT_TOTAL_BUDGET` to prevent carryover from ballooning
    /// when many small files are read — once the budget is exhausted, subsequent
    /// entries fall back to preview mode.
    pub fn remember_read_file(&mut self, path: &str, offset: usize, limit: usize, output: &str) {
        // Strip line numbers to store raw content
        let raw_content = Self::strip_line_numbers(output);

        let line_count = raw_content.lines().count();

        let content = if line_count <= FULL_CONTENT_LINE_THRESHOLD {
            // Small file — check budget before preserving full content.
            let truncated = truncate_str(&raw_content, MAX_FULL_CONTENT_CHARS);
            let new_chars = truncated.len();

            // Sum chars of existing Full entries (excluding the entry we're about to replace).
            let existing_full_chars: usize = self
                .read_files
                .iter()
                .filter(|e| e.path != path)
                .map(|e| match &e.content {
                    ReadContent::Full(s) => s.len(),
                    ReadContent::Preview(_) => 0,
                })
                .sum();

            if existing_full_chars + new_chars <= FULL_CONTENT_TOTAL_BUDGET {
                ReadContent::Full(truncated.to_string())
            } else {
                // Budget exhausted — fall back to preview.
                let preview_lines: Vec<&str> = raw_content
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .take(6)
                    .collect();
                let preview = preview_lines.join(" | ");
                let preview = truncate_str(&preview, 320);
                ReadContent::Preview(preview.to_string())
            }
        } else {
            // Large file — summary only
            let preview_lines: Vec<&str> = raw_content
                .lines()
                .filter(|l| !l.trim().is_empty())
                .take(6)
                .collect();
            let preview = preview_lines.join(" | ");
            let preview = truncate_str(&preview, 320);
            ReadContent::Preview(preview.to_string())
        };

        let entry = ReadFileEntry {
            path: path.to_string(),
            span: format!("lines {}-{}", offset + 1, offset + limit),
            content,
        };

        // Remove previous entry for the same path
        self.read_files.retain(|e| e.path != path);
        self.read_files.push(entry);
        cap_vec(&mut self.read_files, MAX_READ_FILES);
    }

    /// Record an active artifact (file written, URL fetched, etc.).
    pub fn remember_artifact(&mut self, artifact: &str) {
        let normalized = artifact.trim();
        if normalized.is_empty() {
            return;
        }
        let s = truncate_str(normalized, 240).to_string();
        append_unique(
            &mut self.active_artifacts,
            s.to_string(),
            MAX_ACTIVE_ARTIFACTS,
        );
    }

    /// Record a work-log entry.
    pub fn remember_work(&mut self, entry: &str) {
        let normalized = entry.trim();
        if normalized.is_empty() {
            return;
        }
        let s = truncate_str(normalized, 320).to_string();
        self.work_log.push(s);
        cap_vec(&mut self.work_log, MAX_WORK_LOG);
    }

    /// Record the user's latest prompt for context preservation.
    ///
    /// This records the input in `user_goals` (used by compaction to
    /// retain high-level context across turns) but does **NOT** set
    /// `task_focus.goal`. Goals must be set explicitly via
    /// `set_goal()` (e.g. triggered by the `/goal` slash command).
    ///
    /// This matches Hermes's design: auto-continue only fires when
    /// a goal is explicitly active, not on every user input.
    pub fn remember_user_goal(&mut self, prompt: &str) {
        let normalized: String = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
        if normalized.is_empty() {
            return;
        }
        let s = truncate_str(&normalized, 240).to_string();
        append_unique(&mut self.user_goals, s, MAX_USER_GOALS);
        // NOTE: task_focus.goal is NOT set here. Use set_goal().
    }

    /// Explicitly set an active goal for auto-continue.
    /// Called by the `/goal` slash command.
    pub fn set_goal(&mut self, goal: &str) {
        let normalized: String = goal.split_whitespace().collect::<Vec<_>>().join(" ");
        if normalized.is_empty() {
            return;
        }
        let s = truncate_str(&normalized, 240).to_string();
        self.task_focus.goal = s.clone();
        append_unique(&mut self.task_focus.recent_goals, s, MAX_USER_GOALS);
    }

    /// Record a tool invocation result into the carryover.
    /// Dispatches to the appropriate bucket based on tool name.
    pub fn record_tool_result(
        &mut self,
        tool_name: &str,
        tool_input: &serde_json::Value,
        tool_output: &str,
        is_error: bool,
        resolved_file_path: Option<&str>,
    ) {
        if is_error {
            return;
        }

        // Track artifacts from file paths
        if let Some(path) = resolved_file_path {
            self.remember_artifact(path);
        }

        match tool_name {
            "read" | "read_file" => {
                if let Some(path) = resolved_file_path {
                    let offset = tool_input
                        .get("offset")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as usize;
                    let limit = tool_input
                        .get("limit")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(200) as usize;
                    self.remember_read_file(path, offset, limit, tool_output);
                    self.remember_work(&format!(
                        "Read file {} (lines {}-{})",
                        path,
                        offset + 1,
                        offset + limit
                    ));
                }
            }
            "write" | "edit" => {
                if let Some(path) = resolved_file_path {
                    self.remember_work(&format!("Wrote/edited file {}", path));
                }
            }
            "bash" => {
                let command = tool_input
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let summary = tool_output.lines().next().unwrap_or("no output").trim();
                let summary = truncate_str(summary, 120);
                self.remember_work(&format!(
                    "Ran bash: {} [{}]",
                    truncate_str(command, 160),
                    summary
                ));
            }
            "glob" => {
                let pattern = tool_input
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                self.remember_work(&format!("Expanded glob {}", truncate_str(pattern, 180)));
            }
            "grep" => {
                let pattern = tool_input
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                self.remember_work(&format!(
                    "Searched with grep pattern={}",
                    truncate_str(pattern, 160)
                ));
            }
            "web_fetch" => {
                let url = tool_input.get("url").and_then(|v| v.as_str()).unwrap_or("");
                if !url.is_empty() {
                    self.remember_artifact(url);
                    self.remember_work(&format!("Fetched remote content from {}", url));
                }
            }
            "web_search" => {
                let query = tool_input
                    .get("query")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !query.is_empty() {
                    self.remember_work(&format!("Web search: {}", truncate_str(query, 180)));
                }
            }
            "skill_view" | "skill_list" => {
                let name = tool_input
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !name.is_empty() {
                    self.remember_artifact(&format!("skill:{}", name));
                    self.remember_work(&format!("Loaded skill {}", name));
                }
            }
            _ => {}
        }
    }

    /// Format the carryover as a text block suitable for injecting into
    /// a compression prompt or system prompt.
    ///
    /// This is the key method: it converts the ephemeral tracking data
    /// back into text that an LLM can read after context compression.
    pub fn to_context_text(&self) -> String {
        let mut parts = Vec::new();

        if !self.user_goals.is_empty() {
            parts.push("## User Goals".to_string());
            for goal in &self.user_goals {
                parts.push(format!("- {}", goal));
            }
        }

        if !self.read_files.is_empty() {
            parts.push("## Files Read".to_string());
            for entry in &self.read_files {
                match &entry.content {
                    ReadContent::Full(content) => {
                        parts.push(format!(
                            "- {} ({}, full content preserved):",
                            entry.path, entry.span
                        ));
                        parts.push(format!("  ````text\n{}\n  ````", content));
                    }
                    ReadContent::Preview(preview) => {
                        parts.push(format!("- {} ({}) — {}", entry.path, entry.span, preview));
                    }
                }
            }
        }

        if !self.active_artifacts.is_empty() {
            parts.push("## Active Artifacts".to_string());
            for artifact in &self.active_artifacts {
                parts.push(format!("- {}", artifact));
            }
        }

        if !self.work_log.is_empty() {
            parts.push("## Work Done".to_string());
            for entry in &self.work_log {
                parts.push(format!("- {}", entry));
            }
        }

        if !self.verified_work.is_empty() {
            parts.push("## Verified Completed".to_string());
            for entry in &self.verified_work {
                parts.push(format!("- {}", entry));
            }
        }

        if !self.task_focus.goal.is_empty() {
            parts.push("## Task Focus".to_string());
            parts.push(format!("- Current goal: {}", self.task_focus.goal));
            if !self.task_focus.next_step.is_empty() {
                parts.push(format!("- Next step: {}", self.task_focus.next_step));
            }
            if !self.task_focus.active_artifacts.is_empty() {
                parts.push("- Active artifacts:".to_string());
                for a in &self.task_focus.active_artifacts {
                    parts.push(format!("  - {}", a));
                }
            }
            if !self.task_focus.verified_state.is_empty() {
                parts.push("- Verified state:".to_string());
                for v in &self.task_focus.verified_state {
                    parts.push(format!("  - {}", v));
                }
            }
        }

        if parts.is_empty() {
            String::new()
        } else {
            parts.join("\n")
        }
    }

    /// Check if the carryover has any useful information.
    pub fn has_data(&self) -> bool {
        !self.read_files.is_empty()
            || !self.active_artifacts.is_empty()
            || !self.work_log.is_empty()
            || !self.user_goals.is_empty()
    }

    /// Check if there is an active goal that has not been marked complete.
    /// Used by the query loop to decide whether to inject a continuation
    /// prompt when the model returns without tool calls.
    pub fn has_pending_goal(&self) -> bool {
        !self.task_focus.goal.is_empty()
    }

    /// Clear the current task focus goal, signalling that the task is done.
    pub fn clear_goal(&mut self) {
        self.task_focus.goal.clear();
        self.task_focus.next_step.clear();
    }

    /// Build a continuation prompt to inject when the model stops
    /// prematurely. This nudges the LLM to keep working on the
    /// active goal instead of giving up.
    pub fn build_continuation_prompt(&self) -> Option<String> {
        if self.task_focus.goal.is_empty() {
            return None;
        }
        let mut parts = vec![
            "[system] You stopped before completing the task. Continue working on the goal."
                .to_string(),
            format!("[system] Goal: {}", self.task_focus.goal),
        ];
        if !self.task_focus.next_step.is_empty() {
            parts.push(format!("[system] Next step: {}", self.task_focus.next_step));
        }
        if !self.task_focus.active_artifacts.is_empty() {
            parts.push(format!(
                "[system] Active artifacts: {}",
                self.task_focus.active_artifacts.join(", ")
            ));
        }
        if !self.verified_work.is_empty() {
            let recent: Vec<&str> = self
                .verified_work
                .iter()
                .rev()
                .take(5)
                .map(|s| s.as_str())
                .collect();
            parts.push(format!(
                "[system] Already completed (do NOT redo): {}",
                recent.join("; ")
            ));
        }
        parts.push("[system] Use the appropriate tools to continue. Do NOT just describe what you would do — actually do it.".to_string());
        Some(parts.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Append a value, deduplicating (move-to-end semantics), then cap.
fn append_unique(vec: &mut Vec<String>, value: String, limit: usize) {
    vec.retain(|v| v != &value);
    vec.push(value);
    cap_vec(vec, limit);
}

/// Cap a vector to `limit` items, dropping from the front.
fn cap_vec<T>(vec: &mut Vec<T>, limit: usize) {
    while vec.len() > limit {
        vec.remove(0);
    }
}

fn truncate_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        // Find the largest char boundary <= max to avoid slicing inside a multi-byte char
        match s.char_indices().take_while(|(idx, _)| *idx <= max).last() {
            Some((idx, c)) if idx + c.len_utf8() <= max => &s[..idx + c.len_utf8()],
            Some((idx, _)) => &s[..idx],
            None => "",
        }
    }
}

/// Extract a likely file path from tool input.
/// Checks "file_path", "path", "root" keys.
pub fn resolve_file_path(tool_input: &serde_json::Value) -> Option<String> {
    for key in &["file_path", "path", "root"] {
        if let Some(val) = tool_input.get(*key).and_then(|v| v.as_str()) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- remember_user_goal: never sets task_focus.goal ---

    #[test]
    fn test_remember_user_goal_no_auto_goal() {
        let mut c = Carryover::default();
        c.remember_user_goal("fix the login bug");
        assert!(c.user_goals.contains(&"fix the login bug".to_string()));
        assert!(
            c.task_focus.goal.is_empty(),
            "remember_user_goal must NOT set task_focus.goal"
        );
    }

    #[test]
    fn test_remember_user_goal_casual_no_goal() {
        let mut c = Carryover::default();
        c.remember_user_goal("test");
        assert!(c.user_goals.contains(&"test".to_string()));
        assert!(c.task_focus.goal.is_empty());
    }

    // --- set_goal: explicit goal activation ---

    #[test]
    fn test_set_goal_activates() {
        let mut c = Carryover::default();
        c.set_goal("implement auth");
        assert_eq!(c.task_focus.goal, "implement auth");
        assert!(c.has_pending_goal());
    }

    #[test]
    fn test_set_goal_empty_ignored() {
        let mut c = Carryover::default();
        c.set_goal("");
        assert!(!c.has_pending_goal());
    }

    // --- build_continuation_prompt ---

    #[test]
    fn test_continuation_prompt_with_goal() {
        let mut c = Carryover::default();
        c.task_focus.goal = "implement auth".into();
        let prompt = c.build_continuation_prompt();
        assert!(prompt.is_some());
        let text = prompt.unwrap();
        assert!(text.contains("implement auth"));
        assert!(text.contains("[system]"));
    }

    #[test]
    fn test_continuation_prompt_no_goal() {
        let c = Carryover::default();
        assert!(c.build_continuation_prompt().is_none());
    }

    // --- has_pending_goal ---

    #[test]
    fn test_has_pending_goal() {
        let mut c = Carryover::default();
        assert!(!c.has_pending_goal());
        c.task_focus.goal = "do stuff".into();
        assert!(c.has_pending_goal());
    }

    // --- clear_goal ---

    #[test]
    fn test_clear_goal() {
        let mut c = Carryover::default();
        c.task_focus.goal = "do stuff".into();
        c.task_focus.next_step = "step 1".into();
        c.clear_goal();
        assert!(c.task_focus.goal.is_empty());
        assert!(c.task_focus.next_step.is_empty());
    }

    // --- full-content budget ---

    #[test]
    fn test_full_content_within_budget() {
        // A small file well within budget should be stored as Full.
        let mut c = Carryover::default();
        // 20 lines of ~40 chars each ≈ 800 chars — well under 12,000 budget.
        let output: String = (0..20)
            .map(|i| format!("{:>6} | line {}", i + 1, i))
            .collect::<Vec<_>>()
            .join("\n");
        c.remember_read_file("/small.rs", 0, 20, &output);
        assert!(matches!(c.read_files[0].content, ReadContent::Full(_)));
    }

    #[test]
    fn test_full_content_budget_exhausted() {
        // Read multiple small files until budget is exhausted.
        // Each line: ~70 chars of content after stripping line numbers.
        // 80 lines × ~70 chars = ~5,600 chars per file.
        // Budget is 12,000: files 0 & 1 fit (~11,200), file 2 exceeds.
        let mut c = Carryover::default();

        for i in 0..3 {
            let content: String = (0..80)
                .map(|j| {
                    format!(
                        "{:>6} | {} line {:04} padding_padding_padding_padding_padding",
                        j + 1,
                        i,
                        j
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            c.remember_read_file(&format!("/file_{}.rs", i), 0, 80, &content);
        }

        // First two files should be Full (within budget)
        assert!(matches!(c.read_files[0].content, ReadContent::Full(_)));
        assert!(matches!(c.read_files[1].content, ReadContent::Full(_)));
        // Third file should fall back to Preview (budget exhausted)
        assert!(matches!(c.read_files[2].content, ReadContent::Preview(_)));
    }

    #[test]
    fn test_full_content_re_read_same_file() {
        // Re-reading the same file should not double-count budget.
        let mut c = Carryover::default();
        let content: String = (0..100)
            .map(|j| format!("{:>6} | line {:04} padding_padding_padding", j + 1, j))
            .collect::<Vec<_>>()
            .join("\n");

        // Read same file twice
        c.remember_read_file("/file.rs", 0, 100, &content);
        c.remember_read_file("/file.rs", 0, 100, &content);

        assert_eq!(c.read_files.len(), 1);
        // Should still be Full — old entry was removed before budget check
        assert!(matches!(c.read_files[0].content, ReadContent::Full(_)));
    }
}
