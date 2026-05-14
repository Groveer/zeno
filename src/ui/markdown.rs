//! Custom markdown renderer for LLM output.

//!

//! Uses `pulldown-cmark` for parsing, then renders into styled ratatui `Line`s

//! with **all markdown markers hidden** — headings show only the title text

//! (colored by level), lists use `` / `1.`, blockquotes show `▍`, and

//! `**bold**` / `*italic*` / `` `code` `` markers are removed with styles applied.

//! Tables are rendered with Unicode box-drawing characters.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;

use super::theme;

// ── Syntect globals (loaded once) ───────────────────────────────

static SYNTAX_SET: std::sync::LazyLock<SyntaxSet> =
    std::sync::LazyLock::new(SyntaxSet::load_defaults_newlines);

static THEME_SET: std::sync::LazyLock<ThemeSet> = std::sync::LazyLock::new(ThemeSet::load_defaults);

const SYNTAX_THEME: &str = "base16-ocean.dark";

/// Map a syntect `syntect::highlighting::Style` to a ratatui `Style`.
fn syntect_to_ratatui(s: syntect::highlighting::Style) -> Style {
    let fg = Color::Rgb(s.foreground.r, s.foreground.g, s.foreground.b);
    let mut style = Style::new().fg(fg);
    if s.font_style
        .contains(syntect::highlighting::FontStyle::BOLD)
    {
        style = style.add_modifier(Modifier::BOLD);
    }
    if s.font_style
        .contains(syntect::highlighting::FontStyle::ITALIC)
    {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if s.font_style
        .contains(syntect::highlighting::FontStyle::UNDERLINE)
    {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    style
}

/// Highlight a line of code using syntect, returning styled `Span`s.
fn highlight_code_line(highlighter: &mut HighlightLines, line: &str) -> Vec<Span<'static>> {
    match highlighter.highlight_line(line, &SYNTAX_SET) {
        Ok(ranges) => ranges
            .into_iter()
            .map(|(style, text)| Span::styled(text.to_string(), syntect_to_ratatui(style)))
            .collect(),
        Err(_) => {
            // Fallback: single span with code foreground
            vec![Span::styled(
                line.to_string(),
                Style::new().fg(theme::CODE_FG),
            )]
        }
    }
}

// ── Heading colors by level ────────────────────────────────────

const H1_FG: Color = theme::HEADING_1;
const H2_FG: Color = theme::HEADING;
const H3_FG: Color = Color::Rgb(120, 160, 230);
const H4_FG: Color = Color::Rgb(180, 160, 255);
const H5_FG: Color = Color::Rgb(200, 140, 220);
const H6_FG: Color = Color::Rgb(160, 160, 180);

fn heading_fg(level: u8) -> Color {
    match level {
        1 => H1_FG,
        2 => H2_FG,
        3 => H3_FG,
        4 => H4_FG,
        5 => H5_FG,
        _ => H6_FG,
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

// ── Table rendering state ──────────────────────────────────────

/// A single cell in the table being built.
#[derive(Default)]
struct TableCell {
    text: String,
    width: usize,
}

/// A row of cells.
struct TableRow {
    cells: Vec<TableCell>,
}

/// Accumulated table state — we collect all rows first, then emit formatted lines.
struct TableState {
    alignments: Vec<Alignment>,
    head: Option<TableRow>,
    body: Vec<TableRow>,
    /// Current cell content being accumulated.
    current_cell: String,
    /// Number of columns (set from TableHead).
    num_cols: usize,
    /// True when we're inside the <thead> section.
    in_head: bool,
}

impl TableState {
    fn new(alignments: Vec<Alignment>) -> Self {
        let num_cols = alignments.len();
        Self {
            alignments,
            head: None,
            body: Vec::new(),
            current_cell: String::new(),
            num_cols,
            in_head: false,
        }
    }

    fn push_cell(&mut self) {
        let text = std::mem::take(&mut self.current_cell);
        let width = crate::utils::display_width(text.as_str());
        let cell = TableCell { text, width };
        if self.in_head {
            if let Some(ref mut head) = self.head {
                head.cells.push(cell);
            }
        } else {
            if let Some(last) = self.body.last_mut() {
                last.cells.push(cell);
            }
        }
    }

    fn start_row(&mut self) {
        if self.in_head {
            self.head = Some(TableRow { cells: Vec::new() });
        } else {
            self.body.push(TableRow { cells: Vec::new() });
        }
    }

    /// Calculate column widths from all rows, then emit formatted lines.
    fn render(self) -> Vec<Line<'static>> {
        let num_cols = self.num_cols;
        if num_cols == 0 {
            return Vec::new();
        }

        // Compute max width per column
        let mut col_widths = vec![0usize; num_cols];
        for row in self.head.iter().chain(self.body.iter()) {
            for (i, cell) in row.cells.iter().enumerate() {
                if i < num_cols {
                    col_widths[i] = col_widths[i].max(cell.width);
                }
            }
        }

        // Minimum column width of 1
        for w in &mut col_widths {
            *w = (*w).max(1);
        }

        let mut lines = Vec::new();

        // Table style
        let border_fg = theme::BORDER;
        let head_fg = theme::HEADING;
        let head_style = Style::new().fg(head_fg).add_modifier(Modifier::BOLD);
        let cell_style = Style::new().fg(theme::TEXT);

        // ── Top border: ┌─────┬─────┐
        lines.push(Line::from(make_border(
            &col_widths,
            '┌',
            '┬',
            '┐',
            '─',
            border_fg,
        )));

        // ── Header row
        if let Some(head) = self.head {
            lines.push(make_row(
                &head.cells,
                &col_widths,
                &self.alignments,
                '│',
                head_style,
                cell_style,
                border_fg,
            ));

            // ── Header separator: ├─────┼─────┤  (use ╪ for aligned columns)
            lines.push(Line::from(make_border(
                &col_widths,
                '├',
                '┼',
                '┤',
                '─',
                border_fg,
            )));
        }

        // ── Body rows
        for (row_idx, row) in self.body.iter().enumerate() {
            lines.push(make_row(
                &row.cells,
                &col_widths,
                &self.alignments,
                '│',
                head_style,
                cell_style,
                border_fg,
            ));
            // Row separator between body rows (lighter)
            if row_idx + 1 < self.body.len() {
                lines.push(Line::from(make_border(
                    &col_widths,
                    '├',
                    '┼',
                    '┤',
                    '╌',
                    border_fg,
                )));
            }
        }

        // ── Bottom border: └─────┴─────┘
        lines.push(Line::from(make_border(
            &col_widths,
            '└',
            '┴',
            '┘',
            '─',
            border_fg,
        )));

        lines
    }
}

/// Build a border line like ┌─────┬─────┐
fn make_border(
    col_widths: &[usize],
    left: char,
    mid: char,
    right: char,
    fill: char,
    fg: Color,
) -> Vec<Span<'static>> {
    let style = Style::new().fg(fg);
    let mut spans = vec![Span::styled(String::from(left), style)];
    for (i, &w) in col_widths.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(String::from(mid), style));
        }
        // +2 for padding (1 space each side)
        spans.push(Span::styled(fill.to_string().repeat(w + 2), style));
    }
    spans.push(Span::styled(String::from(right), style));
    spans
}

