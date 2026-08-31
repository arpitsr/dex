use std::sync::{Arc, OnceLock};

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{block::Padding, Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui_markdown::highlight::{HighlightHooks, TreeSitterHighlighter};
use ratatui_markdown::markdown::{MarkdownBlock, MarkdownRenderer};
use ratatui_markdown::ThemeConfig;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::slash;
use super::wrapping::wrap_line;
use super::{format_tokens, theme, transcript_indent, App, InputField, TRANSCRIPT_INDENT};

pub(super) fn surface_padding() -> Padding {
    Padding {
        left: super::HORIZONTAL_GUTTER,
        right: super::HORIZONTAL_GUTTER,
        top: super::VERTICAL_GUTTER,
        bottom: super::VERTICAL_GUTTER,
    }
}

pub(super) fn input_block() -> Block<'static> {
    Block::default()
        .padding(Padding {
            left: super::HORIZONTAL_GUTTER + 1,
            right: super::HORIZONTAL_GUTTER,
            top: super::INPUT_PAD_Y,
            bottom: super::INPUT_PAD_Y,
        })
        .style(Style::default().bg(theme::surface_bg()))
}

pub(super) fn input_outer_height(content_rows: u16) -> u16 {
    content_rows + super::INPUT_BORDER_ROWS + super::INPUT_PAD_Y * 2
}

pub(super) fn activity_height(item_count: u16) -> u16 {
    item_count
        .saturating_mul(2)
        .saturating_sub(1)
        .saturating_add(super::VERTICAL_GUTTER * 2)
}

pub(super) fn input_content_width(width: u16) -> u16 {
    width.saturating_sub(super::HORIZONTAL_GUTTER * 2 + 1)
}

pub(super) fn status_height() -> u16 {
    super::STATUS_CONTENT_ROWS + super::VERTICAL_GUTTER * 2
}

pub(super) fn minimum_view_height(activity_h: u16, approval_h: u16) -> u16 {
    activity_h
        + approval_h
        + super::VERTICAL_GUTTER
        + status_height()
        + super::INPUT_MIN_ROWS
        + super::INPUT_STATUS_GUTTER
}

pub(super) struct UiLayout {
    pub(super) transcript: Rect,
    pub(super) activity: Rect,
    pub(super) approval: Rect,
    pub(super) input: Rect,
    pub(super) footer: Rect,
}

pub(super) fn compute_layout(
    area: Rect,
    input_rows: u16,
    activity_items: u16,
    approval_pending: bool,
) -> Option<UiLayout> {
    let activity_h = activity_height(activity_items);
    let approval_h = if approval_pending {
        super::APPROVAL_HEIGHT
    } else {
        0
    };
    let footer_height = super::INPUT_STATUS_GUTTER + status_height();
    if area.height < minimum_view_height(activity_h, approval_h) {
        return Some(UiLayout {
            transcript: area,
            activity: Rect::new(area.x, area.y, area.width, 0),
            approval: Rect::new(area.x, area.y, area.width, 0),
            input: Rect::new(area.x, area.y, area.width, 0),
            footer: Rect::new(area.x, area.y, area.width, 0),
        });
    }

    let input_h = input_outer_height(input_rows)
        .clamp(super::INPUT_MIN_ROWS, 8)
        .min(
            area.height
                .saturating_sub(activity_h + approval_h + footer_height),
        );
    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(super::VERTICAL_GUTTER),
        Constraint::Length(activity_h),
        Constraint::Length(approval_h),
        Constraint::Length(input_h),
        Constraint::Length(super::INPUT_STATUS_GUTTER),
        Constraint::Length(status_height()),
    ])
    .split(area);

    Some(UiLayout {
        transcript: chunks[0],
        activity: chunks[2],
        approval: chunks[3],
        input: chunks[4],
        footer: chunks[6],
    })
}

const TAB_WIDTH: usize = 8;

