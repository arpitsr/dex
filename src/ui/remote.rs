use std::collections::VecDeque;
use std::io::{self, IsTerminal};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use crossterm::event::{self, DisableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::cli::Args;
use crate::client::http::{ChatOptions, DaemonClient};
use crate::core::types::{
    ApiProtocol, ApprovalDecision as CoreApprovalDecision, PermissionMode, Provider, SinkLine,
};
use crate::protocol::{ApprovalDecision as ProtocolApprovalDecision, DaemonInfo, StreamEvent};
use crate::session::Session;

use super::slash::{complete_slash, handle_slash, slash_suggestions};
use super::{
    append_sink_line, push_info, render_user_prompt, resolve_approval, scroll_transcript, view,
    App, DisableAlternateScroll, EnableAlternateScroll, PendingApproval, TerminalCleanup,
};

/// Messages flowing from the per-turn worker thread into the UI loop.
enum WorkerMessage {
    /// A stream event from the daemon.
    Stream(StreamEvent),
    /// The SSE stream closed; carries a transport error if any.
    Finished(Option<String>),
}

/// Client-server TUI: renders the exact same `App` view as the local engine,
/// but every turn is executed by the daemon and streamed back over SSE.
struct RemoteApp {
    app: App,
    client: DaemonClient,
    session_id: String,
    options: ChatOptions,
    worker_tx: mpsc::Sender<WorkerMessage>,
    worker_rx: mpsc::Receiver<WorkerMessage>,
    /// Paired with the current turn's worker; approval overlays resolve
    /// through it.
    decision_tx: mpsc::Sender<CoreApprovalDecision>,
    /// Shared with the active worker so approvals arriving after a cancel
    /// request are denied instead of parking the turn on the overlay.
    cancel_flag: Arc<AtomicBool>,
}

/// Build a display-only config from the daemon's reported runtime info. The
/// client never talks to the model provider itself; this only feeds the
/// status footer and slash-command suggestions.
fn display_config(info: &DaemonInfo) -> crate::llm::config::LlmConfig {
    crate::llm::config::LlmConfig {
        provider: Provider::parse(&info.provider).unwrap_or(Provider::OpenCode),
        api_key: String::new(),
        base_url: String::new(),
        model: info.model.clone(),
        available_models: if info.available_models.is_empty() {
            vec![info.model.clone()]
        } else {
            info.available_models.clone()
        },
        api: ApiProtocol::Responses,
        account_id: None,
        thinking_effort: None,
        context_window: info.context_window,
        permission: PermissionMode::parse(&info.permission).unwrap_or(PermissionMode::AskWrites),
        max_tool_iterations: 0,
        max_prompt_tokens: 0,
        max_turn_seconds: 0,
        client: reqwest::blocking::Client::new(),
    }
}

pub(crate) fn run_ratatui_repl_with_remote(args: &Args, daemon_url: &str) -> std::io::Result<()> {
    if !std::io::stdout().is_terminal() {
        return Err(std::io::Error::other(
            "interactive UI requires a terminal (TTY); use `dex connect <url> \"prompt\"` for one-shot",
        ));
    }

    let client = DaemonClient::new(daemon_url)
        .map_err(|e| std::io::Error::other(format!("failed to connect to daemon: {e}")))?;
    client
        .wait_until_ready(Duration::from_secs(10))
        .map_err(|e| std::io::Error::other(format!("daemon not ready: {e}")))?;

    // The daemon owns the model/provider/permission and the workspace; mirror
    // its state so the UI shows what turns will actually use.
    let info = client
        .get_config()
        .map_err(|e| std::io::Error::other(format!("failed to read daemon config: {e}")))?;

    let session = client
        .create_session(&info.cwd, args.session_name.as_deref())
        .map_err(|e| std::io::Error::other(format!("failed to create session: {e}")))?;

    // Per-request overrides so client flags keep working in remote mode.
    let options = ChatOptions {
        skill_dirs: args
            .skill_dirs
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        base_url: args.base_url.clone(),
        model: args.model.clone(),
        permission: args.permission.map(|mode| match mode {
            PermissionMode::ReadOnly => "read-only".to_string(),
            PermissionMode::AskWrites => "ask-writes".to_string(),
            PermissionMode::AskShell => "ask-shell".to_string(),
            PermissionMode::Trusted => "trusted".to_string(),
        }),
    };

    let (worker_tx, worker_rx) = mpsc::channel::<WorkerMessage>();
    let (decision_tx, _decision_rx) = mpsc::channel::<CoreApprovalDecision>();
    let cancel_flag = Arc::new(AtomicBool::new(false));

    let mut app = App {
        transcript: Vec::new(),
        input: crate::ui::input::InputField::new(),
        config: display_config(&info),
        messages: Vec::new(),
        tool_state: crate::agent::state::ToolState::default(),
        session: Session::in_memory(info.cwd.clone()),
        skills: Vec::new(),
        turn_start: 0,
        cwd: info.cwd.clone(),
        git_branch: info.git_branch.clone(),
        git_dirty: info.git_dirty,
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
    };
    push_info(
        &mut app,
        format!(
            "connected to {daemon_url} · workspace {} · model {}",
            info.cwd, info.model
        ),
    );

    let mut remote = RemoteApp {
        app,
        client,
        session_id: session.session_id,
        options,
        worker_tx,
        worker_rx,
        decision_tx,
        cancel_flag,
    };

    // Detect the terminal background before raw mode / the alternate screen
    // take over; surface colors are resolved from this once.
    super::theme::detect_background();
    enable_raw_mode()?;
    // Correct fix: consume any OSC 10/11 reply that arrived late.
    // terminal_colorsaurus writes `\x1b]10;?` / `\x1b]11;?` and reads the
    // reply; on timeout the reply (`\x1b]10;rgb:…\x07`) stays in the tty
    // queue and crossterm parses it as Alt+`]` + plain chars + Ctrl-G.
    // Drain with a short deadline so the full burst is consumed before the
    // event loop starts. No input hack — just consume at the source.
    {
        let drain_deadline = Instant::now() + Duration::from_millis(80);
        while Instant::now() < drain_deadline {
            let remaining = drain_deadline.saturating_duration_since(Instant::now());
            if event::poll(remaining)? {
                let _ = event::read();
            } else {
                break;
            }
        }
    }
    let _cleanup = TerminalCleanup;
    let mut stdout = io::stdout();
    // No mouse capture: capturing the mouse makes the terminal hand over
    // click-drag events and stops its own text selection entirely. Instead
    // alternate scroll (DECSET 1007) keeps wheel scrolling working by
    // delivering it as Up/Down arrows, and selection/copy stay native.
    execute!(
        stdout,
        DisableMouseCapture,
        EnterAlternateScreen,
        EnableAlternateScroll
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut run = || -> std::io::Result<()> {
        // Events consumed while classifying arrow bursts, replayed on the
        // next iterations of the loop.
        let mut pending: VecDeque<Event> = VecDeque::new();
        loop {
            // Advance the animation frame so the spinner + status update.
            remote.app.tick = remote.app.tick.wrapping_add(1);
            terminal.draw(|f| view(f, &mut remote.app))?;

            // Drain worker messages: the transcript updates live while the
            // turn streams in on the worker thread.
            loop {
                match remote.worker_rx.try_recv() {
                    Ok(WorkerMessage::Stream(event)) => handle_stream_event(&mut remote, event),
                    Ok(WorkerMessage::Finished(error)) => finish_turn(&mut remote, error),
                    Err(mpsc::TryRecvError::Empty) | Err(mpsc::TryRecvError::Disconnected) => break,
                }
            }

            let next = if let Some(event) = pending.pop_front() {
                event
            } else if event::poll(Duration::from_millis(50))? {
                event::read()?
            } else {
                continue;
            };

            match next {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    handle_key_event(&mut remote, key, &mut pending)?;
                }
                Event::Paste(s) => {
                    // Tabs would render as tab stops and desync the frame;
                    // expand them and drop other control characters.
                    for c in s.chars() {
                        match c {
                            '\t' => {
                                for _ in 0..4 {
                                    remote.app.input.insert_char(' ');
                                }
                            }
                            c if !c.is_control() => remote.app.input.insert_char(c),
                            _ => {}
                        }
                    }
                }
                Event::Resize(..) => {} // frame recomputed each draw
                _ => {}
            }

            if remote.app.quit {
                break;
            }
        }
        Ok(())
    };

    let res = run();
    // Always restore the terminal, even if the loop returned early via `?`.
    disable_raw_mode().ok();
    let _ = execute!(
        io::stdout(),
        DisableAlternateScroll,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    res
}

fn handle_stream_event(remote: &mut RemoteApp, event: StreamEvent) {
    match event {
        StreamEvent::AssistantText(text) => {
            append_sink_line(&mut remote.app, SinkLine::Assistant(text));
        }
        StreamEvent::ToolCall { name, args } => {
            let preview = args.as_str().unwrap_or_default().to_string();
            append_sink_line(
                &mut remote.app,
                SinkLine::ToolInput(format!("{name} {preview}")),
            );
        }
        StreamEvent::ToolResult {
            name,
            summary,
            success,
            preview,
            duration,
        } => {
            append_sink_line(
                &mut remote.app,
                SinkLine::ToolOutput {
                    name,
                    summary,
                    success,
                    preview,
                    duration,
                },
            );
        }
        StreamEvent::ApprovalRequired { name, input, .. } => {
            // Invariant: the daemon parks at most one approval per turn
            // (agent thread blocks until it is resolved), so overwriting would
            // drop the prior sender. If it happens, deny the stale one.
            if let Some(stale) = remote.app.pending_approval.take() {
                let _ = stale.response.send(CoreApprovalDecision::Deny);
            }
            remote.app.pending_approval = Some(PendingApproval {
                name,
                input,
                response: remote.decision_tx.clone(),
                selected: 0,
            });
        }
        StreamEvent::TurnComplete { usage, .. } => {
            if let Some(usage) = usage {
                remote.app.tool_state.last_usage = Some(usage);
            }
        }
        StreamEvent::Usage { tokens } => {
            // Live context usage: emitted by the daemon after every LLM call
            // so the status bar updates mid-turn, not just at completion.
            remote.app.tool_state.last_usage = Some(tokens);
        }
        StreamEvent::TurnFailed { error } => {
            append_sink_line(&mut remote.app, SinkLine::Error(error));
        }
        StreamEvent::System(msg) => {
            append_sink_line(&mut remote.app, SinkLine::System(msg));
        }
        StreamEvent::Error(msg) => {
            append_sink_line(&mut remote.app, SinkLine::Error(msg));
        }
    }
}

fn finish_turn(remote: &mut RemoteApp, error: Option<String>) {
    if let Some(error) = error {
        append_sink_line(&mut remote.app, SinkLine::Error(error));
    }
    let app = &mut remote.app;
    app.busy = false;
    app.cancel_requested = false;
    app.active_tool = None;
    remote.cancel_flag.store(false, Ordering::SeqCst);
    if let Some(started) = app.turn_started.take() {
        let tokens = app
            .tool_state
            .last_usage
            .unwrap_or_else(|| crate::agent::compaction::estimate_tokens(&app.messages));
        app.last_activity = Some(format!(
            "worked for {:.1}s · {} tokens",
            started.elapsed().as_secs_f64(),
            super::format_tokens(tokens)
        ));
    }
}

/// How long to watch for a follow-up arrow before deciding a plain Up/Down
/// was a real keypress rather than the tail of a mouse-wheel burst.
const ARROW_LOOKAHEAD: Duration = Duration::from_millis(25);

/// Route a key press, telling real arrow presses from mouse-wheel scrolls.
/// Without mouse capture the wheel reaches the app through alternate scroll
/// (DECSET 1007): one wheel notch arrives as a burst of plain Up/Down
/// presses queued back-to-back, while real presses — and key auto-repeat —
/// are spaced tens of milliseconds apart. So a plain arrow is only treated
/// as a wheel scroll when a second plain arrow shows up within the
/// lookahead window; anything else read meanwhile is replayed from
/// `pending` so no input is dropped.
fn handle_key_event(
    remote: &mut RemoteApp,
    key: crossterm::event::KeyEvent,
    pending: &mut VecDeque<Event>,
) -> std::io::Result<()> {
    if !remote.app.busy
        && key.modifiers.is_empty()
        && matches!(key.code, KeyCode::Up | KeyCode::Down)
    {
        let mut delta = arrow_delta(key.code);
        let mut wheel = false;
        let deadline = Instant::now() + ARROW_LOOKAHEAD;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            if !event::poll(remaining)? {
                break;
            }
            match event::read()? {
                Event::Key(k)
                    if k.kind == KeyEventKind::Press
                        && k.modifiers.is_empty()
                        && matches!(k.code, KeyCode::Up | KeyCode::Down) =>
                {
                    wheel = true;
                    delta += arrow_delta(k.code);
                }
                other => pending.push_back(other),
            }
        }
        if wheel {
            scroll_transcript(&mut remote.app, delta);
            return Ok(());
        }
    }
    handle_key(remote, key);
    Ok(())
}

fn arrow_delta(code: KeyCode) -> i32 {
    if code == KeyCode::Up {
        -1
    } else {
        1
    }
}

fn handle_key(remote: &mut RemoteApp, key: crossterm::event::KeyEvent) {
    let app = &mut remote.app;

    // Approval overlay takes precedence: the worker is blocked until a
    // decision arrives.
    if app.pending_approval.is_some() {
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // Deny the pending approval and cancel the turn; another
                // Ctrl+C once idle quits.
                resolve_approval(app, CoreApprovalDecision::Deny);
                request_cancel(remote);
            }
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
                resolve_approval(app, CoreApprovalDecision::Once);
            }
            KeyCode::Char('s') | KeyCode::Char('S') => {
                resolve_approval(app, CoreApprovalDecision::Session);
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                resolve_approval(app, CoreApprovalDecision::Deny);
            }
            KeyCode::Enter => {
                let decision =
                    app.pending_approval
                        .as_ref()
                        .map(|approval| match approval.selected {
                            0 => CoreApprovalDecision::Once,
                            1 => CoreApprovalDecision::Session,
                            _ => CoreApprovalDecision::Deny,
                        });
                if let Some(decision) = decision {
                    resolve_approval(app, decision);
                }
            }
            _ => {}
        }
        return;
    }

    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if app.busy {
                if app.cancel_requested {
                    // Second Ctrl+C while a cancel is already in flight: the
                    // daemon is stuck, force-quit rather than stay trapped.
                    app.quit = true;
                } else {
                    request_cancel(remote);
                }
            } else {
                app.quit = true;
            }
        }
        KeyCode::Esc if app.busy => {
            request_cancel(remote);
        }
        _ if !app.busy && !slash_suggestions(app).is_empty() => match key.code {
            KeyCode::Up => {
                app.slash_selected = app.slash_selected.saturating_sub(1);
            }
            KeyCode::Down => {
                let last = slash_suggestions(app).len().saturating_sub(1);
                app.slash_selected = (app.slash_selected + 1).min(last);
            }
            KeyCode::Tab => {
                complete_slash(app);
            }
            KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                complete_slash(app);
            }
            _ => app.input.handle_key(key),
        },
        KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
            submit_prompt(remote);
        }
        KeyCode::PageUp => {
            scroll_transcript(app, -20);
        }
        KeyCode::PageDown => {
            scroll_transcript(app, 20);
        }
        KeyCode::Up => {
            if app.busy || key.modifiers.contains(KeyModifiers::SHIFT) {
                scroll_transcript(app, -1);
            } else if app.history_index.is_some()
                || app.input.lines.len() <= 1
                || app.input.row == 0
            {
                app.history_up();
            } else {
                app.input.handle_key(key);
            }
        }
        KeyCode::Down => {
            if app.busy || key.modifiers.contains(KeyModifiers::SHIFT) {
                scroll_transcript(app, 1);
            } else if app.history_index.is_some()
                || app.input.lines.len() <= 1
                || app.input.row + 1 >= app.input.lines.len()
            {
                app.history_down();
            } else {
                app.input.handle_key(key);
            }
        }
        _ => {
            app.input.handle_key(key);
        }
    }
}

