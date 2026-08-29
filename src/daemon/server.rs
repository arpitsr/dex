use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::routing::post;
use axum::{Json, Router};
use futures::stream::Stream;
use serde_json::json;
use tokio::sync::mpsc;

use crate::agent::r#loop::process_turn;
use crate::agent::state::{GlobalCancellation, ToolState};
use crate::core::console::Console;
use crate::core::types::{ApprovalDecision, ApprovalRequest, ChatMessage, SinkLine};
use crate::llm::config::LlmConfig;
use crate::llm::prompt::system_prompt;
use crate::protocol::{ApprovalResponse, ChatRequest, CreateSessionRequest, StreamEvent};
use crate::session::{self, Session};

use super::DaemonState;

pub(crate) fn router(state: Arc<DaemonState>) -> Router {
    Router::new()
        .route("/api/sessions", post(create_session).get(list_sessions))
        .route("/api/sessions/{id}/chat", post(chat))
        .route("/api/sessions/{id}/approve", post(approve))
        .route("/api/sessions/{id}/cancel", post(cancel))
        .with_state(state)
}

async fn create_session(
    State(state): State<Arc<DaemonState>>,
    Json(req): Json<CreateSessionRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let session = session::Session::new(req.cwd.clone(), req.name.clone())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let session_id = session.id().to_string();
    let path = session
        .path()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    let entry = super::SessionEntry {
        path: path.clone().into(),
        name: req.name,
        cwd: req.cwd,
    };
    state
        .sessions
        .write()
        .await
        .insert(session_id.clone(), entry);

    Ok(Json(json!({
        "session_id": session_id,
        "path": path,
    })))
}

async fn list_sessions(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    let sessions: Vec<serde_json::Value> = state
        .sessions
        .read()
        .await
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
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = mpsc::channel::<StreamEvent>(64);

    // Spawn the agent loop in a blocking task.
    let state_clone = state.clone();
    let sid = session_id.clone();
    tokio::task::spawn(async move {
        if let Err(e) = run_agent_turn(state_clone, &sid, req, tx.clone()).await {
            let _ = tx.send(StreamEvent::Error(e.to_string())).await;
        }
    });

    // Convert the receiver into an SSE stream.
    let event_stream = async_stream::stream! {
        let mut rx = rx;
        while let Some(event) = rx.recv().await {
            let sse_event = Event::default()
                .data(event.to_sse());
            yield Ok(sse_event);
        }
    };

    Sse::new(event_stream).keep_alive(
        axum::response::sse::KeepAlive::default()
            .interval(std::time::Duration::from_secs(30))
            .text("ping"),
    )
}

