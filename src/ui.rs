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
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind,
    KeyModifiers, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::execute;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{block::Padding, Block, Borders, Paragraph};
use ratatui::Terminal;
use ratatui_markdown::markdown::{MarkdownBlock, MarkdownRenderer};
use ratatui_markdown::ThemeConfig;
use unicode_width::UnicodeWidthChar as _;

use crate::{
    Args, ChatMessage, LlmConfig, Session, SinkLine, Skill, ToolState,
};

/// Events from the agent worker thread into the UI loop.
enum UiEvent {
    /// Updated conversation + tool state after a turn completes.
    State { messages: Vec<ChatMessage>, tool_state: ToolState },
    Done,
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
        InputField { lines: vec![String::new()], row: 0, col: 0 }
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
}

impl App {
    fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        if self.history_index.is_none() {
            self.history_draft = self.input.text();
        }
        let idx = self.history_index.map(|i| (i + 1).min(self.history.len() - 1)).unwrap_or(0);
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

/// Braille spinner frames, matching the headless console spinner.
const UI_SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn ui_status(app: &App) -> String {
    let cwd = compact_path(&app.cwd);
    let git = app.git_branch.as_ref().map(|branch| {
        format!(" · {}{}", branch, if app.git_dirty { "*" } else { "" })
    }).unwrap_or_default();
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
        "{} · {}{} · {} / {} tokens ({}%)",
        cwd,
        app.config.model,
        git,
        format_tokens(tokens),
        format_tokens(app.config.context_window),
        context_pct
    )
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
            blocks.push(MarkdownBlock::TaskItem { text: rest.to_string(), indent: 0, checked: false });
            i += 1;
        } else if let Some(rest) = t.strip_prefix("- [x] ") {
            blocks.push(MarkdownBlock::TaskItem { text: rest.to_string(), indent: 0, checked: true });
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
            while i < lines.len() && !lines[i].trim_start().is_empty() && !is_block_start(lines[i].trim_start()) {
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
    let blocks = split_markdown(s);
    // Width zero disables the renderer's own wrapping. The final transcript
    // wrapper knows the actual terminal width, so
    // using two independent widths can otherwise spill a character onto a
    // new row at column zero.
    let renderer = MarkdownRenderer::new(0);
    renderer.render(&blocks, &ThemeConfig::default())
}

fn append_sink_line(app: &mut App, sl: SinkLine) {
    match sl {
        SinkLine::Assistant(s) => {
            // The model often streams an empty line at both sides of a
            // paragraph. Keep one intentional separator, but don't let those
            // stream boundaries turn the transcript into a wall of whitespace.
            if s.trim().is_empty() {
                if !app.transcript.last().is_some_and(|line| line.spans.is_empty()) {
                    app.transcript.push(Line::from(String::new()));
                }
            } else {
                // A response that follows tool traffic starts a new visual
                // block, even when the model did not emit a blank markdown
                // line between the two.
                if app.transcript.last().is_some_and(|line| {
                    is_tool_line(line) || is_user_line(line)
                }) {
                    push_transcript_gap(app);
                }
                for line in markdown_lines(s.trim_end()) {
                    app.transcript.push(indent_output(line));
                }
            }
        }
        SinkLine::ToolInput(s) => {
            // Keep a tool call attached to its result, but separate the pair
            // from surrounding prose and from the previous tool pair.
            if app.transcript.last().is_some_and(|line| !line.spans.is_empty()) {
                push_transcript_gap(app);
            }
            let mut it = s.splitn(2, ' ');
            let name = it.next().unwrap_or("").to_string();
            let arg = it.next().unwrap_or("").to_string();
            app.active_tool = Some(name.clone());
            app.transcript.push(Line::from(vec![
                Span::styled("▸ ", Style::default().fg(Color::Yellow)),
                Span::styled(name, Style::default().fg(Color::Yellow)),
                Span::styled(format!(" {arg}"), Style::default().fg(Color::DarkGray)),
            ]));
        }
        SinkLine::ToolOutput { name, summary } => {
            let failed = summary.starts_with("failed ·");
            let color = if failed { Color::LightRed } else { Color::LightGreen };
            app.transcript.push(Line::from(vec![
                Span::styled("└ ", Style::default().fg(color)),
                Span::styled(if failed { "✗ " } else { "✓ " }, Style::default().fg(color)),
                Span::styled(name, Style::default().fg(Color::DarkGray)),
                Span::styled(format!(" {summary}"), Style::default().fg(color)),
            ]));
        }
        SinkLine::System(s) => app
            .transcript
            .push(Line::from(vec![
                Span::styled("· ", Style::default().fg(Color::DarkGray)),
                Span::styled(s, Style::default().fg(Color::DarkGray)),
            ])),
        SinkLine::Error(s) => {
            if app.transcript.last().is_some_and(|line| !line.spans.is_empty()) {
                push_transcript_gap(app);
            }
            app.transcript.push(Line::from(vec![
                Span::styled("! ", Style::default().fg(Color::Red)),
                Span::styled(format!("error: {s}"), Style::default().fg(Color::Red)),
            ]));
        }
    }
    app.autoscroll = true;
}

fn is_tool_line(line: &Line<'static>) -> bool {
    line.spans.first().is_some_and(|span| {
        span.content.as_ref().starts_with("▸") || span.content.as_ref().starts_with("└")
    })
}

fn is_user_line(line: &Line<'static>) -> bool {
    line.spans.iter().any(|span| span.style.bg.is_some())
}

fn push_transcript_gap(app: &mut App) {
    if !app.transcript.last().is_some_and(|line| line.spans.is_empty()) {
        app.transcript.push(Line::from(String::new()));
    }
}

fn push_info(app: &mut App, text: String) {
    app.transcript.push(Line::from(Span::styled(text, Style::default().fg(Color::Cyan))));
}

fn indent_output(mut line: Line<'static>) -> Line<'static> {
    line.spans.insert(0, Span::raw("  "));
    line
}

fn view(f: &mut ratatui::Frame, app: &mut App) {
    let area = f.area();
    // Avoid ratatui's `min > max` panic on degenerate/zero-size areas.
    if area.height < 9 {
        return;
    }
    // Keep the composer large enough to feel like an input box, while letting
    // it grow for pasted/multi-line prompts. The four other footer rows are
    // fixed below; the transcript receives the remaining space.
    let input_width = area.width;
    let input_rows = render_input(&app.input, input_width).0.len() as u16;
    let pending_total = app.pending_steering.len() + app.pending_followups.len();
    let visible_pending = pending_total.min(3) as u16;
    let extra_queue_line = u16::from(pending_total > 3);
    let activity_items = 1 + visible_pending + extra_queue_line;
    let activity_h = activity_items * 2 + 1;
    let input_h = (input_rows + 2)
        .clamp(3, 8)
        .min(area.height.saturating_sub(activity_h + 3));
    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1), // breathing room between transcript and input
        Constraint::Length(activity_h), // activity plus queue and breathing room
        Constraint::Length(input_h), // input box
        Constraint::Length(1), // gap between input box and status bar
        Constraint::Length(1), // status; the input's top border is the divider
    ])
    .split(area);

    let visible = chunks[0].height as usize;
    // Pre-wrap each transcript line to the terminal width so the scroll
    // offset is exact (one Vec entry == one rendered row) and overflow can't
    // happen. While a turn runs, append a live "working…" line under the
    // just-submitted input so progress is visible where the user is looking.
    let mut display: Vec<Line<'static>> = Vec::new();
    for line in &app.transcript {
        display.extend(wrap_line_display(line, chunks[0].width));
    }
    let total = display.len();
    let max_scroll = (total.saturating_sub(visible)) as u16;
    if app.autoscroll {
        app.scroll = max_scroll;
    } else {
        app.scroll = app.scroll.min(max_scroll);
        // Re-enable autoscroll once the user scrolls back to the bottom.
        if app.scroll >= max_scroll {
            app.autoscroll = true;
        }
    }
    let transcript = Paragraph::new(display)
        .style(Style::default().fg(Color::Gray))
        .scroll((app.scroll, 0));
    f.render_widget(transcript, chunks[0]);

    // Keep activity feedback in one place above the input. Completed timing
    // is captured once and remains static until the next turn starts.
    if app.busy || app.last_activity.is_some() {
        let (activity_text, activity_color) = if app.busy {
            let frame = UI_SPINNER[(app.tick / 4) as usize % UI_SPINNER.len()];
            let tool = app
                .active_tool
                .as_ref()
                .map(|name| format!(" · {name}"))
                .unwrap_or_default();
            (format!("{} working…{}", frame, tool), Color::DarkGray)
        } else {
            (app.last_activity.clone().unwrap_or_default(), Color::LightGreen)
        };
        let mut activity_lines = vec![Line::from(Span::styled(
            activity_text,
            Style::default().fg(activity_color),
        ))];
        let mut shown = 0;
        for pending in app.pending_steering.iter().take(3) {
            activity_lines.push(Line::from(Span::styled(
                format!("steer · {pending}"),
                Style::default().fg(Color::Yellow),
            )));
            shown += 1;
        }
        for pending in app.pending_followups.iter().take(3usize.saturating_sub(shown)) {
            activity_lines.push(Line::from(Span::styled(
                format!("follow-up · {pending}"),
                Style::default().fg(Color::Yellow),
            )));
            shown += 1;
        }
        if pending_total > shown {
            activity_lines.push(Line::from(Span::styled(
                format!("+{} more queued", pending_total - shown),
                Style::default().fg(Color::Yellow),
            )));
        }
        let mut spaced_activity = Vec::with_capacity(activity_lines.len() * 2 - 1);
        for (index, line) in activity_lines.into_iter().enumerate() {
            if index > 0 {
                spaced_activity.push(Line::from(String::new()));
            }
            spaced_activity.push(line);
        }
        f.render_widget(
            Paragraph::new(spaced_activity)
            .block(Block::default().padding(Padding::vertical(1))),
            chunks[2],
        );
    }

    // The input field is delimited by top and bottom borders (matching the
    // Pi coding agent). When the input is focused (not busy), the borders
    // use the accent color to make the active typing area obvious without
    // competing with the conversation. When busy, they go dim.
    let border_color = if app.busy { Color::DarkGray } else { Color::LightGreen };
    let input_block = Block::new()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(Style::default().fg(border_color));
    // Compute the inner area after the block's top and bottom borders.
    let input_inner = input_block.inner(chunks[3]);
    // The inner width accounts for horizontal padding (2 on each side).
    let inner_w = input_inner.width;
    let (lines, cursor) = render_input(&app.input, inner_w);
    let content_rows = input_inner.height;
    let scroll = (cursor.0 + 1).saturating_sub(content_rows);
    let input_p = Paragraph::new(lines)
        .style(Style::default().fg(Color::White))
        .scroll((scroll, 0))
        .block(input_block);
    f.render_widget(input_p, chunks[3]);
    if !app.busy {
        // The paragraph is scrolled up by `scroll` rows; offset the hardware
        // cursor by the same amount so it stays on the cursor's visible line.
        let cur_y = cursor.2.saturating_sub(scroll);
        f.set_cursor_position((input_inner.x + cursor.1, input_inner.y + cur_y));
    }

    let scroll_hint = if !app.autoscroll { "▲ more above · " } else { "" };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(scroll_hint, Style::default().fg(Color::DarkGray)),
            Span::styled(ui_status(app), Style::default().fg(Color::Cyan)),
        ])),
        chunks[4],
    );
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
        let cw = line[ci..cn].chars().next().unwrap().width().unwrap_or(0).max(1);
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
    app.scroll = (app.scroll as i32).saturating_add(delta).clamp(0, u16::MAX as i32) as u16;
}

