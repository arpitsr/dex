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
use crate::core::console::{CancellationToken, Console, TraceWriter};
use crate::core::format::git_context;
use crate::core::types::{ApprovalDecision, ApprovalRequest, ChatMessage, SinkLine};
use crate::llm::config::LlmConfig;
use crate::llm::prompt::system_prompt;
use crate::protocol::{
    ApprovalResponse, ChatRequest, CreateSessionRequest, DaemonInfo, EventsResponse,
    LoadSkillRequest, ReattachResponse, SkillInfo, StreamEnvelope, StreamEvent,
};
use crate::session::{self, Session};
use crate::skills::{discover_skills, skill_dirs};

use super::{DaemonState, PendingApproval, SessionEntry};

pub(crate) fn router(state: Arc<DaemonState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/config", get(get_config))
        .route("/api/skills", get(list_skills))
        .route("/api/sessions", post(create_session).get(list_sessions))
        .route("/api/sessions/{id}/chat", post(chat))
        .route("/api/sessions/{id}/approve", post(approve))
        .route("/api/sessions/{id}/cancel", post(cancel))
        .route("/api/sessions/{id}/skill", post(load_skill))
        // P10: versioned reattach/replay, P9: trace, P8: undo.
        .route("/api/sessions/{id}/events", get(session_events))
        .route("/api/sessions/{id}/reattach", post(reattach))
        .route("/api/sessions/{id}/trace", get(session_trace))
        .route("/api/sessions/{id}/undo", post(session_undo))
        .route("/api/sessions/{id}/waive", post(session_waive))
        .route("/api/sessions/{id}/name", post(session_name))
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
            let provider_name = std::env::var("DEX_PROVIDER")
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

async fn list_skills() -> Json<serde_json::Value> {
    let skills = tokio::task::spawn_blocking(|| {
        let dirs = skill_dirs();
        discover_skills(&dirs)
    })
    .await
    .unwrap_or_default();
    let infos: Vec<SkillInfo> = skills
        .into_iter()
        .map(|s| SkillInfo {
            name: s.name,
            description: s.description,
        })
        .collect();
    Json(json!({ "skills": infos }))
}

async fn load_skill(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
    Json(req): Json<LoadSkillRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if req.name.is_empty()
        || !req
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let entry = {
        let sessions = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.get(&session_id).cloned()
    }
    .ok_or(StatusCode::NOT_FOUND)?;
    let skill_name = req.name.clone();
    let extra_dirs = req.skill_dirs.clone();
    let (skill, content) = tokio::task::spawn_blocking(move || {
        let mut dirs = skill_dirs();
        dirs.extend(extra_dirs.iter().map(std::path::PathBuf::from));
        let skills = discover_skills(&dirs);
        let skill = skills.into_iter().find(|s| s.name == skill_name)?;
        let content = std::fs::read_to_string(&skill.path).ok()?;
        Some((skill, content))
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .ok_or(StatusCode::NOT_FOUND)?;
    let skill_for_msg = skill.clone();
    let content_for_msg = content.clone();
    tokio::task::spawn_blocking(move || {
        let mut session = if entry.path.exists() {
            Session::from_path(&entry.path).map_err(|e| format!("load session: {e}"))?
        } else {
            Session::new(entry.cwd.clone(), entry.name.clone())
                .map_err(|e| format!("create session: {e}"))?
        };
        let msg = ChatMessage {
            role: "user".to_string(),
            content: Some(format!(
                "--- Skill: {} ---\n{}",
                skill_for_msg.name, content_for_msg
            )),
            tool_calls: None,
            tool_call_id: None,
            name: Some("skill".to_string()),
        };
        session
            .append_message(msg)
            .map_err(|e| format!("append: {e}"))?;
        Ok::<(), String>(())
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json!({
        "name": skill.name,
        "description": skill.description,
        "content": content,
    })))
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
    // P10: disk-backed listing. Sessions created before a restart live on in
    // the JSONL, so the list is rebuilt from the session directory (the
    // in-memory `sessions` map is seeded the same way at startup).
    //
    // Merge disk entries over in-memory so a session created in this process
    // (whose file already exists) is listed once.
    let mut by_id: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::new();
    for (path, header) in session::Session::list_all().unwrap_or_default() {
        let name = header.name().map(|n| n.to_string());
        let message_count = crate::session::load_messages_from_session(&path)
            .map(|m| m.len())
            .unwrap_or(0);
        let turn_state = session::Session::last_turn_state(&path);
        by_id.insert(
            header.id().to_string(),
            json!({
                "session_id": header.id(),
                "path": path.display().to_string(),
                "name": name,
                "cwd": header.cwd(),
                "created_at": header.timestamp(),
                "message_count": message_count,
                "turn_state": turn_state,
            }),
        );
    }
    // Preserve in-memory sessions that have no file yet (shouldn't happen).
    {
        let sessions = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
        for (id, entry) in sessions.iter() {
            by_id.entry(id.clone()).or_insert_with(|| {
                json!({
                    "session_id": id,
                    "path": entry.path.to_string_lossy(),
                    "name": entry.name,
                    "cwd": entry.cwd,
                    "created_at": "",
                    "message_count": 0,
                    "turn_state": "unknown",
                })
            });
        }
    }
    let mut sessions: Vec<serde_json::Value> = by_id.into_values().collect();
    sessions.sort_by(|a, b| {
        let a = a
            .get("created_at")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let b = b
            .get("created_at")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        b.cmp(a)
    });
    Json(json!({ "sessions": sessions }))
}

async fn chat(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
    headers: axum::http::HeaderMap,
    Json(req): Json<ChatRequest>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    // P10 versioned protocol: a client MAY declare its protocol version; a
    // newer-than-supported version is rejected. Absence stays backward-compatible.
    if let Some(protocol) = headers.get("x-dex-protocol").and_then(|v| v.to_str().ok()) {
        if protocol != "1" {
            return Err(StatusCode::BAD_REQUEST);
        }
    }
    // P10 idempotency: the same `Idempotency-Key` within 60s replays the
    // recorded terminal event instead of re-running effects.
    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .filter(|k| !k.is_empty())
        .map(ToOwned::to_owned);
    let request_hash = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        serde_json::to_string(&req).unwrap_or_default().hash(&mut h);
        h.finish()
    };
    // Idempotency replay is routed through the SAME channel/stream as a live
    // turn (single return type below): the recorded terminal envelope is just
    // pushed and the stream closes.
    let mut replay_envelope: Option<StreamEnvelope> = None;
    if let Some(key) = &idempotency_key {
        if let Some(terminal) = state.idempotent_replay(key, &session_id, request_hash) {
            replay_envelope = serde_json::from_str::<StreamEnvelope>(&terminal).ok();
        }
    }
    // Reject concurrent turns on the same session up front so the
    // append-only session log stays consistent. A replay must not hold the
    // active-turn slot, so it is checked before registration.
    {
        let sessions = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if !sessions.contains_key(&session_id) {
            return Err(StatusCode::NOT_FOUND);
        }
    }
    if replay_envelope.is_none() {
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
    }

    let (tx, mut rx) = mpsc::channel::<StreamEnvelope>(256);

    if let Some(env) = replay_envelope {
        // Replay: emit the recorded terminal envelope, then close.
        let tx = tx.clone();
        std::thread::spawn(move || {
            let _ = tx.blocking_send(env);
        });
    } else {
        // Run the whole (blocking) agent turn on the blocking pool. Everything
        // below — session IO, config building, the LLM call and tool
        // execution — is synchronous, so it must never run on a runtime worker.
        let state_for_turn = state.clone();
        let sid = session_id.clone();
        let idem_key = idempotency_key;
        let cancel = CancellationToken::new();
        tokio::task::spawn_blocking(move || {
            run_agent_turn(state_for_turn, sid, req, cancel, tx, idem_key, request_hash);
        });
    }

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

/// Run one agent turn and push numbered `StreamEnvelope`s into `tx`. Fully
/// blocking; called from `spawn_blocking` only.
fn run_agent_turn(
    state: Arc<DaemonState>,
    session_id: String,
    req: ChatRequest,
    cancel: CancellationToken,
    tx: mpsc::Sender<StreamEnvelope>,
    idempotency_key: Option<String>,
    request_hash: u64,
) {
    // Use a guard so active_turns/cancel_tokens/pending approvals are cleaned
    // even when run_turn_inner panics inside spawn_blocking.
    struct TurnGuard {
        state: Arc<DaemonState>,
        session_id: String,
    }
    impl Drop for TurnGuard {
        fn drop(&mut self) {
            {
                let mut pending = self
                    .state
                    .pending_approvals
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                pending.retain(|_, p| {
                    if p.session_id == self.session_id {
                        let _ = p.response.send(ApprovalDecision::Deny);
                        false
                    } else {
                        true
                    }
                });
            }
            self.state
                .active_turns
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&self.session_id);
            self.state
                .cancel_tokens
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&self.session_id);
        }
    }
    let _guard = TurnGuard {
        state: state.clone(),
        session_id: session_id.clone(),
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_turn_inner(&state, &session_id, &req, &cancel, &tx)
    }));
    // Drop the guard now before sending the terminal event so a new turn can
    // be accepted promptly; drop ordering handles pending approvals/active turns.
    drop(_guard);

    let terminal = match result {
        Ok(Ok((response, usage, cached))) => StreamEvent::TurnComplete {
            response,
            usage,
            cached,
        },
        Ok(Err(error)) => StreamEvent::TurnFailed { error },
        Err(_) => StreamEvent::TurnFailed {
            error: "turn panicked".to_string(),
        },
    };
    // P10/P8: the terminal event gets a seq, is journaled (reopening the
    // session file so an in-flight handle is untouched), dedup'd via
    // Idempotency-Key, and only then emitted.
    let seq = state.next_seq(&session_id);
    let env = StreamEnvelope {
        seq,
        event: terminal,
    };
    let serialized = serde_json::to_string(&env).unwrap_or_default();
    if let Some(path) = state
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&session_id)
        .map(|e| e.path.clone())
    {
        if let Ok(mut journal) = Session::from_path(&path) {
            let _ =
                journal.append_event(seq, &serde_json::to_string(&env.event).unwrap_or_default());
            // Durable turn_failed marker for runs that did not finish normally.
            if matches!(&env.event, StreamEvent::TurnFailed { .. })
                && crate::session::Session::last_turn_state(&path) == "interrupted"
            {
                let _ = journal.turn_event("turn_failed");
            }
        }
    }
    if let Some(key) = idempotency_key {
        state.idempotency_record(&key, &session_id, request_hash, serialized);
    }
    let _ = tx.blocking_send(env);
}

