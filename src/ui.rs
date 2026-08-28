// Ratatui-based interactive REPL — the sole interactive UI (the hand-rolled
// TerminalEditor was removed). Immediate-mode full-buffer redraw; the agent
// turn runs on a worker thread calling the real crate::process_turn, with its
// streamed output routed through crate::set_console_sink into the loop.
//
// Markdown in assistant output is rendered with ratatui-markdown: we split a
// fragment into MarkdownBlocks (the crate's parser is private) and let the
// crate do styling/wrapping. Resize is free (Event::Resize).

use std::env;
use std::io::IsTerminal;
use std::process::Command;
use std::sync::{mpsc, Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{block::Padding, Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Terminal;
use ratatui_markdown::highlight::{HighlightHooks, TreeSitterHighlighter};
use ratatui_markdown::markdown::{MarkdownBlock, MarkdownRenderer};
use ratatui_markdown::ThemeConfig;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{Args, ChatMessage, LlmConfig, Provider, Session, SinkLine, Skill, ToolState};

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

/// A minimal single-line/multi-line input editor with an inline block cursor.
/// Avoids tui_textarea's default (which underlines the cursor line and does not
/// wrap long input, causing overflow). Text wraps in a Paragraph so long lines
/// never overflow the box.
struct InputField {
    lines: Vec<String>,
    row: usize,
    col: usize,
}
impl InputField {
    fn new() -> Self {
        InputField {
            lines: vec![String::new()],
            row: 0,
            col: 0,
        }
    }
    fn from_text(text: &str) -> Self {
        let lines: Vec<String> = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n').map(|s| s.to_string()).collect()
        };
        let row = lines.len().saturating_sub(1);
        let col = lines[row].len();
        InputField { lines, row, col }
    }
    fn text(&self) -> String {
        self.lines.join("\n")
    }
    fn reset(&mut self) {
        self.lines = vec![String::new()];
        self.row = 0;
        self.col = 0;
    }
    fn insert_char(&mut self, c: char) {
        if c == '\n' {
            let line = std::mem::take(&mut self.lines[self.row]);
            let (left, right) = line.split_at(self.col);
            self.lines.insert(self.row + 1, right.to_string());
            self.lines[self.row] = left.to_string();
            self.row += 1;
            self.col = 0;
            return;
        }
        if self.col > self.lines[self.row].len() {
            self.col = self.lines[self.row].len();
        }
        self.lines[self.row].insert(self.col, c);
        self.col += c.len_utf8();
    }
    fn handle_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;
        match key.code {
            KeyCode::Char(c) => self.insert_char(c),
            KeyCode::Enter => self.insert_char('\n'),
            KeyCode::Backspace => {
                if self.col == 0 {
                    if self.row > 0 {
                        let removed = self.lines.remove(self.row);
                        self.row -= 1;
                        self.col = self.lines[self.row].len();
                        self.lines[self.row].push_str(&removed);
                    }
                } else {
                    let line = &mut self.lines[self.row];
                    let mut idx = self.col;
                    while idx > 0 && !line.is_char_boundary(idx - 1) {
                        idx -= 1;
                    }
                    line.remove(idx - 1);
                    self.col = idx - 1;
                }
            }
            KeyCode::Delete => {
                let line = &mut self.lines[self.row];
                if self.col < line.len() {
                    let mut idx = self.col;
                    while idx < line.len() && !line.is_char_boundary(idx + 1) {
                        idx += 1;
                    }
                    line.remove(idx);
                } else if self.row + 1 < self.lines.len() {
                    let removed = self.lines.remove(self.row + 1);
                    self.lines[self.row].push_str(&removed);
                }
            }
            KeyCode::Left => {
                if self.col > 0 {
                    let line = &self.lines[self.row];
                    let mut idx = self.col;
                    while idx > 0 && !line.is_char_boundary(idx - 1) {
                        idx -= 1;
                    }
                    self.col = idx - 1;
                } else if self.row > 0 {
                    self.row -= 1;
                    self.col = self.lines[self.row].len();
                }
            }
            KeyCode::Right => {
                let line = &self.lines[self.row];
                if self.col < line.len() {
                    let mut idx = self.col;
                    while idx < line.len() && !line.is_char_boundary(idx + 1) {
                        idx += 1;
                    }
                    self.col = idx + 1;
                } else if self.row + 1 < self.lines.len() {
                    self.row += 1;
                    self.col = 0;
                }
            }
            KeyCode::Up if self.row > 0 => {
                self.row -= 1;
                self.clamp_col();
            }
            KeyCode::Down if self.row + 1 < self.lines.len() => {
                self.row += 1;
                self.clamp_col();
            }
            KeyCode::Home => self.col = 0,
            KeyCode::End => self.col = self.lines[self.row].len(),
            KeyCode::Tab => self.insert_char('\t'),
            _ => {}
        }
    }
    fn clamp_col(&mut self) {
        let max = self.lines[self.row].len();
        if self.col > max {
            self.col = max;
        }
    }
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
const INPUT_BORDER_ROWS: u16 = 2;
const STATUS_CONTENT_ROWS: u16 = 1;
const INPUT_MIN_ROWS: u16 = 3;

const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/quit", "Exit the REPL"),
    ("/clear", "Clear conversation history"),
    ("/new", "Start a new session"),
    ("/session", "Show current session details"),
    ("/resume", "List or resume a session"),
    ("/permissions", "Show permission mode and workspace"),
    ("/name", "Rename the current session"),
    ("/model", "Show or switch the model"),
    ("/provider", "Show or switch the provider"),
    ("/help", "Show available commands"),
];