/// Wrap a styled `Line` into rows that each fit `width`, greedy char-based
/// (matching the input field's wrap) while preserving spans. One returned
/// `Line` == one rendered row, so a scroll offset is exact.
fn wrap_line_display(line: &Line<'static>, width: u16) -> Vec<Line<'static>> {
    let w = width.max(1) as usize;
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;
    // Assistant output carries a plain two-space prefix. Treat it as a
    // hanging indent so wrapped continuation rows receive the same prefix.
    // Submitted input has a background style on its prefix and is excluded.
    let output_indent = line.spans.first().is_some_and(|span| {
        span.content.as_ref() == "  " && span.style.bg.is_none()
    });
    let indent_width = if output_indent { 2.min(w) } else { 0 };
    let mut graphemes = line.styled_graphemes(Style::default());
    if output_indent {
        graphemes.next();
        graphemes.next();
        cur.push(Span::raw(" ".repeat(indent_width)));
        cur_w = indent_width;
    }
    for sg in graphemes {
        let cw = sg.symbol.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>().max(1);
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
            .map(|span| span.content.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>())
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
        app.transcript.push(Line::from(String::new()));
    }
    match line {
        "/quit" => return true,
        "/clear" => {
            app.messages.truncate(1);
            push_info(app, "history cleared.".to_string());
        }
        "/new" => {
            app.messages.truncate(1);
            let cwd = env::current_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
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
        "/resume" => match Session::list(&app.cwd) {
            Ok(sessions) if !sessions.is_empty() => {
                push_info(app, "sessions:".to_string());
                for (i, (path, header)) in sessions.iter().enumerate() {
                    let name = header.name().unwrap_or("(unnamed)");
                    push_info(app, format!("  {}: {} ({})", i, name, path.display()));
                }
                push_info(app, "(selecting a session by index is not implemented)".to_string());
            }
            _ => push_info(app, "no sessions found.".to_string()),
        },
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
        "/help" => {
            push_info(app, "commands: /quit /clear /new /session /resume /name <n> /skill:<name> /model [<m>]".to_string());
            push_info(app, "keys: Enter send · Shift+Enter newline · ↑↓ history · PgUp/PgDn/mouse scroll".to_string());
            push_info(app, "while working: Enter queues steer · Alt+Enter queues follow-up · Esc/Ctrl+C cancels and restores queued input".to_string());
        }
        _ if line.starts_with("/model ") => {
            let m = line["/model ".len()..].trim().to_string();
            if !m.is_empty() {
                app.config.model = m.clone();
                push_info(app, format!("switched to model: {}", app.config.model));
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
    app.turn_start = messages.len();
    messages.push(ChatMessage {
        role: "user".to_string(),
        content: Some(line),
        tool_calls: None,
        tool_call_id: None,
        name: None,
    });
    let config = app.config.clone();
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
        let _ = tx.send(UiEvent::State { messages, tool_state });
        let _ = tx.send(UiEvent::Done);
    });
}

fn render_user_prompt(app: &mut App, line: &str) {
    // Give the submitted prompt its own lightly highlighted block with
    // breathing room above and below, so it is easy to distinguish from
    // model output.
    let user_bg = Style::default()
        .fg(Color::White)
        .bg(Color::Rgb(20, 38, 54));
    // Padding is part of the highlighted surface, rather than an unstyled
    // gap outside it.
    app.transcript
        .push(Line::from(Span::styled(" ", user_bg)));
    for sub in line.split('\n') {
        app.transcript.push(Line::from(vec![
            Span::styled("  ", user_bg),
            Span::styled(sub.to_string(), user_bg),
            Span::styled(" ", user_bg),
        ]));
    }
    app.transcript
        .push(Line::from(Span::styled(" ", user_bg)));
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
    let config = match LlmConfig::from_env(args.base_url.clone(), args.model.clone()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("LLM config error: {}", e);
            std::process::exit(1);
        }
    };

    let cwd = env::current_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    let session = resolve_session(args, &cwd);

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
        git_branch,
        git_dirty,
        autoscroll: true,
        scroll: 0,
        tick: 0,
        quit: false,
        history: Vec::new(),
        history_index: None,
        history_draft: String::new(),
    };

    enable_raw_mode()?;
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

    let mut run = || -> std::io::Result<()> {
    loop {
        // Advance the animation frame so the spinner + status update smoothly.
        app.tick = app.tick.wrapping_add(1);
        terminal.draw(|f| view(f, &mut app))?;

        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                        if app.busy {
                            app.cancel_requested = true;
                            crate::request_cancel();
                        } else {
                            app.quit = true;
                        }
                    } else if key.code == KeyCode::Esc && app.busy {
                        app.cancel_requested = true;
                        crate::request_cancel();
                    } else if key.code == KeyCode::PageUp {
                        let page = terminal.size().map(|s| s.height.saturating_sub(3) as i32).unwrap_or(20);
                        scroll_transcript(&mut app, -page);
                    } else if key.code == KeyCode::PageDown {
                        let page = terminal.size().map(|s| s.height.saturating_sub(3) as i32).unwrap_or(20);
                        scroll_transcript(&mut app, page);
                    } else if key.code == KeyCode::Up {
                        if app.busy || key.modifiers.contains(KeyModifiers::SHIFT) {
                            scroll_transcript(&mut app, -1);
                        } else if app.history_index.is_some() || app.input.lines.len() <= 1 || app.input.row == 0 {
                            app.history_up();
                        } else {
                            app.input.handle_key(key);
                        }
                    } else if key.code == KeyCode::Down {
                        if app.busy || key.modifiers.contains(KeyModifiers::SHIFT) {
                            scroll_transcript(&mut app, 1);
                        } else if app.history_index.is_some() || app.input.lines.len() <= 1 || app.input.row + 1 >= app.input.lines.len() {
                            app.history_down();
                        } else {
                            app.input.handle_key(key);
                        }
                    } else if key.code == KeyCode::Enter && !key.modifiers.contains(KeyModifiers::SHIFT) {
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
            if let Some(index) = app.pending_steering.iter().position(|text| text == &accepted) {
                app.pending_steering.remove(index);
            }
            render_user_prompt(&mut app, &accepted);
        }
        while let Ok(accepted) = followup_accepted_rx.try_recv() {
            if let Some(index) = app.pending_followups.iter().position(|text| text == &accepted) {
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
                UiEvent::State { messages, tool_state } => {
                    // Persist the messages produced during the turn.
                    let start = app.turn_start.min(messages.len());
                    for msg in &messages[start..] {
                        let _ = app.session.append_message(msg.clone());
                    }
                    app.messages = messages;
                    app.tool_state = tool_state;
                    app.turn_start = app.messages.len();
                    (app.git_branch, app.git_dirty) = git_context(&app.cwd);
                }
                UiEvent::Done => {
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
    // Always restore the terminal, even if the loop returned early via `?`.
    disable_raw_mode().ok();
    let _ = execute!(std::io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
    res
}
