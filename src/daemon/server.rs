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
    FollowupRequest, LoadSkillRequest, ReattachResponse, SkillInfo, SteerRequest, StreamEnvelope,
    StreamEvent,
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
        .route("/api/sessions/{id}/steer", post(steer))
        .route("/api/sessions/{id}/followup", post(followup))
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
            api: config.api.name().to_string(),
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
            let permission = std::env::var("DEX_PERMISSION")
                .ok()
                .filter(|v| crate::core::types::PermissionMode::parse(v).is_ok())
                .unwrap_or_else(|| "ask-writes".to_string());
            let model = std::env::var("OPENAI_MODEL")
                .ok()
                .unwrap_or_else(|| "unknown".to_string());
            let provider_name = std::env::var("DEX_PROVIDER")
                .ok()
                .unwrap_or_else(|| "opencode".to_string());
            DaemonInfo {
                provider: provider_name,
                model,
                api: std::env::var("OPENAI_API")
                    .ok()
                    .and_then(|name| crate::core::types::ApiProtocol::parse(&name))
                    .map(|api| api.name().to_string())
                    .unwrap_or_else(|| "openai-responses".to_string()),
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
            api: "openai-responses".into(),
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
    // Pre-create per-turn channels so `POST /steer` / `POST /followup`
    // have a target as soon as the turn is registered (avoids a race where
    // the client sends steering in the gap between `active_turns` insert and
    // the `spawn_blocking` thread creating its channels).
    let mut steering_rx_opt: Option<std_mpsc::Receiver<String>> = None;
    let mut followup_rx_opt: Option<std_mpsc::Receiver<String>> = None;
    let mut cancel_for_turn: Option<CancellationToken> = None;
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
        cancel_for_turn = Some(cancel);
        // Steering / follow-up queues for this turn (mirrors old local
        // `event.rs` channels). Insert now so the HTTP handlers can push
        // immediately.
        let (steering_tx, steering_rx) = std_mpsc::channel::<String>();
        let (followup_tx, followup_rx) = std_mpsc::channel::<String>();
        state
            .steering_txs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.clone(), steering_tx);
        state
            .followup_txs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.clone(), followup_tx);
        steering_rx_opt = Some(steering_rx);
        followup_rx_opt = Some(followup_rx);
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
        let cancel = cancel_for_turn.unwrap_or_default();
        tokio::task::spawn_blocking(move || {
            run_agent_turn(
                state_for_turn,
                sid,
                req,
                cancel,
                tx,
                idem_key,
                request_hash,
                steering_rx_opt,
                followup_rx_opt,
            );
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
#[allow(clippy::too_many_arguments)]
fn run_agent_turn(
    state: Arc<DaemonState>,
    session_id: String,
    req: ChatRequest,
    cancel: CancellationToken,
    tx: mpsc::Sender<StreamEnvelope>,
    idempotency_key: Option<String>,
    request_hash: u64,
    steering_rx: Option<std_mpsc::Receiver<String>>,
    followup_rx: Option<std_mpsc::Receiver<String>>,
) {
    // Use a guard so active_turns/cancel_tokens/pending approvals/steering
    // are cleaned even when run_turn_inner panics inside spawn_blocking.
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
            self.state
                .steering_txs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&self.session_id);
            self.state
                .followup_txs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&self.session_id);
        }
    }
    let _guard = TurnGuard {
        state: state.clone(),
        session_id: session_id.clone(),
    };
    // Steering / follow-up channels for this turn (mirrors the old in-memory
    // `event.rs` submit path). `POST /steer` and `POST /followup` push into
    // these; the agent loop consumes them between iterations / chained turns.
    // When `chat` pre-creates the queues to avoid a race, reuse them; else
    // (direct `run_agent_turn` calls, e.g. tests) create them here.
    let (steering_rx, followup_rx) = match (steering_rx, followup_rx) {
        (Some(sr), Some(fr)) => (sr, fr),
        _ => {
            let (steering_tx, sr) = std_mpsc::channel::<String>();
            let (followup_tx, fr) = std_mpsc::channel::<String>();
            state
                .steering_txs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(session_id.clone(), steering_tx);
            state
                .followup_txs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(session_id.clone(), followup_tx);
            (sr, fr)
        }
    };
    let (steering_accepted_tx, steering_accepted_rx) = std_mpsc::channel::<String>();
    let (followup_accepted_tx, followup_accepted_rx) = std_mpsc::channel::<String>();
    // Forward accepted steers/follow-ups onto the SSE stream so the remote
    // TUI can clear its `pending_*` badge and render the prompt. Journaled
    // so a reattach replay reconstructs the transcript.
    {
        let tx_clone = tx.clone();
        let state_clone = state.clone();
        let sid = session_id.clone();
        let entry_path = state
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&sid)
            .map(|e| e.path.clone());
        std::thread::spawn(move || {
            let mut journal = entry_path
                .as_deref()
                .and_then(|p| Session::from_path(p).ok());
            while let Ok(content) = steering_accepted_rx.recv() {
                let event = StreamEvent::SteeringAccepted {
                    content: content.clone(),
                };
                let seq = state_clone.next_seq(&sid);
                if let Some(j) = journal.as_mut() {
                    let _ = j.append_event(seq, &serde_json::to_string(&event).unwrap_or_default());
                }
                let _ = tx_clone.blocking_send(StreamEnvelope { seq, event });
            }
        });
    }
    {
        let tx_clone = tx.clone();
        let state_clone = state.clone();
        let sid = session_id.clone();
        let entry_path = state
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&sid)
            .map(|e| e.path.clone());
        std::thread::spawn(move || {
            let mut journal = entry_path
                .as_deref()
                .and_then(|p| Session::from_path(p).ok());
            while let Ok(content) = followup_accepted_rx.recv() {
                let event = StreamEvent::FollowupAccepted {
                    content: content.clone(),
                };
                let seq = state_clone.next_seq(&sid);
                if let Some(j) = journal.as_mut() {
                    let _ = j.append_event(seq, &serde_json::to_string(&event).unwrap_or_default());
                }
                let _ = tx_clone.blocking_send(StreamEnvelope { seq, event });
            }
        });
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_turn_inner(
            &state,
            &session_id,
            &req,
            &cancel,
            &tx,
            Some(&steering_rx),
            Some(&steering_accepted_tx),
            Some(&followup_rx),
            Some(&followup_accepted_tx),
        )
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