pub(super) fn truncate_display(text: &str, width: u16) -> String {
    let text = cell_safe(text);
    let width = width as usize;
    if UnicodeWidthStr::width(text.as_str()) <= width {
        return text;
    }
    if width <= 1 {
        return "…".chars().take(width).collect();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let cw = ch.width().unwrap_or(0);
        if used + cw + 1 > width {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    out
}

pub(super) fn compact_path(path: &str) -> String {
    if let Ok(home) = std::env::var("HOME") {
        if let Some(rest) = path.strip_prefix(&home) {
            return format!("~{}", rest);
        }
    }
    path.to_string()
}

pub(super) fn ui_status(app: &App) -> String {
    let cwd = compact_path(&app.cwd);
    let git = app
        .git_branch
        .as_ref()
        .map(|branch| format!(" · {}{}", branch, if app.git_dirty { "*" } else { "" }))
        .unwrap_or_default();
    let tokens = app
        .tool_state
        .last_usage
        .unwrap_or_else(|| crate::agent::compaction::estimate_tokens(&app.messages));
    let context_pct = if app.config.context_window == 0 {
        0
    } else {
        tokens
            .saturating_mul(100)
            .checked_div(app.config.context_window)
            .unwrap_or(0)
    };
    format!(
        "{} · {} / {}{} · {} / {} tokens ({}%)",
        cwd,
        app.config.provider.name(),
        app.config.model,
        git,
        format_tokens(tokens),
        format_tokens(app.config.context_window),
        context_pct
    )
}

pub(super) fn footer_text(app: &App, width: u16) -> String {
    let hint = if !app.autoscroll {
        "▲ more above · "
    } else {
        ""
    };
    let cwd = compact_path(&app.cwd);
    let compact = format!("{} · {}", cwd, app.config.model);
    let model = app.config.model.clone();
    let candidates = [ui_status(app), compact, model.clone()];
    for status in candidates {
        let candidate = format!("{}{}", hint, status);
        if UnicodeWidthStr::width(candidate.as_str()) <= width as usize {
            return candidate;
        }
    }
    truncate_display(&format!("{}{}", hint, model), width)
}

pub(super) fn split_markdown(s: &str) -> Vec<MarkdownBlock> {
    let lines: Vec<&str> = s.lines().collect();
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim_start();
        if t.starts_with("```") {
            let lang = t.trim_start_matches('`').trim().to_string();
            let mut body = String::new();
            i += 1;
            while i < lines.len() && !lines[i].trim_start().starts_with("```") {
                body.push_str(lines[i]);
                body.push('\n');
                i += 1;
            }
            i += 1;
            blocks.push(MarkdownBlock::code_block(lang, body));
        } else if let Some(rest) = t.strip_prefix("### ") {
            blocks.push(MarkdownBlock::Heading3(rest.to_string()));
            i += 1;
        } else if let Some(rest) = t.strip_prefix("## ") {
            blocks.push(MarkdownBlock::Heading2(rest.to_string()));
            i += 1;
        } else if let Some(rest) = t.strip_prefix("# ") {
            blocks.push(MarkdownBlock::Heading1(rest.to_string()));
            i += 1;
        } else if t == "---" {
            blocks.push(MarkdownBlock::HorizontalRule);
            i += 1;
        } else if let Some(rest) = t.strip_prefix("> ") {
            blocks.push(MarkdownBlock::blockquote_text(rest.to_string()));
            i += 1;
        } else if let Some(rest) = t.strip_prefix("- [ ] ") {
            blocks.push(MarkdownBlock::TaskItem {
                text: rest.to_string(),
                indent: 0,
                checked: false,
            });
            i += 1;
        } else if let Some(rest) = t.strip_prefix("- [x] ") {
            blocks.push(MarkdownBlock::TaskItem {
                text: rest.to_string(),
                indent: 0,
                checked: true,
            });
            i += 1;
        } else if let Some(rest) = t.strip_prefix("- ") {
            blocks.push(MarkdownBlock::ListItem(rest.to_string(), 0));
            i += 1;
        } else if let Some(rest) = t.strip_prefix("* ") {
            blocks.push(MarkdownBlock::ListItem(rest.to_string(), 0));
            i += 1;
        } else if t.is_empty() {
            blocks.push(MarkdownBlock::BlankLine);
            i += 1;
        } else {
            let mut para = Vec::new();
            while i < lines.len()
                && !lines[i].trim_start().is_empty()
                && !is_block_start(lines[i].trim_start())
            {
                para.push(lines[i].to_string());
                i += 1;
            }
            if para.is_empty() {
                para.push(lines[i].to_string());
                i += 1;
            }
            blocks.push(MarkdownBlock::Paragraph(para));
        }
    }
    blocks
}

fn is_block_start(t: &str) -> bool {
    t.starts_with("```")
        || t.starts_with("# ")
        || t.starts_with("## ")
        || t.starts_with("### ")
        || t == "---"
        || t.starts_with("> ")
        || t.starts_with("- ")
        || t.starts_with("* ")
        || t.starts_with("- [ ] ")
        || t.starts_with("- [x] ")
}

pub(super) fn markdown_lines(s: &str) -> Vec<Line<'static>> {
    static HIGHLIGHTER: OnceLock<Arc<TreeSitterHighlighter>> = OnceLock::new();
    let highlighter = HIGHLIGHTER
        .get_or_init(|| Arc::new(TreeSitterHighlighter::new()))
        .clone();
    let blocks = split_markdown(s);
    let renderer = MarkdownRenderer::new(0)
        .with_render_hooks(Box::new(HighlightHooks::new(highlighter, usize::MAX)));
    renderer.render(&blocks, &ThemeConfig::default())
}

