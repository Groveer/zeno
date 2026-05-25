//! Scrollable output area for conversation history.
//!
//! Displays assistant text, tool calls, and tool results with
//! distinct styling.  Supports scrolling through history.
//!
//! ## Module structure
//!
//! - [`segment`] — `OutputSegment` enum and segment-to-lines conversion
//! - [`cache`] — cached line rebuilding + text wrapping
//! - [`render`] — terminal rendering with scroll indicator

pub mod cache;
pub mod render;
pub mod segment;

use ratatui::{Frame, layout::Rect};

use crate::gateway::UiCommand;

use super::component::Component;
pub use segment::OutputSegment;

/// Scrollable output state.
pub struct OutputState {
    /// All rendered segments so far.
    pub(crate) segments: Vec<OutputSegment>,
    /// Scroll offset from the bottom (0 = bottom / newest).
    pub(crate) scroll: usize,
    /// Whether auto-scroll is enabled (follows new output).
    pub(crate) auto_scroll: bool,
    /// Cached rendered lines (built by build_cached_lines, invalidated on push/clear).
    pub(crate) cached_lines: Vec<ratatui::text::Line<'static>>,
    /// Generation counter: incremented on push/clear, compared by render() to detect staleness.
    cache_gen: u64,
    /// Last width used to build cache; if width changes, we rebuild.
    cache_width: usize,
    /// Per-agent current status line (task_index → latest activity).
    sub_agent_lines: std::collections::BTreeMap<usize, String>,
}

impl OutputState {
    pub fn new() -> Self {
        Self {
            segments: Vec::new(),
            scroll: 0,
            auto_scroll: true,
            cached_lines: Vec::new(),
            cache_gen: 0,
            cache_width: 0,
            sub_agent_lines: std::collections::BTreeMap::new(),
        }
    }

    /// Invalidate the line cache (call after any mutation to segments).
    fn bump_gen(&mut self) {
        self.cache_gen += 1;
    }

    pub fn push(&mut self, seg: OutputSegment) {
        self.segments.push(seg);
        self.bump_gen();
    }

    /// Call after in-place mutation of a segment (e.g. TextDelta append).
    /// Invalidates the render cache without adding a new segment.
    pub fn mark_dirty(&mut self) {
        self.bump_gen();
    }

    pub fn clear(&mut self) {
        self.segments.clear();
        self.scroll = 0;
        self.auto_scroll = true;
        self.cached_lines.clear();
        self.sub_agent_lines.clear();
        self.bump_gen();
    }

    /// Scroll up by N lines (towards older content).
    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll = self.scroll.saturating_add(lines);
        self.auto_scroll = false;
    }

    /// Scroll down by N lines (towards newer content).
    pub fn scroll_down(&mut self, lines: usize) {
        if self.scroll <= lines {
            self.scroll = 0;
            self.auto_scroll = true;
        } else {
            self.scroll -= lines;
        }
    }
}

impl Component for OutputState {
    fn update(&mut self, cmd: UiCommand) {
        match cmd {
            UiCommand::AppendText(text) => {
                if let Some(OutputSegment::Text(existing)) = self.segments.last_mut() {
                    existing.push_str(&text);
                    self.mark_dirty();
                } else {
                    self.push(OutputSegment::Text(text));
                }
            }
            UiCommand::AppendReasoning(text) => {
                if let Some(OutputSegment::Reasoning(existing)) = self.segments.last_mut() {
                    existing.push_str(&text);
                    self.mark_dirty();
                } else {
                    self.push(OutputSegment::Reasoning(text));
                }
            }
            UiCommand::ClearOutput => {
                self.clear();
            }
            UiCommand::ToolStart {
                name,
                input_summary,
            } => {
                let display = if input_summary.is_empty() {
                    String::new()
                } else {
                    input_summary
                };
                self.push(OutputSegment::ToolExecuting {
                    name,
                    summary: display,
                });
            }
            UiCommand::ToolComplete { name, output } => {
                if let Some(last) = self.segments.iter_mut().rev().find(
                    |s| matches!(s, OutputSegment::ToolExecuting { name: n, .. } if *n == name),
                ) {
                    let summary =
                        match std::mem::replace(last, OutputSegment::Status(String::new())) {
                            OutputSegment::ToolExecuting { summary: op, .. } => {
                                if op.is_empty() {
                                    format!("{} → {}", name, output)
                                } else {
                                    format!("{} → {}", op, output)
                                }
                            }
                            _ => output.clone(),
                        };
                    *last = OutputSegment::ToolComplete(summary);
                    self.mark_dirty();
                } else {
                    self.push(OutputSegment::ToolComplete(format!(
                        "{} → {}",
                        name, output
                    )));
                }
            }
            UiCommand::ToolError { name, error } => {
                if let Some(last) = self.segments.iter_mut().rev().find(
                    |s| matches!(s, OutputSegment::ToolExecuting { name: n, .. } if *n == name),
                ) {
                    let summary =
                        match std::mem::replace(last, OutputSegment::Status(String::new())) {
                            OutputSegment::ToolExecuting { summary: op, .. } => {
                                if op.is_empty() {
                                    format!("{} → {}", name, error)
                                } else {
                                    format!("{} → {}", op, error)
                                }
                            }
                            _ => error.clone(),
                        };
                    *last = OutputSegment::ToolError(summary);
                    self.mark_dirty();
                } else {
                    self.push(OutputSegment::ToolError(format!("{} → {}", name, error)));
                }
            }
            UiCommand::ToolDiff { diff, .. } => {
                self.push(OutputSegment::Diff(diff));
            }
            UiCommand::ShowError(err) => {
                self.push(OutputSegment::Error(err));
            }
            UiCommand::ShowStatus(msg) => {
                self.push(OutputSegment::Status(msg));
            }
            // Sub-agent events
            UiCommand::SubAgentStarted { summary } => {
                // New batch — clear old rolling state for a fresh start
                if !self.sub_agent_lines.is_empty() {
                    self.sub_agent_lines.clear();
                    self.segments
                        .retain(|s| !matches!(s, OutputSegment::SubAgentProgress(_)));
                }
                self.push(OutputSegment::Status(summary));
            }
            UiCommand::SubAgentProgress { task_index, line } => {
                // One line per agent — replace the latest activity
                self.sub_agent_lines.insert(task_index, line);

                // Rebuild segment: sorted by index, one line each
                let all_lines: Vec<String> = self
                    .sub_agent_lines
                    .iter()
                    .map(|(idx, l)| format!("#{}: {}", idx, l))
                    .collect();

                if let Some(OutputSegment::SubAgentProgress(buf)) = self
                    .segments
                    .iter_mut()
                    .rev()
                    .find(|s| matches!(s, OutputSegment::SubAgentProgress(_)))
                {
                    *buf = all_lines;
                    self.mark_dirty();
                } else {
                    self.push(OutputSegment::SubAgentProgress(all_lines));
                }
            }
            _ => {} // Non-output commands are ignored
        }
    }

    fn view(&mut self, area: Rect, frame: &mut Frame) {
        self::render::render(frame, area, self);
    }

    fn needs_render(&self) -> bool {
        self.cache_gen > 0
    }

    fn clear_dirty(&mut self) {
        self.cache_gen = 0;
    }
}

impl Default for OutputState {
    fn default() -> Self {
        Self::new()
    }
}