/// Build a data row like │ cell │ cell │
fn make_row(
    cells: &[TableCell],
    col_widths: &[usize],
    alignments: &[Alignment],
    sep: char,
    head_style: Style, // kept for future header row styling
    cell_style: Style,
    border_fg: Color,
) -> Line<'static> {
    let border_style = Style::new().fg(border_fg);
    let mut spans = vec![Span::styled(String::from(sep), border_style)];

    for (i, &col_w) in col_widths.iter().enumerate() {
        let text = cells.get(i).map(|c| c.text.as_str()).unwrap_or("");
        let _ = head_style; // used by caller to choose header/body style
        let style = cell_style;
        let align = alignments.get(i).copied().unwrap_or(Alignment::None);

        // Pad content to display width col_w
        let content_width = crate::utils::display_width(text);
        let pad = col_w.saturating_sub(content_width);

        // Manual padding by display width (spaces are width-1).
        // format!("{:<width$}") pads by *character count*, which breaks for
        // CJK glyphs (display width 2, char count 1) → │ misalignment.
        let padded = match align {
            Alignment::Center => {
                let left_pad = pad / 2;
                let right_pad = pad - left_pad;
                format!("{}{}{}", " ".repeat(left_pad), text, " ".repeat(right_pad))
            }
            Alignment::Right => {
                format!("{}{}", " ".repeat(pad), text)
            }
            Alignment::None | Alignment::Left => {
                format!("{}{}", text, " ".repeat(pad))
            }
        };

        spans.push(Span::styled(String::from(" "), Style::default()));
        spans.push(Span::styled(padded, style));
        spans.push(Span::styled(String::from(" "), Style::default()));
        spans.push(Span::styled(String::from(sep), border_style));
    }

    Line::from(spans)
}

