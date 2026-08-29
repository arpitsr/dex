use std::env;
use std::io::{self, IsTerminal, Write};
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
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Terminal;

use crate::cli::Args;
use crate::client::http::DaemonClient;
use crate::protocol::{ApprovalDecision, StreamEvent};

use super::input::InputField;
use super::render::markdown_lines;
use super::{TerminalCleanup, INPUT_BG, TRANSCRIPT_INDENT, UI_SPINNER};

/// Remote TUI state — talks to the daemon via HTTP instead of running
/// `process_turn` locally.
struct RemoteApp {
    transcript: Vec<Line<'static>>,
    input: InputField,
    client: DaemonClient,
    session_id: String,
    busy: bool,
    autoscroll: bool,
    scroll: u16,
    tick: u16,
    quit: bool,
    turn_started: Option<Instant>,
    active_tool: Option<String>,
    pending_approval: Option<PendingApproval>,
    history: Vec<String>,
    history_index: Option<usize>,
    history_draft: String,
}

impl RemoteApp {
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

struct PendingApproval {
    name: String,
    input: String,
    selected: usize,
}

pub(crate) fn run_ratatui_repl_with_remote(_args: &Args, daemon_url: &str) -> std::io::Result<()> {
    if !std::io::stdout().is_terminal() {
        return Err(std::io::Error::other(
            "interactive UI requires a terminal (TTY); use `ak connect <url> \"prompt\"` for one-shot",
        ));
    }

    let client = DaemonClient::new(daemon_url)
        .map_err(|e| std::io::Error::other(format!("failed to connect to daemon: {e}")))?;

    let cwd = env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    let session = client
        .create_session(&cwd, None)
        .map_err(|e| std::io::Error::other(format!("failed to create session: {e}")))?;

    let mut app = RemoteApp {
        transcript: vec![Line::from(Span::styled(
            format!("connected to {daemon_url} — session {}", session.session_id),
            Style::default().fg(Color::DarkGray),
        ))],
        input: InputField::new(),
        client,
        session_id: session.session_id,
        busy: false,
        autoscroll: true,
        scroll: 0,
        tick: 0,
        quit: false,
        turn_started: None,
        active_tool: None,
        pending_approval: None,
        history: Vec::new(),
        history_index: None,
        history_draft: String::new(),
    };

    enable_raw_mode()?;
    let _cleanup = TerminalCleanup;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut run = || -> std::io::Result<()> {
        loop {
            app.tick = app.tick.wrapping_add(1);
            terminal.draw(|f| remote_view(f, &mut app))?;

            if event::poll(Duration::from_millis(50))? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        handle_key(&mut app, key);
                    }
                    Event::Paste(s) => {
                        for c in s.chars() {
                            app.input.insert_char(c);
                        }
                    }
                    Event::Mouse(mouse) => match mouse.kind {
                        MouseEventKind::ScrollUp => {
                            app.autoscroll = false;
                            app.scroll = app.scroll.saturating_add(3);
                        }
                        MouseEventKind::ScrollDown => {
                            app.autoscroll = false;
                            app.scroll = app.scroll.saturating_sub(3);
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }

            if app.quit {
                break;
            }
        }
        Ok(())
    };

    let res = run();
    disable_raw_mode().ok();
    let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
    res
}

fn handle_key(app: &mut RemoteApp, key: crossterm::event::KeyEvent) {
    // Handle approval overlay first.
    if let Some(ref approval) = app.pending_approval {
        match key.code {
            KeyCode::Up | KeyCode::Left => {
                app.pending_approval.as_mut().unwrap().selected =
                    approval.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Right | KeyCode::Tab => {
                app.pending_approval.as_mut().unwrap().selected = (approval.selected + 1).min(2);
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                send_approval(app, ApprovalDecision::AllowOnce);
            }
            KeyCode::Char('s') | KeyCode::Char('S') => {
                send_approval(app, ApprovalDecision::AllowSession);
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                send_approval(app, ApprovalDecision::Deny);
            }
            KeyCode::Enter => {
                let decision = match app.pending_approval.as_ref().map(|a| a.selected) {
                    Some(0) => ApprovalDecision::AllowOnce,
                    Some(1) => ApprovalDecision::AllowSession,
                    _ => ApprovalDecision::Deny,
                };
                send_approval(app, decision);
            }
            _ => {}
        }
        return;
    }

    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if app.busy {
                let _ = app.client.cancel(&app.session_id);
                app.busy = false;
                app.active_tool = None;
                app.transcript.push(indent_line(Line::from(Span::styled(
                    "turn cancelled",
                    Style::default().fg(Color::Yellow),
                ))));
            } else {
                app.quit = true;
            }
        }
        KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
            submit_prompt(app);
        }
        KeyCode::Up
            if !app.busy
                && (app.history_index.is_some()
                    || app.input.lines.len() <= 1
                    || app.input.row == 0) =>
        {
            history_up(app);
        }
        KeyCode::Down
            if !app.busy
                && (app.history_index.is_some()
                    || app.input.lines.len() <= 1
                    || app.input.row + 1 >= app.input.lines.len()) =>
        {
            history_down(app);
        }
        _ => {
            app.input.handle_key(key);
        }
    }
}