fn slash_suggestions(app: &App) -> Vec<(String, String)> {
    let input = app.input.text();
    if app.busy || !input.starts_with('/') || input.contains('\n') {
        return Vec::new();
    }
    if let Some(query) = input.strip_prefix("/model ") {
        let query = query.to_ascii_lowercase();
        return app
            .config
            .available_models
            .iter()
            .filter(|model| model.to_ascii_lowercase().starts_with(&query))
            .map(|model| {
                (
                    format!("/model {model}"),
                    if model == &app.config.model {
                        "Current model".to_string()
                    } else {
                        "Configured model".to_string()
                    },
                )
            })
            .collect();
    }
    if let Some(query) = input.strip_prefix("/provider ") {
        let query = query.to_ascii_lowercase();
        return ["opencode", "openai-codex"]
            .into_iter()
            .filter(|provider| provider.starts_with(&query))
            .map(|provider| {
                (
                    format!("/provider {provider}"),
                    if *provider == *app.config.provider.name() {
                        "Current provider".to_string()
                    } else {
                        "Available provider".to_string()
                    },
                )
            })
            .collect();
    }
    if input.contains(' ') {
        return Vec::new();
    }
    let query = input.to_ascii_lowercase();
    let mut suggestions: Vec<(String, String)> = SLASH_COMMANDS
        .iter()
        .filter(|(command, _)| command.starts_with(&query))
        .map(|(command, description)| ((*command).to_string(), (*description).to_string()))
        .collect();
    suggestions.extend(
        app.skills
            .iter()
            .map(|skill| {
                (
                    format!("/skill:{}", skill.name),
                    "Load this skill".to_string(),
                )
            })
            .filter(|(command, _)| command.to_ascii_lowercase().starts_with(&query)),
    );
    suggestions
}

fn complete_slash(app: &mut App) -> bool {
    let suggestions = slash_suggestions(app);
    let Some((command, _)) =
        suggestions.get(app.slash_selected.min(suggestions.len().saturating_sub(1)))
    else {
        return false;
    };
    app.input = InputField::from_text(&format!("{command} "));
    app.slash_selected = 0;
    true
}

fn surface_padding() -> Padding {
    Padding {
        left: HORIZONTAL_GUTTER,
        right: HORIZONTAL_GUTTER,
        top: VERTICAL_GUTTER,
        bottom: VERTICAL_GUTTER,
    }
}

fn input_block(border_color: Color) -> Block<'static> {
    Block::new()
        .borders(Borders::TOP | Borders::BOTTOM)
        // Keep the editor compact vertically; the borders provide its visual
        // separation while the horizontal gutter keeps text off the edges.
        .padding(Padding {
            left: HORIZONTAL_GUTTER,
            right: HORIZONTAL_GUTTER,
            top: 0,
            bottom: 0,
        })
        .border_style(Style::default().fg(border_color))
}

fn input_outer_height(content_rows: u16) -> u16 {
    content_rows.saturating_add(INPUT_BORDER_ROWS)
}

fn activity_height(item_count: u16) -> u16 {
    // One gutter above/below the activity surface and one between items.
    item_count
        .saturating_mul(2)
        .saturating_sub(1)
        .saturating_add(VERTICAL_GUTTER * 2)
}

fn input_content_width(width: u16) -> u16 {
    width.saturating_sub(HORIZONTAL_GUTTER * 2)
}

fn status_height() -> u16 {
    STATUS_CONTENT_ROWS + VERTICAL_GUTTER * 2
}

fn minimum_view_height(activity_h: u16) -> u16 {
    activity_h + VERTICAL_GUTTER * 2 + status_height() + INPUT_MIN_ROWS
}

/// Rectangles owned by the main transcript and bottom pane. Keeping geometry
/// separate from rendering mirrors Codex's bottom-pane boundary and ensures
/// every child is measured against the same terminal area.
struct UiLayout {
    transcript: ratatui::layout::Rect,
    activity: ratatui::layout::Rect,
    input: ratatui::layout::Rect,
    footer: ratatui::layout::Rect,
}

