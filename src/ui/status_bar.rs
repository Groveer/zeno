//! Bottom status bar — shows model, token usage, status.

use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
};

use crate::gateway::UiCommand;

use super::theme;

impl super::component::Component for StatusInfo {
    fn update(&mut self, cmd: UiCommand) {
        match cmd {
            UiCommand::SetMode(mode) => self.mode = mode,
            UiCommand::UpdateStatus(info) => *self = info,
            UiCommand::UpdateTokens(tokens) => self.total_tokens = tokens,
            UiCommand::UpdateTurnCount(turns) => self.turn_count = turns,
            UiCommand::SetModel(model) => self.model = model,
            _ => {}
        }
    }

    fn view(&mut self, area: Rect, frame: &mut Frame) {
        render(frame, area, self);
    }

    fn needs_render(&self) -> bool {
        true // Status bar is cheap to render, always redraw
    }
}

/// Data the status bar renders.
#[derive(Debug, Clone)]
pub struct StatusInfo {
    pub model: String,
    pub provider: String,
    pub total_tokens: u64,
    pub context_window: u32,
    pub turn_count: u64,
    pub mcp_server_count: usize,
    pub skill_count: usize,
    /// Current app mode for status display.
    pub mode: AppMode,
    /// Number of queued "steer" messages (user input while agent is running).
    pub steer_count: usize,
    /// Currently active identity name (shown in status bar when set).
    pub active_identity: Option<String>,
    /// Frame counter for status dot animation (blink timing).
    pub tick: u64,
}

/// Format a number with SI-like abbreviations: 1.2K, 3.4M, etc.
fn fmt_compact(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.1}B", n as f64 / 1e9)
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

/// Mirrors the app's mode for status bar rendering.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum AppMode {
    #[default]
    Idle,
    Running,
    WaitingInput,
}

pub fn render(frame: &mut Frame, area: Rect, info: &StatusInfo) {
    let model_color = theme::TEXT_BRIGHT;
    let dim = theme::TEXT_DIM;

    // Build segments
    let mut spans = Vec::new();

    // Provider / Model
    spans.push(Span::styled(
        format!(" {}:{} ", info.provider, info.model),
        Style::new().fg(model_color).bg(theme::SURFACE),
    ));

    // Active identity (if set)
    if let Some(ref id) = info.active_identity {
        spans.push(Span::styled(
            format!("[{}] ", id),
            Style::new().fg(theme::ACCENT).bg(theme::SURFACE),
        ));
    }

    // Separator
    spans.push(Span::styled(" │ ", Style::new().fg(dim)));

    // Tokens — show "used / context" with compact units (K/M/B)
    // Note: context_window is the total input+output capacity, NOT the
    // per-request output limit. Display as "ctx" to avoid confusion.
    let cw = info.context_window;
    let token_text = if cw > 0 {
        format!(
            "{} / {} ctx",
            fmt_compact(info.total_tokens),
            fmt_compact(cw as u64)
        )
    } else {
        format!("{} tok", fmt_compact(info.total_tokens))
    };
    spans.push(Span::styled(token_text, Style::new().fg(theme::TEXT)));

    // If there were turns, show count
    if info.turn_count > 0 {
        spans.push(Span::styled(
            format!(" ({} turns)", info.turn_count),
            Style::new().fg(dim),
        ));
    }

    // Push everything to the right: MCP servers + skills
    let mut parts = Vec::new();
    if info.mcp_server_count > 0 {
        parts.push(format!("{} MCP", info.mcp_server_count));
    }
    if info.skill_count > 0 {
        parts.push(format!("{} skill", info.skill_count));
    }
    let right_text = if parts.is_empty() {
        String::new()
    } else {
        format!("{} ", parts.join(" · "))
    };
    let right_width = right_text.len() as u16;

    // Status indicator — single colored dot, no text.
    //   Idle          → green, solid
    //   Running       → blue, blink
    //   Running+steer → cyan, blink
    //   WaitingInput  → yellow, blink
    let (dot_color, blink) = match info.mode {
        AppMode::Idle => (theme::SUCCESS, false),
        AppMode::Running => {
            if info.steer_count > 0 {
                (theme::CYAN, true)
            } else {
                (theme::ACCENT, true) // blue
            }
        }
        AppMode::WaitingInput => (theme::WARNING, true), // yellow
    };
    // Blink: toggle every 10 frames between bright and dim.
    // At 60fps (active) that's ~6 toggles/sec (3 full cycles); at 10fps (idle) it doesn't blink.
    let show_bright = !blink || (info.tick / 10).is_multiple_of(2);
    let actual_color = if show_bright {
        dot_color
    } else {
        theme::TEXT_DIM
    };

    let status_label = " ● ";
    let status_width: u16 = 3;

    let line = Line::from(spans);
    frame.render_widget(
        ratatui::widgets::Paragraph::new(line)
            .style(Style::new().bg(theme::SURFACE).fg(theme::TEXT)),
        area,
    );

    // Render tool count right-aligned if there's room
    if area.width > right_width + 2 {
        let right_area = Rect {
            x: area.x + area.width.saturating_sub(right_width),
            y: area.y,
            width: right_width,
            height: 1,
        };
        frame.render_widget(
            ratatui::widgets::Paragraph::new(Span::styled(
                right_text,
                Style::new().fg(theme::TEXT_DIM).bg(theme::SURFACE),
            )),
            right_area,
        );
    }

    // Render status indicator right of tools
    if area.width > right_width + status_width + 2 {
        let status_area = Rect {
            x: area.x + area.width.saturating_sub(right_width + status_width),
            y: area.y,
            width: status_width,
            height: 1,
        };
        frame.render_widget(
            ratatui::widgets::Paragraph::new(Span::styled(
                status_label,
                Style::new().fg(actual_color).bg(theme::SURFACE),
            )),
            status_area,
        );
    }
}
