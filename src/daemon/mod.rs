pub(crate) mod server;

use std::collections::{HashMap, HashSet};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::core::console::CancellationToken;
use crate::core::types::ApprovalDecision;

/// A tool execution awaiting the client's approval decision.
pub(crate) struct PendingApproval {
    pub(crate) session_id: String,
    pub(crate) response: mpsc::Sender<ApprovalDecision>,
    pub(crate) name: String,
    pub(crate) input: String,
}

/// A completed chat turn kept for `Idempotency-Key` dedup (P10): replaying
/// the same key within the window returns the recorded terminal event instead
/// of running the turn again (no duplicate effects).
#[allow(dead_code)]
pub(crate) struct IdempotentTurn {
    session_id: String,
    /// Hash of the request body so a key cannot replay a different prompt.
    request_hash: u64,
    /// Serialized terminal `StreamEvent` (`TurnComplete`/`TurnFailed`).
    terminal: String,
    at: Instant,
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
    /// Per-session steering queue: `POST /steer` pushes into the turn's
    /// `steering_rx` (consumed inside `process_turn` between iterations).
    pub steering_txs: Mutex<HashMap<String, mpsc::Sender<String>>>,
    /// Per-session follow-up queue: `POST /followup` pushes into a turn's
    /// outer loop (`run_agent_turn`) which chains a new `process_turn`
    /// iteration without a new HTTP request.
    pub followup_txs: Mutex<HashMap<String, mpsc::Sender<String>>>,
    /// Per-session next event sequence number (P10) for the SSE journal,
    /// seeded from disk on startup so replays stay consistent across restarts.
    pub event_seqs: Mutex<HashMap<String, u64>>,
    /// `Idempotency-Key` → completed turn, for 60s dedup (P10).
    pub idempotency: Mutex<HashMap<String, IdempotentTurn>>,
    /// Persisted “allow for session” approvals, keyed by `name:hash` (same
    /// scope as `Console::approval_key`). Lives on the daemon so a decision
    /// survives across turns; previously `Console` was per-turn and lost it.
    pub session_approvals: Mutex<HashMap<String, HashSet<String>>>,
}

/// 60-second window during which an `Idempotency-Key` replays its recorded
/// turn instead of running it again.
const IDEMPOTENCY_WINDOW: Duration = Duration::from_secs(60);

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
            steering_txs: Mutex::new(HashMap::new()),
            followup_txs: Mutex::new(HashMap::new()),
            event_seqs: Mutex::new(HashMap::new()),
            idempotency: Mutex::new(HashMap::new()),
            session_approvals: Mutex::new(HashMap::new()),
        }
    }

    #[allow(dead_code)]
    /// Scoped “allow for session” check — mirrors `Console::approval_key`
    /// so daemon and console agree on scope. `write`/`edit` → path, `bash` →
    /// command, else full input hash.
    pub(crate) fn is_session_approved(&self, session_id: &str, name: &str, input: &str) -> bool {
        let key = crate::core::console::Console::approval_key(name, input);
        self.session_approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .is_some_and(|set| set.contains(&key))
    }

    pub(crate) fn record_session_approval(&self, session_id: &str, name: &str, input: &str) {
        let key = crate::core::console::Console::approval_key(name, input);
        self.session_approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(session_id.to_string())
            .or_default()
            .insert(key);
    }

    /// Check for a replayable turn under `Idempotency-Key`. Falls through
    /// (None) when the key is unknown, stale, or names a different session or
    /// request hash.
    fn idempotent_replay(&self, key: &str, session_id: &str, request_hash: u64) -> Option<String> {
        let mut map = self.idempotency.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, t| t.at.elapsed() < IDEMPOTENCY_WINDOW);
        let turn = map.get(key)?;
        if turn.session_id != session_id || turn.request_hash != request_hash {
            return None;
        }
        Some(turn.terminal.clone())
    }

    /// Record a completed turn for `Idempotency-Key` dedup.
    fn idempotency_record(&self, key: &str, session_id: &str, request_hash: u64, terminal: String) {
        self.idempotency
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                key.to_string(),
                IdempotentTurn {
                    session_id: session_id.to_string(),
                    request_hash,
                    terminal,
                    at: Instant::now(),
                },
            );
    }

    /// Allocate the next event seq for a session.
    fn next_seq(&self, session_id: &str) -> u64 {
        let mut map = self.event_seqs.lock().unwrap_or_else(|e| e.into_inner());
        let next = map.entry(session_id.to_string()).or_insert(0);
        let seq = *next;
        *next += 1;
        seq
    }

    /// Seed `event_seqs` for a session from its persisted journal: the next
    /// allocation continues after the highest journaled seq (0 when empty).
    /// Takes the max with any live counter: the startup rebuild now runs in
    /// the background, so a turn may have allocated seqs before this seeds.
    fn seed_seq(&self, session_id: &str, path: &std::path::Path) {
        let max = crate::session::Session::max_event_seq(path);
        let next = if max > 0 { max + 1 } else { 0 };
        self.event_seqs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(session_id.to_string())
            .and_modify(|seq| *seq = (*seq).max(next))
            .or_insert(next);
    }

    /// Rebuild the session registry from disk (`Session::list_all`) after a
    /// daemon restart, seed per-session event cursors, and mark turns that
    /// were interrupted by the crash (`turn_start` with no terminal entry) as
    /// `turn_failed` so a reattaching client sees the truth instead of a ghost.
    ///
    /// Runs on a background thread at startup: all file IO happens lock-free
    /// and the registry lock is held only for the final insert, so serving
    /// (notably `POST /api/sessions`) never blocks behind the scan. Entries
    /// use `or_insert` so sessions created while the rebuild was in flight
    /// win over their (nonexistent) disk state.
    pub fn rebuild(&self) {
        let mut entries = Vec::new();
        for (path, header) in crate::session::Session::list_all().unwrap_or_default() {
            let id = header.id().to_string();
            self.seed_seq(&id, &path);
            if crate::session::Session::last_turn_state(&path) == "interrupted" {
                // Session::from_path refuses in-memory (pathless) sessions but
                // these all came from disk.
                if let Ok(mut s) = crate::session::Session::from_path(&path) {
                    let _ = s.turn_event("turn_failed").and_then(|_| {
                        s.set_state(
                            "last_error",
                            "turn interrupted by daemon restart; effects may be partial — review before continuing",
                        )
                    });
                }
            }
            entries.push((
                id,
                SessionEntry {
                    path: path.clone(),
                    name: header.name().map(ToOwned::to_owned),
                    cwd: header.cwd().to_string(),
                },
            ));
        }
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        for (id, entry) in entries {
            sessions.entry(id).or_insert(entry);
        }
    }
}

