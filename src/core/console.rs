use std::collections::HashSet;
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use crate::core::types::*;

pub(crate) static INTERRUPTED: AtomicBool = AtomicBool::new(false);

pub(crate) static CANCEL_REQUESTED: AtomicBool = AtomicBool::new(false);

pub(crate) fn request_cancel() {
    CANCEL_REQUESTED.store(true, Ordering::SeqCst);
}

pub(crate) fn take_cancel_requested() -> bool {
    CANCEL_REQUESTED.swap(false, Ordering::SeqCst)
}

pub(crate) fn cancel_requested() -> bool {
    CANCEL_REQUESTED.load(Ordering::SeqCst)
}

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

pub(crate) const RESET: &str = "\x1b[0m";

pub(crate) const TOOL_INPUT_COLOR: &str = "\x1b[1;33m";

pub(crate) const TOOL_OUTPUT_COLOR: &str = "\x1b[0;34m";

pub(crate) const AGENT_COLOR: &str = "\x1b[1;32m";

pub(crate) const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

pub(crate) static CONSOLE_LOCK: Mutex<()> = Mutex::new(());

pub(crate) static TOOL_MUTATION_LOCK: Mutex<()> = Mutex::new(());

pub(crate) static SPINNER_RUNNING: AtomicBool = AtomicBool::new(false);

pub(crate) static SPINNER_DRAWN: AtomicBool = AtomicBool::new(false);

/// When set (ratatui UI mode), the agent routes streamed output here instead
/// of printing to the console.
pub(crate) static CONSOLE_SINK: Mutex<Option<mpsc::Sender<SinkLine>>> = Mutex::new(None);

pub(crate) static APPROVAL_SINK: Mutex<Option<mpsc::Sender<ApprovalRequest>>> = Mutex::new(None);

pub(crate) static SESSION_APPROVALS: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Enable/disable the console sink (used by the ratatui UI path).
pub fn set_console_sink(sink: Option<mpsc::Sender<SinkLine>>) {
    *CONSOLE_SINK.lock().unwrap_or_else(|e| e.into_inner()) = sink;
}

/// Clone of the active sink sender, if any.
pub fn console_sink() -> Option<mpsc::Sender<SinkLine>> {
    CONSOLE_SINK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

pub(crate) fn set_approval_sink(sink: Option<mpsc::Sender<ApprovalRequest>>) {
    *APPROVAL_SINK.lock().unwrap_or_else(|e| e.into_inner()) = sink;
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
pub(crate) fn with_console(f: impl FnOnce()) {
    if console_sink().is_some() {
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
    pub(crate) fn start(label: &str) -> Self {
        if console_sink().is_some() {
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