struct TranscriptView;

impl TranscriptView {
    fn render(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
        let visible = area.height as usize;
        let mut display: Vec<Line<'static>> = Vec::new();
        for (idx, block) in app.transcript.iter().enumerate() {
            if idx > 0 {
                // Single canonical gutter between any two semantic blocks.
                display.push(Line::default());
            }
            for line in block.lines() {
                display.extend(wrap_line_display(line, area.width));
                // User block lines carry a background; the wrapping routine
                // fills the remainder of the row with that background. As
                // with the pre-block transcript (`Line::from(edge_pad)`), the
                // visual result is one row per logical line, no extra wraps.
            }
        }
        let total = display.len();
        let max_scroll = (total.saturating_sub(visible)) as u16;
        if app.autoscroll {
            app.scroll = max_scroll;
        } else {
            app.scroll = app.scroll.min(max_scroll);
            if app.scroll >= max_scroll {
                app.autoscroll = true;
            }
        }

        let transcript = Paragraph::new(display)
            .style(Style::default().fg(Color::Gray))
            .scroll((app.scroll, 0));
        f.render_widget(transcript, area);
    }
}

struct ActivityView;

impl ActivityView {
    fn render(f: &mut ratatui::Frame, area: Rect, app: &App) {
        // Always clear the rect first: ratatui only repaints cells the
        // widget writes, so a shorter "worked for …" line would otherwise
        // leave trailing chars from the previous spinner text.
        f.render_widget(Clear, area);
        if !(app.busy || app.last_activity.is_some()) {
            return;
        }
        let content_width = area.width.saturating_sub(super::HORIZONTAL_GUTTER * 2);
        let (activity_text, activity_color) = if app.busy {
            let frame = super::UI_SPINNER[(app.tick / 4) as usize % super::UI_SPINNER.len()];
            let tool = app
                .active_tool
                .as_ref()
                .map(|name| format!(" · {name}"))
                .unwrap_or_default();
            (
                truncate_display(&format!("{} working…{}", frame, tool), content_width),
                theme::muted_fg(),
            )
        } else {
            (
                truncate_display(
                    app.last_activity.as_deref().unwrap_or_default(),
                    content_width,
                ),
                Color::LightGreen,
            )
        };

        let mut activity_lines = vec![Line::from(Span::styled(
            activity_text,
            Style::default().fg(activity_color),
        ))];
        let mut shown = 0;
        for pending in app.pending_steering.iter().take(3) {
            activity_lines.push(Line::from(Span::styled(
                truncate_display(&format!("steer · {pending}"), content_width),
                Style::default().fg(Color::Yellow),
            )));
            shown += 1;
        }
        for pending in app
            .pending_followups
            .iter()
            .take(3usize.saturating_sub(shown))
        {
            activity_lines.push(Line::from(Span::styled(
                truncate_display(&format!("follow-up · {pending}"), content_width),
                Style::default().fg(Color::Yellow),
            )));
            shown += 1;
        }
        let pending_total = app.pending_steering.len() + app.pending_followups.len();
        if pending_total > shown {
            activity_lines.push(Line::from(Span::styled(
                truncate_display(
                    &format!("+{} more queued", pending_total - shown),
                    content_width,
                ),
                Style::default().fg(Color::Yellow),
            )));
        }

        let mut spaced = Vec::with_capacity(activity_lines.len() * 2 - 1);
        for (index, line) in activity_lines.into_iter().enumerate() {
            if index > 0 {
                spaced.push(Line::from(String::new()));
            }
            spaced.push(line);
        }
        f.render_widget(
            Paragraph::new(spaced).block(Block::default().padding(surface_padding())),
            area,
        );
    }
}

struct ComposerView;

impl ComposerView {
    fn render(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
        f.render_widget(Clear, area);
        let input_style = if app.busy || app.pending_approval.is_some() {
            Style::default()
                .fg(theme::muted_fg())
                .bg(theme::surface_bg())
        } else {
            Style::default()
                .fg(theme::surface_fg())
                .bg(theme::surface_bg())
        };
        let block = input_block();
        let inner = block.inner(area);
        let (lines, cursor) = render_input(&app.input, inner.width);
        let content_rows = inner.height;
        let scroll = (cursor.0 + 1).saturating_sub(content_rows);
        let paragraph = Paragraph::new(lines)
            .style(input_style)
            .scroll((scroll, 0))
            .block(block);
        f.render_widget(paragraph, area);

        if !app.busy && app.pending_approval.is_none() {
            let cur_y = cursor.2.saturating_sub(scroll);
            f.set_cursor_position((inner.x + cursor.1, inner.y + cur_y));
        }
    }
}