/// Start the daemon HTTP server on an already-bound listener.
pub(crate) async fn run_daemon(listener: TcpListener) -> Result<(), Box<dyn std::error::Error>> {
    let state = std::sync::Arc::new(DaemonState::new());
    // Rebuild in-memory state from the persisted JSONL on a background thread:
    // scanning every session (headers, event seqs, turn state) costs ~0.5s
    // with a few thousand sessions and would delay /health and TUI first
    // paint. Fresh sessions use uuid ids so they never collide with rebuilt
    // ones; `seed_seq` takes the max so a racing turn can't rewind a counter.
    let warm = state.clone();
    tokio::task::spawn_blocking(move || warm.rebuild());

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idempotency_key_replays_same_turn_and_rejects_different_request() {
        let state = DaemonState::new();
        state.idempotency_record("key-1", "sess-1", 42, "{\"seq\":9}".into());
        // Same session + same request hash: replay.
        assert_eq!(
            state.idempotent_replay("key-1", "sess-1", 42),
            Some("{\"seq\":9}".to_string())
        );
        // Different request hash under the same key: do NOT replay (a key
        // cannot launder a different prompt).
        assert_eq!(state.idempotent_replay("key-1", "sess-1", 43), None);
        // Different session under the same key: no replay.
        assert_eq!(state.idempotent_replay("key-1", "sess-2", 42), None);
        // Unknown key: no replay.
        assert_eq!(state.idempotent_replay("nope", "sess-1", 42), None);
    }

    #[test]
    fn event_seq_allocates_monotonically_per_session() {
        let state = DaemonState::new();
        assert_eq!(state.next_seq("a"), 0);
        assert_eq!(state.next_seq("a"), 1);
        assert_eq!(state.next_seq("b"), 0);
        assert_eq!(state.next_seq("a"), 2);
    }

    #[test]
    fn event_seq_is_seeded_from_disk_after_restart() {
        // Touches the shared sessions dir; serialize against env-redirecting tests.
        let _guard = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Simulate a prior run: a session with events already journaled.
        let mut s = crate::session::Session::new("/tmp/dex-seq-test".into(), None).unwrap();
        s.append_event(0, "{\"type\":\"system\",\"data\":\"x\"}")
            .unwrap();
        s.append_event(1, "{\"type\":\"system\",\"data\":\"y\"}")
            .unwrap();
        let path = s.path().unwrap().to_path_buf();
        let state = DaemonState::new();
        state.seed_seq(s.id(), &path);
        // The next allocation continues after the journal, not from zero.
        assert_eq!(state.next_seq(s.id()), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rebuild_marks_interrupted_turns_failed_and_registers_sessions() {
        // Depends on where the sessions dir resolves; serialize against tests
        // that redirect XDG_DATA_HOME.
        let _guard = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // A session killed mid-turn: turn_start with no terminal entry.
        let mut s = crate::session::Session::new("/tmp/dex-rebuild-test".into(), None).unwrap();
        let id = s.id().to_string();
        s.turn_event("turn_start").unwrap();
        assert_eq!(
            crate::session::Session::last_turn_state(s.path().unwrap()),
            "interrupted"
        );
        let path = s.path().unwrap().to_path_buf();
        drop(s);

        let state = DaemonState::new();
        state.rebuild();
        // The session is in the registry after restart.
        {
            let sessions = state.sessions.lock().unwrap();
            assert!(
                sessions.contains_key(&id),
                "registry must be rebuilt from disk"
            );
        }
        // The interrupted turn is now durably failed.
        assert_eq!(crate::session::Session::last_turn_state(&path), "failed");
        let _ = std::fs::remove_file(&path);
    }
}
