use std::convert::Infallible;
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::Stream;
use serde_json::json;
use tokio::sync::mpsc;

use crate::agent::r#loop::process_turn;
use crate::agent::state::ToolState;
use crate::core::console::{Console, CancellationToken};
use crate::core::format::git_context;
use crate::core::types::{ApprovalDecision, ApprovalRequest, ChatMessage, SinkLine};
use crate::llm::config::LlmConfig;
use crate::llm::prompt::system_prompt;
use crate::protocol::{
    ApprovalResponse, ChatRequest, CreateSessionRequest, DaemonInfo, StreamEvent,
};
use crate::session::{self, Session};
use crate::skills::{discover_skills, skill_dirs};

use super::{DaemonState, PendingApproval, SessionEntry};

pub(crate) fn router(state: Arc<DaemonState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/config", get(get_config))
        .route("/api/sessions", post(create_session).get(list_sessions))
        .route("/api/sessions/{id}/chat", post(chat))
        .route("/api/sessions/{id}/approve", post(approve))
        .route("/api/sessions/{id}/cancel", post(cancel))
        .with_state(state)
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

/// Best-effort runtime info so remote clients can render the same status
/// footer as the local TUI. Resolved from the daemon's own environment.
fn resolve_daemon_info() -> DaemonInfo {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (git_branch, git_dirty) = git_context(&cwd);
    match LlmConfig::from_env(None, None, None) {
        Ok(config) => DaemonInfo {
            provider: config.provider.name().to_string(),
            model: config.model.clone(),
            available_models: config.available_models.clone(),
            context_window: config.context_window,
            permission: match config.permission {
                crate::core::types::PermissionMode::ReadOnly => "read-only".into(),
                crate::core::types::PermissionMode::AskWrites => "ask-writes".into(),
                crate::core::types::PermissionMode::AskShell => "ask-shell".into(),
                crate::core::types::PermissionMode::Trusted => "trusted".into(),
            },
            cwd,
            git_branch,
            git_dirty,
        },
        Err(_) => {
            // Config is incomplete (e.g. no API key yet); report what we can
            // so the client still renders.
            let file = crate::llm::config::load_file_config().ok();
            let permission = file
                .as_ref()
                .and_then(|f| crate::llm::config::permission_from_env_or_file(f).ok())
                .map(|mode| match mode {
                    crate::core::types::PermissionMode::ReadOnly => "read-only".to_string(),
                    crate::core::types::PermissionMode::AskWrites => "ask-writes".to_string(),
                    crate::core::types::PermissionMode::AskShell => "ask-shell".to_string(),
                    crate::core::types::PermissionMode::Trusted => "trusted".to_string(),
                })
                .unwrap_or_else(|| "ask-writes".to_string());
            let model = std::env::var("OPENAI_MODEL")
                .ok()
                .or_else(|| file.as_ref().and_then(|f| f.model.clone()))
                .unwrap_or_else(|| "unknown".to_string());
            let provider_name = std::env::var("OYE_PROVIDER")
                .ok()
                .or_else(|| file.as_ref().and_then(|f| f.provider.clone()))
                .unwrap_or_else(|| "opencode".to_string());
            DaemonInfo {
                provider: provider_name,
                model,
                available_models: Vec::new(),
                context_window: 128_000,
                permission,
                cwd,
                git_branch,
                git_dirty,
            }
        }
    }
}

async fn get_config() -> Json<DaemonInfo> {
    // `LlmConfig::from_env` builds a blocking reqwest client, which must not
    // be created or dropped on a runtime worker.
    let info = tokio::task::spawn_blocking(resolve_daemon_info)
        .await
        .unwrap_or(DaemonInfo {
            provider: "opencode".into(),
            model: "unknown".into(),
            available_models: Vec::new(),
            context_window: 128_000,
            permission: "ask-writes".into(),
            cwd: String::new(),
            git_branch: None,
            git_dirty: false,
        });
    Json(info)
}

async fn create_session(
    State(state): State<Arc<DaemonState>>,
    Json(req): Json<CreateSessionRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // Tools run in the daemon's working directory (the server owns the
    // workspace), so sessions are recorded against it. A co-located client's
    // cwd matches anyway; a remote client's cwd is not meaningful on the
    // server and would be misleading in session listings.
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| req.cwd.clone());
    let session = Session::new(cwd.clone(), req.name.clone())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let session_id = session.id().to_string();
    let path = session
        .path()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    let entry = SessionEntry {
        path: path.clone().into(),
        name: req.name,
        cwd,
    };
    state
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(session_id.clone(), entry);

    Ok(Json(json!({
        "session_id": session_id,
        "path": path,
    })))
}

async fn list_sessions(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    let sessions: Vec<serde_json::Value> = state
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(id, entry)| {
            json!({
                "session_id": id,
                "path": entry.path.to_string_lossy(),
                "name": entry.name,
                "cwd": entry.cwd,
            })
        })
        .collect();

    Json(json!({ "sessions": sessions }))
}