async fn run_agent_turn(
    state: Arc<DaemonState>,
    session_id: &str,
    req: ChatRequest,
    tx: mpsc::Sender<StreamEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Resolve session path.
    let entry = state
        .sessions
        .read()
        .await
        .get(session_id)
        .cloned()
        .ok_or("session not found")?;

    let mut session = if entry.path.exists() {
        Session::from_path(&entry.path).map_err(|e| format!("failed to load session: {e}"))?
    } else {
        Session::new(entry.cwd.clone(), entry.name.clone())
            .map_err(|e| format!("failed to create session: {e}"))?
    };

    // Build config from environment.
    let config = LlmConfig::from_env(None, None, None)
        .map_err(|e| format!("failed to build config: {e}"))?;

    // Build messages.
    let mut messages: Vec<ChatMessage> = Vec::new();
    messages.push(ChatMessage {
        role: "system".into(),
        content: Some(system_prompt(&[])),
        tool_calls: None,
        tool_call_id: None,
        name: None,
    });
    messages.extend(session::load_messages_from_session(&entry.path).unwrap_or_default());
    messages.push(ChatMessage {
        role: "user".into(),
        content: Some(req.prompt),
        tool_calls: None,
        tool_call_id: None,
        name: None,
    });

    let mut tool_state = ToolState::load();

    // Create channels for the console.
    let (sink_tx_std, sink_rx_std) = std::sync::mpsc::channel::<SinkLine>();
    let (approval_tx_std, approval_rx_std) = std::sync::mpsc::channel::<ApprovalRequest>();

    // Use the daemon console that flags remote approval handling.
    let console = Console::daemon(sink_tx_std, approval_tx_std);

    // Bridge sink: std → tokio for SSE streaming.
    let (sink_tx, mut sink_rx) = mpsc::channel::<SinkLine>(64);
    tokio::task::spawn_blocking(move || {
        while let Ok(sl) = sink_rx_std.recv() {
            if sink_tx.blocking_send(sl).is_err() {
                break;
            }
        }
    });

    // Spawn a task to forward sink events as StreamEvents.
    let tx_forward = tx.clone();
    tokio::task::spawn(async move {
        while let Some(sl) = sink_rx.recv().await {
            let event = match sl {
                SinkLine::Assistant(s) => StreamEvent::AssistantText(s),
                SinkLine::ToolInput(s) => {
                    let parts: Vec<&str> = s.splitn(2, ' ').collect();
                    let name = parts.first().unwrap_or(&"").to_string();
                    let args = if parts.len() > 1 {
                        serde_json::Value::String(parts[1].to_string())
                    } else {
                        serde_json::Value::Null
                    };
                    StreamEvent::ToolCall { name, args }
                }
                SinkLine::ToolOutput { name, summary } => {
                    let success = !summary.starts_with("failed");
                    StreamEvent::ToolResult {
                        name,
                        summary,
                        success,
                    }
                }
                SinkLine::System(s) => StreamEvent::System(s),
                SinkLine::Error(s) => StreamEvent::Error(s),
            };
            let _ = tx_forward.send(event).await;
        }
    });

    // Bridge approval requests: the agent loop sends ApprovalRequests via std
    // mpsc; this task converts them to StreamEvents and stores the response
    // sender so the POST /approve handler can resolve them.
    let tx_approval = tx.clone();
    let state_for_approval = state.clone();
    tokio::task::spawn(async move {
        while let Ok(req) = approval_rx_std.recv() {
            let request_id = uuid::Uuid::new_v4().to_string();

            // Store the response sender so /approve can resolve it.
            state_for_approval
                .pending_approvals
                .write()
                .await
                .insert(request_id.clone(), req.response);

            // Notify the client via SSE.
            let _ = tx_approval
                .send(StreamEvent::ApprovalRequired {
                    request_id,
                    name: req.name,
                    input: req.input,
                })
                .await;
        }
    });

    // Store a cancellation token for this session.
    let cancellation = Arc::new(GlobalCancellation);
    state
        .cancellations
        .write()
        .await
        .insert(session_id.to_string(), cancellation.clone());

    // Run the agent loop (blocking, in a spawn_blocking context).
    let result: Result<String, String> = {
        let config = config.clone();
        tokio::task::spawn_blocking(move || {
            process_turn(
                &config,
                &mut messages,
                &mut tool_state,
                None,
                None,
                Some(&mut session),
                &config,
                &*cancellation,
                &console,
            )
            .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| format!("agent task panicked: {e}"))?
    };

    // Clean up pending approvals and cancellation token.
    state.pending_approvals.write().await.remove(session_id);
    state.cancellations.write().await.remove(session_id);

    match result {
        Ok(response) => {
            let _ = tx.send(StreamEvent::TurnComplete { response }).await;
        }
        Err(e) => {
            let _ = tx
                .send(StreamEvent::TurnFailed {
                    error: e.to_string(),
                })
                .await;
        }
    }

    Ok(())
}

async fn approve(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
    Json(req): Json<ApprovalResponse>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // The request_id comes from the client's response to an ApprovalRequired
    // SSE event. For now we match by session (one pending approval per session).
    // A more robust design would include request_id in the ApprovalResponse.
    let decision = match req.decision {
        crate::protocol::ApprovalDecision::AllowOnce => ApprovalDecision::Once,
        crate::protocol::ApprovalDecision::AllowSession => ApprovalDecision::Session,
        crate::protocol::ApprovalDecision::Deny => ApprovalDecision::Deny,
    };

    // Find and resolve the pending approval for this session.
    let sender = state.pending_approvals.write().await.remove(&session_id);

    match sender {
        Some(tx) => {
            let _ = tx.send(decision);
            Ok(Json(json!({ "status": "ok" })))
        }
        None => Err(StatusCode::NOT_FOUND),
    }
}

async fn cancel(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // Deny any pending approval for this session.
    if let Some(sender) = state.pending_approvals.write().await.remove(&session_id) {
        let _ = sender.send(ApprovalDecision::Deny);
    }

    // The GlobalCancellation token would need to be checked inside process_turn.
    // For now we deny pending approvals as a best-effort cancellation.
    // TODO: Wire GlobalCancellation into the agent loop's main loop condition.

    Ok(Json(json!({ "status": "ok" })))
}
