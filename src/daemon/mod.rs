pub(crate) mod server;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{mpsc, Arc};

use tokio::sync::RwLock;

use crate::agent::state::GlobalCancellation;
use crate::core::types::ApprovalDecision;

/// Shared state for the daemon.
#[allow(dead_code)]
pub(crate) struct DaemonState {
    pub base_dir: PathBuf,
    pub sessions: RwLock<HashMap<String, SessionEntry>>,
    /// Pending approval requests keyed by request ID. The sender side resolves
    /// the blocking `approve_tool` call in the agent loop.
    pub pending_approvals: RwLock<HashMap<String, mpsc::Sender<ApprovalDecision>>>,
    /// Per-session cancellation flags.
    pub cancellations: RwLock<HashMap<String, Arc<GlobalCancellation>>>,
}

#[derive(Clone)]
pub(crate) struct SessionEntry {
    pub path: PathBuf,
    pub name: Option<String>,
    pub cwd: String,
}

impl DaemonState {
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            base_dir,
            sessions: RwLock::new(HashMap::new()),
            pending_approvals: RwLock::new(HashMap::new()),
            cancellations: RwLock::new(HashMap::new()),
        }
    }
}

/// Start the daemon HTTP server.
pub(crate) async fn run_daemon(addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    let base_dir = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ak")
        .join("sessions");

    let state = Arc::new(DaemonState::new(base_dir));

    let app = server::router(state.clone());

    println!("ak daemon listening on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