fn compute_layout(
    area: ratatui::layout::Rect,
    input_rows: u16,
    activity_items: u16,
) -> Option<UiLayout> {
    let activity_h = activity_height(activity_items);
    let footer_height = VERTICAL_GUTTER * 2 + status_height();
    if area.height < minimum_view_height(activity_h) {
        // Degrade gracefully on a short terminal. Keeping the transcript
        // visible is preferable to returning a blank frame; the bottom pane
        // will be restored automatically on the next resize.
        return Some(UiLayout {
            transcript: area,
            activity: Rect::new(area.x, area.y, area.width, 0),
            input: Rect::new(area.x, area.y, area.width, 0),
            footer: Rect::new(area.x, area.y, area.width, 0),
        });
    }

    let input_h = input_outer_height(input_rows)
        .clamp(INPUT_MIN_ROWS, 8)
        .min(area.height.saturating_sub(activity_h + footer_height));
    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(VERTICAL_GUTTER),
        Constraint::Length(activity_h),
        Constraint::Length(input_h),
        Constraint::Length(VERTICAL_GUTTER),
        Constraint::Length(status_height()),
    ])
    .split(area);

    Some(UiLayout {
        transcript: chunks[0],
        activity: chunks[2],
        input: chunks[3],
        footer: chunks[5],
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
        let border_color = if app.busy {
            Color::DarkGray
        } else {
            Color::LightGreen
        };
        let block = input_block(border_color);
        let inner = block.inner(area);
        let (lines, cursor) = render_input(&app.input, inner.width);
        let content_rows = inner.height;
        let scroll = (cursor.0 + 1).saturating_sub(content_rows);
        let paragraph = Paragraph::new(lines)
            .style(Style::default().fg(Color::White))
            .scroll((scroll, 0))
            .block(block);
        f.render_widget(paragraph, area);

        if !app.busy {
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
        let suggestions = slash_suggestions(app);
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
    fn render(f: &mut ratatui::Frame, app: &App) {
        let Some(approval) = app.pending_approval.as_ref() else {
            return;
        };
        let area = centered_rect(76, 12, f.area());
        f.render_widget(Clear, area);
        let block = Block::default()
            .title(" Approval required ")
            .borders(Borders::ALL)
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

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
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
    let layout = compute_layout(area, input_rows, activity_items).expect("layout always exists");

    TranscriptView::render(f, layout.transcript, app);
    BottomPane::render(f, &layout, app);
    SlashSuggestionsView::render(f, layout.input, app);
    ApprovalOverlay::render(f, app);
}

/// Greedily wrap a logical line into segments of display width <= `w`.
/// `col` is a *byte* offset into `line`; the returned tuple is
/// `(segments, cursor_segment_index, cursor_x_in_cells)`.
fn wrap_line(line: &str, w: usize, col: usize) -> (Vec<String>, u16, u16) {
    let w = w.max(1);
    let col = col.min(line.len());
    // Collect char byte boundaries so we can measure per-character widths.
    let mut bounds: Vec<usize> = line.char_indices().map(|(i, _)| i).collect();
    bounds.push(line.len());

    // First pass: split into byte-range segments greedily by display width.
    let mut segs: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    let mut cur_w = 0usize;
    for k in 0..bounds.len() - 1 {
        let ci = bounds[k];
        let cn = bounds[k + 1];
        let cw = line[ci..cn]
            .chars()
            .next()
            .unwrap()
            .width()
            .unwrap_or(0)
            .max(1);
        if cur_w + cw > w && ci > start {
            segs.push((start, ci));
            start = ci;
            cur_w = 0;
        }
        cur_w += cw;
    }
    segs.push((start, line.len()));

    let strings: Vec<String> = segs.iter().map(|&(s, e)| line[s..e].to_string()).collect();

    // Second pass: locate the cursor within a segment.
    let mut cur_seg: u16 = 0;
    let mut cur_x: u16 = 0;
    for (i, &(s, e)) in segs.iter().enumerate() {
        if col >= s && col <= e {
            cur_seg = i as u16;
            cur_x = line[s..col]
                .chars()
                .map(|c| c.width().unwrap_or(0).max(1))
                .sum::<usize>() as u16;
            break;
        }
    }
    (strings, cur_seg, cur_x)
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
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;
    // Assistant output carries the shared transcript prefix. Treat it as a
    // hanging indent so wrapped continuation rows receive the same prefix.
    // Submitted input has a background style on its prefix and is excluded.
    let output_indent = line.spans.first().is_some_and(|span| {
        span.content.as_ref() == transcript_indent() && span.style.bg.is_none()
    });
    let indent_width = if output_indent {
        TRANSCRIPT_INDENT.min(w)
    } else {
        0
    };
    let mut graphemes = line.styled_graphemes(Style::default());
    if output_indent {
        // The prefix is one grapheme. Do not consume the first character of
        // the actual transcript content (e.g. `▸`, `└`, or a code fence).
        graphemes.next();
        cur.push(Span::raw(" ".repeat(indent_width)));
        cur_w = indent_width;
    }
    for sg in graphemes {
        let cw = sg
            .symbol
            .chars()
            .map(|c| c.width().unwrap_or(0))
            .sum::<usize>()
            .max(1);
        if cur_w + cw > w && !cur.is_empty() {
            out.push(Line::from(std::mem::take(&mut cur)));
            cur.push(Span::raw(" ".repeat(indent_width)));
            cur_w = indent_width;
        }
        cur.push(Span::styled(sg.symbol.to_string(), sg.style));
        cur_w += cw;
    }
    if !cur.is_empty() {
        out.push(Line::from(cur));
    }
    if out.is_empty() {
        out.push(Line::from(String::new()));
    }
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

/// Handle a slash command. Returns true if the REPL should quit.
fn handle_slash(app: &mut App, line: &str) -> bool {
    // Gap before command output so it reads as its own turn (pi-style).
    if app.transcript.len() > 1 {
        push_transcript_gap(app);
    }
    match line {
        "/quit" => return true,
        "/clear" => {
            app.messages.truncate(1);
            let _ = app.session.clear_messages();
            push_info(app, "history cleared.".to_string());
        }
        "/new" => {
            app.messages.truncate(1);
            let cwd = env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            match Session::new(cwd, None) {
                Ok(mut s) => {
                    s.append_message(app.messages[0].clone()).ok();
                    app.session = s;
                    push_info(app, "new session started.".to_string());
                }
                Err(e) => push_info(app, format!("could not start new session: {}", e)),
            }
        }
        "/session" => {
            push_info(app, format!("session: {}", app.session.display_name()));
            if let Some(path) = app.session.path() {
                push_info(app, format!("path: {}", path.display()));
            }
            push_info(app, format!("turns: {}", app.session.count()));
        }
        "/permissions" => {
            push_info(app, format!("permission mode: {:?}", app.config.permission));
            push_info(app, format!("workspace: {}", app.cwd));
        }
        "/resume" => match Session::list(&app.cwd) {
            Ok(sessions) if !sessions.is_empty() => {
                push_info(app, "sessions:".to_string());
                for (i, (path, header)) in sessions.iter().enumerate() {
                    let name = header.name().unwrap_or("(unnamed)");
                    push_info(app, format!("  {}: {} ({})", i, name, path.display()));
                }
            }
            _ => push_info(app, "no sessions found.".to_string()),
        },
        _ if line.starts_with("/resume ") => {
            let selector = line["/resume ".len()..].trim();
            match Session::resume(&app.cwd, selector) {
                Ok(session) => {
                    let loaded = session
                        .path()
                        .and_then(|p| crate::load_messages_from_session(p).ok())
                        .unwrap_or_default();
                    let system = app.messages.first().cloned();
                    app.messages = loaded;
                    if let Some(system) = system {
                        app.messages.insert(0, system);
                    }
                    app.session = session;
                    push_info(
                        app,
                        format!("resumed session: {}", app.session.display_name()),
                    );
                }
                Err(e) => push_info(app, format!("could not resume session: {}", e)),
            }
        }
        _ if line.starts_with("/name ") => {
            let name = line["/name ".len()..].trim().to_string();
            if !name.is_empty() {
                app.session.set_name(name.clone()).ok();
                push_info(app, format!("session name: {}", name));
            }
        }
        _ if line.starts_with("/skill:") => {
            let name = line["/skill:".len()..].trim();
            if let Some(skill) = app.skills.iter().find(|s| s.name == name) {
                let content = std::fs::read_to_string(&skill.path).unwrap_or_default();
                app.messages.push(ChatMessage {
                    role: "user".to_string(),
                    content: Some(format!("--- Skill: {} ---\n{}", skill.name, content)),
                    tool_calls: None,
                    tool_call_id: None,
                    name: Some("skill".to_string()),
                });
                let _ = app
                    .session
                    .append_message(app.messages.last().cloned().unwrap());
                push_info(app, format!("loaded skill: {}", skill.name));
            } else {
                push_info(app, format!("skill not found: {}", name));
                push_info(app, "available skills:".to_string());
                let names: Vec<String> = app.skills.iter().map(|s| s.name.clone()).collect();
                for n in names {
                    push_info(app, format!("  - {}", n));
                }
            }
        }
        "/model" => {
            push_info(app, format!("current model: {}", app.config.model));
        }
        "/provider" => {
            push_info(
                app,
                format!("current provider: {}", app.config.provider.name()),
            );
            push_info(
                app,
                "available providers: opencode, openai-codex".to_string(),
            );
        }
        "/help" => {
            push_info(
                app,
                "commands: /quit /clear /new /session /resume [index|path] /permissions /name <n> /skill:<name> /model [<m>] /provider [<name>]"
                    .to_string(),
            );
            push_info(
                app,
                "keys: Enter send · Shift+Enter newline · ↑↓ history · PgUp/PgDn/mouse scroll"
                    .to_string(),
            );
            push_info(app, "while working: Enter queues steer · Alt+Enter queues follow-up · Esc/Ctrl+C cancels and restores queued input".to_string());
        }
        _ if line.starts_with("/model ") => {
            let m = line["/model ".len()..].trim().to_string();
            if !m.is_empty() {
                app.config.model = m.clone();
                if !app
                    .config
                    .available_models
                    .iter()
                    .any(|candidate| candidate == &m)
                {
                    app.config.available_models.push(m.clone());
                }
                let _ = app.session.set_state("model", &m);
                push_info(app, format!("switched to model: {}", app.config.model));
            }
        }
        _ if line.starts_with("/provider ") => {
            let name = line["/provider ".len()..].trim();
            match Provider::parse(name) {
                Ok(provider) if provider == app.config.provider => {
                    push_info(
                        app,
                        format!("provider already selected: {}", provider.name()),
                    );
                }
                Ok(provider) => match app.config.switch_provider(provider) {
                    Ok(()) => {
                        let _ = app.session.set_state("provider", provider.name());
                        push_info(app, format!("switched to provider: {}", provider.name()));
                    }
                    Err(error) => push_info(app, format!("could not switch provider: {}", error)),
                },
                Err(error) => push_info(app, error),
            }
        }
        _ => push_info(app, format!("unknown command: {}", line)),
    }
    false
}

fn submit(
    app: &mut App,
    tx: &mpsc::Sender<UiEvent>,
    steering_tx: &mpsc::Sender<String>,
    steering_accepted_tx: &mpsc::Sender<String>,
    followup_tx: &mpsc::Sender<String>,
    followup_accepted_tx: &mpsc::Sender<String>,
    is_followup: bool,
) {
    let line: String = app.input.text().trim().to_string();
    app.history_push(line.clone());
    app.input.reset();
    if line.is_empty() {
        return;
    }

    // A running turn keeps ownership of the conversation state. Queue the
    // new prompt for the worker and keep the editor available for more input.
    if app.busy {
        if is_followup {
            app.pending_followups.push(line.clone());
            let _ = followup_tx.send(line);
        } else {
            app.pending_steering.push(line.clone());
            let _ = steering_tx.send(line);
        }
        return;
    }
    if line.starts_with('/') {
        if handle_slash(app, &line) {
            app.quit = true;
        }
        return;
    }

    render_user_prompt(app, &line);

    // The answer streams in via the sink.
    let mut messages = std::mem::take(&mut app.messages);
    let mut tool_state = std::mem::take(&mut app.tool_state);
    let user_message = ChatMessage {
        role: "user".to_string(),
        content: Some(line.clone()),
        tool_calls: None,
        tool_call_id: None,
        name: None,
    };
    // Make the user's message and turn boundary durable before any network or
    // tool work begins. The completion handler appends subsequent messages.
    let _ = app.session.turn_event("turn_start");
    let _ = app.session.append_message(user_message.clone());
    messages.push(user_message);
    app.turn_start = messages.len();
    let config = app.config.clone();
    let mut persist_session = app.session.clone();
    let steering_rx = app.steering_rx.take();
    let followup_rx = app.followup_rx.take();

    let tx = tx.clone();
    let steering_accepted_tx = steering_accepted_tx.clone();
    let followup_accepted_tx = followup_accepted_tx.clone();
    app.busy = true;
    app.turn_started = Some(Instant::now());
    app.active_tool = None;
    app.last_activity = None;
    thread::spawn(move || {
        let result = loop {
            let result = crate::process_turn(
                &config,
                &mut messages,
                &mut tool_state,
                steering_rx.as_ref(),
                Some(&steering_accepted_tx),
                Some(&mut persist_session),
            );
            if result.is_err() {
                break result;
            }
            let followups: Vec<String> = followup_rx
                .as_ref()
                .map(|rx| rx.try_iter().collect())
                .unwrap_or_default();
            if followups.is_empty() {
                break result;
            }
            for content in followups {
                let _ = followup_accepted_tx.send(content.clone());
                messages.push(ChatMessage {
                    role: "user".to_string(),
                    content: Some(content),
                    tool_calls: None,
                    tool_call_id: None,
                    name: Some("follow-up".to_string()),
                });
            }
        };
        if let Err(e) = &result {
            if let Some(sink) = crate::console_sink() {
                let _ = sink.send(SinkLine::Error(format!("{}", e)));
            }
        }
        let _ = persist_session.turn_event(if result.is_ok() {
            "turn_complete"
        } else {
            "turn_failed"
        });
        let _ = tx.send(UiEvent::State {
            messages,
            tool_state,
            session: Box::new(persist_session),
        });
        let _ = tx.send(UiEvent::Done {
            success: result.is_ok(),
        });
    });
}

fn render_user_prompt(app: &mut App, line: &str) {
    // Keep submitted prompts on the same transcript grid as assistant output:
    // shared horizontal indentation and a single vertical gutter above/below.
    let user_bg = Style::default().fg(Color::White).bg(Color::Rgb(20, 38, 54));
    let horizontal_pad = " ".repeat(TRANSCRIPT_INDENT);
    let edge_pad = Span::styled(" ", user_bg);
    app.transcript.push(Line::from(String::new()));
    app.transcript.push(Line::from(edge_pad.clone()));
    for sub in line.split('\n') {
        app.transcript.push(Line::from(vec![
            Span::styled(horizontal_pad.clone(), user_bg),
            Span::styled(sub.to_string(), user_bg),
            edge_pad.clone(),
        ]));
    }
    app.transcript.push(Line::from(edge_pad));
    app.transcript.push(Line::from(String::new()));
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
        assert_eq!(input_content_width(80), 78);
        assert_eq!(input_content_width(1), 0);
        assert_eq!(activity_height(1), 3);
        assert_eq!(activity_height(3), 7);
        assert_eq!(status_height(), 3);
    }

    #[test]
    fn minimum_view_height_accounts_for_all_gutters() {
        // transcript/activity gap + activity + input minimum + input/status gap
        // + status surface
        assert_eq!(minimum_view_height(activity_height(1)), 11);
        assert_eq!(minimum_view_height(activity_height(3)), 15);
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
        let layout = compute_layout(area, 1, 1).expect("terminal should fit layout");
        assert_eq!(layout.transcript.y, 0);
        assert!(layout.transcript.height > 0);
        assert_eq!(layout.input.y + layout.input.height + 1, layout.footer.y);
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
        let commands = slash_suggestions(&app);
        let names: Vec<_> = commands.iter().map(|(name, _)| name.as_str()).collect();
        assert!(names.contains(&"/session"));
        assert!(names.contains(&"/skill:rust"));
        assert!(!names.contains(&"/model"));

        app.input = InputField::from_text("/model ");
        let models = slash_suggestions(&app);
        assert!(models.iter().any(|(name, _)| name == "/model test-model"));

        app.input = InputField::from_text("/provider ");
        let providers = slash_suggestions(&app);
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
        assert!(complete_slash(&mut app));
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
        let layout = compute_layout(Rect::new(0, 0, 80, 24), 1, 1).unwrap();
        assert!(layout.transcript.bottom() <= layout.activity.top());
        assert!(layout.activity.bottom() <= layout.input.top());
        assert!(layout.input.bottom() < layout.footer.top());
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
}

/// Resolve the session for the REPL. Any failure — a corrupt or truncated
/// session file (e.g. left behind by a previous crash), a read-only dir, etc.
/// — falls back to a fresh session instead of panicking at startup. This is
/// what prevents the "run session crash" when resuming a bad session.
fn resolve_session(args: &Args, cwd: &str) -> Session {
    let result = if args.no_session {
        Ok(Session::in_memory(cwd.to_string()))
    } else if args.new_session {
        Session::new(cwd.to_string(), args.session_name.clone())
    } else if let Some(path) = &args.session_path {
        if path.exists() {
            Session::from_path(path)
        } else {
            Session::new(cwd.to_string(), args.session_name.clone())
        }
    } else {
        Session::open_or_continue(cwd.to_string(), None, false)
    };

    match result {
        Ok(mut s) => {
            if let Some(name) = &args.session_name {
                s.set_name(name.clone()).ok();
            }
            s
        }
        Err(e) => {
            eprintln!(
                "[session] could not open existing session ({}); starting a fresh one",
                e
            );
            match Session::new(cwd.to_string(), args.session_name.clone()) {
                Ok(mut s) => {
                    if let Some(name) = &args.session_name {
                        s.set_name(name.clone()).ok();
                    }
                    s
                }
                Err(e2) => {
                    eprintln!(
                        "[session] could not create session ({}); running in-memory",
                        e2
                    );
                    Session::in_memory(cwd.to_string())
                }
            }
        }
    }
}

pub fn run_ratatui_repl(args: &Args) -> std::io::Result<()> {
    if !std::io::stdout().is_terminal() {
        return Err(std::io::Error::other(
            "interactive UI requires a terminal (TTY); pass a prompt instead, e.g. `ak \"your prompt\"`",
        ));
    }
    let mut config =
        match LlmConfig::from_env(args.base_url.clone(), args.model.clone(), args.permission) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("LLM config error: {}", e);
                std::process::exit(1);
            }
        };

    let cwd = env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let session = resolve_session(args, &cwd);
    if let Some(path) = session.path() {
        if let Ok(state) = crate::load_session_state(path) {
            if let Some(provider_name) = state.get("provider") {
                match Provider::parse(provider_name) {
                    Ok(provider) => {
                        if let Err(error) = config.switch_provider(provider) {
                            eprintln!("[session] could not restore provider: {}", error);
                        }
                    }
                    Err(error) => eprintln!("[session] could not restore provider: {}", error),
                }
            }
            if let Some(model) = state.get("model") {
                config.model = model.clone();
            }
        }
    }

    let mut skill_dirs = crate::skill_dirs();
    skill_dirs.extend(args.skill_dirs.iter().cloned());
    let skills = crate::discover_skills(&skill_dirs);

    let mut messages = if session.count() > 0 {
        if let Some(path) = session.path() {
            crate::load_messages_from_session(path).unwrap_or_default()
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };
    messages.insert(
        0,
        ChatMessage {
            role: "system".to_string(),
            content: Some(crate::system_prompt(&skills)),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        },
    );

    let tool_state = ToolState::load();
    let (git_branch, git_dirty) = git_context(&cwd);

    let mut app = App {
        transcript: Vec::new(),
        input: InputField::new(),
        config,
        messages,
        tool_state,
        session,
        skills,
        turn_start: 0,
        cwd,
        busy: false,
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
        git_branch,
        git_dirty,
        autoscroll: true,
        scroll: 0,
        tick: 0,
        quit: false,
        history: Vec::new(),
        history_index: None,
        history_draft: String::new(),
        slash_selected: 0,
    };

    enable_raw_mode()?;
    let _cleanup = TerminalCleanup;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let (tx, rx) = mpsc::channel::<UiEvent>();
    let (mut steering_tx, steering_rx) = mpsc::channel::<String>();
    app.steering_rx = Some(steering_rx);
    let (steering_accepted_tx, steering_accepted_rx) = mpsc::channel::<String>();
    let (mut followup_tx, followup_rx) = mpsc::channel::<String>();
    app.followup_rx = Some(followup_rx);
    let (followup_accepted_tx, followup_accepted_rx) = mpsc::channel::<String>();
    let (sink_tx, sink_rx) = mpsc::channel::<SinkLine>();
    crate::set_console_sink(Some(sink_tx));
    let (approval_tx, approval_rx) = mpsc::channel::<crate::ApprovalRequest>();
    crate::set_approval_sink(Some(approval_tx));
    app.approval_rx = Some(approval_rx);

    let mut run = || -> std::io::Result<()> {
        loop {
            // Advance the animation frame so the spinner + status update smoothly.
            app.tick = app.tick.wrapping_add(1);
            terminal.draw(|f| view(f, &mut app))?;

            if let Some(rx) = &app.approval_rx {
                if let Ok(request) = rx.try_recv() {
                    app.pending_approval = Some(PendingApproval {
                        name: request.name,
                        input: request.input,
                        response: request.response,
                        selected: 0,
                    });
                }
            }

            if event::poll(Duration::from_millis(50))? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if app.pending_approval.is_some() {
                            match key.code {
                                KeyCode::Up | KeyCode::Left => {
                                    if let Some(approval) = app.pending_approval.as_mut() {
                                        approval.selected = approval.selected.saturating_sub(1);
                                    }
                                }
                                KeyCode::Down | KeyCode::Right | KeyCode::Tab => {
                                    if let Some(approval) = app.pending_approval.as_mut() {
                                        approval.selected = (approval.selected + 1).min(2);
                                    }
                                }
                                KeyCode::Char('y') | KeyCode::Char('Y') => {
                                    resolve_approval(&mut app, crate::ApprovalDecision::Once);
                                }
                                KeyCode::Char('s') | KeyCode::Char('S') => {
                                    resolve_approval(&mut app, crate::ApprovalDecision::Session);
                                }
                                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                                    resolve_approval(&mut app, crate::ApprovalDecision::Deny);
                                }
                                KeyCode::Enter => {
                                    let decision =
                                        app.pending_approval.as_ref().map(
                                            |approval| match approval.selected {
                                                0 => crate::ApprovalDecision::Once,
                                                1 => crate::ApprovalDecision::Session,
                                                _ => crate::ApprovalDecision::Deny,
                                            },
                                        );
                                    if let Some(decision) = decision {
                                        resolve_approval(&mut app, decision);
                                    }
                                }
                                _ => {}
                            }
                        } else if key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            if app.busy {
                                app.cancel_requested = true;
                                crate::request_cancel();
                            } else {
                                app.quit = true;
                            }
                        } else if key.code == KeyCode::Esc && app.busy {
                            app.cancel_requested = true;
                            crate::request_cancel();
                        } else if !app.busy && !slash_suggestions(&app).is_empty() {
                            match key.code {
                                KeyCode::Up => {
                                    app.slash_selected = app.slash_selected.saturating_sub(1);
                                }
                                KeyCode::Down => {
                                    let last = slash_suggestions(&app).len().saturating_sub(1);
                                    app.slash_selected = (app.slash_selected + 1).min(last);
                                }
                                KeyCode::Tab => {
                                    complete_slash(&mut app);
                                }
                                KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                                    complete_slash(&mut app);
                                }
                                _ => app.input.handle_key(key),
                            }
                        } else if key.code == KeyCode::PageUp {
                            let page = terminal
                                .size()
                                .map(|s| s.height.saturating_sub(3) as i32)
                                .unwrap_or(20);
                            scroll_transcript(&mut app, -page);
                        } else if key.code == KeyCode::PageDown {
                            let page = terminal
                                .size()
                                .map(|s| s.height.saturating_sub(3) as i32)
                                .unwrap_or(20);
                            scroll_transcript(&mut app, page);
                        } else if key.code == KeyCode::Up {
                            if app.busy || key.modifiers.contains(KeyModifiers::SHIFT) {
                                scroll_transcript(&mut app, -1);
                            } else if app.history_index.is_some()
                                || app.input.lines.len() <= 1
                                || app.input.row == 0
                            {
                                app.history_up();
                            } else {
                                app.input.handle_key(key);
                            }
                        } else if key.code == KeyCode::Down {
                            if app.busy || key.modifiers.contains(KeyModifiers::SHIFT) {
                                scroll_transcript(&mut app, 1);
                            } else if app.history_index.is_some()
                                || app.input.lines.len() <= 1
                                || app.input.row + 1 >= app.input.lines.len()
                            {
                                app.history_down();
                            } else {
                                app.input.handle_key(key);
                            }
                        } else if key.code == KeyCode::Enter
                            && !key.modifiers.contains(KeyModifiers::SHIFT)
                        {
                            submit(
                                &mut app,
                                &tx,
                                &steering_tx,
                                &steering_accepted_tx,
                                &followup_tx,
                                &followup_accepted_tx,
                                key.modifiers.contains(KeyModifiers::ALT),
                            );
                        } else {
                            app.input.handle_key(key);
                        }
                    }
                    Event::Paste(s) => {
                        for c in s.chars() {
                            app.input.insert_char(c);
                        }
                    }
                    Event::Mouse(mouse) => match mouse.kind {
                        MouseEventKind::ScrollUp => scroll_transcript(&mut app, -3),
                        MouseEventKind::ScrollDown => scroll_transcript(&mut app, 3),
                        _ => {}
                    },
                    Event::Resize(..) => {} // nothing: frame recomputed each draw
                    _ => {}
                }
            }

            while let Ok(accepted) = steering_accepted_rx.try_recv() {
                if let Some(index) = app
                    .pending_steering
                    .iter()
                    .position(|text| text == &accepted)
                {
                    app.pending_steering.remove(index);
                }
                render_user_prompt(&mut app, &accepted);
            }
            while let Ok(accepted) = followup_accepted_rx.try_recv() {
                if let Some(index) = app
                    .pending_followups
                    .iter()
                    .position(|text| text == &accepted)
                {
                    app.pending_followups.remove(index);
                }
                render_user_prompt(&mut app, &accepted);
            }

            // Streamed console output (incremental assistant/tool lines).
            while let Ok(sl) = sink_rx.try_recv() {
                append_sink_line(&mut app, sl);
            }

            while let Ok(ev) = rx.try_recv() {
                match ev {
                    UiEvent::State {
                        messages,
                        tool_state,
                        session,
                    } => {
                        app.messages = messages;
                        app.tool_state = tool_state;
                        app.session = *session;
                        app.turn_start = app.messages.len();
                        (app.git_branch, app.git_dirty) = git_context(&app.cwd);
                    }
                    UiEvent::Done { success } => {
                        let _ = app.session.turn_event(if success {
                            "turn_complete"
                        } else {
                            "turn_failed"
                        });
                        app.busy = false;
                        app.active_tool = None;
                        if let Some(started) = app.turn_started {
                            let tokens = app
                                .tool_state
                                .last_usage
                                .unwrap_or_else(|| crate::estimate_tokens(&app.messages));
                            app.last_activity = Some(format!(
                                "worked for {:.1}s · {} tokens",
                                started.elapsed().as_secs_f64(),
                                format_tokens(tokens)
                            ));
                            app.turn_started = None;
                            push_transcript_gap(&mut app);
                        }
                        if app.cancel_requested {
                            let mut restored = Vec::new();
                            restored.append(&mut app.pending_steering);
                            restored.append(&mut app.pending_followups);
                            if !restored.is_empty() {
                                app.input = InputField::from_text(&restored.join("\n"));
                            }
                            app.cancel_requested = false;
                        }
                        let (next_tx, next_rx) = mpsc::channel::<String>();
                        steering_tx = next_tx;
                        app.steering_rx = Some(next_rx);
                        let (next_followup_tx, next_followup_rx) = mpsc::channel::<String>();
                        followup_tx = next_followup_tx;
                        app.followup_rx = Some(next_followup_rx);
                    }
                }
            }

            if app.quit {
                break;
            }
        }
        Ok(())
    };
    let res = run();
    crate::set_approval_sink(None);
    // Always restore the terminal, even if the loop returned early via `?`.
    disable_raw_mode().ok();
    let _ = execute!(std::io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
    res
}
