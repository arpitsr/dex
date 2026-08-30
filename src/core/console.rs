use std::collections::HashSet;
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::agent::state::CancellationSource;

use crate::core::types::*;

pub(crate) static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigint(_: i32) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

pub(crate) fn install_sigint_handler() {
    // Minimal libc binding so we don't need the libc crate. We must use
    // `sigaction` (not `signal`) WITHOUT SA_RESTART: otherwise a SIGINT
    // arriving while blocked in read(2) on the tty restarts the syscall
    // and the flag is never observed until another key is pressed.
    #[repr(C)]
    struct SigAction {
        handler: extern "C" fn(i32),
        mask: [u64; 16],
        flags: i32,
        restorer: usize,
    }
    unsafe extern "C" {
        fn sigaction(signum: i32, act: *const SigAction, old: *mut SigAction) -> i32;
    }
    let action = SigAction {
        handler: handle_sigint,
        mask: [0; 16],
        flags: 0, // no SA_RESTART => read() returns EINTR
        restorer: 0,
    };
    unsafe {
        sigaction(2, &action, std::ptr::null_mut());
    }
}

/// Returns true once per interrupt (consumes the flag).
pub(crate) fn take_interrupt() -> bool {
    INTERRUPTED.swap(false, Ordering::SeqCst)
}

/// Sticky interrupt state: true once Ctrl+C has been pressed, and stays true
/// until the flag is consumed via `take_interrupt`. Use for poll-based loops
/// that check cancellation every iteration.
pub(crate) fn is_interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// A cancellation signal scoped to a single agent turn. Unlike the
/// process-global `CANCEL_REQUESTED` flag, a token is per-session, idempotent,
/// and not sticky: once dropped the turn it cancelled is finished and a new
/// turn gets a fresh, un-cancelled token. This prevents one client's Cancel
/// from leaking into another session, and prevents a stale cancellation from
/// spuriously aborting a later turn.
#[derive(Clone)]
pub(crate) struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    pub(crate) fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Signal cancellation for the turn this token belongs to.
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// True until `reset` is called; safe to poll from worker threads.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Cached cancellation (like `is_cancelled`); kept for symmetry with
    /// `CancellationSource`.
    pub(crate) fn take_cancelled(&self) -> bool {
        self.cancelled.swap(true, Ordering::SeqCst)
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancellationSource for CancellationToken {
    fn is_cancelled(&self) -> bool {
        self.is_cancelled()
    }
    fn take_cancelled(&self) -> bool {
        self.take_cancelled()
    }
}

pub(crate) const RESET: &str = "\x1b[0m";

pub(crate) const TOOL_INPUT_COLOR: &str = "\x1b[1;33m";

pub(crate) const TOOL_OUTPUT_COLOR: &str = "\x1b[0;34m";

pub(crate) const AGENT_COLOR: &str = "\x1b[1;32m";

pub(crate) const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

pub(crate) static CONSOLE_LOCK: Mutex<()> = Mutex::new(());

pub(crate) static TOOL_MUTATION_LOCK: Mutex<()> = Mutex::new(());

pub(crate) static SPINNER_RUNNING: AtomicBool = AtomicBool::new(false);

pub(crate) static SPINNER_DRAWN: AtomicBool = AtomicBool::new(false);

/// Bundles the output sinks used by a single agent turn. Previously these were
/// process-global statics (CONSOLE_SINK / APPROVAL_SINK / SESSION_APPROVALS);
/// threading a `Console` through `process_turn` removes the singleton that the
/// REPL worker thread used to read via crate-level globals.
pub(crate) struct Console {
    sink: Option<mpsc::Sender<SinkLine>>,
    approval: Option<mpsc::Sender<ApprovalRequest>>,
    session_approvals: Mutex<Option<HashSet<String>>>,
    /// When true, the daemon handles approvals remotely via SSE instead of
    /// prompting on stdin. The approval channel is still used to send requests;
    /// a separate mechanism resolves them when the client POSTs back.
    pub(crate) remote_approval: bool,
}

impl Clone for Console {
    fn clone(&self) -> Self {
        Self {
            sink: self.sink.clone(),
            approval: self.approval.clone(),
            session_approvals: Mutex::new(
                self.session_approvals
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
            ),
            remote_approval: self.remote_approval,
        }
    }
}

impl Console {
    #[allow(dead_code)]
    pub(crate) fn new(
        sink: mpsc::Sender<SinkLine>,
        approval: mpsc::Sender<ApprovalRequest>,
    ) -> Self {
        Self {
            sink: Some(sink),
            approval: Some(approval),
            session_approvals: Mutex::new(None),
            remote_approval: false,
        }
    }

    /// A console with no sinks: streamed output prints directly to the terminal
    /// (used by the one-shot CLI path).
    pub(crate) fn none() -> Self {
        Self {
            sink: None,
            approval: None,
            session_approvals: Mutex::new(None),
            remote_approval: false,
        }
    }

    /// Create a console for the daemon that handles approvals remotely.
    pub(crate) fn daemon(
        sink: mpsc::Sender<SinkLine>,
        approval: mpsc::Sender<ApprovalRequest>,
    ) -> Self {
        Self {
            sink: Some(sink),
            approval: Some(approval),
            session_approvals: Mutex::new(None),
            remote_approval: true,
        }
    }

    pub(crate) fn sink(&self) -> Option<&mpsc::Sender<SinkLine>> {
        self.sink.as_ref()
    }

    pub(crate) fn approval(&self) -> Option<&mpsc::Sender<ApprovalRequest>> {
        self.approval.as_ref()
    }

    pub(crate) fn session_approved(&self, name: &str) -> bool {
        self.session_approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|approved| approved.contains(name))
    }

    pub(crate) fn record_session_approval(&self, name: &str) {
        self.session_approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(HashSet::new)
            .insert(name.to_string());
    }
}

