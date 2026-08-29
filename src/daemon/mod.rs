pub(crate) mod server;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{broadcast, RwLock};

/// Shared state for the daemon.
#[allow(dead_code)]
pub(crate) struct DaemonState {
    pub base_dir: PathBuf,
    pub sessions: RwLock<HashMap<String, SessionEntry>>,
    pub broadcast_tx: broadcast::Sender<String>,
}

#[derive(Clone)]
pub(crate) struct SessionEntry {
    pub path: PathBuf,
    pub name: Option<String>,
    pub cwd: String,
}

impl DaemonState {
    pub fn new(base_dir: PathBuf) -> Self {
        let (broadcast_tx, _) = broadcast::channel(64);
        Self {
            base_dir,
            sessions: RwLock::new(HashMap::new()),
            broadcast_tx,
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