fn request_cancel(remote: &mut RemoteApp) {
    let app = &mut remote.app;
    if !app.busy {
        return;
    }
    app.cancel_requested = true;
    remote.cancel_flag.store(true, Ordering::SeqCst);
    // If an approval is blocking the turn, deny it first so the agent thread
    // can unwind.
    if app.pending_approval.take().is_some() {
        let _ = remote.decision_tx.send(CoreApprovalDecision::Deny);
    }
    match remote.client.cancel(&remote.session_id) {
        Ok(()) => push_info(app, "cancelling...".to_string()),
        Err(e) => push_info(app, format!("cancel failed: {e}")),
    }
}

fn submit_prompt(remote: &mut RemoteApp) {
    if remote.app.busy {
        return;
    }
    let line = remote.app.input.text().trim().to_string();
    if line.is_empty() {
        return;
    }

    if line.starts_with('/') && !line.contains('\n') {
        remote.app.history_push(line.clone());
        remote.app.input.reset();
        if handle_remote_slash(remote, &line) {
            remote.app.quit = true;
        }
        return;
    }

    remote.app.history_push(line.clone());
    remote.app.input.reset();

    // Render the user prompt with the shared transcript grid.
    render_user_prompt(&mut remote.app, &line);

    let app = &mut remote.app;
    app.busy = true;
    app.cancel_requested = false;
    app.active_tool = None;
    app.last_activity = None;
    app.turn_started = Some(std::time::Instant::now());
    remote.cancel_flag.store(false, Ordering::SeqCst);

    // Spawn the worker: it consumes the daemon's SSE stream and forwards
    // events into the UI loop. Approval requests park the worker until the
    // overlay resolves them via the decision channel.
    let client = remote.client.clone();
    let session_id = remote.session_id.clone();
    let options = remote.options.clone();
    let prompt = line;
    let event_tx = remote.worker_tx.clone();
    let decision_rx = remote.take_decision_receiver();
    let cancel_flag = remote.cancel_flag.clone();

    std::thread::spawn(move || {
        let result = client.chat(&session_id, &prompt, options, &mut |event| {
            let is_approval = matches!(event, StreamEvent::ApprovalRequired { .. });
            let _ = event_tx.send(WorkerMessage::Stream(event));
            if is_approval {
                // After a cancel request, deny automatically so the turn can
                // unwind without user interaction.
                if cancel_flag.load(Ordering::SeqCst) {
                    return Some(ProtocolApprovalDecision::Deny);
                }
                // Block until the user answers the overlay; the decision is
                // POSTed back to the daemon.
                match decision_rx.recv() {
                    Ok(CoreApprovalDecision::Once) => Some(ProtocolApprovalDecision::AllowOnce),
                    Ok(CoreApprovalDecision::Session) => {
                        Some(ProtocolApprovalDecision::AllowSession)
                    }
                    Ok(CoreApprovalDecision::Deny) | Err(_) => Some(ProtocolApprovalDecision::Deny),
                }
            } else {
                None
            }
        });
        let _ = event_tx.send(WorkerMessage::Finished(result.err().map(|e| e.to_string())));
    });
}