/// Erase the drawn spinner frame, if any. Caller holds CONSOLE_LOCK.
pub(crate) fn erase_spinner_frame() {
    if SPINNER_DRAWN.swap(false, Ordering::SeqCst) {
        let mut out = io::stdout();
        let _ = out.write_all(b"\r\x1b[2K");
        let _ = out.flush();
    }
}

/// Run `f` with the spinner suspended so output never interleaves with frames.
/// `ui_mode` is true when streamed output is routed through a sink instead of
/// the terminal (e.g. the ratatui REPL), in which case console IO is skipped.
pub(crate) fn with_console(ui_mode: bool, f: impl FnOnce()) {
    if ui_mode {
        return; // UI mode: output is routed through the sink; skip console IO
    }
    let _lock = CONSOLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    erase_spinner_frame();
    f()
}

/// Animates `<label> ...` frames until dropped (no-op when stdout is piped).
pub(crate) struct SpinnerGuard {
    #[allow(dead_code)]
    pub(crate) worker: Option<thread::JoinHandle<()>>,
}

impl SpinnerGuard {
    pub(crate) fn start(console: &Console, label: &str) -> Self {
        if console.sink().is_some() {
            return Self { worker: None }; // UI mode shows "working…" in the status bar
        }
        if !io::stdout().is_terminal() {
            return Self { worker: None };
        }
        SPINNER_RUNNING.store(true, Ordering::SeqCst);
        let label = label.to_string();
        let worker = thread::spawn(move || {
            let mut out = io::stdout();
            for frame in SPINNER_FRAMES.iter().cycle() {
                if !SPINNER_RUNNING.load(Ordering::SeqCst) {
                    break;
                }
                {
                    let _lock = CONSOLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                    if !SPINNER_RUNNING.load(Ordering::SeqCst) {
                        break;
                    }
                    let _ = write!(out, "\r\x1b[2K{}{frame}{RESET} {label} ...", AGENT_COLOR);
                    let _ = out.flush();
                    SPINNER_DRAWN.store(true, Ordering::SeqCst);
                }
                thread::sleep(Duration::from_millis(80));
            }
        });
        Self {
            worker: Some(worker),
        }
    }
}

impl Drop for SpinnerGuard {
    fn drop(&mut self) {
        if self.worker.take().is_some() {
            // Final erase under the lock: after RUNNING flips false no new
            // frame can appear, and the worker exits on its next tick
            // without blocking turn teardown.
            let _lock = CONSOLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            SPINNER_RUNNING.store(false, Ordering::SeqCst);
            erase_spinner_frame();
        }
    }
}