fn submit_prompt(app: &mut RemoteApp) {
    let line = app.input.text().trim().to_string();
    app.history_push(line.clone());
    app.input.reset();
    if line.is_empty() || app.busy {
        return;
    }

    // Render the user prompt in the transcript.
    let user_bg = Style::default().fg(Color::White).bg(INPUT_BG);
    let pad = " ".repeat(TRANSCRIPT_INDENT);
    app.transcript.push(Line::from(String::new()));
    for sub in line.split('\n') {
        app.transcript.push(Line::from(vec![
            Span::styled(pad.clone(), user_bg),
            Span::styled(sub.to_string(), user_bg),
        ]));
    }
    app.transcript.push(Line::from(String::new()));

    app.busy = true;
    app.turn_started = Some(Instant::now());

    // Send the prompt to the daemon and stream events back.
    let session_id = app.session_id.clone();
    let prompt = line.clone();

    // We need to collect all events synchronously since the TUI is single-threaded.
    // The daemon streams events via SSE; we read them all, then process.
    let events = match app
        .client
        .chat_with_approval(&session_id, &prompt, |name, input| {
            // Show approval overlay in the TUI and wait for user input.
            // Since we're in a blocking read loop, we can't show the overlay here.
            // Instead, default to AllowOnce for now — the overlay approach needs
            // async event handling which we'll add next.
            // TODO: Implement async approval with the approval overlay.
            eprintln!("\n  Approve {name}? ({input}) [y/N/s] ");
            io::stderr().flush().ok();
            let mut answer = String::new();
            io::stdin().read_line(&mut answer).ok();
            match answer.trim().to_ascii_lowercase().as_str() {
                "y" | "yes" => ApprovalDecision::AllowOnce,
                "s" | "session" => ApprovalDecision::AllowSession,
                _ => ApprovalDecision::Deny,
            }
        }) {
        Ok(e) => e,
        Err(e) => {
            app.transcript.push(indent_line(Line::from(Span::styled(
                format!("error: {e}"),
                Style::default().fg(Color::Red),
            ))));
            app.busy = false;
            return;
        }
    };

    // Process all collected events into the transcript.
    for event in &events {
        match event {
            StreamEvent::AssistantText(text) => {
                if !text.trim().is_empty() {
                    for line in markdown_lines(text.trim_end()) {
                        app.transcript.push(indent_line(line));
                    }
                }
            }
            StreamEvent::ToolCall { name, args } => {
                let arg_str = if args.is_null() {
                    String::new()
                } else {
                    format!(" {args}")
                };
                app.active_tool = Some(name.clone());
                app.transcript.push(indent_line(Line::from(vec![
                    Span::styled("▸ ", Style::default().fg(Color::Yellow)),
                    Span::styled(name.clone(), Style::default().fg(Color::Yellow)),
                    Span::styled(arg_str, Style::default().fg(Color::DarkGray)),
                ])));
            }
            StreamEvent::ToolResult {
                name,
                summary,
                success,
            } => {
                let color = if *success {
                    Color::LightGreen
                } else {
                    Color::LightRed
                };
                let icon = if *success { "✓" } else { "✗" };
                app.transcript.push(indent_line(Line::from(vec![
                    Span::styled("└ ", Style::default().fg(color)),
                    Span::styled(format!("{icon} "), Style::default().fg(color)),
                    Span::styled(name.clone(), Style::default().fg(Color::DarkGray)),
                    Span::styled(format!(" {summary}"), Style::default().fg(color)),
                ])));
            }
            StreamEvent::ApprovalRequired {
                name,
                input,
                request_id,
            } => {
                // Store the request_id so we can send it back.
                app.pending_approval = Some(PendingApproval {
                    name: name.clone(),
                    input: input.clone(),
                    selected: 0,
                });
                // TODO: Send request_id with the approval response.
                let _ = request_id;
            }
            StreamEvent::TurnComplete { .. } => {}
            StreamEvent::TurnFailed { error } => {
                app.transcript.push(indent_line(Line::from(Span::styled(
                    format!("error: {error}"),
                    Style::default().fg(Color::Red),
                ))));
            }
            StreamEvent::System(msg) => {
                app.transcript.push(indent_line(Line::from(vec![
                    Span::styled("· ", Style::default().fg(Color::DarkGray)),
                    Span::styled(msg.clone(), Style::default().fg(Color::DarkGray)),
                ])));
            }
            StreamEvent::Error(msg) => {
                app.transcript.push(indent_line(Line::from(Span::styled(
                    format!("error: {msg}"),
                    Style::default().fg(Color::Red),
                ))));
            }
        }
    }

    // Turn complete.
    app.busy = false;
    app.active_tool = None;
    if let Some(started) = app.turn_started {
        app.transcript.push(Line::from(String::new()));
        let _ = started; // could show duration
        app.turn_started = None;
    }
}