async fn chat(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
    Json(req): Json<ChatRequest>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    // Reject concurrent turns on the same session up front so the
    // append-only session log stays consistent.
    {
        let sessions = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if !sessions.contains_key(&session_id) {
            return Err(StatusCode::NOT_FOUND);
        }
    }
    {
        let mut active = state.active_turns.lock().unwrap_or_else(|e| e.into_inner());
        if active.contains(&session_id) {
            return Err(StatusCode::CONFLICT);
        }
        active.insert(session_id.clone());
    }

    // Register a fresh per-turn cancellation token before spawning so a
    // /cancel arriving during turn setup is still observed. The agent loop
    // and the stream reader poll it; it never leaks across sessions or
    // later turns.
    let cancel = CancellationToken::new();
    state
        .cancel_tokens
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(session_id.clone(), cancel.clone());

    let (tx, mut rx) = mpsc::channel::<StreamEvent>(256);

    // Run the whole (blocking) agent turn on the blocking pool. Everything
    // below — session IO, config building, the LLM call and tool execution —
    // is synchronous, so it must never run on a runtime worker.
    let state_for_turn = state.clone();
    let sid = session_id.clone();
    tokio::task::spawn_blocking(move || {
        run_agent_turn(state_for_turn, sid, req, cancel, tx);
    });

    // Convert the receiver into an SSE stream. Each event is serialized
    // exactly once: axum adds the `data:` prefix, so hand it raw JSON.
    let event_stream = async_stream::stream! {
        while let Some(event) = rx.recv().await {
            let data = serde_json::to_string(&event).unwrap_or_default();
            yield Ok(Event::default().data(data));
        }
    };

    Ok(Sse::new(event_stream).keep_alive(
        axum::response::sse::KeepAlive::default()
            .interval(Duration::from_secs(15))
            .text("ping"),
    ))
}

/// Run one agent turn and push `StreamEvent`s into `tx`. Fully blocking;
/// called from `spawn_blocking` only.
fn run_agent_turn(
    state: Arc<DaemonState>,
    session_id: String,
    req: ChatRequest,
    cancel: CancellationToken,
    tx: mpsc::Sender<StreamEvent>,
) {
    let result = run_turn_inner(&state, &session_id, &req, &cancel, &tx);

    // Resolve any approvals still pending for this session (the turn is over;
    // nothing can consume them anymore).
    {
        let mut pending = state
            .pending_approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        pending.retain(|_, p| {
            if p.session_id == session_id {
                let _ = p.response.send(ApprovalDecision::Deny);
                false
            } else {
                true
            }
        });
    }
    state
        .active_turns
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&session_id);
    state
        .cancel_tokens
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&session_id);

    match result {
        Ok((response, usage)) => {
            // Stream closed (client disconnected): stop trying to deliver.
            let _ = tx.blocking_send(StreamEvent::TurnComplete { response, usage });
        }
        Err(error) => {
            let _ = tx.blocking_send(StreamEvent::TurnFailed { error });
        }
    }
}