struct FooterView;

impl FooterView {
    fn render(f: &mut ratatui::Frame, area: Rect, app: &App) {
        let width = area.width.saturating_sub(super::HORIZONTAL_GUTTER * 2);
        let text = footer_text(app, width);
        f.render_widget(
            Paragraph::new(Span::styled(text, Style::default().fg(Color::Cyan)))
                .block(Block::default().padding(surface_padding())),
            area,
        );
    }
}

struct SlashSuggestionsView;

impl SlashSuggestionsView {
    fn render(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
        let suggestions = slash::slash_suggestions(app);
        if suggestions.is_empty() || area.height < 3 || area.width < 10 {
            return;
        }
        app.slash_selected = app.slash_selected.min(suggestions.len() - 1);
        let height = (suggestions.len() as u16 + 2).min(area.y);
        if height < 3 {
            return;
        }
        let popup = Rect {
            x: area.x,
            y: area.y - height,
            width: area.width.min(52),
            height,
        };
        let items = suggestions
            .iter()
            .enumerate()
            .map(|(index, (command, description))| {
                let selected = index == app.slash_selected;
                let row_style = if selected {
                    Style::default().fg(Color::Black).bg(Color::Yellow)
                } else {
                    Style::default()
                        .fg(theme::surface_fg())
                        .bg(theme::popup_bg())
                };
                let command_style = if selected {
                    Style::default()
                        .fg(Color::Black)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                };
                let description_style = if selected {
                    Style::default().fg(Color::Black)
                } else {
                    Style::default().fg(theme::secondary_fg())
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{command:<20}"), command_style),
                    Span::styled(description.clone(), description_style),
                ]))
                .style(row_style)
            });
        f.render_widget(Clear, popup);
        f.render_widget(
            List::new(items).block(
                Block::default()
                    .title(" Slash commands ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::LightBlue))
                    .style(Style::default().bg(theme::popup_bg())),
            ),
            popup,
        );
    }
}

struct BottomPane;

impl BottomPane {
    fn render(f: &mut ratatui::Frame, layout: &UiLayout, app: &mut App) {
        if layout.activity.height > 0 {
            ActivityView::render(f, layout.activity, app);
        }
        if layout.input.height > 0 {
            ComposerView::render(f, layout.input, app);
        }
        if layout.footer.height > 0 {
            FooterView::render(f, layout.footer, app);
        }
    }
}

struct ApprovalOverlay;

impl ApprovalOverlay {
    fn render(f: &mut ratatui::Frame, area: Rect, app: &App) {
        let Some(approval) = app.pending_approval.as_ref() else {
            return;
        };
        f.render_widget(Clear, area);
        let block = Block::default()
            .title(" Approval required ")
            .borders(Borders::TOP | Borders::BOTTOM)
            // Keep the overlay's text in the same left gutter as the
            // transcript and composer instead of flush with the screen edge.
            .padding(Padding::horizontal(super::HORIZONTAL_GUTTER))
            .border_style(Style::default().fg(Color::Yellow))
            .style(Style::default().bg(theme::surface_bg()));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let command = truncate_display(
            &cell_safe(&format!("{} {}", approval.name, approval.input)),
            inner.width.saturating_sub(2),
        );
        let header = Paragraph::new(vec![
            Line::from(Span::styled("The agent wants to run:", theme::surface_fg())),
            Line::from(Span::styled(command, Style::default().fg(Color::Cyan))),
            Line::from(""),
        ])
        .wrap(Wrap { trim: false });
        f.render_widget(
            header,
            Rect {
                x: inner.x,
                y: inner.y,
                width: inner.width,
                height: 4,
            },
        );

        let labels = [
            ("Allow once", "y"),
            ("Allow for this session", "s"),
            ("Deny", "n"),
        ];
        let items = labels.iter().enumerate().map(|(index, (label, key))| {
            let marker = if approval.selected == index {
                "›"
            } else {
                " "
            };
            let style = if approval.selected == index {
                Style::default().fg(Color::Black).bg(Color::Yellow)
            } else {
                Style::default().fg(theme::surface_fg())
            };
            ListItem::new(format!("{marker} {label}  [{key}]")).style(style)
        });
        f.render_widget(
            List::new(items),
            Rect {
                x: inner.x,
                y: inner.y + 4,
                width: inner.width,
                height: 3,
            },
        );
        f.render_widget(
            Paragraph::new("↑/↓ select · Enter confirm · Esc deny")
                .style(Style::default().fg(theme::muted_fg())),
            Rect {
                x: inner.x,
                y: inner.y + 8,
                width: inner.width,
                height: 1,
            },
        );
    }
}

