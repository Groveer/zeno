//! Bottom status bar — shows model, token usage, status.

use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
};

use super::theme;

/// Data the status bar renders.
pub struct StatusInfo {
    pub model: String,
    pub provider: String,
    pub total_tokens: u64,
    pub context_window: u32,
    pub turn_count: u64,
    pub tool_count: usize,
    /// Current app mode for status display.
    pub mode: AppMode,
    /// Number of queued "steer" messages (user input while agent is running).
    pub steer_count: usize,
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

    // Push everything to the right: tool count
    let right_text = format!("{} tools ", info.tool_count);
    let right_width = right_text.len() as u16;

    // Status indicator (before right-aligned tools)
    let (status_label, status_color) = match info.mode {
        AppMode::Idle => (" ● ", theme::SUCCESS),
        AppMode::Running => {
            if info.steer_count > 0 {
                (" ⟳ thinking… ", theme::ACCENT)
            } else {
                (" ◌ thinking… ", theme::WARNING)
            }
        }
        AppMode::WaitingInput => ("  input ", theme::ACCENT),
    };
    let status_width = status_label.len() as u16;

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
                Style::new().fg(status_color).bg(theme::SURFACE),
            )),
            status_area,
        );
    }
}
