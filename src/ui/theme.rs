#![allow(dead_code)]
//! Color theme for the zeno TUI.

//!

//! Defines a dark, professional color palette inspired by modern terminal

//! coding assistants.

use ratatui::style::Color;

// ── Core palette ──────────────────────────────────────────────

pub const BG: Color = Color::Rgb(18, 18, 24); // near-black
pub const SURFACE: Color = Color::Rgb(28, 28, 38); // slightly lighter
pub const BORDER: Color = Color::Rgb(60, 60, 80);

pub const TEXT: Color = Color::Rgb(220, 220, 230); // off-white
pub const TEXT_DIM: Color = Color::Rgb(140, 140, 155);
pub const TEXT_BRIGHT: Color = Color::Rgb(255, 255, 255);

// ── Accents ───────────────────────────────────────────────────

pub const ACCENT: Color = Color::Rgb(100, 180, 255); // blue-ish
pub const ACCENT_DIM: Color = Color::Rgb(60, 120, 200);
pub const SUCCESS: Color = Color::Rgb(100, 200, 130); // green
pub const WARNING: Color = Color::Rgb(240, 200, 80); // yellow
pub const ERROR: Color = Color::Rgb(240, 100, 100); // red

// ── Code highlighting ─────────────────────────────────────────

pub const CODE_BG: Color = Color::Rgb(24, 24, 34);
pub const CODE_BORDER: Color = Color::Rgb(50, 50, 70);
pub const CODE_FG: Color = Color::Rgb(210, 210, 220);
pub const INLINE_CODE_BG: Color = Color::Rgb(40, 40, 55);
pub const INLINE_CODE_FG: Color = Color::Rgb(220, 180, 120);

// ── Markdown ──────────────────────────────────────────────────

pub const HEADING: Color = Color::Rgb(140, 180, 255); // blue
pub const HEADING_1: Color = Color::Rgb(180, 210, 255); // light blue
pub const STRONG: Color = Color::Rgb(255, 255, 255); // white bold
pub const EMPHASIS: Color = Color::Rgb(200, 200, 220); // light italic
pub const LINK: Color = Color::Rgb(100, 180, 255); // blue
pub const BLOCKQUOTE_FG: Color = Color::Rgb(150, 150, 170); // muted
pub const BLOCKQUOTE_BAR: Color = Color::Rgb(80, 80, 110); // bar color
pub const LIST_MARKER: Color = Color::Rgb(100, 180, 255); // bullet color
pub const HR_COLOR: Color = Color::Rgb(60, 60, 80); // horizontal rule

// ── Tool output ───────────────────────────────────────────────

pub const TOOL_BG: Color = Color::Rgb(22, 28, 42);
pub const TOOL_BORDER: Color = Color::Rgb(40, 50, 80);
pub const TOOL_LABEL: Color = Color::Rgb(130, 160, 220);

// ── Diff highlighting ────────────────────────────────────────

pub const DIFF_DEL: Color = Color::Rgb(240, 100, 100); // red (same as ERROR)
pub const DIFF_ADD: Color = Color::Rgb(100, 200, 130); // green (same as SUCCESS)
pub const DIFF_ARROW: Color = Color::Rgb(180, 180, 190); // muted arrow ""