fn run_turn_inner(
    state: &Arc<DaemonState>,
    session_id: &str,
    req: &ChatRequest,
    cancel: &CancellationToken,
    tx: &mpsc::Sender<StreamEnvelope>,
) -> Result<(String, Option<u64>, Option<u64>), String> {
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

    // Persist plan forwarded by the client (remote TUI slash commands). Empty
    // string clears. Invalid JSON is rejected explicitly rather than silently
    // storing garbage (which would come back as an empty plan on reload).
    if let Some(plan_json) = &req.plan {
        if plan_json.is_empty() {
            session
                .set_state("plan", &crate::core::types::Plan::default().to_json())
                .map_err(|e| format!("failed to persist plan: {e}"))?;
        } else {
            let plan: crate::core::types::Plan = serde_json::from_str(plan_json)
                .map_err(|e| format!("invalid plan JSON from client: {e}"))?;
            session
                .set_state("plan", &plan.to_json())
                .map_err(|e| format!("failed to persist plan: {e}"))?;
        }
    }

    // Permission ceiling: daemon policy (file/env) is max; client may only go stricter.
    let daemon_perm = crate::llm::config::load_file_config()
        .ok()
        .and_then(|f| crate::llm::config::permission_from_env_or_file(&f).ok())
        .unwrap_or(crate::core::types::PermissionMode::AskWrites);
    if let Some(req_perm_str) = &req.permission {
        let req_perm = crate::core::types::PermissionMode::parse(req_perm_str)?;
        if req_perm.permissiveness() > daemon_perm.permissiveness() {
            return Err(format!(
                "permission escalation denied: daemon ceiling is {:?} (client requested {:?}); use a stricter mode or change daemon config",
                daemon_perm, req_perm
            ));
        }
    }
    // Build the config from the daemon's own environment/config file, with
    // optional per-request overrides sent by the client (now validated).
    let mut config = LlmConfig::from_env(
        req.base_url.clone().filter(|v| !v.is_empty()),
        req.model.clone().filter(|v| !v.is_empty()),
        req.permission
            .as_deref()
            .map(crate::core::types::PermissionMode::parse)
            .transpose()?,
    )
    .map_err(|e| format!("failed to build config: {e}"))?;
    // P9: auto-detect the verification command at the daemon boundary (its
    // filesystem is the workspace); the agent loop consumes config only.
    if config.verify_command.is_none() {
        config.verify_command = crate::llm::config::detect_verify_command();
    }

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
    // Durable journal (P8): a turn only exists once turn_start is recorded,
    // and an io::Error here fails the turn instead of being swallowed.
    session
        .turn_event("turn_start")
        .map_err(|e| format!("failed to record turn_start: {e}"))?;
    session
        .append_message(user_message.clone())
        .map_err(|e| format!("failed to persist prompt: {e}"))?;
    messages.push(user_message);

    // Per-turn redacted trace journal (P9): `<session>.trace.jsonl`, 0600.
    let trace = session
        .path()
        .map(|p| p.with_extension("trace.jsonl"))
        .and_then(|p| TraceWriter::open(p).ok());

    // The agent loop reports through std channels; bridge them onto the
    // tokio sender with dedicated threads.
    let (sink_tx, sink_rx) = std_mpsc::channel::<SinkLine>();
    let (approval_tx, approval_rx) = std_mpsc::channel::<ApprovalRequest>();
    let console = Console::daemon(sink_tx, approval_tx).with_trace(trace);

    // Sink bridge: SinkLines arrive from the streaming LLM reader and tool
    // executor; forward them as numbered StreamEnvelopes on a dedicated
    // thread, journaling each one for replay (P10).
    {
        let stream_tx = tx.clone();
        let state = state.clone();
        let session_path = session.path().map(|p| p.to_path_buf());
        let sid = session_id.to_string();
        std::thread::spawn(move || {
            // Reopen so journal writes never fight the agent loop's handle;
            // events land in the separate `<id>.events.jsonl` file.
            let mut journal = session_path
                .as_deref()
                .and_then(|p| Session::from_path(p).ok());
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
                    SinkLine::ToolOutput {
                        name,
                        summary,
                        success,
                        preview,
                        duration,
                    } => StreamEvent::ToolResult {
                        name,
                        summary,
                        success,
                        preview,
                        duration,
                    },
                    SinkLine::System(text) => StreamEvent::System(text),
                    SinkLine::Error(text) => StreamEvent::Error(text),
                    SinkLine::Usage { tokens, cached } => {
                        StreamEvent::Usage { tokens, cached }
                    }
                    SinkLine::Plan(plan) => StreamEvent::Plan {
                        goal: plan.goal,
                        steps: plan.steps,
                        constraints: plan.constraints,
                        acceptance: plan.acceptance,
                        budget: plan.budget,
                    },
                };
                let seq = state.next_seq(&sid);
                if let Some(s) = journal.as_mut() {
                    let _ = s.append_event(seq, &serde_json::to_string(&event).unwrap_or_default());
                }
                let _ = stream_tx.blocking_send(StreamEnvelope { seq, event });
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
                let name_clone = request.name.clone();
                let input_clone = request.input.clone();
                let parked = PendingApproval {
                    session_id: session_id.clone(),
                    response: request.response,
                    name: name_clone,
                    input: input_clone,
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
                let _ = stream_tx.blocking_send(StreamEnvelope {
                    seq: state.next_seq(&session_id),
                    event: StreamEvent::ApprovalRequired {
                        request_id,
                        name: request.name,
                        input: request.input,
                    },
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
    let cached = tool_state.last_cached;
    // Durable terminal marker (P8): a completed turn is recorded before the
    // event is relayed; a failed one gets `turn_failed` in run_agent_turn.
    match &turn_result {
        Ok(_) => session
            .turn_event("turn_complete")
            .map_err(|e| format!("failed to record turn_complete: {e}"))?,
        Err(_) => session
            .turn_event("turn_failed")
            .map_err(|e| format!("failed to record turn_failed: {e}"))?,
    }
    turn_result
        .map_err(|e| e.to_string())
        .map(|response| (response, usage, cached))
}

async fn approve(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
    Json(req): Json<ApprovalResponse>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let pending = state
        .pending_approvals
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&req.request_id);

    match pending {
        Some(pending) if pending.session_id == session_id => {
            let decision = match req.decision {
                crate::protocol::ApprovalDecision::AllowOnce => ApprovalDecision::Once,
                crate::protocol::ApprovalDecision::AllowSession => ApprovalDecision::Session,
                crate::protocol::ApprovalDecision::Deny => ApprovalDecision::Deny,
            };
            // Audit: best-effort, redacted input hash, actor, request_id
            {
                let Some(base) = std::env::var_os("XDG_DATA_HOME")
                    .map(std::path::PathBuf::from)
                    .or_else(|| {
                        std::env::var_os("HOME")
                            .map(|h| std::path::PathBuf::from(h).join(".local/share"))
                    })
                else {
                    let _ = pending.response.send(decision);
                    return Ok(Json(json!({ "status": "ok" })));
                };
                let path = base.join("dex/audit.jsonl");
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                std::hash::Hash::hash(&pending.input, &mut hasher);
                use std::hash::Hasher;
                let input_hash = format!("{:016x}", hasher.finish());
                let decision_str = match decision {
                    ApprovalDecision::Once => "once",
                    ApprovalDecision::Session => "session",
                    ApprovalDecision::Deny => "deny",
                };
                let record = serde_json::json!({
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "session_id": session_id,
                    "request_id": req.request_id,
                    "tool": pending.name,
                    "input_hash": input_hash,
                    "decision": decision_str,
                    "actor": "remote",
                });
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                {
                    let mut line = record.to_string();
                    line.push('\n');
                    let _ = std::io::Write::write_all(&mut file, line.as_bytes());
                }
            }
            let _ = pending.response.send(decision);
            Ok(Json(json!({ "status": "ok" })))
        }
        Some(pending) => {
            // Restore on cross-session attempt so the legitimate session can
            // still resolve it.
            state
                .pending_approvals
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(req.request_id, pending);
            Err(StatusCode::NOT_FOUND)
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

/// Resolve a session file path from the registry, or 404.
fn session_path(
    state: &Arc<DaemonState>,
    session_id: &str,
) -> Result<std::path::PathBuf, StatusCode> {
    state
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(session_id)
        .map(|e| e.path.clone())
        .filter(|p| p.exists())
        .ok_or(StatusCode::NOT_FOUND)
}

/// `GET /api/sessions/{id}/events?since=<seq>` — replay journaled stream
/// events after a cursor (P10). Missing journal file replays nothing.
async fn session_events(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<EventsResponse>, StatusCode> {
    let path = session_path(&state, &session_id)?;
    let since = params
        .get("since")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let events = tokio::task::spawn_blocking(move || {
        let mut events = Vec::new();
        for (seq, payload) in Session::load_events(&path, since).unwrap_or_default() {
            if let Ok(event) = serde_json::from_str::<StreamEvent>(&payload) {
                events.push(StreamEnvelope { seq, event });
            }
        }
        let next_seq = events
            .last()
            .map(|e| e.seq.saturating_add(1))
            .unwrap_or(since);
        (events, next_seq)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let (events, next_seq) = events;
    Ok(Json(EventsResponse { events, next_seq }))
}

/// `POST /api/sessions/{id}/reattach` — re-register a persisted session after
/// a daemon restart (or a client reconnect) and return the replay cursor.
async fn reattach(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
) -> Result<Json<ReattachResponse>, StatusCode> {
    let mut sessions = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
    let entry = sessions
        .get(&session_id)
        .cloned()
        .ok_or(StatusCode::NOT_FOUND)?;
    if !entry.path.exists() {
        return Err(StatusCode::NOT_FOUND);
    }
    state.seed_seq(&session_id, &entry.path);
    // Prune stale idempotency-recorded seq: reattach hands the client the
    // cursor to resume from.
    let seq = Session::max_event_seq(&entry.path);
    sessions.insert(
        session_id.clone(),
        SessionEntry {
            path: entry.path.clone(),
            name: entry.name,
            cwd: entry.cwd,
        },
    );
    Ok(Json(ReattachResponse {
        session_id: session_id.clone(),
        seq,
    }))
}

/// `GET /api/sessions/{id}/trace` — the per-session (redacted) trace journal
/// rows for this session, for cost/outcome queries (P9).
async fn session_trace(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let path = session_path(&state, &session_id)?;
    let trace_path = path.with_extension("trace.jsonl");
    let rows = tokio::task::spawn_blocking(move || -> Vec<serde_json::Value> {
        let Ok(text) = std::fs::read_to_string(&trace_path) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json!({ "trace": rows })))
}

/// `POST /api/sessions/{id}/undo` — revert the last recorded change (P8).
/// Refuses when the file moved on since (after_hash mismatch) or is too big.
async fn session_undo(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let path = session_path(&state, &session_id)?;
    let result = tokio::task::spawn_blocking(move || {
        let mut session = Session::from_path(&path)?;
        session::undo_last_change(&mut session)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    match result {
        Ok(message) => Ok(Json(json!({ "status": "ok", "message": message }))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(StatusCode::NOT_FOUND),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => Err(StatusCode::CONFLICT),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// `POST /api/sessions/{id}/waive` with `{"reason": ...}` — record a
/// `waived` verification disposition. A missing/empty reason is a 400 (P9:
/// waived requires a reason).
async fn session_waive(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
    Json(req): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let reason = req
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if reason.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let path = session_path(&state, &session_id)?;
    let reason = reason.to_string();
    let result = tokio::task::spawn_blocking(move || {
        let mut session = Session::from_path(&path)?;
        // A waive is a recorded, user-authored message the model sees next.
        session.append_message(ChatMessage {
            role: "user".into(),
            content: Some(format!("[verify waived] {reason}")),
            tool_calls: None,
            tool_call_id: None,
            name: Some("waive".into()),
        })?;
        session.set_state(
            "verify",
            &serde_json::json!({
                "disposition": "waived",
                "reason": reason,
                "timestamp": chrono::Utc::now().to_rfc3339(),
            })
            .to_string(),
        )
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    result.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json!({ "status": "ok" })))
}

/// `POST /api/sessions/{id}/name` with `{"name": ...}` — rename a session
/// (remote counterpart of local `/name`).
async fn session_name(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
    Json(req): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let name = req
        .get("name")
        .and_then(serde_json::Value::as_str)
        .map(str::trim);
    let Some(name) = name.filter(|n| !n.is_empty()) else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let path = session_path(&state, &session_id)?;
    let name = name.to_string();
    let result = tokio::task::spawn_blocking(move || {
        let mut session = Session::from_path(&path)?;
        session.set_name(name)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    result.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json!({ "status": "ok" })))
}
