pub(crate) mod server;

use std::collections::{HashMap, HashSet};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Mutex;

use crate::core::console::CancellationToken;
use crate::core::types::ApprovalDecision;

/// A tool execution awaiting the client's approval decision.
pub(crate) struct PendingApproval {
    pub(crate) session_id: String,
    pub(crate) response: mpsc::Sender<ApprovalDecision>,
}

/// Shared state for the daemon. Plain mutexes are fine here: every critical
/// section is short and never holds the lock across an `.await`.
pub(crate) struct DaemonState {
    pub sessions: Mutex<HashMap<String, SessionEntry>>,
    /// Pending approval requests keyed by request ID (as sent to the client
    /// in the `ApprovalRequired` stream event). The sender resolves the
    /// blocking `approve_tool` call inside the agent loop.
    pub pending_approvals: Mutex<HashMap<String, PendingApproval>>,
    /// Sessions with a turn currently in flight; one turn at a time per
    /// session keeps the append-only session log consistent.
    pub active_turns: Mutex<HashSet<String>>,
    /// Per-session cancellation tokens for in-flight turns. POST /cancel
    /// signals the token so this turn unwinds without touching other
    /// sessions; the entry is removed when the turn finishes.
    pub cancel_tokens: Mutex<HashMap<String, CancellationToken>>,
}

#[derive(Clone)]
pub(crate) struct SessionEntry {
    pub path: PathBuf,
    pub name: Option<String>,
    pub cwd: String,
}

impl DaemonState {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            pending_approvals: Mutex::new(HashMap::new()),
            active_turns: Mutex::new(HashSet::new()),
            cancel_tokens: Mutex::new(HashMap::new()),
        }
    }
}

/// Start the daemon HTTP server on an already-bound listener.
pub(crate) async fn run_daemon(listener: TcpListener) -> Result<(), Box<dyn std::error::Error>> {
    let state = std::sync::Arc::new(DaemonState::new());

    let app = server::router(state.clone());

    let addr = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();
    println!("dex daemon listening on {addr}");

    // tokio refuses blocking fds; the std listener must be non-blocking
    // before registration.
    listener.set_nonblocking(true)?;
    let listener = tokio::net::TcpListener::from_std(listener)?;
    axum::serve(listener, app).await?;

    Ok(())
}
