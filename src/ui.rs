// Ratatui-based interactive REPL — the sole interactive UI (the hand-rolled
// TerminalEditor was removed). Immediate-mode full-buffer redraw; the agent
// turn runs on a worker thread calling the real crate::process_turn, with its
// streamed output routed through crate::set_console_sink into the loop.
//
// Markdown in assistant output is rendered with ratatui-markdown: we split a
// fragment into MarkdownBlocks (the crate's parser is private) and let the
// crate do styling/wrapping. Resize is free (Event::Resize).

use std::env;
use std::process::Command;
use std::sync::{mpsc, Arc, OnceLock};
use std::time::Instant;

use crossterm::event::DisableMouseCapture;
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, LeaveAlternateScreen};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{block::Padding, Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui_markdown::highlight::{HighlightHooks, TreeSitterHighlighter};
use ratatui_markdown::markdown::{MarkdownBlock, MarkdownRenderer};
use ratatui_markdown::ThemeConfig;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{ChatMessage, LlmConfig, Session, SinkLine, Skill, ToolState};

mod event;
mod input;
mod slash;
mod wrapping;
use input::InputField;
use wrapping::wrap_line;

pub(crate) use event::run_ratatui_repl;

/// Events from the agent worker thread into the UI loop.
enum UiEvent {
    /// Updated conversation + tool state after a turn completes.
    State {
        messages: Vec<ChatMessage>,
        tool_state: ToolState,
        session: Box<Session>,
    },
    Done {
        success: bool,
    },
}

struct App {
    transcript: Vec<Line<'static>>,
    input: InputField,
    config: LlmConfig,
    messages: Vec<ChatMessage>,
    tool_state: ToolState,
    session: Session,
    skills: Vec<Skill>,
    /// messages.len() at submit time, for session persistence.
    turn_start: usize,
    cwd: String,
    git_branch: Option<String>,
    git_dirty: bool,
    turn_started: Option<Instant>,
    active_tool: Option<String>,
    last_activity: Option<String>,
    steering_rx: Option<mpsc::Receiver<String>>,
    followup_rx: Option<mpsc::Receiver<String>>,
    pending_steering: Vec<String>,
    pending_followups: Vec<String>,
    cancel_requested: bool,
    approval_rx: Option<mpsc::Receiver<crate::ApprovalRequest>>,
    pending_approval: Option<PendingApproval>,
    busy: bool,
    autoscroll: bool,
    /// transcript scroll offset in rendered rows
    scroll: u16,
    /// animation frame counter (advanced once per draw) for the spinner
    tick: u64,
    quit: bool,
    history: Vec<String>,
    history_index: Option<usize>,
    history_draft: String,
    slash_selected: usize,
}

struct PendingApproval {
    name: String,
    input: String,
    response: mpsc::Sender<crate::ApprovalDecision>,
    selected: usize,
}

struct TerminalCleanup;
impl Drop for TerminalCleanup {
    fn drop(&mut self) {
        disable_raw_mode().ok();
        let _ = execute!(std::io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
    }
}

impl App {
    fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        if self.history_index.is_none() {
            self.history_draft = self.input.text();
        }
        let idx = self
            .history_index
            .map(|i| (i + 1).min(self.history.len() - 1))
            .unwrap_or(0);
        self.history_index = Some(idx);
        self.input = InputField::from_text(&self.history[self.history.len() - 1 - idx]);
    }

    fn history_down(&mut self) {
        match self.history_index {
            None => {}
            Some(0) => {
                self.history_index = None;
                self.input = InputField::from_text(&self.history_draft);
            }
            Some(idx) => {
                let new_idx = idx - 1;
                self.history_index = Some(new_idx);
                self.input = InputField::from_text(&self.history[self.history.len() - 1 - new_idx]);
            }
        }
    }

    fn history_push(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        if self.history.last() != Some(&text) {
            self.history.push(text);
        }
        self.history_index = None;
        self.history_draft.clear();
    }
}

/// Shared spacing rules for transcript, activity, and composer surfaces. Keeping
/// these in one place prevents streamed output, submitted prompts, wrapped rows,
/// and footer surfaces from acquiring subtly different vertical rhythms.
const VERTICAL_GUTTER: u16 = 1;
const HORIZONTAL_GUTTER: u16 = 1;
const TRANSCRIPT_INDENT: usize = HORIZONTAL_GUTTER as usize;
const INPUT_BORDER_ROWS: u16 = 0;
const INPUT_PAD_Y: u16 = 1;
const STATUS_CONTENT_ROWS: u16 = 1;
const INPUT_MIN_ROWS: u16 = 3;
const INPUT_STATUS_GUTTER: u16 = 0;
const APPROVAL_HEIGHT: u16 = 11;

fn surface_padding() -> Padding {
    Padding {
        left: HORIZONTAL_GUTTER,
        right: HORIZONTAL_GUTTER,
        top: VERTICAL_GUTTER,
        bottom: VERTICAL_GUTTER,
    }
}

const SUBMITTED_PROMPT_BG: Color = Color::Rgb(20, 38, 54);
const INPUT_BG: Color = SUBMITTED_PROMPT_BG;

fn input_block() -> Block<'static> {
    Block::default()
        .padding(Padding {
            left: HORIZONTAL_GUTTER + 1,
            right: HORIZONTAL_GUTTER,
            top: INPUT_PAD_Y,
            bottom: INPUT_PAD_Y,
        })
        .style(Style::default().bg(INPUT_BG))
}