fn send_approval(app: &mut RemoteApp, _decision: ApprovalDecision) {
    if app.pending_approval.is_some() {
        app.pending_approval = None;
        // The approval was already handled inline during chat_with_approval.
        // This is a placeholder for the async overlay approach.
    }
}

fn history_up(app: &mut RemoteApp) {
    if app.history.is_empty() {
        return;
    }
    if app.history_index.is_none() {
        app.history_draft = app.input.text();
    }
    let idx = app
        .history_index
        .map(|i| (i + 1).min(app.history.len() - 1))
        .unwrap_or(0);
    app.history_index = Some(idx);
    app.input = InputField::from_text(&app.history[app.history.len() - 1 - idx]);
}

fn history_down(app: &mut RemoteApp) {
    match app.history_index {
        None => {}
        Some(0) => {
            app.history_index = None;
            app.input = InputField::from_text(&app.history_draft);
        }
        Some(idx) => {
            let new_idx = idx - 1;
            app.history_index = Some(new_idx);
            app.input = InputField::from_text(&app.history[app.history.len() - 1 - new_idx]);
        }
    }
}

fn indent_line(mut line: Line<'static>) -> Line<'static> {
    line.spans
        .insert(0, Span::raw(" ".repeat(TRANSCRIPT_INDENT)));
    line
}

// --- Rendering ---

fn remote_view(f: &mut ratatui::Frame, app: &mut RemoteApp) {
    let area = f.area();

    // Split: transcript (top), status bar, input (bottom).
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),    // transcript
            Constraint::Length(1), // status
            Constraint::Length(3), // input
        ])
        .split(area);

    render_transcript(f, app, chunks[0]);
    render_status(f, app, chunks[1]);
    render_input(f, app, chunks[2]);

    // Approval overlay.
    if let Some(ref approval) = app.pending_approval {
        render_approval_overlay(f, area, approval);
    }
}

fn render_transcript(f: &mut ratatui::Frame, app: &RemoteApp, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" transcript ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.transcript.is_empty() {
        return;
    }

    // Calculate visible lines based on scroll.
    let total_lines = app.transcript.len();
    let visible_height = inner.height as usize;
    let max_scroll = total_lines.saturating_sub(visible_height);
    let scroll = if app.autoscroll {
        max_scroll
    } else {
        app.scroll as usize
    };
    let start = total_lines.saturating_sub(visible_height + scroll);
    let end = (start + visible_height).min(total_lines);

    let visible: Vec<Line<'static>> = app.transcript[start..end].to_vec();
    let paragraph = Paragraph::new(visible).wrap(Wrap { trim: false });
    f.render_widget(paragraph, inner);
}

fn render_status(f: &mut ratatui::Frame, app: &RemoteApp, area: Rect) {
    let status = if app.busy {
        let frame = UI_SPINNER[app.tick as usize % UI_SPINNER.len()];
        let tool = app
            .active_tool
            .as_deref()
            .map(|t| format!(" {t}"))
            .unwrap_or_default();
        format!("{frame} working{tool} ...")
    } else {
        format!("session: {} (remote)", app.session_id)
    };
    let style = if app.busy {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let paragraph = Paragraph::new(Line::from(Span::styled(status, style)));
    f.render_widget(paragraph, area);
}

fn render_input(f: &mut ratatui::Frame, app: &RemoteApp, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" prompt ")
        .style(Style::default().bg(INPUT_BG));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let text = app.input.text();
    let lines: Vec<Line<'static>> = text
        .split('\n')
        .map(|l| Line::from(l.to_string()))
        .collect();
    let paragraph = Paragraph::new(lines).style(Style::default().fg(Color::White));
    f.render_widget(paragraph, inner);

    // Show cursor.
    let cursor_row = inner.y + app.input.row as u16;
    let cursor_col = inner.x + app.input.col as u16;
    f.set_cursor_position((cursor_col, cursor_row));
}

fn render_approval_overlay(f: &mut ratatui::Frame, area: Rect, approval: &PendingApproval) {
    let overlay_height = 7;
    let overlay_width = 50.min(area.width - 4);
    let x = (area.width - overlay_width) / 2;
    let y = (area.height - overlay_height) / 2;
    let rect = Rect::new(x, y, overlay_width, overlay_height);

    let block = Block::default()
        .title(" approval required ")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black).fg(Color::White));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let mut lines = vec![
        Line::from(Span::styled(
            format!("tool: {}", approval.name),
            Style::default().fg(Color::Yellow),
        )),
        Line::from(Span::styled(
            format!("input: {}", approval.input),
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
    ];

    let choices = ["allow once (y)", "allow session (s)", "deny (n)"];
    for (i, choice) in choices.iter().enumerate() {
        let style = if i == approval.selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(Span::styled(format!("  {choice}"), style)));
    }

    let paragraph = Paragraph::new(lines);
    f.render_widget(paragraph, inner);
}