fn run_turn_inner(
    state: &Arc<DaemonState>,
    session_id: &str,
    req: &ChatRequest,
    cancel: &CancellationToken,
    tx: &mpsc::Sender<StreamEvent>,
) -> Result<(String, Option<u64>), String> {
    let entry = {
        let sessions = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.get(session_id).cloned()
    }
    .ok_or_else(|| "session not found".to_string())?;

    // Resume the session created via POST /api/sessions; fall back to a fresh
    // one if the file vanished.
    let mut session = if entry.path.exists() {
        Session::from_path(&entry.path).map_err(|e| format!("failed to load session: {e}"))?
    } else {
        Session::new(entry.cwd.clone(), entry.name.clone())
            .map_err(|e| format!("failed to create session: {e}"))?
    };

    // Build the config from the daemon's own environment/config file, with
    // optional per-request overrides sent by the client.
    let config = LlmConfig::from_env(
        req.base_url.clone().filter(|v| !v.is_empty()),
        req.model.clone().filter(|v| !v.is_empty()),
        req.permission
            .as_deref()
            .map(crate::core::types::PermissionMode::parse)
            .transpose()?,
    )
    .map_err(|e| format!("failed to build config: {e}"))?;

    // Skills are resolved on the daemon (its filesystem is the workspace).
    let mut dirs = skill_dirs();
    dirs.extend(req.skill_dirs.iter().map(std::path::PathBuf::from));
    let skills = discover_skills(&dirs);

    // Rebuild the conversation: system prompt + persisted history + prompt.
    let mut messages: Vec<ChatMessage> = Vec::new();
    messages.push(ChatMessage {
        role: "system".into(),
        content: Some(system_prompt(&skills)),
        tool_calls: None,
        tool_call_id: None,
        name: None,
    });
    if let Some(path) = session.path() {
        messages.extend(session::load_messages_from_session(path).unwrap_or_default());
    }
    let user_message = ChatMessage {
        role: "user".into(),
        content: Some(req.prompt.clone()),
        tool_calls: None,
        tool_call_id: None,
        name: None,
    };
    let _ = session.turn_event("turn_start");
    let _ = session.append_message(user_message.clone());
    messages.push(user_message);

    // The agent loop reports through std channels; bridge them onto the
    // tokio sender with dedicated threads.
    let (sink_tx, sink_rx) = std_mpsc::channel::<SinkLine>();
    let (approval_tx, approval_rx) = std_mpsc::channel::<ApprovalRequest>();
    let console = Console::daemon(sink_tx, approval_tx);

    // Sink bridge: SinkLines arrive from the streaming LLM reader and tool
    // executor; forward them as StreamEvents on a dedicated thread.
    {
        let stream_tx = tx.clone();
        std::thread::spawn(move || {
            while let Ok(sl) = sink_rx.recv() {
                let event = match sl {
                    SinkLine::Assistant(text) => StreamEvent::AssistantText(text),
                    SinkLine::ToolInput(preview) => {
                        let mut parts = preview.splitn(2, ' ');
                        let name = parts.next().unwrap_or_default().to_string();
                        let args = parts.next().unwrap_or_default().to_string();
                        StreamEvent::ToolCall {
                            name,
                            args: serde_json::Value::String(args),
                        }
                    }
                    SinkLine::ToolOutput { name, summary } => {
                        let success = !summary.starts_with("failed");
                        StreamEvent::ToolResult {
                            name,
                            summary,
                            success,
                        }
                    }
                    SinkLine::System(text) => StreamEvent::System(text),
                    SinkLine::Error(text) => StreamEvent::Error(text),
                };
                let _ = stream_tx.blocking_send(event);
            }
        });
    }

    // Approval bridge: each ApprovalRequest gets a fresh request_id; the
    // response sender is parked in the shared state so POST /approve can
    // resolve it. The agent thread blocks on that sender until then.
    {
        let state = state.clone();
        let session_id = session_id.to_string();
        let stream_tx = tx.clone();
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            while let Ok(request) = approval_rx.recv() {
                // A cancellation was requested: don't surface new approvals,
                // deny them so the agent thread can unwind.
                if cancel.is_cancelled() {
                    let _ = request.response.send(ApprovalDecision::Deny);
                    continue;
                }
                let request_id = uuid::Uuid::new_v4().to_string();
                let parked = PendingApproval {
                    session_id: session_id.clone(),
                    response: request.response,
                };
                let replaced = state
                    .pending_approvals
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(request_id.clone(), parked);
                if let Some(stale) = replaced {
                    // Should not happen (request_ids are unique); deny to
                    // avoid a deadlock in a stray agent thread.
                    let _ = stale.response.send(ApprovalDecision::Deny);
                }
                let _ = stream_tx.blocking_send(StreamEvent::ApprovalRequired {
                    request_id,
                    name: request.name,
                    input: request.input,
                });
            }
        });
    }

    let mut tool_state = ToolState::load();
    let turn_result = process_turn(
        &config,
        &mut messages,
        &mut tool_state,
        None,
        None,
        Some(&mut session),
        &config,
        cancel,
        &console,
    );
    let usage = tool_state.last_usage;
    turn_result
        .map_err(|e| e.to_string())
        .map(|response| (response, usage))
}

async fn approve(
    State(state): State<Arc<DaemonState>>,
    Path(_session_id): Path<String>,
    Json(req): Json<ApprovalResponse>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sender = state
        .pending_approvals
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&req.request_id);

    match sender {
        Some(pending) => {
            let decision = match req.decision {
                crate::protocol::ApprovalDecision::AllowOnce => ApprovalDecision::Once,
                crate::protocol::ApprovalDecision::AllowSession => ApprovalDecision::Session,
                crate::protocol::ApprovalDecision::Deny => ApprovalDecision::Deny,
            };
            let _ = pending.response.send(decision);
            Ok(Json(json!({ "status": "ok" })))
        }
        None => Err(StatusCode::NOT_FOUND),
    }
}

async fn cancel(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
) -> Json<serde_json::Value> {
    // Ask the in-flight turn to unwind: the LLM stream reader and the agent
    // loop poll this token between steps. A missing entry means no turn is
    // running for the session, so there is nothing to cancel.
    if let Some(token) = state
        .cancel_tokens
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&session_id)
    {
        token.cancel();
    }

    // Deny any approvals still pending for this session so agent threads
    // blocked on them wake up promptly.
    {
        let mut pending = state
            .pending_approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let stale: Vec<String> = pending
            .iter()
            .filter(|(_, p)| p.session_id == session_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            if let Some(p) = pending.remove(&id) {
                let _ = p.response.send(ApprovalDecision::Deny);
            }
        }
    }

    Json(json!({ "status": "ok" }))
}