// ── Renderer ───────────────────────────────────────────────────

struct MdRenderer {
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    inline_styles: Vec<Style>,
    quote_depth: usize,
    list_stack: Vec<Option<u64>>,
    needs_blank: bool,
    in_code_block: bool,
    pending_link_url: Option<String>,
    heading_level: Option<u8>,
    /// Active table state (when inside a Table block).
    table: Option<TableState>,
    /// Active syntect highlighter for the current code block.
    code_highlighter: Option<HighlightLines<'static>>,
}

impl MdRenderer {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            current: Vec::new(),
            inline_styles: Vec::new(),
            quote_depth: 0,
            list_stack: Vec::new(),
            needs_blank: false,
            in_code_block: false,
            pending_link_url: None,
            heading_level: None,
            table: None,
            code_highlighter: None,
        }
    }

    fn current_style(&self) -> Style {
        self.inline_styles
            .iter()
            .fold(Style::default(), |acc, &s| acc.patch(s))
    }

    fn push_span(&mut self, content: String, style: Style) {
        self.current.push(Span::styled(content, style));
    }

    fn flush_line(&mut self) {
        let spans = std::mem::take(&mut self.current);
        if !spans.is_empty() {
            self.lines.push(Line::from(spans));
        }
    }

    fn maybe_blank(&mut self) {
        if self.needs_blank && !self.lines.is_empty() {
            self.lines.push(Line::default());
        }
        self.needs_blank = false;
    }

    fn quote_prefix(&self) -> Vec<Span<'static>> {
        let mut v = Vec::with_capacity(self.quote_depth);
        for _ in 0..self.quote_depth {
            v.push(Span::styled(
                String::from("▍ "),
                Style::new().fg(theme::BLOCKQUOTE_BAR),
            ));
        }
        v
    }

    /// True when we are inside a table (collecting cells, not emitting lines).
    fn in_table(&self) -> bool {
        self.table.is_some()
    }

    fn handle(&mut self, event: Event<'_>) {
        // ── Table events need special routing ───────────────
        match &event {
            Event::Start(Tag::Table(alignment)) => {
                self.flush_line();
                self.maybe_blank();
                self.table = Some(TableState::new(alignment.to_vec()));
                return;
            }
            Event::Start(Tag::TableHead) => {
                if let Some(t) = &mut self.table {
                    t.in_head = true;
                    t.start_row();
                }
                return;
            }
            Event::Start(Tag::TableRow) => {
                if let Some(t) = &mut self.table {
                    t.in_head = false;
                    t.start_row();
                }
                return;
            }
            Event::Start(Tag::TableCell) => {
                // just start accumulating text
                return;
            }
            Event::End(TagEnd::Table) => {
                if let Some(table) = self.table.take() {
                    let table_lines = table.render();
                    self.lines.extend(table_lines);
                }
                self.needs_blank = true;
                return;
            }
            Event::End(TagEnd::TableHead) => {
                if let Some(t) = &mut self.table {
                    t.in_head = false;
                }
                return;
            }
            Event::End(TagEnd::TableRow) => {
                // row already started in Start(TableRow)
                return;
            }
            Event::End(TagEnd::TableCell) => {
                if let Some(t) = &mut self.table {
                    t.push_cell();
                }
                return;
            }
            _ => {}
        }

        // ── Non-table events ────────────────────────────────
        match event {
            // ── block starts ─────────────────────────────
            Event::Start(Tag::Paragraph) => {
                self.maybe_blank();
                self.current.extend(self.quote_prefix());
            }

            Event::Start(Tag::Heading { level, .. }) => {
                self.maybe_blank();
                let lvl = heading_level(level);
                self.heading_level = Some(lvl);
                let fg = heading_fg(lvl);
                let style = Style::new().fg(fg).add_modifier(Modifier::BOLD);
                self.inline_styles.push(style);
            }

            Event::Start(Tag::BlockQuote(_)) => {
                self.flush_line();
                self.maybe_blank();
                self.quote_depth += 1;
            }

            Event::Start(Tag::List(start)) => {
                self.list_stack.push(start);
                if self.list_stack.len() == 1 {
                    self.maybe_blank();
                }
            }

            Event::Start(Tag::Item) => {
                self.flush_line();
                self.current.extend(self.quote_prefix());
                let indent = self.list_stack.len().saturating_sub(1) * 2;
                if indent > 0 {
                    self.push_span(" ".repeat(indent), Style::default());
                }
                let marker = if let Some(last) = self.list_stack.last_mut() {
                    match last {
                        Some(idx) => {
                            *idx += 1;
                            let n = *idx - 1;
                            format!("{}. ", n)
                        }
                        None => String::from(" "),
                    }
                } else {
                    String::from(" ")
                };
                self.push_span(marker, Style::new().fg(theme::LIST_MARKER));
            }

            Event::Start(Tag::CodeBlock(kind)) => {
                self.in_code_block = true;
                self.flush_line();
                self.maybe_blank();

                // Create syntect highlighter based on language tag
                let lang: String = match kind {
                    CodeBlockKind::Fenced(info) => info
                        .trim()
                        .split(',')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code_highlighter = if lang.is_empty() {
                    None
                } else {
                    SYNTAX_SET
                        .find_syntax_by_token(&lang)
                        .map(|syntax| HighlightLines::new(syntax, &THEME_SET.themes[SYNTAX_THEME]))
                };
            }

            Event::Start(Tag::Strong) => {
                self.inline_styles
                    .push(Style::new().fg(theme::STRONG).add_modifier(Modifier::BOLD));
            }

            Event::Start(Tag::Emphasis) => {
                self.inline_styles.push(
                    Style::new()
                        .fg(theme::EMPHASIS)
                        .add_modifier(Modifier::ITALIC),
                );
            }

            Event::Start(Tag::Strikethrough) => {
                self.inline_styles
                    .push(Style::new().add_modifier(Modifier::CROSSED_OUT));
            }

            Event::Start(Tag::Link { dest_url, .. }) => {
                self.inline_styles.push(
                    Style::new()
                        .fg(theme::LINK)
                        .add_modifier(Modifier::UNDERLINED),
                );
                self.pending_link_url = Some(dest_url.into_string());
            }

            Event::Start(Tag::Image { title, .. }) => {
                let alt = if title.is_empty() {
                    String::from("[img]")
                } else {
                    format!("[img: {}]", title)
                };
                self.push_span(alt, Style::new().fg(theme::TEXT_DIM));
            }

            // ── block ends ───────────────────────────────
            Event::End(TagEnd::Paragraph) => {
                self.flush_line();
                self.needs_blank = true;
            }

            Event::End(TagEnd::Heading(_)) => {
                self.inline_styles.pop();
                let lvl = self.heading_level.take().unwrap_or(1);
                self.flush_line();
                if lvl <= 2 {
                    // Check if this heading looks like a numbered list item
                    // (e.g. "1. 列表测试" or "2. 代码块测试") — skip underline
                    let is_numbered_item = self.lines.last().is_some_and(|line| {
                        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                        let trimmed = text.trim();
                        trimmed.chars().next().is_some_and(|c| c.is_ascii_digit())
                            && trimmed.contains(". ")
                    });
                    if !is_numbered_item {
                        let width: usize = self.lines.last().map(|l| l.width()).unwrap_or(0);
                        if width > 0 {
                            let ch = if lvl == 1 { '═' } else { '─' };
                            let fg = heading_fg(lvl);
                            self.lines.push(Line::from(vec![Span::styled(
                                ch.to_string().repeat(width.min(60)),
                                Style::new().fg(fg),
                            )]));
                        }
                    }
                }
                self.needs_blank = true;
            }

            Event::End(TagEnd::BlockQuote(_)) => {
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }

            Event::End(TagEnd::List(_)) => {
                self.flush_line();
                self.list_stack.pop();
                self.needs_blank = true;
            }

            Event::End(TagEnd::Item) => {
                self.flush_line();
            }

            Event::End(TagEnd::CodeBlock) => {
                self.in_code_block = false;
                self.code_highlighter = None;
                self.flush_line();
                self.needs_blank = true;
            }

            Event::End(TagEnd::Strong)
            | Event::End(TagEnd::Emphasis)
            | Event::End(TagEnd::Strikethrough) => {
                self.inline_styles.pop();
            }

            Event::End(TagEnd::Link) => {
                self.inline_styles.pop();
                if let Some(url) = self.pending_link_url.take() {
                    self.push_span(
                        format!(" ({})", url),
                        Style::new().fg(theme::LINK).add_modifier(Modifier::DIM),
                    );
                }
            }

            Event::End(TagEnd::Image) => {}

            // ── inline content ────────────────────────────
            Event::Text(text) => {
                if self.in_table() {
                    // Accumulate cell text
                    if let Some(t) = &mut self.table {
                        t.current_cell.push_str(&text);
                    }
                } else if self.in_code_block {
                    let gutter_style = Style::new().fg(theme::TEXT_DIM);
                    for (i, line) in text.lines().enumerate() {
                        if i > 0 || !self.current.is_empty() {
                            self.flush_line();
                        }
                        // Gutter prefix
                        self.push_span(String::from(" "), gutter_style);

                        // Use syntect highlighter if available, otherwise fallback
                        if let Some(ref mut highlighter) = self.code_highlighter {
                            let spans = highlight_code_line(highlighter, line);
                            self.current.extend(spans);
                        } else {
                            let code_style = Style::new().fg(theme::CODE_FG).bg(theme::CODE_BG);
                            self.push_span(line.to_string(), code_style);
                        }
                    }
                } else {
                    let style = self.current_style();
                    self.push_span(text.to_string(), style);
                }
            }

            Event::Code(code) => {
                if self.in_table() {
                    if let Some(t) = &mut self.table {
                        t.current_cell.push_str(&code);
                    }
                } else {
                    let code_style = Style::new()
                        .fg(theme::INLINE_CODE_FG)
                        .bg(theme::INLINE_CODE_BG);
                    self.push_span(code.to_string(), code_style);
                }
            }

            Event::SoftBreak => {
                if self.in_table() {
                    // soft break inside table cell → space
                    if let Some(t) = &mut self.table {
                        t.current_cell.push(' ');
                    }
                } else {
                    self.push_span(String::from(" "), Style::default());
                }
            }

            Event::HardBreak => {
                if self.in_table() {
                    // hard break inside cell → newline in cell text
                    if let Some(t) = &mut self.table {
                        t.current_cell.push('\n');
                    }
                } else {
                    self.flush_line();
                }
            }

            Event::Rule => {
                self.maybe_blank();
                self.lines.push(Line::from(vec![Span::styled(
                    "─".repeat(40),
                    Style::new().fg(theme::HR_COLOR),
                )]));
                self.needs_blank = true;
            }

            Event::TaskListMarker(checked) => {
                let marker = if checked {
                    String::from(" ")
                } else {
                    String::from("○ ")
                };
                self.push_span(marker, Style::new().fg(theme::LIST_MARKER));
            }

            // Skip unsupported
            Event::Html(_)
            | Event::InlineHtml(_)
            | Event::FootnoteReference(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_) => {}

            Event::Start(_) => {}
            Event::End(_) => {}
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        if !self.current.is_empty() {
            self.flush_line();
        }
        self.lines
    }
}

// ── Public API ─────────────────────────────────────────────────

/// Convert a block of markdown text into styled `Line`s.
///
/// All markdown markers (`#`, `**`, `*`, `` ` ``, `-`, `>`) are **hidden**.
/// Headings get distinct colors by level, H1/H2 get underline decorations,
/// lists use `` / `1.`, blockquotes show `▍`, tables use box-drawing chars.
pub fn render_markdown(text: &str) -> Vec<Line<'static>> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_TABLES);

    let parser = Parser::new_ext(text, opts);
    let mut renderer = MdRenderer::new();

    for event in parser {
        renderer.handle(event);
    }

    renderer.finish()
}

// ── Tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_heading_no_hash() {
        let lines = render_markdown("# Hello\n## World\n### Deep");
        for line in &lines {
            for span in &line.spans {
                assert!(
                    !span.content.starts_with('#'),
                    "Heading markers should be hidden, got: {:?}",
                    span.content
                );
            }
        }
    }

    #[test]
    fn test_heading_colors_distinct() {
        let lines = render_markdown("# H1\n## H2\n### H3\n#### H4\n##### H5\n###### H6");
        let styled: Vec<_> = lines
            .iter()
            .filter(|l| l.spans.iter().any(|s| s.style.fg.is_some()))
            .collect();
        assert!(
            styled.len() >= 6,
            "Expected 6+ styled heading lines, got {}",
            styled.len()
        );
    }

    #[test]
    fn test_bold_italic_no_markers() {
        let lines = render_markdown("This is **bold** and *italic*");
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(
            !all_text.contains("**"),
            "** markers should be hidden: {:?}",
            all_text
        );
        assert!(
            !all_text.contains('*'),
            "* markers should be hidden: {:?}",
            all_text
        );
        assert!(all_text.contains("bold"));
        assert!(all_text.contains("italic"));
    }

    #[test]
    fn test_list_no_dash() {
        let lines = render_markdown("- item A\n- item B");
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(
            !all_text.contains("- "),
            "List dashes should be replaced: {:?}",
            all_text
        );
        assert!(
            all_text.contains(''),
            "Bullet markers should be present: {:?}",
            all_text
        );
    }

    #[test]
    fn test_ordered_list() {
        let md = "1. first\n2. second\n3. third";
        let lines = render_markdown(md);
        for (i, line) in lines.iter().enumerate() {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            eprintln!("[simple] {:02}: {:?}", i, text);
        }
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(all_text.contains("1."), "Ordered list marker 1. missing");
        assert!(all_text.contains("2."), "Ordered list marker 2. missing");

        // Each list item must be on its own line
        let line_texts: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        let combined: String = line_texts.join("|");
        assert!(
            !combined.contains("1.2.") && !combined.contains("1. 2."),
            "List items 1 and 2 should not be on the same line: {:?}",
            combined
        );
        assert!(
            !combined.contains("2.3.") && !combined.contains("2. 3."),
            "List items 2 and 3 should not be on the same line: {:?}",
            combined
        );
    }

    #[test]
    fn test_blockquote_bar() {
        let lines = render_markdown("> quoted text");
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(
            !all_text.contains('>'),
            "> markers should be replaced: {:?}",
            all_text
        );
        assert!(
            all_text.contains('▍'),
            "Blockquote bar should be present: {:?}",
            all_text
        );
    }

    #[test]
    fn test_code_inline_no_backticks() {
        let lines = render_markdown("Use `cargo build` to compile");
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(
            !all_text.contains('`'),
            "Backticks should be hidden: {:?}",
            all_text
        );
        assert!(
            all_text.contains("cargo build"),
            "Code text should be present: {:?}",
            all_text
        );
    }

    #[test]
    fn test_hr_rule() {
        let lines = render_markdown("---");
        let has_rule = lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.content.contains('─') && s.content.len() > 10)
        });
        assert!(has_rule, "Horizontal rule should render as ── line");
    }

    #[test]
    fn test_h1_underline() {
        let lines = render_markdown("# Title");
        let has_double = lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content.contains('═')));
        assert!(has_double, "H1 should have ═ underline");
    }

    #[test]
    fn test_h2_underline() {
        let lines = render_markdown("## Subtitle");
        let has_single = lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.content.contains('─') && !s.content.contains('═'))
        });
        assert!(has_single, "H2 should have ─ underline");
    }

    #[test]
    fn test_chinese_content() {
        let lines = render_markdown("# 中文标题\n\n这是一段**粗体**和*斜体*文字\n\n- 列表项");
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(all_text.contains("中文标题"));
        assert!(all_text.contains("粗体"));
        assert!(all_text.contains("列表项"));
        assert!(!all_text.contains('#'));
        assert!(!all_text.contains("**"));
    }

    #[test]
    fn test_table_basic() {
        let md = "| A | B | C |\n|---|---|---|\n| 1 | 2 | 3 |\n| 4 | 5 | 6 |";
        let lines = render_markdown(md);

        // Should produce border lines and data lines
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();

        // No raw pipe markers should remain (they're replaced by box-drawing)
        assert!(
            !all_text.contains("| "),
            "Raw pipe markers should be replaced: {:?}",
            all_text
        );
        // Box-drawing chars should be present
        assert!(
            all_text.contains('┌') || all_text.contains('│') || all_text.contains('└'),
            "Table should have box-drawing borders: {:?}",
            all_text
        );
        // Cell content should be present
        assert!(
            all_text.contains('A') && all_text.contains('1') && all_text.contains('4'),
            "Cell content should be present: {:?}",
            all_text
        );
    }

    #[test]
    fn test_table_alignment() {
        let md = "| Left | Center | Right |\n|:-----|:------:|------:|\n| L | C | R |";
        let lines = render_markdown(md);
        // Should render without panicking
        assert!(!lines.is_empty(), "Table should produce lines");

        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(
            all_text.contains('L') && all_text.contains('C') && all_text.contains('R'),
            "Cell content should be present: {:?}",
            all_text
        );
    }

    #[test]
    fn test_table_no_raw_pipes() {
        let md = "| 功能 | 说明 | 状态 |\n|:---|:---|:---|\n| 渲染 | 支持 |  |";
        let lines = render_markdown(md);
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        // No raw markdown pipe syntax
        assert!(
            !all_text.contains("|:"),
            "Alignment markers should be hidden: {:?}",
            all_text
        );
        assert!(
            all_text.contains("渲染"),
            "Chinese cell content should be present: {:?}",
            all_text
        );
    }

    #[test]
    fn test_table_cjk_alignment() {
        // CJK characters are display-width 2; verify that all data rows
        // have the same total display width (so │ borders line up).
        // Note: uses crate::utils::display_width() for measurement because
        // Line.width() uses unicode-width which under-reports PUA icons ( = 1
        // instead of 2). display_width() correctly reports the terminal width.
        let md = "| 功能 | 说明 | 状态 |\n|:-----|:------:|------:|\n| 渲染 | 支持 |  |\n| 表格 | 支持 |  |\n| ABC | DEF | G |";
        let lines = render_markdown(md);

        // Collect the display width of each data row (lines containing │).
        // Use crate::utils::display_width() for correct PUA/emoji measurement.
        let row_widths: Vec<usize> = lines
            .iter()
            .filter(|l| l.spans.iter().any(|s| s.content.contains('│')))
            .map(|l| {
                let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                crate::utils::display_width(&text)
            })
            .collect();

        // All rows should have the same width
        if row_widths.len() > 1 {
            let first = row_widths[0];
            for (i, &w) in row_widths.iter().enumerate() {
                assert_eq!(
                    w, first,
                    "Row {} has width {} but expected {} — CJK alignment broken",
                    i, w, first
                );
            }
        }
    }

    #[test]
    fn test_ordered_list_followed_by_blocks() {
        // List followed by independent blockquote and code block (not inside list items)
        let md = "1. 有序列表项 1\n2. 有序列表项 2\n3. 引用与代码块\n\n> 这是一个引用块，用来强调某段话。\n\n```\nfn main() {\n    println!(\"Hello, Markdown!\");\n}\n```";
        let lines = render_markdown(md);

        // Debug: print all lines
        for (i, line) in lines.iter().enumerate() {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            eprintln!("[followed] {:02}: {:?}", i, text);
        }

        let line_texts: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        // Each list item must be on its own line
        let combined: String = line_texts.join("|");
        assert!(
            !combined.contains("2.3.") && !combined.contains("2. 3."),
            "List items 2 and 3 should not be on the same line: {:?}",
            combined
        );

        // "3." marker must exist
        assert!(
            combined.contains("3."),
            "Ordered list marker 3. missing: {:?}",
            combined
        );

        // Blockquote must exist
        assert!(
            combined.contains('▍'),
            "Blockquote bar missing: {:?}",
            combined
        );

        // Code block must exist
        assert!(
            combined.contains("fn main()"),
            "Code block content missing: {:?}",
            combined
        );
    }

    #[test]
    fn test_ordered_list_with_block_elements() {
        // List item containing blockquote and code block
        let md = "1. 有序列表项 1\n2. 有序列表项 2\n3. 引用与代码块\n\n   > 这是一个引用块\n\n   ```\n   fn main() {\n       println!(\"Hello, Markdown!\");\n   }\n   ```";
        let lines = render_markdown(md);

        // Debug: print all lines
        for (i, line) in lines.iter().enumerate() {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            eprintln!("{:02}: {:?}", i, text);
        }

        // Each list item should be on its own line
        let line_texts: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        // "2." and "3." must be on separate lines (not concatenated on one line)
        let combined: String = line_texts.join("|");
        assert!(
            !combined.contains("2.3.") && !combined.contains("2. 3."),
            "List items 2 and 3 should not be on the same line: {:?}",
            combined
        );
        assert!(
            combined.contains("3."),
            "Ordered list marker 3. missing: {:?}",
            combined
        );
        assert!(
            combined.contains('▍'),
            "Blockquote bar missing: {:?}",
            combined
        );
        assert!(
            combined.contains("fn main()"),
            "Code block content missing: {:?}",
            combined
        );
    }

    #[test]
    fn test_screenshot_scenario() {
        // Reproduce the exact scenario from the screenshot
        // The numbered items are headings (## 1. / ## 2. / ## 3.)
        let md = r#"## 1. 列表测试

- 项目 A
- 项目 B
  - 子项目 B1
  - 子项目 B2

## 2. 代码块测试 (Rust)

```rust
fn main() {
    println!("Hello, Markdown!");
}
```

## 3. 表格测试

| 功能 | 状态 | 备注 |
|------|------|------|
| 代码补全 |  已完成 | |
"#;
        let lines = render_markdown(md);

        // Debug: print all lines
        for (i, line) in lines.iter().enumerate() {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            eprintln!("[screenshot] {:02}: {:?}", i, text);
        }

        let line_texts: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        // Bug 1: "2. 代码块测试" should not be concatenated after "子项目 B2"
        let combined: String = line_texts.join("|");
        assert!(
            !combined.contains("B22.") && !combined.contains("B2 2."),
            "List item '2.' should not be concatenated after 'B2': {:?}",
            combined
        );
        assert!(
            combined.contains("2. 代码块测试"),
            "Heading '2. 代码块测试' should be present: {:?}",
            combined
        );
    }
}