fn input_outer_height(content_rows: u16) -> u16 {
    content_rows
        .saturating_add(INPUT_BORDER_ROWS)
        .saturating_add(INPUT_PAD_Y * 2)
}

fn activity_height(item_count: u16) -> u16 {
    // One gutter above/below the activity surface and one between items.
    item_count
        .saturating_mul(2)
        .saturating_sub(1)
        .saturating_add(VERTICAL_GUTTER * 2)
}

fn input_content_width(width: u16) -> u16 {
    // Must match input_block's horizontal padding: HORIZONTAL_GUTTER+1 on the
    // left, HORIZONTAL_GUTTER on the right.
    width.saturating_sub(HORIZONTAL_GUTTER * 2 + 1)
}

fn status_height() -> u16 {
    STATUS_CONTENT_ROWS + VERTICAL_GUTTER * 2
}

fn minimum_view_height(activity_h: u16, approval_h: u16) -> u16 {
    activity_h
        + approval_h
        + VERTICAL_GUTTER
        + status_height()
        + INPUT_MIN_ROWS
        + INPUT_STATUS_GUTTER
}

/// Rectangles owned by the main transcript and bottom pane. Keeping geometry
/// separate from rendering mirrors Codex's bottom-pane boundary and ensures
/// every child is measured against the same terminal area.
struct UiLayout {
    transcript: ratatui::layout::Rect,
    activity: ratatui::layout::Rect,
    approval: ratatui::layout::Rect,
    input: ratatui::layout::Rect,
    footer: ratatui::layout::Rect,
}