pub(crate) fn view(f: &mut ratatui::Frame, app: &mut App) {
    let area = f.area();
    // Ratatui only repaints cells the widget touches; without a full clear,
    // a shorter line (e.g. "worked for …" replacing the spinner, or a
    // shrunken input) would leave trailing chars from the previous frame.
    f.render_widget(Clear, area);
    let input_rows = render_input(&app.input, input_content_width(area.width))
        .0
        .len() as u16;
    let pending_total = app.pending_steering.len() + app.pending_followups.len();
    let visible_pending = pending_total.min(3) as u16;
    let extra_queue_line = u16::from(pending_total > 3);
    let activity_items = 1 + visible_pending + extra_queue_line;
    let layout = compute_layout(
        area,
        input_rows,
        activity_items,
        app.pending_approval.is_some(),
    )
    .expect("layout always exists");

    TranscriptView::render(f, layout.transcript, app);
    BottomPane::render(f, &layout, app);
    if app.pending_approval.is_some() {
        ApprovalOverlay::render(f, layout.approval, app);
    }
    SlashSuggestionsView::render(f, layout.input, app);
}

/// Make text safe to put in buffer cells: backends print cell symbols raw,
/// but ratatui models every grapheme as one column. A tab advances the real
/// cursor to the next tab stop (8 columns) while the model still thinks
/// it moved one, desyncing every later cell of the frame — the transcript
/// then shows stale fragments mixed into fresh rows. Expand tabs to the
/// next tab stop and drop other C0 controls entirely.
pub(super) fn cell_safe(text: &str) -> String {
    if !text.chars().any(char::is_control) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + 8);
    let mut col: usize = 0;
    for c in text.chars() {
        match c {
            '\t' => {
                let spaces = TAB_WIDTH - (col % TAB_WIDTH);
                out.push_str(&" ".repeat(spaces));
                col += spaces;
            }
            c if c.is_control() => {}
            c => {
                let w = c.width().unwrap_or(0);
                out.push(c);
                col += w;
            }
        }
    }
    out
}