impl RemoteApp {
    /// Swap in a fresh decision channel per turn; the worker for this turn
    /// owns the old receiver.
    fn take_decision_receiver(&mut self) -> mpsc::Receiver<CoreApprovalDecision> {
        let (tx, rx) = mpsc::channel();
        self.decision_tx = tx;
        rx
    }
}

/// Slash commands for remote mode. Locally-answered commands are handled
/// here; everything else defers to the shared `slash` module. Returns true
/// when the app should quit.
fn handle_remote_slash(remote: &mut RemoteApp, line: &str) -> bool {
    let app = &mut remote.app;
    match line {
        "/quit" => return true,
        "/clear" | "/new" => {
            // History lives on the daemon: start a fresh session so the next
            // turn begins with an empty conversation.
            match remote.client.create_session(&app.cwd, None) {
                Ok(session) => {
                    remote.session_id = session.session_id;
                    push_info(app, "new session started.".to_string());
                }
                Err(e) => push_info(app, format!("could not start new session: {e}")),
            }
        }
        "/session" => {
            push_info(app, format!("session: {} (on daemon)", remote.session_id));
        }
        "/help" => {
            push_info(
                app,
                "commands: /quit /clear /new /session /permissions /model [<m>]".to_string(),
            );
            push_info(
                app,
                "keys: Enter send · Shift+Enter newline · ↑↓ history · PgUp/PgDn/wheel scroll"
                    .to_string(),
            );
            push_info(
                app,
                "mouse: drag to select text and copy · wheel scrolls".to_string(),
            );
            push_info(
                app,
                "while working: Esc/Ctrl+C cancels the turn".to_string(),
            );
        }
        l if l.starts_with("/provider ")
            || l.starts_with("/resume")
            || l.starts_with("/name ")
            || l.starts_with("/skill:") =>
        {
            // Provider/session management runs on the daemon host; the
            // in-memory client session cannot represent it.
            push_info(
                app,
                "this command is managed on the daemon host; not supported from a remote client yet"
                    .to_string(),
            );
        }
        _ => {
            let had_model = app.config.model.clone();
            let had_permission = app.config.permission;
            let quit = handle_slash(app, line);
            // Forward mutations made by handle_slash (model/permission)
            // so future turns use the same overrides. Base URL and skill dirs
            // are daemon-owned and not forwarded.
            if app.config.model != had_model {
                remote.options.model = Some(app.config.model.clone());
            }
            if app.config.permission != had_permission {
                remote.options.permission = Some(match app.config.permission {
                    PermissionMode::ReadOnly => "read-only".to_string(),
                    PermissionMode::AskWrites => "ask-writes".to_string(),
                    PermissionMode::AskShell => "ask-shell".to_string(),
                    PermissionMode::Trusted => "trusted".to_string(),
                });
            }
            return quit;
        }
    }
    false
}