#[allow(clippy::too_many_arguments, unused_assignments)]
fn run_turn_inner(
    state: &Arc<DaemonState>,
    session_id: &str,
    req: &ChatRequest,
    cancel: &CancellationToken,
    tx: &mpsc::Sender<StreamEnvelope>,
    steering_rx: Option<&std_mpsc::Receiver<String>>,
    steering_accepted_tx: Option<&std_mpsc::Sender<String>>,
    followup_rx: Option<&std_mpsc::Receiver<String>>,
    followup_accepted_tx: Option<&std_mpsc::Sender<String>>,
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

    // Permission ceiling: daemon policy (env) is max; client may only go stricter.
    let daemon_perm = crate::llm::config::permission_from_env()
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
    // Build the config from the daemon's own environment, with
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
    // Verification is opt-in (DEX_VERIFY / config verify_command). No
    // auto-detect by default — pi has no verify hook and auto-running
    // `cargo test` after every edit is the biggest loop tax.
    // Set DEX_VERIFY or config verify_command, or DEX_VERIFY=1 with a manifest,
    // to re-enable: `DEX_VERIFY=1` or explicit `verify_command` in config.
    if config.verify_command.is_none() && std::env::var("DEX_VERIFY").as_deref() == Ok("1") {
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
    // Restore “allow for session” approvals that survived from prior turns
    // (previously the per-turn Console dropped them).
    if let Some(set) = state
        .session_approvals
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(session_id)
        .cloned()
    {
        console.seed_session_approvals(set);
    }
    // Fast-path daemon check before we even park the turn: if the session
    // already approved this exact scoped key, the agent loop's own
    // `session_approved` check will still succeed, but seeding avoids the
    // overlay round-trip entirely for repeated identical calls within the
    // same session. (No early return here — the loop itself short-circuits.)

    // Sink bridge: SinkLines arrive from the streaming LLM reader and tool
    // executor; forward them as numbered StreamEnvelopes on a dedicated
    // thread, journaling each one for replay (P10).
    {
        let stream_tx = tx.clone();
        let state = state.clone();
        let session_path = session.path().map(|p| p.to_path_buf());
        let sid = session_id.to_string();
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            // Reopen so journal writes never fight the agent loop's handle;
            // events land in the separate `<id>.events.jsonl` file.
            let mut journal = session_path
                .as_deref()
                .and_then(|p| Session::from_path(p).ok());
            loop {
                match sink_rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(sl) => {
                        let event = match sl {
                            SinkLine::Assistant(text) => StreamEvent::AssistantText(text),
                            SinkLine::Thinking(text) => StreamEvent::Thinking(text),
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
                            },
                        };
                        let seq = state.next_seq(&sid);
                        if let Some(s) = journal.as_mut() {
                            let _ = s.append_event(
                                seq,
                                &serde_json::to_string(&event).unwrap_or_default(),
                            );
                        }
                        let _ = stream_tx.blocking_send(StreamEnvelope { seq, event });
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if cancel.is_cancelled() {
                            break;
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
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
    #[allow(unused_assignments)]
    // Outer loop for follow-up chaining (mirrors old local `event.rs` loop):
    // `process_turn` consumes steering mid-turn; follow-ups are drained after
    // each successful turn and chained without a new HTTP request.
    let mut final_response = String::new();
    let mut final_usage = None;
    let mut final_cached = None;
    let turn_result: Result<String, Box<dyn std::error::Error>>;
    loop {
        let result = process_turn(
            &config,
            &mut messages,
            &mut tool_state,
            steering_rx,
            steering_accepted_tx,
            Some(&mut session),
            &config,
            cancel,
            &console,
        );
        match result {
            Ok(resp) => {
                final_response = resp;
                final_usage = tool_state.last_usage;
                final_cached = tool_state.last_cached;
                // Drain follow-ups queued while this turn ran.
                let followups: Vec<String> = followup_rx
                    .as_ref()
                    .map(|rx| rx.try_iter().collect())
                    .unwrap_or_default();
                if followups.is_empty() {
                    turn_result = Ok(final_response.clone());
                    break;
                }
                for content in followups {
                    if let Some(tx) = followup_accepted_tx {
                        let _ = tx.send(content.clone());
                    }
                    let msg = ChatMessage {
                        role: "user".into(),
                        content: Some(content.clone()),
                        tool_calls: None,
                        tool_call_id: None,
                        name: Some("follow-up".into()),
                    };
                    session
                        .append_message(msg.clone())
                        .map_err(|e| format!("failed to persist followup: {e}"))?;
                    messages.push(msg);
                }
                if cancel.is_cancelled() {
                    turn_result = Err("cancelled by user".into());
                    break;
                }
                // chained follow-up: loop and run another turn with the same
                // session/messages/tool_state but without new turn_start marker
                // (the followup is already persisted).
                continue;
            }
            Err(e) => {
                turn_result = Err(e);
                break;
            }
        }
    }
    let usage = final_usage;
    let cached = final_cached;
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
                crate::protocol::ApprovalDecision::AllowSession => {
                    // Persist for the whole session so next turns skip the overlay
                    state.record_session_approval(&session_id, &pending.name, &pending.input);
                    ApprovalDecision::Session
                }
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

async fn steer(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
    Json(req): Json<SteerRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let content = req.content.trim().to_string();
    if content.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    // Session must exist; steering only valid while a turn is active.
    {
        let sessions = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if !sessions.contains_key(&session_id) {
            return Err(StatusCode::NOT_FOUND);
        }
    }
    let tx = {
        let map = state.steering_txs.lock().unwrap_or_else(|e| e.into_inner());
        map.get(&session_id).cloned()
    }
    .ok_or(StatusCode::CONFLICT)?;
    tx.send(content).map_err(|_| StatusCode::CONFLICT)?;
    Ok(Json(json!({ "status": "ok" })))
}

async fn followup(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
    Json(req): Json<FollowupRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let content = req.content.trim().to_string();
    if content.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    {
        let sessions = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if !sessions.contains_key(&session_id) {
            return Err(StatusCode::NOT_FOUND);
        }
    }
    let tx = {
        let map = state.followup_txs.lock().unwrap_or_else(|e| e.into_inner());
        map.get(&session_id).cloned()
    }
    .ok_or(StatusCode::CONFLICT)?;
    tx.send(content).map_err(|_| StatusCode::CONFLICT)?;
    Ok(Json(json!({ "status": "ok" })))
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

#[cfg(test)]
mod handler_tests {
    use super::*;
    use axum::extract::{Path, Query};

    fn state_with_session(path: &std::path::Path) -> (Arc<DaemonState>, String) {
        let state = Arc::new(DaemonState::new());
        let session = Session::from_path(path).unwrap();
        let id = session.id().to_string();
        state.sessions.lock().unwrap().insert(
            id.clone(),
            SessionEntry {
                path: path.to_path_buf(),
                name: None,
                cwd: "/tmp/dex-test-cwd".into(),
            },
        );
        (state, id)
    }

    #[tokio::test]
    async fn chat_rejects_unknown_session_and_bad_protocol_version() {
        let state = Arc::new(DaemonState::new());
        let req = ChatRequest {
            prompt: "hi".into(),
            skill_dirs: vec![],
            base_url: None,
            model: None,
            permission: None,
            plan: None,
        };
        // unknown session -> 404
        let r = chat(
            State(state.clone()),
            Path("nope".into()),
            axum::http::HeaderMap::new(),
            Json(req.clone()),
        )
        .await;
        assert!(matches!(r, Err(StatusCode::NOT_FOUND)));

        // newer protocol version -> 400 (checked before session lookup)
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-dex-protocol", "2".parse().unwrap());
        let r = chat(State(state), Path("nope".into()), headers, Json(req)).await;
        assert!(matches!(r, Err(StatusCode::BAD_REQUEST)));
    }

    #[tokio::test]
    async fn steer_and_followup_validate_content_and_turn_state() {
        let state = Arc::new(DaemonState::new());
        // empty content -> 400
        let r = steer(
            State(state.clone()),
            Path("s".into()),
            Json(SteerRequest {
                content: "  ".into(),
            }),
        )
        .await;
        assert!(matches!(r, Err(StatusCode::BAD_REQUEST)));
        let r = followup(
            State(state.clone()),
            Path("s".into()),
            Json(FollowupRequest { content: "".into() }),
        )
        .await;
        assert!(matches!(r, Err(StatusCode::BAD_REQUEST)));
        // unknown session -> 404
        let r = steer(
            State(state.clone()),
            Path("s".into()),
            Json(SteerRequest {
                content: "x".into(),
            }),
        )
        .await;
        assert!(matches!(r, Err(StatusCode::NOT_FOUND)));
        // registered session without active turn -> 409 (nothing to steer)
        state.sessions.lock().unwrap().insert(
            "s".into(),
            SessionEntry {
                path: "/tmp/does-not-exist.jsonl".into(),
                name: None,
                cwd: "/tmp".into(),
            },
        );
        let r = steer(
            State(state.clone()),
            Path("s".into()),
            Json(SteerRequest {
                content: "x".into(),
            }),
        )
        .await;
        assert!(matches!(r, Err(StatusCode::CONFLICT)));
        let r = followup(
            State(state),
            Path("s".into()),
            Json(FollowupRequest {
                content: "x".into(),
            }),
        )
        .await;
        assert!(matches!(r, Err(StatusCode::CONFLICT)));
    }

    #[tokio::test]
    async fn load_skill_rejects_bad_names() {
        let state = Arc::new(DaemonState::new());
        for bad in ["", "../evil", "has space", "slash/ed"] {
            let r = load_skill(
                State(state.clone()),
                Path("s".into()),
                Json(LoadSkillRequest {
                    name: bad.into(),
                    skill_dirs: vec![],
                }),
            )
            .await;
            assert!(
                matches!(r, Err(StatusCode::BAD_REQUEST)),
                "expected 400 for {bad:?}"
            );
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // single-threaded test runtime; guard is intentional
    async fn approve_unknown_request_is_404_and_cross_session_is_restored() {
        let state = Arc::new(DaemonState::new());
        // no such request_id -> 404
        let r = approve(
            State(state.clone()),
            Path("sess-a".into()),
            Json(crate::protocol::ApprovalResponse {
                request_id: "missing".into(),
                decision: crate::protocol::ApprovalDecision::AllowOnce,
            }),
        )
        .await;
        assert!(matches!(r, Err(StatusCode::NOT_FOUND)));

        // pending parked under sess-a; decision sent against sess-b -> 404 and restored
        let (tx, rx) = std::sync::mpsc::channel();
        state.pending_approvals.lock().unwrap().insert(
            "req-1".into(),
            PendingApproval {
                session_id: "sess-a".into(),
                response: tx,
                name: "bash".into(),
                input: "{}".into(),
            },
        );
        let r = approve(
            State(state.clone()),
            Path("sess-b".into()),
            Json(crate::protocol::ApprovalResponse {
                request_id: "req-1".into(),
                decision: crate::protocol::ApprovalDecision::AllowOnce,
            }),
        )
        .await;
        assert!(matches!(r, Err(StatusCode::NOT_FOUND)));
        assert!(
            state
                .pending_approvals
                .lock()
                .unwrap()
                .contains_key("req-1"),
            "pending must be restored for the legitimate session"
        );
        assert!(
            rx.try_recv().is_err(),
            "restored approval must not be resolved"
        );

        // correct session resolves it
        let r = approve(
            State(state.clone()),
            Path("sess-a".into()),
            Json(crate::protocol::ApprovalResponse {
                request_id: "req-1".into(),
                decision: crate::protocol::ApprovalDecision::Deny,
            }),
        )
        .await;
        assert!(r.is_ok());
        assert_eq!(
            rx.try_recv().ok(),
            Some(crate::core::types::ApprovalDecision::Deny)
        );
    }

    #[tokio::test]
    async fn cancel_denies_pending_approvals_for_the_session() {
        let state = Arc::new(DaemonState::new());
        let (tx_a, rx_a) = std::sync::mpsc::channel();
        let (tx_b, rx_b) = std::sync::mpsc::channel();
        {
            let mut pending = state.pending_approvals.lock().unwrap();
            pending.insert(
                "r-a".into(),
                PendingApproval {
                    session_id: "s-a".into(),
                    response: tx_a,
                    name: "write".into(),
                    input: "{}".into(),
                },
            );
            pending.insert(
                "r-b".into(),
                PendingApproval {
                    session_id: "s-b".into(),
                    response: tx_b,
                    name: "write".into(),
                    input: "{}".into(),
                },
            );
        }

        let _ = cancel(State(state), Path("s-a".into())).await;

        assert_eq!(
            rx_a.try_recv().ok(),
            Some(crate::core::types::ApprovalDecision::Deny)
        );
        assert!(rx_b.try_recv().is_err(), "other sessions must be untouched");
    }

    #[tokio::test]
    async fn session_events_replay_after_cursor() {
        // Hand-crafted session files: fully hermetic, no env redirects.
        let dir = std::env::temp_dir().join(format!("dex-srv-events-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let id = "test-events-1";
        let path = dir.join(format!("{id}.jsonl"));
        std::fs::write(
            &path,
            r#"{"type":"session","version":1,"id":"test-events-1","timestamp":"t","cwd":"/tmp/x"}
"#,
        )
        .unwrap();
        std::fs::write(
            dir.join(format!("{id}.events.jsonl")),
            r#"{"seq":0,"payload":{"type":"system","data":"one"}}
{"seq":1,"payload":{"type":"system","data":"two"}}
"#,
        )
        .unwrap();
        let (state, id) = state_with_session(&path);

        let mut params = std::collections::HashMap::new();
        params.insert("since".to_string(), "0".to_string());
        // `since` is exclusive: seq 0 is skipped, seq 1 replays.
        let r = session_events(State(state.clone()), Path(id.clone()), Query(params))
            .await
            .unwrap();
        assert_eq!(r.events.len(), 1);
        assert_eq!(r.events[0].seq, 1);
        assert_eq!(r.next_seq, 2);
        assert!(matches!(r.events[0].event, StreamEvent::System(ref s) if s == "two"));

        let mut params = std::collections::HashMap::new();
        params.insert("since".to_string(), "1".to_string());
        let r = session_events(State(state), Path(id), Query(params))
            .await
            .unwrap();
        assert!(r.events.is_empty(), "fully consumed cursor replays nothing");
        assert_eq!(r.next_seq, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // single-threaded test runtime; guard is intentional
    async fn create_session_registers_and_lists_from_disk() {
        // Redirects where ALL sessions live; serialize against other tests
        // that read/write the sessions dir.
        let _guard = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let data_dir = std::env::temp_dir().join(format!("dex-srv-create-{}", std::process::id()));
        let prev = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("XDG_DATA_HOME", &data_dir);

        let state = Arc::new(DaemonState::new());
        let r = create_session(
            State(state.clone()),
            Json(CreateSessionRequest {
                cwd: "/tmp/dex-create-cwd".into(),
                name: Some("t".into()),
            }),
        )
        .await
        .unwrap();
        let id = r.0["session_id"].as_str().unwrap().to_string();
        assert!(!id.is_empty());
        assert!(
            state.sessions.lock().unwrap().contains_key(&id),
            "session must be registered"
        );

        let listed = list_sessions(State(state)).await.0["sessions"]
            .as_array()
            .unwrap()
            .clone();
        assert!(listed
            .iter()
            .any(|s| s["session_id"].as_str() == Some(id.as_str())));

        // cleanup: the created session file lives under data_dir
        match prev {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        let _ = std::fs::remove_dir_all(&data_dir);
    }
}

#[cfg(test)]
mod permission_gate_tests {
    use super::*;

    /// The daemon permission ceiling is the security boundary between a
    /// remote client and trusted-mode tool execution: a client may only
    /// request a STRICTER mode than the daemon's own. Runs before any LLM
    /// call, so it is testable with no provider.
    #[test]
    fn permission_ceiling_blocks_client_escalation_and_bad_plan() {
        let _guard = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        // Hermetic session storage.
        let data_dir = std::env::temp_dir().join(format!("dex-perm-{}", std::process::id()));
        let prev_data = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("XDG_DATA_HOME", &data_dir);

        let session = Session::new("/tmp/dex-perm-cwd".into(), None).unwrap();
        let path = session.path().unwrap().to_path_buf();
        let id = session.id().to_string();
        drop(session);

        let state = Arc::new(DaemonState::new());
        state.sessions.lock().unwrap().insert(
            id.clone(),
            SessionEntry {
                path: path.clone(),
                name: None,
                cwd: "/tmp".into(),
            },
        );

        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let cancel = CancellationToken::new();

        // Deterministic provider config for the pass-through case: fake key,
        // unroutable local base URL (connection refused, no network).
        let saved: Vec<(&str, Option<std::ffi::OsString>)> = [
            "DEX_PERMISSION",
            "DEX_PROVIDER",
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "OPENAI_API",
        ]
        .iter()
        .map(|k| (*k, std::env::var_os(k)))
        .collect();
        std::env::set_var("DEX_PERMISSION", "read-only");
        std::env::set_var("DEX_PROVIDER", "opencode");
        std::env::set_var("OPENAI_API_KEY", "test-key");
        std::env::set_var("OPENAI_BASE_URL", "http://127.0.0.1:9");
        std::env::set_var("OPENAI_API", "chat");

        let mk_req = |permission: Option<&str>, plan: Option<&str>| ChatRequest {
            prompt: "go".into(),
            skill_dirs: vec![],
            base_url: None,
            model: None,
            permission: permission.map(String::from),
            plan: plan.map(String::from),
        };

        // 1. Client escalating to trusted against a read-only daemon: rejected.
        let err = run_turn_inner(
            &state,
            &id,
            &mk_req(Some("trusted"), None),
            &cancel,
            &tx,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(err.contains("escalation denied"), "got: {err}");
        // Nothing journaled: the turn never started.
        assert_ne!(
            crate::session::Session::last_turn_state(&path),
            "turn_start",
            "rejected turn must not journal turn_start"
        );

        // 2. Client requesting the same (stricter-or-equal) mode passes the
        // gate and fails later, at the LLM call — a different error.
        let err = run_turn_inner(
            &state,
            &id,
            &mk_req(Some("read-only"), None),
            &cancel,
            &tx,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(
            !err.contains("escalation denied"),
            "gate must not fire for non-escalation: {err}"
        );

        // 3. Invalid plan JSON from the client is rejected, not stored.
        let err = run_turn_inner(
            &state,
            &id,
            &mk_req(Some("read-only"), Some("{not json")),
            &cancel,
            &tx,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(err.contains("invalid plan JSON"), "got: {err}");

        for (k, v) in saved {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
        match prev_data {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        let _ = std::fs::remove_dir_all(&data_dir);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("trace.jsonl"));
    }
}

#[cfg(test)]
mod e2e_tests {
    use super::*;
    use crate::client::http::{ChatOptions, DaemonClient};
    use axum::body::Body;
    use axum::extract::State as AxumState;
    use axum::routing::post;
    use axum::Router;
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn spawn_app(app: Router) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    /// Full remote loop over real HTTP: real daemon router + real
    /// DaemonClient + a fake chat-completions provider. The model asks to
    /// `write` a file, the client denies the approval, the denied tool
    /// result flows back, and the model finishes with plain text. Covers
    /// client SSE parsing, the approval round trip, and the daemon turn
    /// machinery in one pass.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)] // env must stay redirected for the whole turn
    async fn client_denies_write_then_turn_completes() {
        let _guard = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        // Fake provider: request 0 asks for a write; later requests finish.
        const TOOL_SSE: &str = concat!(
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"t1","function":{"name":"write","arguments":"{\"path\":\"evil.txt\",\"content\":\"hi\"}"}}]}}]}"#,
            "\n\ndata: [DONE]\n\n"
        );
        const DONE_SSE: &str =
            "data: {\"choices\":[{\"delta\":{\"content\":\"all done\"}}]}\n\ndata: [DONE]\n\n";
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let fake_llm = Router::new().route(
            "/chat/completions",
            post(move |AxumState(_): AxumState<Arc<AtomicUsize>>| {
                let counter = counter.clone();
                async move {
                    let n = counter.fetch_add(1, Ordering::SeqCst);
                    let body = if n == 0 { TOOL_SSE } else { DONE_SSE };
                    axum::http::Response::builder()
                        .status(200)
                        .header("content-type", "text/event-stream")
                        .body(Body::from(body.to_string()))
                        .unwrap()
                }
            }),
        );
        // Axum requires typed state; attach the counter (already Arc'd).
        let fake_llm = fake_llm.with_state(calls.clone());
        let llm_base = spawn_app(fake_llm).await;

        let daemon_base = spawn_app(router(Arc::new(DaemonState::new()))).await;

        // Deterministic daemon environment: approvals on, LLM pointed at
        // the fake provider.
        let data_dir = std::env::temp_dir().join(format!("dex-e2e-{}", std::process::id()));
        let saved: Vec<(&str, Option<std::ffi::OsString>)> = [
            "XDG_DATA_HOME",
            "DEX_PERMISSION",
            "DEX_PROVIDER",
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "OPENAI_API",
            "OPENAI_MODEL",
            "DEX_MODELS",
            "DEX_MODEL_APIS",
            "DEX_CONTEXT_WINDOW",
            "DEX_THINKING_EFFORT",
            "DEX_VERIFY",
        ]
        .iter()
        .map(|k| (*k, std::env::var_os(k)))
        .collect();
        std::env::set_var("XDG_DATA_HOME", &data_dir);
        std::env::set_var("DEX_PERMISSION", "ask-writes");
        std::env::set_var("DEX_PROVIDER", "opencode");
        std::env::set_var("OPENAI_API_KEY", "test-key");
        std::env::set_var("OPENAI_BASE_URL", &llm_base);
        std::env::set_var("OPENAI_API", "chat");
        std::env::set_var("DEX_VERIFY", "true");
        // No file override exists anymore; clear the rest for hermeticity.
        for v in [
            "OPENAI_MODEL",
            "DEX_MODELS",
            "DEX_MODEL_APIS",
            "DEX_CONTEXT_WINDOW",
            "DEX_THINKING_EFFORT",
        ] {
            std::env::remove_var(v);
        }

        // All client work happens on one blocking thread: reqwest::blocking
        // panics if created or dropped inside an async context.
        let (_session_id, events, chat_result) = tokio::task::spawn_blocking(move || {
            let client = DaemonClient::new(&daemon_base).unwrap();
            client.wait_until_ready(Duration::from_secs(10)).unwrap();
            let session_id = client
                .create_session("/tmp/dex-e2e-cwd", Some("e2e"))
                .unwrap()
                .session_id;
            let mut events: Vec<crate::protocol::StreamEvent> = Vec::new();
            let r = client
                .chat(
                    &session_id,
                    "write a file please",
                    ChatOptions::default(),
                    &mut |event| {
                        let decision = match &event {
                            crate::protocol::StreamEvent::ApprovalRequired { .. } => {
                                Some(crate::protocol::ApprovalDecision::Deny)
                            }
                            _ => None,
                        };
                        events.push(event);
                        decision
                    },
                )
                .map_err(|e| e.to_string());
            (session_id, events, r)
            // client drops here, on the blocking pool
        })
        .await
        .unwrap();
        chat_result.unwrap();

        // Pi has no permission popups — default is trusted, so write succeeds without approval.
        let approvals: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                crate::protocol::StreamEvent::ApprovalRequired { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(approvals, Vec::<String>::new(), "{events:?}");

        // The write tool should succeed and create the file.
        assert!(
            events.iter().any(|e| matches!(e,
                crate::protocol::StreamEvent::ToolResult { name, success, .. }
                if name == "write" && *success)),
            "write must surface as successful ToolResult: {events:?}"
        );
        assert!(
            std::path::Path::new("evil.txt").exists(),
            "write must touch disk when trusted"
        );

        // The turn completed with the model's final text.
        let last = events.last().unwrap();
        match last {
            crate::protocol::StreamEvent::TurnComplete { response, .. } => {
                assert_eq!(response, "all done");
            }
            other => panic!("expected TurnComplete, got {other:?}"),
        }
        // The model was called twice: tool call, then final answer.
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        for (k, v) in saved {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
        let _ = std::fs::remove_dir_all(&data_dir);
        let _ = std::fs::remove_file("evil.txt");
    }
}