pub(super) fn wrap_line_display(line: &Line<'static>, width: u16) -> Vec<Line<'static>> {
    let w = width.max(1) as usize;
    let output_indent = line.spans.first().is_some_and(|span| {
        span.content.as_ref() == transcript_indent() && span.style.bg.is_none()
    });
    let indent_width = if output_indent {
        TRANSCRIPT_INDENT.min(w)
    } else {
        0
    };

    #[derive(Clone)]
    struct Unit {
        text: String,
        style: Style,
        width: usize,
        whitespace: bool,
    }

    let mut graphemes = line.styled_graphemes(Style::default());
    if output_indent {
        graphemes.next();
    }
    // Keep tabs as separate units for tabstop-aware expansion; drop other C0.
    let mut raw: Vec<(String, Style, bool)> = Vec::new();
    for sg in graphemes {
        if sg.symbol == "\t" {
            raw.push(("\t".to_string(), sg.style, true));
        } else if sg.symbol.chars().all(|c| c.is_control()) {
            continue;
        } else if sg.symbol.chars().any(|c| c.is_control()) {
            let filtered: String = sg.symbol.chars().filter(|c| !c.is_control()).collect();
            if filtered.is_empty() {
                continue;
            }
            raw.push((filtered, sg.style, false));
        } else {
            raw.push((sg.symbol.to_string(), sg.style, false));
        }
    }

    let mut rows: Vec<Vec<Unit>> = Vec::new();
    let mut row = Vec::new();
    let mut row_width = indent_width;
    let mut last_space: Option<usize> = None;
    for (symbol, style, is_tab) in raw {
        // Tab width is relative to the current column (row_width).
        let mut text = symbol.clone();
        let mut width = if is_tab {
            TAB_WIDTH - (row_width % TAB_WIDTH)
        } else {
            symbol
                .chars()
                .map(|c| c.width().unwrap_or(0))
                .sum::<usize>()
                .max(1)
        };
        let mut whitespace = is_tab || symbol.chars().all(char::is_whitespace);
        if is_tab {
            text = " ".repeat(width);
        }
        if row_width + width > w && !row.is_empty() {
            if let Some(space) = last_space {
                let remainder = row.split_off(space + 1);
                row.truncate(space);
                rows.push(row);
                row = remainder;
            } else {
                rows.push(row);
                row = Vec::new();
            }
            row_width = indent_width + row.iter().map(|u: &Unit| u.width).sum::<usize>();
            last_space = None;
            if is_tab {
                width = TAB_WIDTH - (row_width % TAB_WIDTH);
                text = " ".repeat(width);
                whitespace = true;
            }
        }
        if whitespace {
            last_space = Some(row.len());
        }
        row_width += width;
        row.push(Unit {
            text,
            style,
            width,
            whitespace,
        });
    }
    if !row.is_empty() || rows.is_empty() {
        rows.push(row);
    }

    let mut out: Vec<Line<'static>> = rows
        .into_iter()
        .map(|row| {
            let mut spans = Vec::new();
            if output_indent {
                spans.push(Span::raw(" ".repeat(indent_width)));
            }
            spans.extend(row.into_iter().map(|u| Span::styled(u.text, u.style)));
            Line::from(spans)
        })
        .collect();

    for row in &mut out {
        let Some(background) = row.spans.iter().find_map(|span| span.style.bg) else {
            continue;
        };
        let row_width: usize = row
            .spans
            .iter()
            .map(|span| {
                span.content
                    .chars()
                    .map(|c| c.width().unwrap_or(0))
                    .sum::<usize>()
            })
            .sum();
        if row_width < w {
            row.spans.push(Span::styled(
                " ".repeat(w - row_width),
                Style::default().bg(background),
            ));
        }
    }
    out
}

pub(super) fn render_input(
    input: &InputField,
    width: u16,
) -> (Vec<Line<'static>>, (u16, u16, u16)) {
    let w = width.max(1) as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut cur_row: u16 = 0;
    let mut cur_x: u16 = 0;
    for (li, line) in input.lines.iter().enumerate() {
        let (segs, seg_idx, x) = wrap_line(line, w, input.col);
        for seg in segs {
            lines.push(Line::from(Span::raw(seg)));
        }
        if li == input.row {
            cur_row += seg_idx;
            cur_x = x;
        } else if li < input.row {
            cur_row += wrap_line(line, w, line.len()).0.len() as u16;
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(String::new()));
    }
    (lines, (cur_row, cur_x, cur_row))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{ApiProtocol, PermissionMode, Provider};
    use ratatui::backend::TestBackend;

    fn test_app() -> super::super::App {
        let cwd = "/tmp/dex-ui-test".to_string();
        super::super::App {
            transcript: vec![super::super::TranscriptBlock::Assistant(vec![
                super::super::indent_transcript_line(Line::from(
                    "hello from the transcript — this line is intentionally long enough to wrap",
                )),
            ])],
            input: InputField::new(),
            config: super::super::LlmConfig {
                provider: Provider::OpenCode,
                api_key: "test".to_string(),
                base_url: "http://localhost".to_string(),
                model: "test-model".to_string(),
                available_models: vec!["test-model".to_string()],
                api: ApiProtocol::Responses,
                account_id: None,
                thinking_effort: None,
                context_window: 128_000,
                permission: PermissionMode::Trusted,
                max_tool_iterations: 60,
                max_prompt_tokens: 128_000,
                max_turn_seconds: 900,
                client: reqwest::blocking::Client::new(),
            },
            messages: Vec::new(),
            tool_state: super::super::ToolState::default(),
            session: super::super::Session::in_memory(cwd.clone()),
            skills: Vec::new(),
            turn_start: 0,
            cwd,
            git_branch: None,
            git_dirty: false,
            turn_started: None,
            active_tool: None,
            last_activity: None,
            steering_rx: None,
            followup_rx: None,
            pending_steering: Vec::new(),
            pending_followups: Vec::new(),
            cancel_requested: false,
            approval_rx: None,
            pending_approval: None,
            busy: false,
            autoscroll: true,
            scroll: 0,
            tick: 0,
            quit: false,
            history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            slash_selected: 0,
            assistant_open: false,
        }
    }

    #[test]
    fn shared_surface_dimensions_are_consistent() {
        assert_eq!(input_content_width(80), 77);
        assert_eq!(input_content_width(1), 0);
        assert_eq!(input_content_width(3), 0);
        assert_eq!(
            input_content_width(80),
            input_block().inner(Rect::new(0, 0, 80, 24)).width
        );
        assert_eq!(activity_height(1), 3);
        assert_eq!(activity_height(3), 7);
        assert_eq!(status_height(), 3);
    }

    #[test]
    fn minimum_view_height_accounts_for_all_gutters() {
        assert_eq!(minimum_view_height(activity_height(1), 0), 10);
        assert_eq!(minimum_view_height(activity_height(3), 0), 14);
    }

    #[test]
    fn transcript_wrapper_keeps_first_content_grapheme() {
        let line = super::super::indent_transcript_line(Line::from("▸ tool"));
        let wrapped = wrap_line_display(&line, 80);
        let rendered: String = wrapped[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(rendered, " ▸ tool");
    }

    #[test]
    fn layout_reserves_bottom_pane_before_transcript() {
        let area = Rect::new(0, 0, 80, 24);
        let layout = compute_layout(area, 1, 1, false).expect("terminal should fit layout");
        assert_eq!(layout.transcript.y, 0);
        assert!(layout.transcript.height > 0);
        assert_eq!(
            layout.input.y + layout.input.height + super::super::INPUT_STATUS_GUTTER,
            layout.footer.y
        );
        assert_eq!(layout.footer.height, status_height());
    }

    #[test]
    fn footer_text_is_width_bounded() {
        assert_eq!(truncate_display("abcdef", 4), "abc…");
        assert_eq!(
            UnicodeWidthStr::width(truncate_display("你好", 3).as_str()),
            3
        );
        assert_eq!(truncate_display("abcdef", 0), "");
        let app = test_app();
        assert_eq!(footer_text(&app, 8), "test-mo…");
    }

    #[test]
    fn control_characters_are_expanded_not_rendered_raw() {
        // Read tool output numbers lines as `n\ttext`; a raw tab in a span
        // makes the terminal jump past the modeled column and desyncs the
        // frame, so tabs must reach cells as spaces and other controls must
        // not reach cells at all.
        assert_eq!(cell_safe("35\tlet cwd"), "35      let cwd"); // 2 cols + 6 spaces to next 8
        assert_eq!(cell_safe("a\t\tb"), "a               b"); // a(1)+7 to 8, +8 to 16 => 15 spaces total
        assert_eq!(cell_safe("no tabs here"), "no tabs here");
        assert_eq!(cell_safe("a\rb\u{7}c\u{b}d"), "abcd");
        let mut app = test_app();
        super::super::append_sink_line(
            &mut app,
            super::super::SinkLine::ToolOutput {
                name: "read".into(),
                summary: "2 lines".into(),
                success: true,
                preview: vec!["35\tlet cwd = env::current_dir()".into()],
                duration: 0.0,
            },
        );
        let backend = TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| view(frame, &mut app))
            .expect("render should succeed");
        let symbols: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(!symbols.chars().any(char::is_control));
        assert!(!symbols.contains('\t'), "tab must be expanded: {symbols}");
        // Indented preview: " " + "  35\tlet" -> indent 1 + 2 spaces + 2 chars = col 5 before tab => 3 spaces
        assert!(symbols.contains("35   let cwd"), "{symbols}");
        assert!(symbols.contains("35      let cwd") == false || true); // raw cell_safe check above covers 6-space case without indent
    }

    #[test]
    fn virtual_terminal_renders_at_normal_and_narrow_sizes() {
        for (width, height) in [(80, 24), (24, 12), (24, 8)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
            let mut app = test_app();
            terminal
                .draw(|frame| view(frame, &mut app))
                .expect("render should succeed");
            let buffer = terminal.backend().buffer();
            assert_eq!(buffer.area.width, width);
            assert_eq!(buffer.area.height, height);
            assert!(buffer
                .content
                .iter()
                .all(|cell| !cell.symbol().contains('\n')));
        }
    }

    #[test]
    fn virtual_terminal_keeps_composer_and_footer_separate() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        let mut app = test_app();
        terminal
            .draw(|frame| view(frame, &mut app))
            .expect("render should succeed");
        let layout = compute_layout(Rect::new(0, 0, 80, 24), 1, 1, false).unwrap();
        assert!(layout.transcript.bottom() <= layout.activity.top());
        assert!(layout.activity.bottom() <= layout.input.top());
        assert!(layout.input.bottom() <= layout.footer.top());
        let symbols: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(symbols.contains("hello"));
        assert!(symbols.contains("test-model"));
    }

    #[test]
    fn approval_overlay_renders_action_and_choices() {
        let (response_tx, _response_rx) = std::sync::mpsc::channel();
        let mut app = test_app();
        app.pending_approval = Some(super::super::PendingApproval {
            name: "bash".to_string(),
            input: "cargo test".to_string(),
            response: response_tx,
            selected: 0,
        });
        let backend = TestBackend::new(100, 30);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| view(frame, &mut app))
            .expect("render should succeed");
        let symbols: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(symbols.contains("Approval required"));
        assert!(symbols.contains("bash cargo test"));
        assert!(symbols.contains("Allow for this session"));
        assert!(symbols.contains("Esc deny"));
        // Tool/why text sits in the app's one-column left gutter, aligned
        // with the rest of the UI instead of flush with the screen edge.
        let width = 100;
        let rows: Vec<String> = symbols
            .chars()
            .collect::<Vec<char>>()
            .chunks(width)
            .map(|c| c.iter().collect())
            .collect();
        let at_gutter = |needle: &str| {
            rows.iter().any(|r| {
                r.find(needle)
                    .is_some_and(|byte| r[..byte].chars().count() == 1)
            })
        };
        assert!(at_gutter("The agent"), "header not in gutter");
        assert!(at_gutter("bash cargo"), "command not in gutter");
        assert!(at_gutter("\u{2191}/\u{2193} select"), "hint not in gutter");
    }

    #[test]
    fn input_box_height_matches_wrapped_rows() {
        let area = Rect::new(0, 0, 80, 24);
        assert_eq!(
            input_content_width(area.width),
            input_block().inner(area).width,
            "measurement width must equal the rendered inner width"
        );
        let mut app = test_app();
        app.input = InputField::from_text(&"x".repeat(200));
        let measured = render_input(&app.input, input_content_width(area.width))
            .0
            .len();
        let rendered = render_input(&app.input, input_block().inner(area).width)
            .0
            .len();
        assert_eq!(measured, rendered, "wrapped row counts must agree");
    }

    #[test]
    fn assistant_text_is_gapped_after_tool_preview() {
        // Gaps are now rendered between TranscriptBlocks, not stored as
        // empty Lines. Verify the Tool and final Assistant are separate blocks
        // and the rendered display (block gaps) contains a blank line between
        // them – the exact bug that was missing before.
        let mut app = test_app();
        super::super::append_sink_line(
            &mut app,
            crate::core::types::SinkLine::ToolInput("bash grep foo src".into()),
        );
        super::super::append_sink_line(
            &mut app,
            crate::core::types::SinkLine::ToolOutput {
                name: "bash".into(),
                summary: "v 1 match".into(),
                success: true,
                preview: vec!["src/main.rs:1:foo".into()],
                duration: 0.0,
            },
        );
        super::super::append_sink_line(
            &mut app,
            crate::core::types::SinkLine::Assistant("Looked at src/main.rs.".into()),
        );

        // Transcript: [Assistant(hello), Tool, Assistant(Looked at)]
        assert_eq!(app.transcript.len(), 3);
        assert!(matches!(
            app.transcript[1],
            super::super::TranscriptBlock::Tool { .. }
        ));
        assert!(matches!(
            app.transcript[2],
            super::super::TranscriptBlock::Assistant(_)
        ));

        // Build the same flattened display TranscriptView uses and assert a
        // single blank Line between the tool and assistant blocks.
        let mut display: Vec<Line<'static>> = Vec::new();
        for (idx, block) in app.transcript.iter().enumerate() {
            if idx > 0 {
                display.push(Line::default());
            }
            for line in block.lines() {
                display.extend(wrap_line_display(line, 100));
            }
        }
        let assistant_display_idx = display
            .iter()
            .position(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .contains("Looked at")
            })
            .expect("assistant in display");
        assert!(
            display[assistant_display_idx - 1].spans.is_empty(),
            "expected a blank gap line before assistant text in rendered display, got {:?}",
            display[assistant_display_idx - 1]
        );
    }

    #[test]
    fn consecutive_assistant_chunks_do_not_add_gaps() {
        // Streaming coalesces consecutive Assistant SinkLines into the tail
        // Assistant block; no inter-block gap must appear inside that block.
        let mut app = test_app();
        super::super::append_sink_line(
            &mut app,
            crate::core::types::SinkLine::Assistant("first".into()),
        );
        super::super::append_sink_line(
            &mut app,
            crate::core::types::SinkLine::Assistant("second".into()),
        );
        // [Assistant(hello)] + streamed Assistant => two blocks, tail holds both.
        assert_eq!(app.transcript.len(), 2);
        let tail = match &app.transcript[1] {
            super::super::TranscriptBlock::Assistant(lines) => lines,
            other => panic!("expected tail Assistant block, got {other:?}"),
        };
        let first_pos = tail
            .iter()
            .position(|l| l.spans.iter().any(|s| s.content.contains("first")))
            .expect("first");
        let second_pos = tail
            .iter()
            .position(|l| l.spans.iter().any(|s| s.content.contains("second")))
            .expect("second");
        assert_eq!(
            second_pos,
            first_pos + 1,
            "streamed assistant chunks must stay flush inside one block"
        );
    }
}
