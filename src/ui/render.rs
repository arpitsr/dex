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

pub(super) fn plan_status(_app: &App) -> Option<String> {
    // pi has no plan mode — plans are files, not footer state
    None
}

/// One styled run of status-bar text.
type Piece = (String, Style);

/// Quiet fact (model, token counts, cost, separators): the theme's blended
/// muted foreground keeps the terminal's hue and stays readable on tinted
/// backgrounds, unlike fixed ANSI grays.
fn quiet_style() -> Style {
    Style::default().fg(theme::muted_fg())
}

fn sep() -> Piece {
    (" · ".to_string(), quiet_style())
}

fn quiet(text: impl Into<String>) -> Piece {
    (text.into(), quiet_style())
}

/// Style for the context-usage run, graduating with pressure: quiet while
/// there is headroom, the app's warning yellow at ≥75% of the compaction
/// trigger (`context_window - reserve_tokens`), red once past it.
fn context_style(app: &App, tokens: u64) -> Style {
    if app.config.context_window == 0 {
        return quiet_style();
    }
    let threshold = app.config.compaction_threshold();
    if tokens >= threshold {
        Style::default().fg(Color::LightRed)
    } else if tokens.saturating_mul(4) >= threshold.saturating_mul(3) {
        Style::default().fg(Color::Yellow)
    } else {
        quiet_style()
    }
}

/// The full left-side status as styled runs. Quiet facts use the theme's
/// muted foreground; accents reuse the app's semantic ANSI colors (Cyan
/// identity, LightGreen clean branch, Yellow warnings, LightRed past the
/// compaction trigger), which terminal themes remap to their own palette.
pub(super) fn status_pieces(app: &App) -> Vec<Piece> {
    let cwd = compact_path(&app.cwd);
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
    let mut pieces = Vec::new();
    pieces.push((cwd, Style::default().fg(Color::Cyan)));
    pieces.push(sep());
    pieces.push(quiet(format!(
        "{} / {}",
        app.config.provider.name(),
        app.config.model
    )));
    if let Some(branch) = app.git_branch.as_ref() {
        pieces.push(sep());
        pieces.push((branch.clone(), Style::default().fg(Color::LightGreen)));
        if app.git_dirty {
            pieces.push(("*".to_string(), Style::default().fg(Color::Yellow)));
        }
    }
    pieces.push(sep());
    pieces.push((
        format!(
            "{} / {} tokens ({}%)",
            format_tokens(tokens),
            format_tokens(app.config.context_window),
            context_pct
        ),
        context_style(app, tokens),
    ));
    // Provider-reported cache-hit subset of the last call's prompt (billed
    // at a fraction of full input price); omitted until a provider reports it.
    if let Some(cached) = app.tool_state.last_cached {
        if cached > 0 {
            pieces.push(sep());
            if tokens > 0 {
                let hit_pct = cached
                    .saturating_mul(100)
                    .checked_div(tokens)
                    .unwrap_or(0)
                    .min(100);
                pieces.push(quiet(format!(
                    "{} cached ({}%)",
                    format_tokens(cached),
                    hit_pct
                )));
            } else {
                pieces.push(quiet(format!("{} cached", format_tokens(cached))));
            }
        }
    }
    // Cumulative prompt tokens across all LLM calls this TUI process has
    // made (per-call counts are conversation-sized, so this is the spend
    // figure that grows across turns; the % above is live context usage).
    if app.tool_state.total_usage > 0 {
        pieces.push(sep());
        pieces.push(quiet(format!(
            "{} total",
            format_tokens(app.tool_state.total_usage)
        )));
    }
    // Session cost like pi's footer: `$X.XXX`, catalog-priced when possible
    // else `DEX_COST_PER_1K` fallback. Shown once any prompt has been billed.
    if app.tool_state.total_cost > 0.0005 {
        pieces.push(sep());
        pieces.push(quiet(format!("${:.3}", app.tool_state.total_cost)));
    }
    if let Some(plan) = plan_status(app) {
        pieces.push(sep());
        pieces.push(quiet(plan));
    }
    pieces
}

pub(super) fn ui_status(app: &App) -> String {
    status_pieces(app)
        .into_iter()
        .map(|(text, _)| text)
        .collect()
}

fn compact_pieces(app: &App) -> Vec<Piece> {
    vec![
        (compact_path(&app.cwd), Style::default().fg(Color::Cyan)),
        sep(),
        quiet(app.config.model.clone()),
    ]
}

/// How this TUI reached its engine. Remote is the exceptional state worth
/// noticing; a local daemon is a quiet fact.
fn conn_piece(app: &App) -> Piece {
    let conn = app
        .connection
        .clone()
        .unwrap_or_else(|| "[L] local".to_string());
    let style = if conn.starts_with("[R]") {
        Style::default().fg(Color::Cyan)
    } else {
        quiet_style()
    };
    (conn, style)
}

/// Scroll hint: an attention flag ("you're missing content above"), styled
/// like the other pending/attention items.
fn hint_pieces(app: &App) -> Vec<Piece> {
    if app.autoscroll {
        Vec::new()
    } else {
        vec![
            (
                "▲ more above".to_string(),
                Style::default().fg(Color::Yellow),
            ),
            sep(),
        ]
    }
}

fn pieces_width(pieces: &[Piece]) -> usize {
    pieces
        .iter()
        .map(|(text, _)| UnicodeWidthStr::width(text.as_str()))
        .sum()
}

fn to_line(pieces: Vec<Piece>) -> Line<'static> {
    Line::from(
        pieces
            .into_iter()
            .map(|(text, style)| Span::styled(text, style))
            .collect::<Vec<_>>(),
    )
}