fn compute_layout(
    area: ratatui::layout::Rect,
    input_rows: u16,
    activity_items: u16,
    approval_pending: bool,
) -> Option<UiLayout> {
    let activity_h = activity_height(activity_items);
    let approval_h = if approval_pending { APPROVAL_HEIGHT } else { 0 };
    let footer_height = INPUT_STATUS_GUTTER + status_height();
    if area.height < minimum_view_height(activity_h, approval_h) {
        // Degrade gracefully on a short terminal. Keeping the transcript
        // visible is preferable to returning a blank frame; the bottom pane
        // will be restored automatically on the next resize.
        return Some(UiLayout {
            transcript: area,
            activity: Rect::new(area.x, area.y, area.width, 0),
            approval: Rect::new(area.x, area.y, area.width, 0),
            input: Rect::new(area.x, area.y, area.width, 0),
            footer: Rect::new(area.x, area.y, area.width, 0),
        });
    }

    let input_h = input_outer_height(input_rows).clamp(INPUT_MIN_ROWS, 8).min(
        area.height
            .saturating_sub(activity_h + approval_h + footer_height),
    );
    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(VERTICAL_GUTTER),
        Constraint::Length(activity_h),
        Constraint::Length(approval_h),
        Constraint::Length(input_h),
        Constraint::Length(INPUT_STATUS_GUTTER),
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

fn truncate_display(text: &str, width: u16) -> String {
    let width = width as usize;
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
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

/// Braille spinner frames, matching the headless console spinner.
const UI_SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn ui_status(app: &App) -> String {
    let cwd = compact_path(&app.cwd);
    let git = app
        .git_branch
        .as_ref()
        .map(|branch| format!(" · {}{}", branch, if app.git_dirty { "*" } else { "" }))
        .unwrap_or_default();
    let tokens = app
        .tool_state
        .last_usage
        .unwrap_or_else(|| crate::estimate_tokens(&app.messages));
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

fn footer_text(app: &App, width: u16) -> String {
    let hint = if !app.autoscroll {
        "▲ more above · "
    } else {
        ""
    };
    let cwd = compact_path(&app.cwd);
    let compact = format!("{} · {}", cwd, app.config.model);
    let model = app.config.model.clone();
    // Prefer preserving complete semantic fields. Only fall back to a hard
    // truncation when even the shortest useful status cannot fit.
    let candidates = [ui_status(app), compact, model.clone()];
    for status in candidates {
        let candidate = format!("{}{}", hint, status);
        if UnicodeWidthStr::width(candidate.as_str()) <= width as usize {
            return candidate;
        }
    }
    truncate_display(&format!("{}{}", hint, model), width)
}

fn compact_path(path: &str) -> String {
    if let Ok(home) = env::var("HOME") {
        if let Some(rest) = path.strip_prefix(&home) {
            return format!("~{}", rest);
        }
    }
    path.to_string()
}

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn git_context(cwd: &str) -> (Option<String>, bool) {
    let branch = Command::new("git")
        .args(["-C", cwd, "branch", "--show-current"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|branch| !branch.is_empty());
    let dirty = branch.is_some()
        && Command::new("git")
            .args(["-C", cwd, "status", "--porcelain"])
            .output()
            .ok()
            .is_some_and(|output| !output.stdout.is_empty());
    (branch, dirty)
}

/// Split a markdown fragment into blocks the crate can render.
fn split_markdown(s: &str) -> Vec<MarkdownBlock> {
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
            i += 1; // closing fence
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

/// Render a streamed assistant fragment as styled lines via ratatui-markdown.
fn markdown_lines(s: &str) -> Vec<Line<'static>> {
    static HIGHLIGHTER: OnceLock<Arc<TreeSitterHighlighter>> = OnceLock::new();
    let highlighter = HIGHLIGHTER
        .get_or_init(|| Arc::new(TreeSitterHighlighter::new()))
        .clone();
    let blocks = split_markdown(s);
    // Width zero disables the renderer's own wrapping. The final transcript
    // wrapper knows the actual terminal width, so using two independent
    // widths can otherwise spill a character onto a new row at column zero.
    let renderer = MarkdownRenderer::new(0)
        .with_render_hooks(Box::new(HighlightHooks::new(highlighter, usize::MAX)));
    renderer.render(&blocks, &ThemeConfig::default())
}

fn append_sink_line(app: &mut App, sl: SinkLine) {
    match sl {
        SinkLine::Assistant(s) => {
            // The model often streams an empty line at both sides of a
            // paragraph. Keep one intentional separator, but don't let those
            // stream boundaries turn the transcript into a wall of whitespace.
            if s.trim().is_empty() {
                if !app
                    .transcript
                    .last()
                    .is_some_and(|line| line.spans.is_empty())
                {
                    push_transcript_gap(app);
                }
            } else {
                // A response that follows tool traffic starts a new visual
                // block, even when the model did not emit a blank markdown
                // line between the two.
                if app
                    .transcript
                    .last()
                    .is_some_and(|line| is_tool_line(line) || is_user_line(line))
                {
                    push_transcript_gap(app);
                }
                for line in markdown_lines(s.trim_end()) {
                    app.transcript.push(indent_transcript_line(line));
                }
            }
        }
        SinkLine::ToolInput(s) => {
            dim_intermediate_assistant_block(app);
            // Keep a tool call attached to its result, but separate the pair
            // from surrounding prose and from the previous tool pair.
            if app
                .transcript
                .last()
                .is_some_and(|line| !line.spans.is_empty())
            {
                push_transcript_gap(app);
            }
            let mut it = s.splitn(2, ' ');
            let name = it.next().unwrap_or("").to_string();
            let arg = it.next().unwrap_or("").to_string();
            app.active_tool = Some(name.clone());
            app.transcript.push(indent_transcript_line(Line::from(vec![
                Span::styled("▸ ", Style::default().fg(Color::Yellow)),
                Span::styled(name, Style::default().fg(Color::Yellow)),
                Span::styled(format!(" {arg}"), Style::default().fg(Color::DarkGray)),
            ])));
        }
        SinkLine::ToolOutput { name, summary } => {
            let failed = summary.starts_with("failed ·");
            let color = if failed {
                Color::LightRed
            } else {
                Color::LightGreen
            };
            app.transcript.push(indent_transcript_line(Line::from(vec![
                Span::styled("└ ", Style::default().fg(color)),
                Span::styled(if failed { "✗ " } else { "✓ " }, Style::default().fg(color)),
                Span::styled(name, Style::default().fg(Color::DarkGray)),
                Span::styled(format!(" {summary}"), Style::default().fg(color)),
            ])));
        }
        SinkLine::System(s) => app.transcript.push(indent_transcript_line(Line::from(vec![
            Span::styled("· ", Style::default().fg(Color::DarkGray)),
            Span::styled(s, Style::default().fg(Color::DarkGray)),
        ]))),
        SinkLine::Error(s) => {
            if app
                .transcript
                .last()
                .is_some_and(|line| !line.spans.is_empty())
            {
                push_transcript_gap(app);
            }
            app.transcript.push(indent_transcript_line(Line::from(vec![
                Span::styled("! ", Style::default().fg(Color::Red)),
                Span::styled(format!("error: {s}"), Style::default().fg(Color::Red)),
            ])));
        }
    }
    app.autoscroll = true;
}

fn is_tool_line(line: &Line<'static>) -> bool {
    line.spans.iter().any(|span| {
        span.content.as_ref().starts_with("▸") || span.content.as_ref().starts_with("└")
    })
}

fn is_user_line(line: &Line<'static>) -> bool {
    line.spans.iter().any(|span| span.style.bg.is_some())
}

fn is_assistant_line(line: &Line<'static>) -> bool {
    !line.spans.is_empty()
        && !is_user_line(line)
        && !line.spans.iter().any(|span| {
            let text = span.content.as_ref();
            text.starts_with("▸") || text.starts_with("└") || text.starts_with("· ")
        })
}

/// Model prose immediately before a tool call is intermediate work rather
/// than the final answer. Lower its contrast once the tool boundary confirms
/// that classification. The final response after the tool result is untouched.
fn dim_intermediate_assistant_block(app: &mut App) {
    for line in app.transcript.iter_mut().rev() {
        if line.spans.is_empty() || !is_assistant_line(line) {
            break;
        }
        for span in &mut line.spans {
            span.style = span.style.fg(Color::DarkGray);
        }
    }
}

fn push_transcript_gap(app: &mut App) {
    if !app
        .transcript
        .last()
        .is_some_and(|line| line.spans.is_empty())
    {
        app.transcript.push(Line::from(String::new()));
    }
}

fn push_info(app: &mut App, text: String) {
    app.transcript
        .push(indent_transcript_line(Line::from(Span::styled(
            text,
            Style::default().fg(Color::Cyan),
        ))));
}

fn resolve_approval(app: &mut App, decision: crate::ApprovalDecision) {
    if let Some(approval) = app.pending_approval.take() {
        let _ = approval.response.send(decision);
    }
}

fn transcript_indent() -> String {
    " ".repeat(TRANSCRIPT_INDENT)
}

/// Put every transcript content line on the shared conversation gutter.
fn indent_transcript_line(mut line: Line<'static>) -> Line<'static> {
    line.spans.insert(0, Span::raw(transcript_indent()));
    line
}

struct TranscriptView;

impl TranscriptView {
    fn render(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
        let visible = area.height as usize;
        // Pre-wrap before rendering so one transcript entry corresponds to one
        // scrollable terminal row. This is the same invariant used by Codex's
        // history wrapping helpers.
        let mut display: Vec<Line<'static>> = Vec::new();
        for line in &app.transcript {
            display.extend(wrap_line_display(line, area.width));
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
        if !(app.busy || app.last_activity.is_some()) {
            return;
        }
        let content_width = area.width.saturating_sub(HORIZONTAL_GUTTER * 2);
        let (activity_text, activity_color) = if app.busy {
            let frame = UI_SPINNER[(app.tick / 4) as usize % UI_SPINNER.len()];
            let tool = app
                .active_tool
                .as_ref()
                .map(|name| format!(" · {name}"))
                .unwrap_or_default();
            (
                truncate_display(&format!("{} working…{}", frame, tool), content_width),
                Color::DarkGray,
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
        let input_style = if app.busy || app.pending_approval.is_some() {
            Style::default().fg(Color::DarkGray).bg(INPUT_BG)
        } else {
            Style::default().fg(Color::White).bg(INPUT_BG)
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
        let width = area.width.saturating_sub(HORIZONTAL_GUTTER * 2);
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
                    Style::default().fg(Color::White).bg(Color::Rgb(18, 25, 38))
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
                    Style::default().fg(Color::Gray)
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
                    .style(Style::default().bg(Color::Rgb(18, 25, 38))),
            ),
            popup,
        );
    }
}

/// The Codex-style bottom pane: transient activity, persistent composer, and
/// contextual footer are rendered as independent children of one region.
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
            .border_style(Style::default().fg(Color::Yellow))
            .style(Style::default().bg(Color::Black));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let command = truncate_display(
            &format!("{} {}", approval.name, approval.input),
            inner.width.saturating_sub(2),
        );
        let header = Paragraph::new(vec![
            Line::from(Span::styled(
                "The agent wants to run:",
                Style::default().fg(Color::White),
            )),
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
                Style::default().fg(Color::Gray)
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
                .style(Style::default().fg(Color::DarkGray)),
            Rect {
                x: inner.x,
                y: inner.y + 8,
                width: inner.width,
                height: 1,
            },
        );
    }
}

fn view(f: &mut ratatui::Frame, app: &mut App) {
    let area = f.area();
    // Measure the composer using the exact inner width it will receive, then
    // allocate the bottom pane before rendering any child.
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

/// Move the transcript scroll offset by `delta` rows (negative = up) and drop
/// autoscroll so the view stays where the user put it.
fn scroll_transcript(app: &mut App, delta: i32) {
    app.autoscroll = false;
    app.scroll = (app.scroll as i32)
        .saturating_add(delta)
        .clamp(0, u16::MAX as i32) as u16;
}

/// Wrap a styled `Line` into rows that each fit `width`, greedy char-based
/// (matching the input field's wrap) while preserving spans. One returned
/// `Line` == one rendered row, so a scroll offset is exact.
fn wrap_line_display(line: &Line<'static>, width: u16) -> Vec<Line<'static>> {
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
        // The prefix is rendered separately, including on continuation rows.
        graphemes.next();
    }
    let units: Vec<Unit> = graphemes
        .map(|sg| Unit {
            text: sg.symbol.to_string(),
            style: sg.style,
            width: sg
                .symbol
                .chars()
                .map(|c| c.width().unwrap_or(0))
                .sum::<usize>()
                .max(1),
            whitespace: sg.symbol.chars().all(char::is_whitespace),
        })
        .collect();

    let mut rows: Vec<Vec<Unit>> = Vec::new();
    let mut row = Vec::new();
    let mut row_width = indent_width;
    let mut last_space = None;
    for unit in units {
        if row_width + unit.width > w && !row.is_empty() {
            if let Some(space) = last_space {
                // Drop the break-space rather than leaving a leading space on
                // the next row. The following word starts at column zero
                // (after the normal transcript indent).
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
        }
        if unit.whitespace {
            last_space = Some(row.len());
        }
        row_width += unit.width;
        row.push(unit);
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

    // User-submitted lines carry a background style. Extend that style across
    // the transcript width so the padding reads as a deliberate highlighted
    // surface rather than a small patch behind the text.
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

/// Render the input field into wrapped Lines, returning the lines plus the
/// cursor position as (scroll_row, x, y_within_box) for an inline block cursor.
fn render_input(input: &InputField, width: u16) -> (Vec<Line<'static>>, (u16, u16, u16)) {
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
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use crate::{ApiProtocol, PermissionMode, Provider};
    use ratatui::backend::TestBackend;

    fn test_app() -> App {
        let cwd = "/tmp/ak-ui-test".to_string();
        App {
            transcript: vec![indent_transcript_line(Line::from(
                "hello from the transcript — this line is intentionally long enough to wrap",
            ))],
            input: InputField::new(),
            config: LlmConfig {
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
            tool_state: ToolState::default(),
            session: Session::in_memory(cwd.clone()),
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
        // transcript/activity gap + activity + input minimum + input/status gap
        // + status surface
        assert_eq!(minimum_view_height(activity_height(1), 0), 10);
        assert_eq!(minimum_view_height(activity_height(3), 0), 14);
    }

    #[test]
    fn transcript_wrapper_keeps_first_content_grapheme() {
        let line = indent_transcript_line(Line::from("▸ tool"));
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
        let area = ratatui::layout::Rect::new(0, 0, 80, 24);
        let layout = compute_layout(area, 1, 1, false).expect("terminal should fit layout");
        assert_eq!(layout.transcript.y, 0);
        assert!(layout.transcript.height > 0);
        assert_eq!(
            layout.input.y + layout.input.height + INPUT_STATUS_GUTTER,
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
    fn slash_suggestions_filter_by_prefix_and_include_skills() {
        let mut app = test_app();
        app.skills.push(Skill {
            name: "rust".to_string(),
            description: "Rust help".to_string(),
            path: "/tmp/rust/SKILL.md".into(),
        });
        app.input = InputField::from_text("/s");
        let commands = crate::ui::slash::slash_suggestions(&app);
        let names: Vec<_> = commands.iter().map(|(name, _)| name.as_str()).collect();
        assert!(names.contains(&"/session"));
        assert!(names.contains(&"/skill:rust"));
        assert!(!names.contains(&"/model"));

        app.input = InputField::from_text("/model ");
        let models = crate::ui::slash::slash_suggestions(&app);
        assert!(models.iter().any(|(name, _)| name == "/model test-model"));

        app.input = InputField::from_text("/provider ");
        let providers = crate::ui::slash::slash_suggestions(&app);
        assert!(providers
            .iter()
            .any(|(name, _)| name == "/provider opencode"));
        assert!(providers
            .iter()
            .any(|(name, _)| name == "/provider openai-codex"));
    }

    #[test]
    fn slash_completion_replaces_input_with_selected_command() {
        let mut app = test_app();
        app.input = InputField::from_text("/mo");
        assert!(crate::ui::slash::complete_slash(&mut app));
        assert_eq!(app.input.text(), "/model ");
        assert_eq!(app.input.row, 0);
        assert_eq!(app.input.col, "/model ".len());
    }

    #[test]
    fn provider_names_are_parsed_for_runtime_switching() {
        assert_eq!(Provider::parse("codex").unwrap().name(), "openai-codex");
        assert_eq!(Provider::parse("OpenCode").unwrap().name(), "opencode");
        assert!(Provider::parse("unknown").is_err());
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
        let (response_tx, _response_rx) = mpsc::channel();
        let mut app = test_app();
        app.pending_approval = Some(PendingApproval {
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
    }

    #[test]
    fn intermediate_assistant_block_is_dimmed_before_tool_call() {
        let mut app = test_app();
        app.transcript.clear();
        app.transcript
            .push(indent_transcript_line(Line::from(Span::styled(
                "I will inspect the project first.",
                Style::default().fg(Color::White),
            ))));
        append_sink_line(&mut app, SinkLine::ToolInput("read README.md".to_string()));
        let assistant = &app.transcript[0];
        assert_eq!(assistant.spans[1].style.fg, Some(Color::DarkGray));
    }

    #[test]
    fn input_box_height_matches_wrapped_rows() {
        // The row count used to size the composer must be measured at the same
        // width the composer actually renders at, otherwise the box is too
        // short and the cursor pins to the first row once text wraps.
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
}
