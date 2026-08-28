use std::env;
use std::io::IsTerminal;
use std::sync::mpsc;
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
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::Terminal;

use crate::{
    ApprovalDecision, ApprovalRequest, Args, ChatMessage, LlmConfig, Provider, Session, SinkLine,
    ToolState,
};

use super::slash::{complete_slash, handle_slash, slash_suggestions};
use super::{
    append_sink_line, format_tokens, git_context, push_transcript_gap, resolve_approval,
    scroll_transcript, view, App, InputField, PendingApproval, TerminalCleanup, UiEvent,
    SUBMITTED_PROMPT_BG, TRANSCRIPT_INDENT,
};

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
                &config,
                &crate::agent::state::GlobalCancellation,
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
    let user_bg = Style::default().fg(Color::White).bg(SUBMITTED_PROMPT_BG);
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

pub(crate) fn run_ratatui_repl(args: &Args) -> std::io::Result<()> {
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
    let (approval_tx, approval_rx) = mpsc::channel::<ApprovalRequest>();
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
                                    resolve_approval(&mut app, ApprovalDecision::Once);
                                }
                                KeyCode::Char('s') | KeyCode::Char('S') => {
                                    resolve_approval(&mut app, ApprovalDecision::Session);
                                }
                                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                                    resolve_approval(&mut app, ApprovalDecision::Deny);
                                }
                                KeyCode::Enter => {
                                    let decision =
                                        app.pending_approval.as_ref().map(
                                            |approval| match approval.selected {
                                                0 => ApprovalDecision::Once,
                                                1 => ApprovalDecision::Session,
                                                _ => ApprovalDecision::Deny,
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