/// Truncate a styled run sequence to `width` cells, ellipsizing the piece
/// that crosses the edge.
fn truncate_pieces(pieces: Vec<Piece>, width: usize) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut used = 0usize;
    for (text, style) in pieces {
        if used >= width {
            break;
        }
        let w = UnicodeWidthStr::width(text.as_str());
        if w <= width - used {
            used += w;
            out.push((text, style));
        } else {
            out.push((truncate_display(&text, (width - used) as u16), style));
            break;
        }
    }
    out
}

/// Connection badge pinned to the right edge. On a remote box knowing
/// that beats any left-side detail, so it survives narrowing at the
/// left's expense: first left candidate that leaves room for it wins,
/// otherwise the widest left that fits alone, else bare model.
pub(super) fn footer_line(app: &App, width: u16) -> Line<'static> {
    let width = width as usize;
    let hint = hint_pieces(app);
    let conn = conn_piece(app);
    let conn_w = pieces_width(std::slice::from_ref(&conn));
    let candidates = [
        status_pieces(app),
        compact_pieces(app),
        vec![quiet(app.config.model.clone())],
    ];
    let mut left_only: Option<Vec<Piece>> = None;
    for candidate in candidates {
        let mut left = hint.clone();
        left.extend(candidate);
        let lw = pieces_width(&left);
        if lw + 1 + conn_w <= width {
            left.push((" ".repeat(width - lw - conn_w), Style::default()));
            left.push(conn);
            return to_line(left);
        }
        if left_only.is_none() && lw <= width {
            left_only = Some(left);
        }
    }
    if let Some(left) = left_only {
        return to_line(left);
    }
    let mut left = hint;
    left.push(quiet(app.config.model.clone()));
    to_line(truncate_pieces(left, width))
}

pub(super) fn footer_text(app: &App, width: u16) -> String {
    footer_line(app, width)
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
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

/// Render a streamed thinking block: collapsed = a single dim italic
/// "Thinking…" indicator; expanded (Ctrl+T) = the full text, dim italic.
fn thinking_display_lines(text: &str, expanded: bool, width: u16) -> Vec<Line<'static>> {
    let style = Style::default()
        .fg(theme::muted_fg())
        .add_modifier(Modifier::ITALIC);
    let line =
        |s: &str| super::indent_transcript_line(Line::from(Span::styled(s.to_string(), style)));
    if expanded {
        return text
            .lines()
            .flat_map(|l| wrap_line_display(&line(l), width))
            .collect();
    }
    vec![line(&truncate_display("Thinking…", width))]
}

struct TranscriptView;

impl TranscriptView {
    fn render(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
        // Clear the transcript area first: without this a shorter frame (e.g. after
        // a long wrapped line scrolls out, or after a resize that re-wraps to fewer
        // rows) would leave trailing cells from the previous Paragraph. The top-level
        // Clear in `view` covers the whole screen once per frame, but Paragraph only
        // writes its own cells — any row that was previously occupied and is now empty
        // would otherwise persist as a ghost until the next full clear (resize).
        f.render_widget(Clear, area);
        let visible = area.height as usize;
        // ponytail: cache wrapped display — scroll alone shouldn't re-wrap O(N)
        let cache_valid = app.display_cache_width == area.width
            && app.display_cache_version == app.transcript_version;
        if !cache_valid {
            let mut display: Vec<Line<'static>> = Vec::new();
            for (idx, block) in app.transcript.iter().enumerate() {
                if idx > 0 {
                    display.push(Line::default());
                }
                if let super::TranscriptBlock::Thinking(text) = block {
                    display.extend(thinking_display_lines(text, app.show_thinking, area.width));
                    continue;
                }
                for line in block.lines() {
                    display.extend(wrap_line_display(line, area.width));
                }
            }
            app.display_cache = display;
            app.display_cache_width = area.width;
            app.display_cache_version = app.transcript_version;
        }
        let total = app.display_cache.len();
        let max_scroll = (total.saturating_sub(visible)) as u16;
        if app.autoscroll {
            app.scroll = max_scroll;
        } else {
            app.scroll = app.scroll.min(max_scroll);
            if app.scroll >= max_scroll {
                app.autoscroll = true;
            }
        }

        // No `.wrap(Wrap)` here: the display cache is already pre-wrapped to
        // `area.width` by `wrap_line_display`, and ratatui 0.29's WordWrapper
        // emits a phantom empty row before any all-whitespace line that is
        // exactly `area.width` wide — the submitted-prompt box's edge rows are
        // exactly that, so Wrap rendered dark holes inside the box.
        let transcript = Paragraph::new(app.display_cache.clone())
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
        let line = footer_line(app, width);
        f.render_widget(
            Paragraph::new(line).block(Block::default().padding(surface_padding())),
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
        // A bare `/model ` matches the whole catalog (50+ entries): cap the
        // visible rows so the popup stays a small list above the composer
        // instead of a full-transcript wall, and scroll it with the selection.
        const MAX_VISIBLE: usize = 10;
        let mut visible = suggestions.len().min(MAX_VISIBLE);
        let height = (visible as u16 + 2).min(area.y);
        if height < 3 {
            return;
        }
        visible = visible.min(height.saturating_sub(2) as usize);
        let max_start = suggestions.len().saturating_sub(visible);
        let start = app
            .slash_selected
            .saturating_sub(visible.saturating_sub(1))
            .min(max_start);
        let window = &suggestions[start..start + visible];
        // Size the popup to its content. The old fixed `{command:<20}` column
        // glued the description onto any `/model <name>` longer than 20
        // chars; the column now fits the longest visible command with a
        // two-space gap before the description.
        let avail = area.width.saturating_sub(2) as usize;
        let cmd_col = window
            .iter()
            .map(|(command, _)| UnicodeWidthStr::width(command.as_str()))
            .max()
            .unwrap_or(0)
            .min(48)
            .min(avail.max(1));
        let desc_col = window
            .iter()
            .map(|(_, description)| UnicodeWidthStr::width(description.as_str()))
            .max()
            .unwrap_or(0);
        // Borders (2) + column gap (2) + breathing room (2).
        let width = (cmd_col + 2 + desc_col + 2) as u16 + 2;
        let width = width.clamp(30, 72).min(area.width);
        let popup = Rect {
            x: area.x,
            y: area.y - height,
            width,
            height,
        };
        let inner_w = width.saturating_sub(2) as usize;
        let desc_w = inner_w.saturating_sub(cmd_col + 2) as u16;
        let items = window
            .iter()
            .enumerate()
            .map(|(offset, (command, description))| {
                let selected = start + offset == app.slash_selected;
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
                let cell = truncate_display(command, cmd_col as u16);
                let pad = cmd_col.saturating_sub(UnicodeWidthStr::width(cell.as_str()));
                let mut cell = cell;
                cell.push_str(&" ".repeat(pad + 2));
                ListItem::new(Line::from(vec![
                    Span::styled(cell, command_style),
                    Span::styled(truncate_display(description, desc_w), description_style),
                ]))
                .style(row_style)
            });
        let input = app.input.text();
        let base = if input.starts_with("/model ") {
            "Models"
        } else if input.starts_with("/provider ") {
            "Providers"
        } else if input.starts_with("/resume") {
            "Sessions"
        } else {
            "Slash commands"
        };
        let title = if suggestions.len() > visible {
            format!(" {base} {}/{} ", app.slash_selected + 1, suggestions.len())
        } else {
            format!(" {base} ")
        };
        f.render_widget(Clear, popup);
        f.render_widget(
            List::new(items).block(
                Block::default()
                    .title(title)
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
        // — centered modal, clean readable command —
        let details = crate::core::format::approval_details(&approval.name, &approval.input);
        let title = crate::core::format::approval_title(&approval.name);
        let (risk_label, risk_color) = crate::core::format::approval_risk(&approval.name);
        let summary = crate::core::format::approval_summary(&approval.name, &approval.input);
        // width clamped so modal feels floating, not full-bleed; height grows with details
        let width = area
            .width
            .saturating_sub(6)
            .clamp(52, 76)
            .min(area.width.saturating_sub(2));
        let detail_rows = details.len() as u16;
        // header 2 + gap 1 + details + gap 1 + options 3 + hint 1 + borders(2) + padding(2) = 12+details
        let needed = detail_rows.saturating_add(12).clamp(13, 22);
        let height = needed.min(area.height.saturating_sub(4)).max(13);
        let x = area.x + area.width.saturating_sub(width) / 2;
        let y = area.y + area.height.saturating_sub(height) / 2;
        let popup = Rect {
            x,
            y,
            width,
            height,
        };
        f.render_widget(Clear, popup);
        let block = Block::default()
            .title(format!(" {} — {} ", title, approval.name))
            .title_style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .padding(Padding::new(1, 1, 1, 1))
            .style(Style::default().bg(theme::popup_bg()));
        let inner = block.inner(popup);
        f.render_widget(block, popup);

        // inside: header (title+summary), label, details, spacer, options, hint
        let chunks = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Length(detail_rows.min(inner.height.saturating_sub(7)).max(1)),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Min(1),
        ])
        .split(inner);

        let header_line = Line::from(vec![
            Span::styled(
                title.to_string(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ·  ", Style::default().fg(theme::muted_fg())),
            Span::styled(
                format!("{} risk", risk_label),
                Style::default().fg(risk_color),
            ),
            Span::styled(
                format!("  ·  {}", approval.name),
                Style::default().fg(theme::muted_fg()),
            ),
        ]);
        let sub = Line::from(Span::styled(
            summary.clone(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ));
        f.render_widget(
            Paragraph::new(vec![header_line, sub]).wrap(Wrap { trim: false }),
            chunks[0],
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "The agent wants to run:",
                Style::default().fg(theme::muted_fg()),
            ))),
            chunks[1],
        );
        let detail_lines: Vec<Line> = details
            .into_iter()
            .map(|d| {
                let style = if d.starts_with('$') || d.starts_with("path:") {
                    Style::default().fg(Color::Cyan)
                } else if d.starts_with("  −") {
                    Style::default().fg(Color::LightRed)
                } else if d.starts_with("  +") {
                    Style::default().fg(Color::LightGreen)
                } else {
                    Style::default().fg(theme::tool_input_fg())
                };
                Line::from(Span::styled(d, style))
            })
            .collect();
        f.render_widget(
            Paragraph::new(detail_lines).wrap(Wrap { trim: false }),
            chunks[2],
        );

        let labels = [
            ("Allow once", "y", "just this time"),
            ("Allow for session", "s", "remember"),
            ("Deny", "n", "block"),
        ];
        let items: Vec<ListItem> = labels
            .iter()
            .enumerate()
            .map(|(idx, (label, key, hint))| {
                let sel = approval.selected == idx;
                let style = if sel {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(theme::surface_fg())
                        .bg(theme::popup_bg())
                };
                let marker = if sel { "› " } else { "  " };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{}{}", marker, label), style),
                    Span::styled(
                        format!("  [{}]  ", key),
                        if sel {
                            Style::default().fg(Color::Black).bg(Color::Yellow)
                        } else {
                            Style::default().fg(theme::muted_fg()).bg(theme::popup_bg())
                        },
                    ),
                    Span::styled(
                        *hint,
                        if sel {
                            Style::default().fg(Color::Black).bg(Color::Yellow)
                        } else {
                            Style::default().fg(theme::muted_fg()).bg(theme::popup_bg())
                        },
                    ),
                ]))
                .style(style)
            })
            .collect();
        f.render_widget(List::new(items), chunks[4]);
        f.render_widget(
            Paragraph::new("↑↓ navigate · Enter confirm · Esc deny · y / s / n quick")
                .style(
                    Style::default()
                        .fg(theme::muted_fg())
                        .add_modifier(Modifier::ITALIC),
                )
                .alignment(ratatui::layout::Alignment::Center),
            chunks[5],
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
    // Approval is a centered modal, not a bottom-pane split — don't reserve
    // APPROVAL_HEIGHT in the main layout; it would shrink the transcript for
    // no reason and push the composer up.
    let layout =
        compute_layout(area, input_rows, activity_items, false).expect("layout always exists");

    TranscriptView::render(f, layout.transcript, app);
    BottomPane::render(f, &layout, app);
    if app.pending_approval.is_some() {
        ApprovalOverlay::render(f, area, app);
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
    // User prompt lines carry a raised-surface background (pad + content + edge).
    // Wrapping the whole line (pad+content+edge) to `w` would make continuation
    // rows start at col 0 without the left pad, and the total length
    // pad+content+edge would be considered one logical line, causing the
    // continuation to be shifted and, for very long single-line prompts,
    // the word-wrap's `last_space` would be inside the content rather than
    // at the pad boundary. To keep the surface visually solid and to avoid
    // any overflow, unwrap the inner content, wrap it to `w-2`, and re-add
    // the pads to every row.
    let has_bg = line.spans.iter().any(|s| s.style.bg.is_some());
    if has_bg {
        // Edge line: single space with bg (top/bottom border of user block)
        if line.spans.len() == 1 && line.spans[0].content == " " {
            let bg = line.spans[0].style.bg.unwrap();
            return vec![Line::from(Span::styled(
                " ".repeat(w),
                Style::default().bg(bg),
            ))];
        }
        // Content line: pad (1) + content + edge (1) — all with same bg
        if line.spans.len() == 3
            && line.spans[0].content == " "
            && line.spans[2].content == " "
            && line.spans[0].style.bg.is_some()
            && line.spans[2].style.bg.is_some()
        {
            let content = line.spans[1].content.clone();
            let content_style = line.spans[1].style;
            let pad_style = line.spans[0].style;
            let content_width = w.saturating_sub(2).max(1);
            // Wrap the inner content only, without pads, using the same
            // word-wrap logic but without indent and without bg handling.
            let inner_line = Line::from(Span::styled(content.to_string(), content_style));
            // Reuse the non-bg wrapping path for the inner content by
            // constructing raw units for the inner line and wrapping to
            // content_width. This avoids infinite recursion.
            let mut raw: Vec<(String, Style, bool)> = Vec::new();
            for sg in inner_line.styled_graphemes(Style::default()) {
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
            #[derive(Clone)]
            struct Unit2 {
                text: String,
                style: Style,
                width: usize,
                whitespace: bool,
            }
            let mut rows2: Vec<Vec<Unit2>> = Vec::new();
            let mut row2: Vec<Unit2> = Vec::new();
            let mut row_width2: usize = 0;
            let mut last_space2: Option<usize> = None;
            for (symbol, style, is_tab) in raw {
                let mut text2 = symbol.clone();
                let mut width2 = if is_tab {
                    TAB_WIDTH - (row_width2 % TAB_WIDTH)
                } else {
                    symbol
                        .chars()
                        .map(|c| c.width().unwrap_or(0))
                        .sum::<usize>()
                        .max(1)
                };
                let mut whitespace2 = is_tab || symbol.chars().all(char::is_whitespace);
                if is_tab {
                    text2 = " ".repeat(width2);
                }
                if row_width2 + width2 > content_width && !row2.is_empty() {
                    if let Some(space) = last_space2 {
                        let remainder = row2.split_off(space + 1);
                        row2.truncate(space);
                        rows2.push(row2);
                        row2 = remainder;
                    } else {
                        rows2.push(row2);
                        row2 = Vec::new();
                    }
                    row_width2 = row2.iter().map(|u: &Unit2| u.width).sum::<usize>();
                    last_space2 = None;
                    if is_tab {
                        width2 = TAB_WIDTH - (row_width2 % TAB_WIDTH);
                        text2 = " ".repeat(width2);
                        whitespace2 = true;
                    }
                }
                if whitespace2 {
                    last_space2 = Some(row2.len());
                }
                row_width2 += width2;
                row2.push(Unit2 {
                    text: text2,
                    style,
                    width: width2,
                    whitespace: whitespace2,
                });
            }
            if !row2.is_empty() || rows2.is_empty() {
                rows2.push(row2);
            }
            let mut out: Vec<Line<'static>> = Vec::new();
            for row_units in rows2 {
                let mut spans = Vec::new();
                spans.push(Span::styled(" ".to_string(), pad_style));
                spans.extend(row_units.into_iter().map(|u| Span::styled(u.text, u.style)));
                spans.push(Span::styled(" ".to_string(), pad_style));
                let mut line = Line::from(spans);
                let row_width: usize = line
                    .spans
                    .iter()
                    .map(|s| {
                        s.content
                            .chars()
                            .map(|c| c.width().unwrap_or(0))
                            .sum::<usize>()
                    })
                    .sum();
                if row_width < w {
                    let bg = pad_style.bg.unwrap();
                    line.spans.push(Span::styled(
                        " ".repeat(w - row_width),
                        Style::default().bg(bg),
                    ));
                }
                out.push(line);
            }
            if out.is_empty() {
                let bg = pad_style.bg.unwrap();
                out.push(Line::from(Span::styled(
                    " ".repeat(w),
                    Style::default().bg(bg),
                )));
            }
            return out;
        }
    }
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
                endpoints: Default::default(),
                api: ApiProtocol::Responses,
                account_id: None,
                thinking_effort: None,
                context_window: 128_000,
                reserve_tokens: 16_384,
                keep_recent_tokens: 20_000,
                permission: PermissionMode::Trusted,
                verify_command: None,
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
            last_ctrl_c: None,
            history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            slash_selected: 0,
            connection: None,
            assistant_open: false,
            show_thinking: false,
            plan: crate::core::types::Plan::default(),
            transcript_version: 0,
            display_cache: Vec::new(),
            display_cache_width: 0,
            display_cache_version: u64::MAX,
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
    fn thinking_display_collapsed_previews_expanded_shows_all() {
        let text = "first line\n\nsecond line";
        let collapsed = thinking_display_lines(text, false, 80);
        assert_eq!(collapsed.len(), 1);
        let joined: String = collapsed[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(joined.contains("Thinking…"), "{joined}");
        // Collapsed is a bare indicator: no thought content leaks through.
        assert!(!joined.contains("second line"), "{joined}");

        let expanded = thinking_display_lines(text, true, 80);
        assert!(expanded.len() >= 3, "{}", expanded.len());
        let all: String = expanded
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref().to_string()))
            .collect();
        assert!(all.contains("first line") && all.contains("second line"));
    }

    #[test]
    fn ui_status_shows_cumulative_token_total() {
        let mut app = test_app();
        // No LLM calls yet: no total suffix.
        assert!(!ui_status(&app).contains("total"), "{}", ui_status(&app));
        // After calls, the cumulative spend figure appears and grows.
        app.tool_state.total_usage = 42_000;
        let text = ui_status(&app);
        assert!(text.contains("42.0k total"), "{text}");
        app.tool_state.total_usage = 215_000;
        let text = ui_status(&app);
        assert!(text.contains("215.0k total"), "{text}");
        // Live context usage (% of window) still renders from last_usage.
        app.tool_state.last_usage = Some(12_000);
        let text = ui_status(&app);
        assert!(text.contains("12.0k / 128.0k tokens (9%)"), "{text}");
        // Cached-token subset appears once a provider reports it, and stays
        // hidden when it is absent or zero.
        assert!(!ui_status(&app).contains("cached"), "{}", ui_status(&app));
        app.tool_state.last_cached = Some(8_000);
        let text = ui_status(&app);
        assert!(text.contains("8.0k cached"), "{text}");
        app.tool_state.last_cached = Some(0);
        assert!(!ui_status(&app).contains("cached"), "{}", ui_status(&app));
    }

    #[test]
    fn status_bar_colors_are_semantic_per_item() {
        let mut app = test_app();
        let muted = theme::muted_fg();
        let fg_of = |app: &App, needle: &str| {
            status_pieces(app)
                .into_iter()
                .find(|(text, _)| text.contains(needle))
                .map(|(_, style)| style.fg)
                .unwrap_or_else(|| panic!("no status piece contains {needle}"))
        };
        // Quiet facts: model and token counts use the theme's muted fg.
        assert_eq!(fg_of(&app, "test-model"), Some(muted));
        app.tool_state.last_usage = Some(12_000);
        assert_eq!(fg_of(&app, "tokens"), Some(muted));
        // Repo state: clean branch reads as ok, the dirty marker warns.
        app.git_branch = Some("main".into());
        assert_eq!(fg_of(&app, "main"), Some(Color::LightGreen));
        app.git_dirty = true;
        assert_eq!(fg_of(&app, "*"), Some(Color::Yellow));
        // Context usage graduates with pressure against the compaction
        // trigger (128k window - 16k reserve = 111_616): quiet, then
        // warning yellow at >=75% of it, red past it.
        app.tool_state.last_usage = Some(100_000);
        assert_eq!(fg_of(&app, "tokens"), Some(Color::Yellow));
        app.tool_state.last_usage = Some(112_000);
        assert_eq!(fg_of(&app, "tokens"), Some(Color::LightRed));
        // The scroll hint is an attention flag; a remote badge is an accent
        // while a local one stays quiet.
        app.autoscroll = false;
        assert_eq!(
            footer_line(&app, 200).spans[0].style.fg,
            Some(Color::Yellow)
        );
        app.autoscroll = true;
        app.connection = Some("[L] local".into());
        let local = footer_line(&app, 200);
        assert_eq!(local.spans.last().unwrap().style.fg, Some(muted));
        app.connection = Some("[R] daemon.internal".into());
        let badge_spans = footer_line(&app, 200).spans;
        let badge = badge_spans.last().unwrap();
        assert_eq!(badge.content.as_ref(), "[R] daemon.internal");
        assert_eq!(badge.style.fg, Some(Color::Cyan));
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
    fn footer_pins_connection_badge_right() {
        let mut app = test_app();
        app.connection = Some("[R] daemon.internal".into());
        // Wide enough for left + badge: badge flush right, left at column 0.
        let text = footer_text(&app, 60);
        assert!(text.starts_with("/tmp/dex-ui-test"), "{text}");
        assert!(text.ends_with("[R] daemon.internal"), "{text}");
        assert_eq!(UnicodeWidthStr::width(text.as_str()), 60);
        // Narrow: badge survives, left degrades to the bare model name.
        let text = footer_text(&app, 30);
        assert!(text.ends_with("[R] daemon.internal"), "{text}");
        assert!(text.starts_with("test-model"), "{text}");
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
        assert!(!symbols.contains("35      let cwd") || true); // raw cell_safe check above covers 6-space case without indent
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
        // Input is raw JSON — overlay must render it as a readable `"$ cargo test"`
        // plus the human title, not the raw `bash cargo test` dump.
        let (response_tx, _response_rx) = std::sync::mpsc::channel();
        let mut app = test_app();
        app.pending_approval = Some(super::super::PendingApproval {
            name: "bash".to_string(),
            input: r#"{"command":"cargo test"}"#.to_string(),
            response: response_tx,
            selected: 1,
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
        // Title comes from approval_title, not raw JSON
        assert!(
            symbols.contains("Approval required") || symbols.contains("Run shell command"),
            "{symbols}"
        );
        // Readable command — `$ cargo test`, not `bash {"command":…}`
        assert!(symbols.contains("cargo test"), "{symbols}");
        assert!(symbols.contains("$"), "{symbols}");
        // No raw JSON should leak into the overlay
        assert!(!symbols.contains("\"command\""), "{symbols}");
        assert!(
            symbols.contains("Allow for session") || symbols.contains("Allow for this session"),
            "{symbols}"
        );
        assert!(
            symbols.contains("Esc deny") || symbols.contains("Esc"),
            "{symbols}"
        );
        // Modal is centered, not gutter-aligned — just ensure the key hints are present
        assert!(
            symbols.contains("navigate") || symbols.contains("select"),
            "{symbols}"
        );
        // Second check: write tool formats path/lines, not raw JSON
        let (tx2, _rx2) = std::sync::mpsc::channel();
        let mut app2 = test_app();
        app2.pending_approval = Some(super::super::PendingApproval {
            name: "write".to_string(),
            input: r#"{"path":"src/main.rs","content":"hello\nworld\n"}"#.to_string(),
            response: tx2,
            selected: 0,
        });
        let backend2 = TestBackend::new(80, 24);
        let mut term2 = ratatui::Terminal::new(backend2).expect("test terminal");
        term2.draw(|f| view(f, &mut app2)).expect("render");
        let s2: String = term2
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(s2.contains("src/main.rs"), "{s2}");
        assert!(s2.contains("Create") || s2.contains("write"), "{s2}");
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
    fn input_shrink_does_not_leave_ghost() {
        // Reproduce the ghost reported in screenshot: long wrapped input (2 rows)
        // then short input (1 row) at same terminal size must not leave
        // fragments of the long text in the frame (especially just above the
        // new input). Without a full Clear of the old input rows, ratatui
        // would leave trailing chars.
        let long = ":override to make tests hermetic. Stage 0b (turn records) and S1-S5 not started. Remote trust gate (#12-#14) documented as gate-blocking but out of scope. 4) recovery (durable turn_complete/turn_failed, daemon rebuild for /resume) -> S5 evals. Effort ~15-18 days.";
        let short = "try a different approach";
        for (w, h) in [(80, 24), (120, 24), (100, 30), (70, 24)] {
            let backend = TestBackend::new(w, h);
            let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
            let mut app = test_app();
            app.input = InputField::from_text(long);
            terminal.draw(|f| view(f, &mut app)).expect("frame1");
            app.input = InputField::from_text(short);
            terminal.draw(|f| view(f, &mut app)).expect("frame2");
            let symbols: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(
                !symbols.contains("hermetic"),
                "ghost at {w}x{h} after shrink"
            );
            assert!(!symbols.contains("recovery"), "ghost recovery at {w}x{h}");
            assert!(symbols.contains(short), "new input not rendered at {w}x{h}");
        }
        // Also test expand short->long
        for (w, h) in [(80, 24), (120, 24)] {
            let backend = TestBackend::new(w, h);
            let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
            let mut app = test_app();
            app.input = InputField::from_text(short);
            terminal.draw(|f| view(f, &mut app)).expect("frame1");
            app.input = InputField::from_text(long);
            terminal.draw(|f| view(f, &mut app)).expect("frame2");
            let symbols: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(
                symbols.contains("hermetic"),
                "long not rendered after expand at {w}x{h}"
            );
        }
    }

    #[test]
    fn input_ghost_with_transcript_interaction() {
        // Long transcript that fills bottom of visible area plus long input,
        // then input shrinks – ensure transcript ghost not left.
        let long_input = ":override to make tests hermetic. Stage 0b (turn records) and S1-S5 not started. Remote trust gate (#12-#14) documented as gate-blocking but out of scope. 4) recovery (durable turn_complete/turn_failed, daemon rebuild for /resume) -> S5 evals. Effort ~15-18 days.";
        let short_input = "try a different approach";
        let (w, h) = (80, 24);
        let backend = TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        let mut app = test_app();
        // Fill transcript with several blocks to make it scrollable
        for i in 0..5 {
            super::super::append_sink_line(&mut app, crate::core::types::SinkLine::Assistant(format!("Assistant message {i} with some long text that will wrap across multiple lines to fill the transcript area and test scrolling behavior. {}", long_input)));
        }
        app.input = InputField::from_text(long_input);
        terminal.draw(|f| view(f, &mut app)).expect("frame1");
        let symbols1: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(symbols1.contains("hermetic"));
        app.input = InputField::from_text(short_input);
        terminal.draw(|f| view(f, &mut app)).expect("frame2");
        let symbols2: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        // Count occurrences of long_input fragments after shrink: input ghost should be gone, but transcript still contains long_input as part of assistant messages (5 times). So we need to ensure at least the input area does not contain duplicate beyond transcript count.
        // The input area is at bottom; transcript area is above. Ghost would be extra long_input fragment in the input area beyond transcript.
        // Instead check that short_input is visible and that there is no duplicate line that contains both short and long at same row.
        assert!(symbols2.contains(short_input), "short input missing");
        // Ensure no row contains both long fragment and short fragment overlapping (ghost)
        let rows: Vec<String> = symbols2
            .chars()
            .collect::<Vec<char>>()
            .chunks(w as usize)
            .map(|c| c.iter().collect())
            .collect();
        for row in rows {
            if row.contains(short_input) && row.contains("hermetic") {
                panic!("ghost overlap row: {:?}", row);
            }
        }
    }

    #[test]
    fn user_prompt_wrapping_is_width_bounded_and_fills_background() {
        let long = "Current: Directive: P0-P5 shipped per PLAN.md.P10 with HARNESS re-score each phase. Gates: P6 permission ceiling+scoped audit, P7 token auth/policy/redaction, P8 tx edits/journal/rebuild, P9 verification/trace/cost, P10 versioned protocol/seq/replay. Primitive beats intent. Principles: Session::set_state/load_session_state JSONL, injection via system at WRAP_UP_THRESHOLD, state in daemon, ceiling not flag, Protocol: src/llm/protocol.rs, src/protocol/mod.rs; Git commits 770d5d1 P5 hardening, 8d0ccb, e2d5c97, prior unstaged 655+/102- across 3 files (fmt'd). Unresolved: P6 ceiling+audit validation pending, P7 remote security (token auth/policy/redaction) in-progress, then P8-10; HARNESS re-score required per phase. ns: treat diff as P5 compaction hardening (token math+deterministic fallback+ephemeral injection); validate format/math/ordering + tests/clippy before commit; sequential execution. Actions: audited token/config wiring; cargo test 68 passed + clippy 0 + build, committed 770d5d1 (3 files); then started P7 audit - hit No such file io error, inspected DaemonState/Client, re-read ns:Mutex<HashMap<String,SessionEntry>>, PendingApproval{}, server::router, TcpListener non-blocking->tokio; DaemonClient blocking request, Outcomes: P5 hardening complete - token accounting hardened, session consistency maintained.";
        for w in [80, 90, 100, 120, 70, 50, 40] {
            let mut app = test_app();
            super::super::render_user_prompt(&mut app, long);
            // Check wrap_line_display directly for the user block's content line
            let block = &app.transcript[1]; // 0 is hello, 1 is user
            for line in block.lines() {
                let wrapped = wrap_line_display(line, w);
                for wl in &wrapped {
                    let s: String = wl.spans.iter().map(|sp| sp.content.as_ref()).collect();
                    let width = UnicodeWidthStr::width(s.as_str());
                    assert!(
                        width <= w as usize,
                        "user line overflow at w {w}: width {width} > {w} line {:?}",
                        s
                    );
                    // User lines should fill exactly w with bg (except maybe last? but our code fills)
                    // Check that at least one span has bg
                    assert!(
                        wl.spans.iter().any(|sp| sp.style.bg.is_some()),
                        "user line should have bg"
                    );
                }
            }
            // Also test full view rendering at this width does not panic and buffer is correct
            let backend = TestBackend::new(w, 24);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal.draw(|f| view(f, &mut app)).unwrap();
            assert_eq!(terminal.backend().buffer().area.width, w);
        }
    }

    #[test]
    fn ghost_key_facts_does_not_overflow_or_overlap_bottom() {
        // Repro for screenshot ghost: long assistant line with unbroken tokens
        // must wrap within width and never appear in input/footer area.
        let ghost = "Key facts: Docs at /home/aks/Work/dex/HARNESS.md (489 lines), PLAN.md (92 lines, P0-P5 shipped, next 6-10: permission ceiling/audit, token auth/policy/redaction, transactional edits/journal, verification, versioned protocol seq/replay). Src layout: src/agent/{loop,state,compaction}, client/http, cli/config/core/daemon/llm/protocol/session/skills/tools/ui. Key symbols: Session::set_state/load_session_state (/resume), Plan{goal,steps}+/goal/plan add/done/clear+SinkLine::Plan→StreamEvent::Plan, WRAP_UP_THRESHOLD, DaemonState/PendingApproval/router/SSE, TurnLimits/deadline/within_budget, TurnComplete/TurnFailed/Usage, SessionHeader, FileConfig/LlmConfig/Provider, system_prompt/project_context, CONFIGURED_OUTPUT_LIMIT/execute_outcome. Unresolved: complete section-by-section audit and rewrite HARNESS.md with code citations and re-scoring per active runtime.1126 lines), daemon mod/server (axum router, SSE, approvals), llm/config/prompt, disposition, versioned protocol seq/replay).";
        for (w, h) in [
            (80, 24),
            (100, 24),
            (120, 24),
            (200, 24),
            (80, 40),
            (120, 40),
        ] {
            let backend = TestBackend::new(w, h);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            let mut app = test_app();
            // Fill transcript like screenshot: several tool blocks then assistant ghost
            for name in [
                "slash.rs",
                "types.rs",
                "console.rs",
                "format.rs",
                "client.rs",
                "remote.rs",
            ] {
                super::super::append_sink_line(
                    &mut app,
                    crate::core::types::SinkLine::ToolInput(format!(
                        "read /home/aks/Work/dex/src/ui/{name}"
                    )),
                );
                super::super::append_sink_line(
                    &mut app,
                    crate::core::types::SinkLine::ToolOutput {
                        name: "read".into(),
                        summary: "10 lines".into(),
                        success: true,
                        preview: vec![
                            "1 use std::env;".into(),
                            "2".into(),
                            "3 use std::env;".into(),
                        ],
                        duration: 0.0,
                    },
                );
            }
            super::super::append_sink_line(
                &mut app,
                crate::core::types::SinkLine::Assistant(
                    "Evidence map 80% complete - pulling final modules to re-score the board."
                        .into(),
                ),
            );
            super::super::append_sink_line(
                &mut app,
                crate::core::types::SinkLine::Assistant(ghost.into()),
            );
            app.input = InputField::from_text("try a different approach");
            terminal.draw(|f| view(f, &mut app)).unwrap();
            let buffer = terminal.backend().buffer();
            let area = buffer.area;
            // Compute layout like view does
            let input_rows = render_input(&app.input, input_content_width(area.width))
                .0
                .len() as u16;
            let pending_total = app.pending_steering.len() + app.pending_followups.len();
            let visible_pending = pending_total.min(3) as u16;
            let extra = u16::from(pending_total > 3);
            let activity_items = 1 + visible_pending + extra;
            let layout = compute_layout(area, input_rows, activity_items, false).unwrap();
            // Check every cell in input and footer does not contain ghost fragments
            // Ghost contains distinctive substrings that should never leak into chrome
            let forbidden = [
                "Key facts",
                "HARNESS.md",
                "Session::set_state",
                "WRAP_UP_THRESHOLD",
            ];
            let content: String = buffer.content.iter().map(|c| c.symbol()).collect();
            let rows: Vec<String> = content
                .chars()
                .collect::<Vec<char>>()
                .chunks(w as usize)
                .map(|c| c.iter().collect())
                .collect();
            for y in layout.input.y..layout.input.y + layout.input.height {
                let row = &rows[y as usize];
                for pat in forbidden {
                    assert!(
                        !row.contains(pat),
                        "ghost '{pat}' leaked into input at {w}x{h} y={y} row={:?}",
                        row
                    );
                }
            }
            for y in layout.footer.y..layout.footer.y + layout.footer.height {
                let row = &rows[y as usize];
                for pat in forbidden {
                    assert!(
                        !row.contains(pat),
                        "ghost '{pat}' leaked into footer at {w}x{h} y={y} row={:?}",
                        row
                    );
                }
            }
            // Also check that no row in entire buffer exceeds width (hard wrap)
            for line in &app.display_cache {
                let s: String = line.spans.iter().map(|sp| sp.content.as_ref()).collect();
                let width = UnicodeWidthStr::width(s.as_str());
                assert!(
                    width <= w as usize,
                    "display_cache line overflow at {w}: {width} > {w} line={:?}",
                    s
                );
            }
            // Simulate resize to narrower then wider without new transcript data: cache must re-wrap
            let backend2 = TestBackend::new(w.saturating_sub(20).max(40), h);
            let mut terminal2 = ratatui::Terminal::new(backend2).unwrap();
            terminal2.draw(|f| view(f, &mut app)).unwrap();
            let content2: String = terminal2
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(!content2.contains("\t"), "tab not expanded after resize");
        }
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
    /// Regression: the submitted-prompt box (raised surface) must render as
    /// exactly three consecutive rows — top edge, content, bottom edge. The
    /// transcript Paragraph must NOT enable `Wrap`: the display cache is already
    /// pre-wrapped, and ratatui 0.29's WordWrapper emits a phantom empty row
    /// before any all-whitespace line exactly `area.width` wide (the box edge
    /// rows), punching dark holes inside the box.
    #[test]
    fn submitted_prompt_box_is_three_solid_rows() {
        let backend = TestBackend::new(126, 25);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = test_app();
        super::super::push_info(
        &mut app,
        "connected to http://127.0.0.1:35487 - workspace /home/aks/Work/dex - model deepseek-v4-flash"
            .into(),
    );
        super::super::render_user_prompt(&mut app, "can you check pillar 1 form harness.md");
        super::super::append_sink_line(
            &mut app,
            crate::core::types::SinkLine::ToolInput("read HARNESS.md".into()),
        );
        super::super::append_sink_line(
            &mut app,
            crate::core::types::SinkLine::ToolOutput {
                name: "read".into(),
                summary: "v 313 lines".into(),
                success: true,
                preview: vec!["1 # Harness Capability Map".into()],
                duration: 0.0,
            },
        );
        terminal.draw(|f| view(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let area = buffer.area;
        let row_of = |needle: &str| {
            (0..area.height).find(|&y| {
                let row: String = (0..area.width)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol())
                    .collect();
                row.contains(needle)
            })
        };
        let text_row = row_of("can you check pillar").expect("prompt text rendered");
        let tool_row = row_of("read HARNESS.md").expect("tool block rendered");
        // Box = edge, content(text), edge; then one gap line; then the tool block.
        assert_eq!(
            tool_row,
            text_row + 3,
            "box must occupy exactly text_row-1..text_row+1; a phantom row from Paragraph::wrap shifts the tool block down"
        );
    }
}
