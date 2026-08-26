use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, VecDeque};
use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

// --- Skills ---
// Lightweight Agent Skills support. Skills are discovered from directories
// containing a SKILL.md file with YAML frontmatter (name, description).
// Only descriptions are included in the system prompt; full content is
// loaded on demand via /skill:name or when the user message triggers it.

#[derive(Clone, Debug)]
struct Skill {
    name: String,
    description: String,
    path: PathBuf,
}

fn parse_skill(path: &Path) -> Option<Skill> {
    let content = fs::read_to_string(path).ok()?;
    let mut lines = content.lines();
    let first = lines.next()?;
    if first.trim() != "---" {
        return None;
    }
    let mut frontmatter = String::new();
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        frontmatter.push_str(line);
        frontmatter.push('\n');
    }
    let mut name = None;
    let mut description = None;
    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("name:") {
            name = Some(val.trim().to_string());
        } else if let Some(val) = line.strip_prefix("description:") {
            description = Some(val.trim().to_string());
        }
    }
    Some(Skill {
        name: name?,
        description: description.unwrap_or_default(),
        path: path.to_path_buf(),
    })
}

fn discover_skills(dirs: &[PathBuf]) -> Vec<Skill> {
    let mut skills = Vec::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && path.join("SKILL.md").exists() {
                if let Some(skill) = parse_skill(&path.join("SKILL.md")) {
                    skills.push(skill);
                }
            }
        }
    }
    skills
}

fn skill_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    // Project-level skills
    if let Ok(cwd) = env::current_dir() {
        dirs.push(cwd.join(".ak/skills"));
        dirs.push(cwd.join(".agents/skills"));
    }
    // User-level skills
    if let Some(cfg) = env::var_os("XDG_CONFIG_HOME") {
        dirs.push(PathBuf::from(cfg).join("ak/skills"));
    } else if let Some(home) = env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".config/ak/skills"));
    }
    dirs
}

fn format_skills_for_prompt(skills: &[Skill]) -> String {
    let mut out = String::new();
    out.push_str("\n\nAvailable skills:\n");
    for skill in skills {
        out.push_str(&format!("- {}: {}\n", skill.name, skill.description));
    }
    out.push_str("\nTo use a skill, type /skill:<name> or ask about it.\n");
    out
}

// --- Session persistence ---
// Linear JSONL session file: header + message entries.
// Auto-saved after each message exchange so crashes and Ctrl+C
// never lose more than one turn. Simpler than pi's tree model
// because this agent is linear (no branching).

const SESSION_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Clone, Debug)]
struct SessionHeader {
    #[serde(rename = "type")]
    entry_type: String,
    version: u32,
    id: String,
    timestamp: String,
    cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct SessionMessageEntry {
    #[serde(rename = "type")]
    entry_type: String,
    id: String,
    timestamp: String,
    #[serde(flatten)]
    message: ChatMessage,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct SessionInfoEntry {
    #[serde(rename = "type")]
    entry_type: String,
    id: String,
    timestamp: String,
    name: String,
}

#[derive(Debug)]
struct Session {
    header: SessionHeader,
    path: Option<PathBuf>,
    counter: u64,
}

impl Session {
    fn session_dir() -> PathBuf {
        if let Some(dir) = env::var_os("XDG_DATA_HOME") {
            return PathBuf::from(dir).join("ak/sessions");
        }
        env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(".local/share/ak/sessions"))
            .unwrap_or_else(|| PathBuf::from(".ak/sessions"))
    }

    fn cwd_slug(cwd: &str) -> String {
        cwd.replace('/', "-").replace("\\", "-")
    }

    fn new(cwd: String, name: Option<String>) -> io::Result<Self> {
        let id = format!("{}_{}", Self::now_ms(), uuid4());
        let dir = Self::session_dir().join(Self::cwd_slug(&cwd));
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.jsonl", id));
        let header = SessionHeader {
            entry_type: "session".to_string(),
            version: SESSION_VERSION,
            id: id.clone(),
            timestamp: Self::now_iso(),
            cwd,
            name,
        };
        let line = serde_json::to_string(&header).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let mut file = fs::OpenOptions::new().create(true).append(true).open(&path)?;
        writeln!(file, "{}", line)?;
        let session = Self {
            header,
            path: Some(path),
            counter: 0,
        };
        Ok(session)
    }

    fn from_path(path: &Path) -> io::Result<Self> {
        let file = fs::read_to_string(path)?;
        let mut lines = file.lines();
        let first = lines.next().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "empty session file"))?;
        let header: SessionHeader = serde_json::from_str(first)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("bad session header: {}", e)))?;
        let counter = lines.count() as u64;
        Ok(Self { header, path: Some(path.to_path_buf()), counter })
    }

    fn in_memory(cwd: String) -> Self {
        let id = format!("{}_{}", Self::now_ms(), uuid4());
        Self {
            header: SessionHeader {
                entry_type: "session".to_string(),
                version: SESSION_VERSION,
                id,
                timestamp: Self::now_iso(),
                cwd,
                name: None,
            },
            path: None,
            counter: 0,
        }
    }

    fn open_or_continue(cwd: String, session_path: Option<&Path>, no_session: bool) -> io::Result<Self> {
        if no_session {
            return Ok(Self::in_memory(cwd));
        }
        if let Some(path) = session_path {
            if path.exists() {
                return Self::from_path(path);
            }
        }
        // Try to continue the most recent session for this cwd
        let dir = Self::session_dir().join(Self::cwd_slug(&cwd));
        if let Ok(mut entries) = fs::read_dir(&dir) {
            let mut latest: Option<(PathBuf, std::time::SystemTime)> = None;
            while let Some(Ok(entry)) = entries.next() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                    if let Ok(meta) = entry.metadata() {
                        if let Ok(modified) = meta.modified() {
                            if latest.as_ref().map(|(_, t)| modified > *t).unwrap_or(true) {
                                latest = Some((path, modified));
                            }
                        }
                    }
                }
            }
            if let Some((path, _)) = latest {
                return Self::from_path(&path);
            }
        }
        Self::new(cwd, None)
    }

    fn list(cwd: &str) -> io::Result<Vec<(PathBuf, SessionHeader)>> {
        let dir = Self::session_dir().join(Self::cwd_slug(cwd));
        let mut sessions = Vec::new();
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                    if let Ok(text) = fs::read_to_string(&path) {
                        if let Some(first) = text.lines().next() {
                            if let Ok(header) = serde_json::from_str::<SessionHeader>(first) {
                                sessions.push((path, header));
                            }
                        }
                    }
                }
            }
        }
        sessions.sort_by(|a, b| b.1.timestamp.cmp(&a.1.timestamp));
        Ok(sessions)
    }

    fn set_name(&mut self, name: String) -> io::Result<()> {
        self.header.name = Some(name.clone());
        let entry = SessionInfoEntry {
            entry_type: "session_info".to_string(),
            id: self.next_id(),
            timestamp: Self::now_iso(),
            name,
        };
        self.append_line(&entry)
    }

    fn append_message(&mut self, message: ChatMessage) -> io::Result<()> {
        let entry = SessionMessageEntry {
            entry_type: "message".to_string(),
            id: self.next_id(),
            timestamp: Self::now_iso(),
            message,
        };
        self.append_line(&entry)
    }

    fn append_line<T: Serialize>(&mut self, entry: &T) -> io::Result<()> {
        if let Some(path) = &self.path {
            let line = serde_json::to_string(entry).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            let mut file = fs::OpenOptions::new().create(true).append(true).open(path)?;
            writeln!(file, "{}", line)?;
        }
        Ok(())
    }

    fn next_id(&mut self) -> String {
        self.counter += 1;
        format!("{:x}", self.counter)
    }

    fn now_iso() -> String {
        let now = SystemTime::now();
        let secs = now.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
        // Naive RFC3339-ish
        let dt = chrono::DateTime::from_timestamp(secs as i64, 0).unwrap_or_default();
        dt.to_rfc3339()
    }

    fn now_ms() -> u64 {
        SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_millis() as u64
    }

    fn id(&self) -> &str {
        &self.header.id
    }

    fn name(&self) -> Option<&str> {
        self.header.name.as_deref()
    }

    fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn is_persisted(&self) -> bool {
        self.path.is_some()
    }

    fn display_name(&self) -> String {
        self.name().map(|n| n.to_string()).unwrap_or_else(|| self.id().to_string())
    }
}

fn uuid4() -> String {
    let mut bytes = [0u8; 16];
    for b in bytes.iter_mut() {
        *b = rand::random();
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

/// Load messages from a session file (excluding the header and metadata entries).
fn load_messages_from_session(path: &Path) -> io::Result<Vec<ChatMessage>> {
    let text = fs::read_to_string(path)?;
    let mut messages = Vec::new();
    for line in text.lines().skip(1) {
        let value: Value = serde_json::from_str(line)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("bad session line: {}", e)))?;
        if value.get("type").and_then(Value::as_str) == Some("message") {
            let msg: ChatMessage = serde_json::from_value(value)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("bad message: {}", e)))?;
            if msg.role != "system" {
                messages.push(msg);
            }
        }
    }
    Ok(messages)
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigint(_: i32) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

fn install_sigint_handler() {
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
    const SA_RESTART: i32 = 0x1000_0000;
    let action = SigAction {
        handler: handle_sigint,
        mask: [0; 16],
        flags: 0 & !SA_RESTART, // no SA_RESTART => read() returns EINTR
        restorer: 0,
    };
    unsafe {
        sigaction(2, &action, std::ptr::null_mut());
    }
}

/// Returns true once per interrupt (consumes the flag).
fn take_interrupt() -> bool {
    INTERRUPTED.swap(false, Ordering::SeqCst)
}

const RESET: &str = "\x1b[0m";
const PROMPT_COLOR: &str = "\x1b[1;36m";
const INPUT_COLOR: &str = "\x1b[1;37m";
const TOOL_INPUT_COLOR: &str = "\x1b[1;33m";
const TOOL_OUTPUT_COLOR: &str = "\x1b[0;34m";
const AGENT_COLOR: &str = "\x1b[1;32m";
const ERROR_COLOR: &str = "\x1b[1;31m";

// --- Working spinner ---
//
// Braille frames on the current line while the agent works. Transcript
// writers go through with_console(), which erases the frame before
// printing, so real output never interleaves with the animation.

const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

static CONSOLE_LOCK: Mutex<()> = Mutex::new(());
static SPINNER_RUNNING: AtomicBool = AtomicBool::new(false);
static SPINNER_DRAWN: AtomicBool = AtomicBool::new(false);

/// Erase the drawn spinner frame, if any. Caller holds CONSOLE_LOCK.
fn erase_spinner_frame() {
    if SPINNER_DRAWN.swap(false, Ordering::SeqCst) {
        let mut out = io::stdout();
        let _ = out.write_all(b"\r\x1b[2K");
        let _ = out.flush();
    }
}

/// Run `f` with the spinner suspended so output never interleaves with frames.
fn with_console<T>(f: impl FnOnce() -> T) -> T {
    let _lock = CONSOLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    erase_spinner_frame();
    f()
}

/// Animates `<label> ...` frames until dropped (no-op when stdout is piped).
struct SpinnerGuard {
    #[allow(dead_code)]
    worker: Option<thread::JoinHandle<()>>,
}

impl SpinnerGuard {
    fn start(label: &str) -> Self {
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
        Self { worker: Some(worker) }
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

#[derive(Debug)]
enum ToolError {
    Missing(&'static str),
    NotString(&'static str),
    Io(io::Error),
    EditNotUnique(usize),
    Unknown(String),
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolError::Missing(k) => write!(f, "missing argument '{}'", k),
            ToolError::NotString(k) => write!(f, "argument '{}' must be a string", k),
            ToolError::Io(e) => write!(f, "io error: {}", e),
            ToolError::EditNotUnique(n) => write!(f, "edit target appears {} times (need exactly 1)", n),
            ToolError::Unknown(t) => write!(f, "unknown tool '{}'", t),
        }
    }
}

fn arg_str(args: &Map<String, Value>, key: &'static str) -> Result<String, ToolError> {
    match args.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(ToolError::NotString(key)),
        None => Err(ToolError::Missing(key)),
    }
}

fn run_bash(command: &str) -> Result<String, ToolError> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(command)
        .output()
        .map_err(ToolError::Io)?;
    let mut result = String::new();
    result.push_str(&String::from_utf8_lossy(&out.stdout));
    result.push_str(&String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        result.push_str(&format!("\n[exit {}]", out.status.code().unwrap_or(-1)));
    }
    Ok(result)
}

fn tool_read(args: &Map<String, Value>) -> Result<String, ToolError> {
    let path = arg_str(args, "path")?;
    fs::read_to_string(&path).map_err(ToolError::Io)
}

fn tool_bash(args: &Map<String, Value>) -> Result<String, ToolError> {
    run_bash(&arg_str(args, "command")?)
}

fn tool_write(args: &Map<String, Value>) -> Result<String, ToolError> {
    let path = arg_str(args, "path")?;
    let content = arg_str(args, "content")?;
    fs::write(&path, content).map_err(ToolError::Io)?;
    Ok(format!("wrote {}", path))
}

fn tool_edit(args: &Map<String, Value>) -> Result<String, ToolError> {
    let path = arg_str(args, "path")?;
    let old = arg_str(args, "oldText")?;
    let new = arg_str(args, "newText")?;
    let content = fs::read_to_string(&path).map_err(ToolError::Io)?;
    let count = content.matches(&old).count();
    if count != 1 {
        return Err(ToolError::EditNotUnique(count));
    }
    fs::write(&path, content.replacen(&old, &new, 1)).map_err(ToolError::Io)?;
    Ok(format!("edited {}", path))
}

fn tool_grep(args: &Map<String, Value>) -> Result<String, ToolError> {
    let pattern = arg_str(args, "pattern")?;
    let path = arg_str(args, "path").unwrap_or_else(|_| ".".to_string());
    run_bash(&format!("grep -R -I -n -- {} {}", shell_escape(&pattern), shell_escape(&path)))
}

fn tool_find(args: &Map<String, Value>) -> Result<String, ToolError> {
    let pattern = arg_str(args, "pattern")?;
    let path = arg_str(args, "path").unwrap_or_else(|_| ".".to_string());
    run_bash(&format!("find {} -path '*{}*' -print", shell_escape(&path), pattern))
}

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

fn execute(name: &str, args: &Map<String, Value>) -> Result<String, ToolError> {
    match name {
        "read" => tool_read(args),
        "bash" => tool_bash(args),
        "write" => tool_write(args),
        "edit" => tool_edit(args),
        "grep" => tool_grep(args),
        "find" => tool_find(args),
        other => Err(ToolError::Unknown(other.to_string())),
    }
}

fn execute_to_string(name: &str, args: &Map<String, Value>) -> String {
    match execute(name, args) {
        Ok(out) => out,
        Err(e) => format!("Error: {}", e),
    }
}

fn truncate_text(text: &str, max_bytes: usize, max_lines: usize) -> String {
    let mut preview = String::new();
    let mut truncated = false;

    for (line_number, line) in text.lines().enumerate() {
        if line_number >= max_lines {
            truncated = true;
            break;
        }
        let remaining = max_bytes.saturating_sub(preview.len());
        if remaining <= 1 {
            truncated = true;
            break;
        }
        let limit = remaining - 1;
        let end = line
            .char_indices()
            .find(|(index, _)| *index >= limit)
            .map(|(index, _)| index)
            .unwrap_or(line.len());
        preview.push_str(&line[..end]);
        preview.push('\n');
        if end < line.len() {
            truncated = true;
            break;
        }
    }

    if truncated {
        preview.push_str("... [output truncated]");
    } else if preview.ends_with('\n') {
        preview.pop();
    }
    preview
}

fn terminal_preview(text: &str) -> String {
    truncate_text(text, 10 * 1024, 100)
}

// --- Markdown / syntax highlighting for assistant output ---

/// Render an assistant message to the terminal as markdown, with
/// fenced code blocks highlighted via `bat` when available.
fn print_code_block(lang: &str, body: &str) {
    // Try `bat` first (supports language tags + line numbers + theme).
    let bat = ["bat", "batcat"].iter().find_map(|b| {
        which(b).ok().map(|p| (b.to_string(), p))
    });
    if let Some((bin, path)) = bat {
        let mut cmd = Command::new(&path);
        cmd.args([
            "--color=always",
            "--style=plain,header=fault",
            "--paging=never",
        ]);
        if !lang.is_empty() {
            cmd.args(["-l", lang]);
        }
        match cmd.arg("-").stdin(Stdio::piped()).spawn() {
            Ok(mut child) => {
                if let Some(stdin) = child.stdin.as_mut() {
                    let _ = stdin.write_all(body.as_bytes());
                }
                drop(child.stdin.take());
                if let Ok(out) = child.wait_with_output() {
                    if out.status.success() {
                        print!("{}", String::from_utf8_lossy(&out.stdout));
                        return;
                    }
                }
            }
            Err(_) => {}
        }
        let _ = bin;
    }
    // Fallback: dim-colored block.
    print!("\x1b[2m{}\x1b[0m", body);
}

fn which(bin: &str) -> Result<PathBuf, io::Error> {
    let path_var = env::var("PATH").unwrap_or_default();
    for dir in env::split_paths(&path_var) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(io::ErrorKind::NotFound, format!("{} not found", bin)))
}

fn model_tool_result(text: &str) -> String {
    truncate_text(text, 50 * 1024, 2_000)
}

// --- Status line ---
//
// The status line is rendered as part of the input "block":
// [input box][separator][status line]. The editor places the block
// directly below the transcript and pushes it down as output arrives;
// once it reaches the bottom edge of the terminal it docks there and
// stays visible (like pi/codex CLIs).

const CHROME_ROWS: usize = 2;

/// Width of a string as displayed, ignoring ANSI escape sequences.
fn visible_width(s: &str) -> usize {
    let mut w = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip CSI sequence up to its final byte (@ through ~).
            for f in chars.by_ref() {
                if f.is_ascii_alphabetic() || ('@'..='~').contains(&f) {
                    break;
                }
            }
        } else {
            w += 1;
        }
    }
    w
}

/// Truncate a styled status body to `cols` visible columns.
fn truncate_visible(body: &str, cols: usize) -> String {
    let budget = cols.saturating_sub(2);
    let mut visible = String::new();
    let mut w = 0usize;
    let mut in_escape = false;
    for c in body.chars() {
        if c == '\x1b' {
            in_escape = true;
            visible.push(c);
            continue;
        }
        if in_escape {
            visible.push(c);
            if c.is_ascii_alphabetic() || ('@'..='~').contains(&c) {
                in_escape = false;
            }
            continue;
        }
        w += 1;
        if w > budget {
            break;
        }
        visible.push(c);
    }
    visible
}

/// Compose the status line body from segments, left-aligned with dim
/// dividers, truncated to fit the terminal width.
fn status_body(segments: &[String], cols: usize) -> String {
    const DIM: &str = "\x1b[2m";
    let divider = format!("{} │ {}", DIM, RESET);
    let mut body = String::new();
    let mut width = 0usize;
    let mut first = true;
    for seg in segments {
        if !first {
            body.push_str(&divider);
            width += 3;
        }
        first = false;
        body.push_str(seg);
        width += visible_width(seg);
        if width + 4 >= cols {
            break;
        }
    }
    if width >= cols {
        body = truncate_visible(&body, cols);
    }
    body
}

/// Build the status segments shown while waiting for user input.
fn status_segments(config: &LlmConfig, messages: &[ChatMessage], state: &ToolState) -> Vec<String> {
    let model_seg = format!("\x1b[1m{}\x1b[22m", config.model);
    let turns = messages.iter().filter(|m| m.role == "user" && m.name.is_none()).count();
    let msgs_seg = format!("{} msgs", messages.len());
    // Context usage: prefer last API-reported prompt_tokens, else estimate.
    let used = state.last_usage.unwrap_or_else(|| estimate_tokens(messages));
    let pct = ((used as f64 / config.context_window.max(1) as f64) * 100.0).round() as u64;
    let ctx_seg = format!(
        "{}k/{}k ({}% left)",
        (used + 999) / 1000,
        config.context_window / 1000,
        100usize.saturating_sub(pct as usize)
    );
    let tools_seg = format!("{} cached", state.cache.len());
    vec![model_seg, msgs_seg, ctx_seg, tools_seg, format!("turn {}", turns)]
}

/// Plain-text transcript lines used to repaint the screen after a width
/// change re-wraps everything.
fn transcript_replay(messages: &[ChatMessage]) -> Vec<String> {
    let mut lines = vec![format!("{}ak agent{}", AGENT_COLOR, RESET)];
    for m in messages {
        match m.role.as_str() {
            "user" if m.name.is_none() => {
                let body = m.content.clone().unwrap_or_default();
                lines.push(format!("{}> {}{}{}", PROMPT_COLOR, INPUT_COLOR, body, RESET));
            }
            "assistant" => match (&m.tool_calls, &m.content) {
                (Some(calls), _) => {
                    let names: Vec<&str> = calls.iter().map(|c| c.function.name.as_str()).collect();
                    lines.push(format!("{}⋮ {}{}", TOOL_INPUT_COLOR, names.join(", "), RESET));
                }
                (None, Some(text)) => lines.push(text.clone()),
                _ => {}
            },
            _ => {}
        }
    }
    lines
}

// --- OpenAI-compatible LLM integration ---

#[derive(Serialize, Deserialize, Clone, Debug)]
struct ChatMessage {
    role: String,
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<LlmToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct LlmToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: FunctionCall,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct FunctionCall {
    name: String,
    arguments: String,
}

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    tools: Vec<ToolDefinition>,
    stream: bool,
    stream_options: StreamOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Deserialize, Default)]
struct Usage {
    prompt_tokens: u64,
}

#[derive(Serialize)]
struct ToolDefinition {
    #[serde(rename = "type")]
    tool_type: String,
    function: FunctionDef,
}

#[derive(Serialize)]
struct FunctionDef {
    name: String,
    description: String,
    parameters: Value,
}

#[derive(Deserialize)]
struct StreamChunk {
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
}

#[derive(Deserialize)]
struct StreamDelta {
    content: Option<String>,
    tool_calls: Option<Vec<StreamToolCall>>,
}

#[derive(Deserialize)]
struct StreamToolCall {
    index: usize,
    id: Option<String>,
    function: Option<StreamFunctionCall>,
}

#[derive(Deserialize)]
struct StreamFunctionCall {
    name: Option<String>,
    arguments: Option<String>,
}

fn tools_schema() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "read".to_string(),
                description: "Read the contents of a file.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": { "path": { "type": "string", "description": "relative path to the file" } },
                    "required": ["path"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "bash".to_string(),
                description: "Run a shell command.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": { "command": { "type": "string", "description": "shell command to run" } },
                    "required": ["command"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "write".to_string(),
                description: "Write or overwrite a file.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" }
                    },
                    "required": ["path", "content"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "edit".to_string(),
                description: "Replace exactly one occurrence of old text with new text in a file.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "oldText": { "type": "string", "description": "exact existing text to replace" },
                        "newText": { "type": "string", "description": "replacement text" }
                    },
                    "required": ["path", "oldText", "newText"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "grep".to_string(),
                description: "Search file contents for a literal pattern.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string" },
                        "path": { "type": "string", "description": "directory or file to search (default: current directory)" }
                    },
                    "required": ["pattern"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "find".to_string(),
                description: "Find file paths matching a pattern.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string" },
                        "path": { "type": "string", "description": "directory to search (default: current directory)" }
                    },
                    "required": ["pattern"]
                }),
            },
        },
    ]
}

fn project_context() -> Option<String> {
    for name in &["AGENTS.md", "CLAUDE.md"] {
        if let Ok(content) = fs::read_to_string(name) {
            return Some(content);
        }
    }
    None
}

fn system_prompt(skills: &[Skill]) -> String {
    let mut prompt = "You are a coding agent. Use the provided tools to help the user. \
     Prefer reading files before editing. \
     When editing, oldText must match exactly one occurrence in the file. \
     Stop using tools once the requested work is complete. \
     Be concise.".to_string();
    if let Some(ctx) = project_context() {
        prompt.push_str("\n\n--- Project instructions ---\n");
        prompt.push_str(&ctx);
    }
    if !skills.is_empty() {
        prompt.push_str(&format_skills_for_prompt(skills));
    }
    prompt
}

#[derive(Default, Deserialize)]
struct FileConfig {
    api_key: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
    thinking_effort: Option<String>,
    context_window: Option<u64>,
}

fn config_path() -> Option<PathBuf> {
    if let Some(path) = env::var_os("RUSTY_PI_CONFIG") {
        return Some(PathBuf::from(path));
    }
    if let Some(dir) = env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(dir).join("ak/config.json"));
    }
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/ak/config.json"))
}

fn load_file_config() -> Result<FileConfig, Box<dyn std::error::Error>> {
    let Some(path) = config_path() else {
        return Ok(FileConfig::default());
    };
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(FileConfig::default()),
        Err(e) => return Err(format!("could not read {}: {}", path.display(), e).into()),
    };
    serde_json::from_str(&contents)
        .map_err(|e| format!("invalid config file {}: {}", path.display(), e).into())
}

struct LlmConfig {
    api_key: String,
    base_url: String,
    model: String,
    thinking_effort: Option<String>,
    /// Model context window size in tokens (used for compaction + status bar).
    context_window: u64,
    client: reqwest::blocking::Client,
}

impl LlmConfig {
    fn from_env(
        base_url_override: Option<String>,
        model_override: Option<String>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let file = load_file_config()?;
        Ok(Self {
            api_key: env::var("OPENAI_API_KEY")
                .ok()
                .or(file.api_key)
                .ok_or("OPENAI_API_KEY not set and no api_key in config file")?,
            base_url: base_url_override
                .or_else(|| env::var("OPENAI_BASE_URL").ok().filter(|v| !v.is_empty()))
                .or(file.base_url)
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            model: model_override
                .or_else(|| env::var("OPENAI_MODEL").ok())
                .or(file.model)
                .unwrap_or_else(|| "gpt-4o-mini".to_string()),
            thinking_effort: file.thinking_effort,
            // Context window in tokens; configurable via file (`context_window`)
            // or AK_CONTEXT_WINDOW env, with a conservative default.
            context_window: env::var("AK_CONTEXT_WINDOW")
                .ok()
                .and_then(|v| v.parse().ok())
                .or(file.context_window)
                .unwrap_or(128_000),
            client: reqwest::blocking::Client::new(),
        })
    }
}

/// Incremental markdown printer: prose is flushed as soon as a full line
/// arrives; code fences are buffered until closed so they can be highlighted
/// as one block. If a fence is still open when the stream ends, it is
/// flushed as-is.
struct StreamPrinter {
    in_code: bool,
    code_lang: String,
    code_body: String,
}

impl StreamPrinter {
    fn new() -> Self {
        Self { in_code: false, code_lang: String::new(), code_body: String::new() }
    }

    fn feed_line(&mut self, line: &str) {
        with_console(|| self.feed_line_inner(line))
    }

    fn feed_line_inner(&mut self, line: &str) {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            if self.in_code {
                print_code_block(&self.code_lang, &self.code_body);
                self.code_body.clear();
                self.code_lang.clear();
                self.in_code = false;
            } else {
                self.in_code = true;
                self.code_lang =
                    trimmed.trim_start_matches('`').trim().to_string();
            }
        } else if self.in_code {
            self.code_body.push_str(line);
            self.code_body.push('\n');
        } else {
            termimad::print_text(&format!("{}\n", line));
        }
    }

    fn finish(self) {
        if self.in_code && !self.code_body.is_empty() {
            with_console(|| print_code_block(&self.code_lang, &self.code_body));
        }
    }
}

fn read_stream(response: reqwest::blocking::Response) -> Result<(ChatMessage, Option<u64>), Box<dyn std::error::Error>> {
    let mut reader = BufReader::new(response);
    let mut line = String::new();
    let mut content = String::new();
    let mut pending = String::new(); // partial line not yet printed
    let mut printer = StreamPrinter::new();
    let mut tool_calls: Vec<LlmToolCall> = Vec::new();
    let mut usage_tokens: Option<u64> = None;

    loop {
        if take_interrupt() {
            // Ctrl+C during generation: stop consuming the stream and
            // unwind so control returns to the prompt.
            with_console(|| println!());
            io::stdout().flush()?;
            return Err("interrupted".into());
        }
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        let data = data.trim();
        if data == "[DONE]" {
            break;
        }
        let chunk: StreamChunk = serde_json::from_str(data)?;
        if let Some(usage) = &chunk.usage {
            usage_tokens = Some(usage.prompt_tokens);
        }
        for choice in chunk.choices {
            if let Some(text) = choice.delta.content {
                content.push_str(&text);
                // Print complete lines live; keep any partial tail buffered.
                pending.push_str(&text);
                while let Some(pos) = pending.find('\n') {
                    let complete: String = pending.drain(..=pos).collect();
                    printer.feed_line(complete.trim_end_matches('\n'));
                }
                io::stdout().flush()?;
            }
            for delta in choice.delta.tool_calls.unwrap_or_default() {
                while tool_calls.len() <= delta.index {
                    tool_calls.push(LlmToolCall {
                        id: String::new(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: String::new(),
                            arguments: String::new(),
                        },
                    });
                }
                let call = &mut tool_calls[delta.index];
                if let Some(id) = delta.id {
                    call.id = id;
                }
                if let Some(function) = delta.function {
                    if let Some(name) = function.name {
                        call.function.name.push_str(&name);
                    }
                    if let Some(arguments) = function.arguments {
                        call.function.arguments.push_str(&arguments);
                    }
                }
            }
        }
    }

    // Flush any trailing partial line, then close out buffered code blocks.
    if !pending.is_empty() {
        printer.feed_line(&pending);
        io::stdout().flush()?;
    }
    printer.finish();
    io::stdout().flush()?;

    if !content.is_empty() {
        with_console(|| println!());
        io::stdout().flush()?;
    }
    Ok((
        ChatMessage {
            role: "assistant".to_string(),
            content: (!content.is_empty()).then_some(content),
            tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
            tool_call_id: None,
            name: None,
        },
        usage_tokens,
    ))
}

fn call_llm(
    config: &LlmConfig,
    messages: &[ChatMessage],
    with_tools: bool,
) -> Result<(ChatMessage, Option<u64>), Box<dyn std::error::Error>> {
    const MAX_RETRIES: u32 = 3;
    let req = ChatRequest {
        model: config.model.clone(),
        messages: messages.to_vec(),
        tools: if with_tools { tools_schema() } else { Vec::new() },
        stream: true,
        stream_options: StreamOptions { include_usage: true },
        reasoning_effort: config.thinking_effort.clone(),
        prompt_cache_key: Some("ak-agent-session".to_string()),
    };

    for attempt in 0..=MAX_RETRIES {
        let resp = match config
            .client
            .post(format!("{}/chat/completions", config.base_url))
            .bearer_auth(&config.api_key)
            .json(&req)
            .send()
        {
            Ok(resp) => resp,
            Err(e) if attempt < MAX_RETRIES => {
                let delay = Duration::from_millis(500 * 2u64.pow(attempt));
                with_console(|| eprintln!("[llm] request failed: {}; retrying in {:?}", e, delay));
                thread::sleep(delay);
                continue;
            }
            Err(e) => return Err(e.into()),
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text()?;
            let retryable = status.as_u16() == 408
                || status.as_u16() == 429
                || status.is_server_error();
            if retryable && attempt < MAX_RETRIES {
                let delay = Duration::from_millis(500 * 2u64.pow(attempt));
                with_console(|| eprintln!("[llm] API error {}: retrying in {:?}", status, delay));
                thread::sleep(delay);
                continue;
            }
            return Err(format!("API error: {}", body).into());
        }

        return Ok(read_stream(resp)?);
    }

    unreachable!()
}

const CACHE_FILE_NAME: &str = "ak-tool-cache.json";

fn cache_file_path() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("XDG_CACHE_HOME") {
        return Some(PathBuf::from(dir).join(CACHE_FILE_NAME));
    }
    env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".cache").join(CACHE_FILE_NAME))
}

#[derive(Default)]
struct ToolState {
    cache: HashMap<String, String>,
    dirty: bool,
    /// Last API-reported prompt token count for the main conversation.
    last_usage: Option<u64>,
}

impl ToolState {
    fn load() -> Self {
        let mut state = Self::default();
        if let Some(path) = cache_file_path() {
            if let Ok(contents) = fs::read_to_string(&path) {
                if let Ok(map) = serde_json::from_str::<HashMap<String, String>>(&contents) {
                    state.cache = map;
                }
            }
        }
        state
    }

    fn insert(&mut self, key: String, value: String) {
        self.cache.insert(key, value);
        self.dirty = true;
    }

    fn clear(&mut self) {
        if !self.cache.is_empty() {
            self.cache.clear();
            self.dirty = true;
        }
    }

    /// Persist the cache to disk (best-effort; failures are ignored).
    fn save(&self) {
        if !self.dirty {
            return;
        }
        if let Some(path) = cache_file_path() {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Ok(json) = serde_json::to_string(&self.cache) {
                let _ = fs::write(&path, json);
            }
        }
    }
}

/// Compact the conversation history to keep requests bounded:
/// replace old turns with a short summary, keeping the system prompt,
/// the first user message, and everything from the recent window intact.
const KEEP_RECENT_MESSAGES: usize = 12;
const MIN_MESSAGES_TO_SUMMARIZE: usize = 8;

/// Render a message as a compact transcript line for summarization.
fn message_to_transcript(msg: &ChatMessage) -> String {
    let body = msg.content.clone().unwrap_or_default();
    let role = match msg.role.as_str() {
        "assistant" if msg.tool_calls.is_some() => "assistant (tool calls)",
        other => other,
    };
    format!("{}: {}", role, truncate_text(&body, 2_000, 50))
}

fn summarize_old_messages(
    config: &LlmConfig,
    old: &[ChatMessage],
) -> Result<String, Box<dyn std::error::Error>> {
    let transcript: String = old
        .iter()
        .map(|m| message_to_transcript(m))
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = vec![
        ChatMessage {
            role: "system".to_string(),
            content: Some(
                "Summarize the following conversation excerpt between a coding agent and \
                 the user. Preserve: the user's goals and requests, key facts learned about \
                 the codebase (files, paths, important symbols), decisions made, actions \
                 already taken and their outcomes, and any unresolved tasks. Be concise — \
                 at most 15 lines. Output only the summary."
                    .to_string(),
            ),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        },
        ChatMessage {
            role: "user".to_string(),
            content: Some(transcript),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        },
    ];
    let (summary, _) = call_llm(config, &prompt, false)?;
    Ok(summary.content.unwrap_or_default())
}

/// Rough token estimate for a message: ~4 chars per token.
fn estimate_tokens(messages: &[ChatMessage]) -> u64 {
    let chars: usize = messages
        .iter()
        .map(|m| {
            m.content.as_deref().map_or(0, str::len)
                + m.tool_calls
                    .as_ref()
                    .map_or(0, |c| c.iter().map(|t| t.function.arguments.len() + t.function.name.len()).sum())
        })
        .sum();
    (chars as u64) / 4
}

fn compact_history(config: &LlmConfig, messages: &mut Vec<ChatMessage>) {
    // Find where the protected recent window begins (never split a
    // tool_call/tool pairing, so back up to the last non-tool message).
    let total = messages.len();
    if total <= 1 + KEEP_RECENT_MESSAGES {
        return;
    }
    let mut cutoff = total - KEEP_RECENT_MESSAGES;
    while cutoff > 1 && matches!(messages[cutoff].role.as_str(), "tool") {
        cutoff -= 1;
    }
    if cutoff <= 1 + MIN_MESSAGES_TO_SUMMARIZE {
        return; // too little to summarize
    }
    if messages[cutoff].role == "assistant" && messages[cutoff].tool_calls.is_some() {
        return; // would orphan tool calls; skip compaction this round
    }

    // Summarize the old segment (between the first user message and cutoff).
    let old: Vec<ChatMessage> = messages[1..cutoff].to_vec();
    let summarized = match summarize_old_messages(config, &old) {
        Ok(s) => s,
        Err(e) => {
            with_console(|| eprintln!("[history] summarization failed ({}); truncating instead", e));
            // Fall back to plain truncation: drop the old segment entirely.
            messages.drain(1..cutoff);
            return;
        }
    };

    // If a previous summary exists, it sits at index 1 and is part of `old`,
    // so the fresh summary subsumes it. Replace everything before cutoff
    // with a single summary user message.
    let summary_msg = ChatMessage {
        role: "user".to_string(),
        content: Some(format!(
            "[Summary of earlier conversation]\n{}\n[End of summary. Recent messages follow.]",
            summarized.trim()
        )),
        tool_calls: None,
        tool_call_id: None,
        name: Some("summary".to_string()),
    };
    messages.splice(1..cutoff, std::iter::once(summary_msg));
}

/// Maximum number of model round-trips within a single turn.
const MAX_TOOL_ITERATIONS: usize = 60;
/// When this many iterations remain, nudge the model to wrap up.
const WRAP_UP_THRESHOLD: usize = 5;

fn process_turn(
    config: &LlmConfig,
    messages: &mut Vec<ChatMessage>,
    state: &mut ToolState,
) -> Result<String, Box<dyn std::error::Error>> {
    // Spins while the agent works; erased automatically on return.
    let _working = SpinnerGuard::start("Working");
    let mut last_tools: Vec<String> = Vec::new();
    let mut last_usage: Option<u64> = state.last_usage;

    for iteration in 0..MAX_TOOL_ITERATIONS {
        // Nudge the model to finish as we approach the iteration budget.
        let remaining = MAX_TOOL_ITERATIONS - iteration;
        if remaining == WRAP_UP_THRESHOLD {
            messages.push(ChatMessage {
                role: "user".to_string(),
                content: Some(
                    "[System] You are approaching the tool-call limit for this turn. \
                     Before continuing, briefly re-evaluate: (1) why so many tool \
                     calls were needed — e.g. repeated reads, failed edits, or \
                     exploring the wrong paths; (2) what the user's actual task goal \
                     is and the shortest path remaining to reach it. Then recover \
                     toward that goal: avoid repeating failed approaches, prefer \
                     batched/broader tool calls over many small ones, and if the goal \
                     is already (partially) met, state what was accomplished, what \
                     remains, and produce your final answer now."
                        .to_string(),
                ),
                tool_calls: None,
                tool_call_id: None,
                name: Some("system-nudge".to_string()),
            });
        }
        let (message, usage) = match call_llm(config, messages, true) {
            Ok(result) => result,
            Err(e) if e.to_string() == "interrupted" => {
                return Err("interrupted by user (Ctrl+C)".into());
            }
            Err(e) => return Err(e),
        };
        if usage.is_some() {
            last_usage = usage;
        }
        // Compact when either the message count or an estimated token
        // budget is exceeded (API-reported usage takes precedence).
        let est = last_usage.unwrap_or_else(|| estimate_tokens(messages));
        if messages.len() > 1 + KEEP_RECENT_MESSAGES || est > config.context_window / 2 {
            compact_history(config, messages);
        }
        compact_history(config, messages);
        if let Some(calls) = message.tool_calls.clone() {
            messages.push(ChatMessage {
                role: "assistant".to_string(),
                content: message.content,
                tool_calls: Some(calls.clone()),
                tool_call_id: None,
                name: None,
            });
            // Execute all tool calls in this assistant message in parallel.
            let handles: Vec<_> = calls
                .iter()
                .map(|call| {
                    let name = call.function.name.clone();
                    let raw_args = call.function.arguments.clone();
                    thread::spawn(move || {
                        let args: Value = match serde_json::from_str(&raw_args) {
                            Ok(v) => v,
                            Err(e) => {
                                return (name, raw_args, format!("Error: invalid tool arguments: {}", e))
                            }
                        };
                        let args = args.as_object().cloned().unwrap_or_default();
                        let input = serde_json::to_string(&args).unwrap_or_default();
                        let result = execute_to_string(&name, &args);
                        (name, input, result)
                    })
                })
                .collect();

            for (call, handle) in calls.iter().zip(handles) {
                let (name, input, result) = handle.join().unwrap_or_else(|_| {
                    (
                        call.function.name.clone(),
                        String::new(),
                        "Error: tool worker panicked".to_string(),
                    )
                });
                let cache_key = format!("{}:{}", name, input);
                if last_tools.len() >= 6 {
                    last_tools.remove(0);
                }
                last_tools.push(cache_key.clone());
                let repeated_count =
                    last_tools.iter().filter(|k| **k == cache_key).count();
                with_console(|| {
                    eprintln!(
                        "{}[tool input] {} {}{}",
                        TOOL_INPUT_COLOR,
                        call.function.name,
                        terminal_preview(&input),
                        RESET
                    );
                });

                let cacheable = matches!(name.as_str(), "read" | "grep" | "find");
                let result = if repeated_count >= 3 {
                    "Error: repeated identical tool call; choose a different action or finish."
                        .to_string()
                } else if cacheable {
                    if let Some(cached) = state.cache.get(&cache_key) {
                        with_console(|| eprintln!("{}[tool cache hit]{}", TOOL_OUTPUT_COLOR, RESET));
                        cached.clone()
                    } else {
                        let result = result;
                        state.insert(cache_key, result.clone());
                        result
                    }
                } else {
                    if matches!(name.as_str(), "write" | "edit") {
                        state.clear();
                    }
                    result
                };
                with_console(|| {
                    eprintln!(
                        "{}[tool output] {}:\n{}{}",
                        TOOL_OUTPUT_COLOR,
                        name,
                        terminal_preview(&result),
                        RESET
                    );
                });
                messages.push(ChatMessage {
                    role: "tool".to_string(),
                    content: Some(model_tool_result(&result)),
                    tool_calls: None,
                    tool_call_id: Some(call.id.clone()),
                    name: None,
                });
            }
            state.save();
        } else {
            let text = message.content.unwrap_or_default();
            messages.push(ChatMessage {
                role: "assistant".to_string(),
                content: Some(text.clone()),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            });
            state.last_usage = last_usage;
            return Ok(text);
        }
    }
    Err(
        "too many tool iterations: the task did not complete within the per-turn \
         tool-call budget. Partial progress (if any) is preserved in the conversation. \
         To continue, you can ask me to resume the task — optionally on a new path or \
         with a different approach — e.g. \"continue from where you left off\" or \
         \"try a different approach\"."
            .into(),
    )
}

fn run_one_shot(
    prompt: &str,
    args: &Args,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = LlmConfig::from_env(args.base_url.clone(), args.model.clone())?;
    let mut skill_dirs = skill_dirs();
    skill_dirs.extend(args.skill_dirs.iter().cloned());
    let skills = discover_skills(&skill_dirs);
    let mut messages = vec![
        ChatMessage { role: "system".to_string(), content: Some(system_prompt(&skills)), tool_calls: None, tool_call_id: None, name: None },
        ChatMessage { role: "user".to_string(), content: Some(prompt.to_string()), tool_calls: None, tool_call_id: None, name: None },
    ];
    let mut state = ToolState::load();
    let result = process_turn(&config, &mut messages, &mut state);
    if !args.no_session {
        let cwd = env::current_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
        let mut session = if args.new_session {
            let mut s = Session::new(cwd, args.session_name.clone())?;
            if let Some(name) = &args.session_name {
                s.set_name(name.clone())?;
            }
            s
        } else {
            let mut s = Session::open_or_continue(cwd, args.session_path.as_deref(), false)?;
            if let Some(name) = &args.session_name {
                s.set_name(name.clone())?;
            }
            s
        };
        for msg in &messages[1..] { // skip system
            session.append_message(msg.clone())?;
        }
    }
    println!();
    result.map(|_| ())
}

enum Key {
    Char(char),
    Paste(String),
    Enter,
    NewLine,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    CtrlC,
    CtrlD,
    CtrlU,
    CtrlK,
    CtrlW,
    Noop,
}

#[derive(Debug, PartialEq, Eq)]
struct InputVisualRow {
    start: usize,
    end: usize,
    width: usize,
}

struct TerminalEditor {
    stdin: fs::File,
    stdout: io::Stdout,
    original_stty: String,
    last_ctrl_c: Option<std::time::Instant>,
    /// Screen columns of the terminal (queried lazily).
    cols: usize,
    /// Number of visible box rows rendered at the last refresh.
    last_rows: usize,
    /// Cursor row within the rendered block at the last refresh.
    last_cursor_row: usize,
    /// First visual row currently shown when the input is taller than the
    /// editor viewport (the editor scrolls internally).
    view_start: usize,
    /// Preferred visual column for consecutive vertical cursor moves.
    preferred_col: Option<usize>,
    /// Absolute screen row (1-indexed) of the input box top at the last
    /// refresh; 0 when nothing is rendered.
    block_top: usize,
    /// True when the hardware cursor position is unknown (after external
    /// output scrolled the screen) and must be re-queried via DSR before
    /// rendering.
    resync: bool,
    /// Column count at the last refresh (0 before the first render).
    last_cols: usize,
    /// Transcript lines to replay after a width change re-wraps the screen.
    replay: Vec<String>,
    /// Input bytes read from the tty but not yet interpreted (e.g. typed
    /// ahead of a DSR reply).
    pending: VecDeque<u8>,
}

/// Query the terminal size via `stty size`, defaulting to (24, 80).
fn terminal_size() -> (usize, usize) {
    let out = Command::new("stty")
        .arg("size")
        .stdin(Stdio::inherit())
        .output()
        .ok();
    out.and_then(|out| {
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut parts = text.split_whitespace();
        let rows = parts.next()?.parse::<usize>().ok()?;
        let cols = parts.next()?.parse::<usize>().ok()?;
        Some((rows, cols))
    })
    .filter(|&(r, c)| r > 0 && c > 0)
    .unwrap_or((24, 80))
}

#[allow(dead_code)]
fn terminal_cols() -> usize {
    terminal_size().1
}

impl TerminalEditor {
    fn new() -> io::Result<Self> {
        let stdin = fs::File::open("/dev/tty")?;
        let state = Command::new("stty")
            .arg("-g")
            .stdin(Stdio::from(stdin.try_clone()?))
            .output()?;
        if !state.status.success() {
            return Err(io::Error::other("could not read terminal settings"));
        }
        let original_stty = String::from_utf8_lossy(&state.stdout).trim().to_string();
        let raw = Command::new("stty")
            .args([
                // Keep isig ON so Ctrl+C raises SIGINT even while a
                // request is streaming; the handler sets a flag that the
                // stream loop checks.
                "-icanon",
                "-echo",
                "-ixon",
                "-icrnl",
                "-inlcr",
                "-igncr",
                "min",
                "1",
                "time",
                "0",
            ])
            .stdin(Stdio::from(stdin.try_clone()?))
            .status()?;
        if !raw.success() {
            return Err(io::Error::other("could not enable terminal input mode"));
        }
        let mut stdout = io::stdout();
        // Kitty keyboard protocol for Shift+Enter, xterm modifyOtherKeys as
        // the fallback, plus bracketed paste. No capability queries: their
        // replies would land in the input queue as phantom keystrokes.
        stdout.write_all(b"\x1b[?2004h\x1b[>7u\x1b[>4;2m")?;
        stdout.flush()?;
        Ok(Self {
            stdin,
            stdout,
            original_stty,
            last_ctrl_c: None,
            cols: terminal_cols(),
            last_rows: 0,
            last_cursor_row: 0,
            view_start: 0,
            preferred_col: None,
            block_top: 0,
            resync: true,
            last_cols: 0,
            replay: Vec::new(),
            pending: VecDeque::new(),
        })
    }

    /// Read exactly buf.len() bytes from the tty, honoring pushed-back
    /// input first. An EINTR interruption (Ctrl+C with isig enabled)
    /// surfaces as `ErrorKind::Interrupted` so callers can turn it into
    /// a Ctrl+C key event; it must not be retried transparently.
    fn tty_read(&mut self, buf: &mut [u8]) -> io::Result<()> {
        for slot in buf.iter_mut() {
            *slot = if let Some(b) = self.pending.pop_front() {
                b
            } else {
                let mut one = [0; 1];
                match self.stdin.read(&mut one) {
                    Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "tty closed")),
                    Ok(_) => one[0],
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => return Err(e),
                    Err(e) => return Err(e),
                }
            };
        }
        Ok(())
    }

    /// Set the transcript lines used to rebuild the screen after a width
    /// change (re-wrapping shifts every cached row).
    fn set_replay(&mut self, replay: Vec<String>) {
        self.replay = replay;
    }

    fn kitty_key(sequence: &[u8]) -> Option<Key> {
        if !sequence.ends_with(b"u") {
            return None;
        }
        let fields: Vec<u32> = std::str::from_utf8(&sequence[..sequence.len() - 1])
            .ok()?
            .split(';')
            .map(str::parse)
            .collect::<Result<_, _>>()
            .ok()?;
        let codepoint = *fields.first()?;
        let modifiers = fields.get(1).copied().unwrap_or(1).saturating_sub(1);
        let shift = modifiers & 1 != 0;
        let ctrl = modifiers & 4 != 0;

        match codepoint {
            13 => Some(if shift || ctrl { Key::NewLine } else { Key::Enter }),
            127 => Some(Key::Backspace),
            1 if ctrl => Some(Key::Home),
            3 if ctrl => Some(Key::CtrlC),
            4 if ctrl => Some(Key::CtrlD),
            5 if ctrl => Some(Key::End),
            10 if ctrl => Some(Key::NewLine),
            11 if ctrl => Some(Key::CtrlK),
            21 if ctrl => Some(Key::CtrlU),
            23 if ctrl => Some(Key::CtrlW),
            _ if ctrl => match char::from_u32(codepoint)?.to_ascii_lowercase() {
                'a' => Some(Key::Home),
                'c' => Some(Key::CtrlC),
                'd' => Some(Key::CtrlD),
                'e' => Some(Key::End),
                'j' => Some(Key::NewLine),
                'k' => Some(Key::CtrlK),
                'u' => Some(Key::CtrlU),
                'w' => Some(Key::CtrlW),
                _ => Some(Key::Noop),
            },
            _ => char::from_u32(codepoint)
                .filter(|&character| !character.is_control())
                .map(Key::Char),
        }
    }

    fn read_key(&mut self) -> io::Result<Key> {
        // With isig enabled, Ctrl+C raises SIGINT rather than arriving as
        // a 0x03 byte. It can surface two ways: the flag set before we
        // block, or EINTR from read(2) while blocked (SA_RESTART is off).
        if take_interrupt() {
            return Ok(Key::CtrlC);
        }
        let mut byte = [0; 1];
        match self.tty_read(&mut byte) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(Key::CtrlC),
            Err(e) => return Err(e),
        }
        match byte[0] {
            b'\r' => Ok(Key::Enter),
            // Pi binds Ctrl+J as an alias for Shift+Enter. A terminal that
            // sends a plain line feed therefore gets the same newline
            // behavior without confusing it with submit.
            b'\n' => Ok(Key::NewLine),
            8 | 127 => Ok(Key::Backspace),
            1 => Ok(Key::Home),
            5 => Ok(Key::End),
            3 => Ok(Key::CtrlC),
            4 => Ok(Key::CtrlD),
            11 => Ok(Key::CtrlK),
            21 => Ok(Key::CtrlU),
            23 => Ok(Key::CtrlW),
            0x1b => {
                let mut introducer = [0; 1];
                self.tty_read(&mut introducer)?;
                match introducer[0] {
                    // Alt+Enter is the fallback sequence used by terminals
                    // that cannot report Shift+Enter directly.
                    b'\r' | b'\n' => Ok(Key::NewLine),
                    b'[' | b'O' => {
                        let mut sequence = Vec::with_capacity(12);
                        loop {
                            let mut next = [0; 1];
                            self.tty_read(&mut next)?;
                            sequence.push(next[0]);
                            if (0x40..=0x7e).contains(&next[0]) {
                                break;
                            }
                            if sequence.len() >= 32 {
                                return Ok(Key::Noop);
                            }
                        }

                        if let Some(key) = Self::kitty_key(&sequence) {
                            return Ok(key);
                        }
                        Ok(match sequence.as_slice() {
                            // Plain and modified cursor sequences. Kitty's
                            // protocol adds a modifier parameter before the
                            // final cursor letter, so match that suffix too.
                            s if s.ends_with(b"A") => Key::Up,
                            s if s.ends_with(b"B") => Key::Down,
                            s if s.ends_with(b"C") => Key::Right,
                            s if s.ends_with(b"D") => Key::Left,
                            s if s.ends_with(b"H") => Key::Home,
                            s if s.ends_with(b"F") => Key::End,
                            b"1~" | b"7~" => Key::Home,
                            b"3~" => Key::Delete,
                            b"4~" | b"8~" => Key::End,
                            b"200~" => {
                                let end = b"\x1b[201~";
                                let mut pasted = Vec::new();
                                loop {
                                    let mut next = [0; 1];
                                    self.tty_read(&mut next)?;
                                    pasted.push(next[0]);
                                    if pasted.ends_with(end) {
                                        pasted.truncate(pasted.len() - end.len());
                                        break;
                                    }
                                }
                                Key::Paste(String::from_utf8_lossy(&pasted).into_owned())
                            }
                            // Kitty keyboard protocol and xterm
                            // modifyOtherKeys encodings for Enter.
                            b"13u" | b"13;1u" => Key::Enter,
                            b"13;2u" | b"13;2~" | b"27;2;13~" => Key::NewLine,
                            _ => Key::Noop,
                        })
                    }
                    _ => Ok(Key::Noop),
                }
            }
            first => {
                let width = match first {
                    0xc0..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf7 => 4,
                    _ => 1,
                };
                if width == 1 {
                    return Ok(Key::Char(first as char));
                }
                let mut bytes = [0; 4];
                bytes[0] = first;
                self.tty_read(&mut bytes[1..width])?;
                let character = std::str::from_utf8(&bytes[..width])
                    .ok()
                    .and_then(|text| text.chars().next())
                    .unwrap_or('\u{fffd}');
                Ok(Key::Char(character))
            }
        }
    }

    /// Display width of a char; treats anything non-ASCII as width 2 as an
    /// approximation (good enough to avoid wrap-math drift).
    fn char_width(c: char) -> usize {
        if c.is_ascii() { 1 } else { 2 }
    }

    fn prefix_width(cols: usize) -> usize {
        cols.min(2)
    }

    /// Lay out logical input (including explicit newlines) into visual rows.
    /// The final spare column is reserved for the cursor, matching pi's
    /// editor layout and avoiding a deferred terminal wrap at the right edge.
    fn layout(
        line: &[char],
        cursor: usize,
        content_width: usize,
    ) -> (Vec<InputVisualRow>, usize, usize) {
        let cursor = cursor.min(line.len());
        let mut rows = Vec::new();
        let mut start = 0;
        let mut width = 0;
        let mut cursor_row = 0;
        let mut cursor_col = 0;

        let mut wrap_opportunity: Option<(usize, usize)> = None;
        for (index, &character) in line.iter().enumerate() {
            if character == '\n' {
                if index == cursor {
                    cursor_row = rows.len();
                    cursor_col = width;
                }
                rows.push(InputVisualRow { start, end: index, width });
                start = index + 1;
                width = 0;
                wrap_opportunity = None;
                continue;
            }

            let char_width = Self::char_width(character);
            if start < index && width + char_width > content_width {
                if let Some((break_at, break_width)) = wrap_opportunity.take() {
                    if break_at > start
                        && width.saturating_sub(break_width) + char_width <= content_width
                    {
                        rows.push(InputVisualRow {
                            start,
                            end: break_at,
                            width: break_width,
                        });
                        start = break_at;
                        width -= break_width;
                    } else {
                        rows.push(InputVisualRow { start, end: index, width });
                        start = index;
                        width = 0;
                    }
                } else {
                    rows.push(InputVisualRow { start, end: index, width });
                    start = index;
                    width = 0;
                }
            }
            if index == cursor {
                cursor_row = rows.len();
                cursor_col = width;
            }
            width += char_width;

            if character.is_whitespace()
                && matches!(
                    line.get(index + 1),
                    Some(&next) if next != '\n' && !next.is_whitespace()
                )
            {
                wrap_opportunity = Some((index + 1, width));
            }
        }

        if cursor == line.len() {
            cursor_row = rows.len();
            cursor_col = width;
        }
        rows.push(InputVisualRow { start, end: line.len(), width });
        (rows, cursor_row, cursor_col)
    }

    fn cursor_for_visual_row(
        line: &[char],
        rows: &[InputVisualRow],
        row_index: usize,
        desired_col: usize,
    ) -> usize {
        let row = &rows[row_index];
        // A hard-wrapped row does not own the insertion point after its last
        // character; that point belongs to the following visual row.
        let max_col = if row.end < line.len() && line[row.end] == '\n' {
            row.width
        } else if row_index + 1 < rows.len() && rows[row_index + 1].start == row.end {
            row.width.saturating_sub(1)
        } else {
            row.width
        };
        let target_col = desired_col.min(max_col);
        let mut col = 0;
        for (index, &character) in line
            .iter()
            .enumerate()
            .skip(row.start)
            .take(row.end - row.start)
        {
            let next = col + Self::char_width(character);
            if next > target_col {
                return index;
            }
            col = next;
        }
        row.end
    }

    fn move_cursor_vertically(
        &mut self,
        line: &[char],
        cursor: &mut usize,
        direction: isize,
    ) -> bool {
        let prefix_width = Self::prefix_width(self.cols);
        let content_width = self.cols.saturating_sub(prefix_width + 1).max(1);
        let (rows, current_row, current_col) = Self::layout(line, *cursor, content_width);
        let target_row = if direction < 0 {
            if current_row == 0 { return false; }
            current_row - 1
        } else {
            if current_row + 1 >= rows.len() { return false; }
            current_row + 1
        };
        let desired_col = *self.preferred_col.get_or_insert(current_col);
        *cursor = Self::cursor_for_visual_row(line, &rows, target_row, desired_col);
        true
    }

    fn line_bounds(line: &[char], cursor: usize) -> (usize, usize) {
        let cursor = cursor.min(line.len());
        let start = line[..cursor]
            .iter()
            .rposition(|&character| character == '\n')
            .map_or(0, |index| index + 1);
        let end = line[cursor..]
            .iter()
            .position(|&character| character == '\n')
            .map_or(line.len(), |index| cursor + index);
        (start, end)
    }

    /// Ask the terminal for its cursor position via DSR. Bytes consumed
    /// before the actual \x1b[row;colR report (e.g. type-ahead) are pushed
    /// back onto the input queue. Returns row 1 when no report arrives.
    /// ponytail: blocks until the terminal answers; every real terminal
    /// replies to DSR — if a headless oddity ever hangs here, gate it on a
    /// tty check instead of adding read timeouts.
    fn query_cursor_row(&mut self) -> io::Result<usize> {
        self.stdout.write_all(b"\x1b[6n")?;
        self.stdout.flush()?;
        let mut raw: Vec<u8> = Vec::new();
        loop {
            let mut byte = [0; 1];
            self.tty_read(&mut byte)?;
            raw.push(byte[0]);
            if let Some((start, end)) = Self::find_cursor_report(&raw) {
                // Preserve anything that came before the report.
                let prefix: Vec<u8> = raw[..start].to_vec();
                for b in prefix.iter().rev() {
                    self.pending.push_front(*b);
                }
                let body = String::from_utf8_lossy(&raw[start..end]);
                let inner = &body[2..body.len() - 1]; // strip ESC [ ... R
                let row = inner
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .parse::<usize>()
                    .unwrap_or(1);
                return Ok(row.max(1));
            }
            if raw.len() >= 256 {
                break;
            }
        }
        // No well-formed report: keep whatever arrived for the key reader.
        for b in raw.iter().rev() {
            self.pending.push_front(*b);
        }
        Ok(1)
    }

    /// Locate a complete \x1b[row;colR report in buf; returns the byte
    /// range of the report.
    fn find_cursor_report(buf: &[u8]) -> Option<(usize, usize)> {
        for i in 0..buf.len().saturating_sub(1) {
            if buf[i] == 0x1b && buf[i + 1] == b'[' {
                let mut j = i + 2;
                while j < buf.len() && (buf[j].is_ascii_digit() || buf[j] == b';') {
                    j += 1;
                }
                if j < buf.len() && buf[j] == b'R' && j > i + 2 {
                    return Some((i, j + 1));
                }
            }
        }
        None
    }

    /// Erase every row of the previously rendered block (box + chrome).
    fn clear_block(&mut self) -> io::Result<()> {
        if self.block_top == 0 {
            return Ok(());
        }
        let bottom = self.block_top + self.last_rows + CHROME_ROWS; // exclusive
        for row in self.block_top..bottom.min(terminal_size().0.max(1) + 1) {
            write!(self.stdout, "\x1b[{};1H\x1b[K", row)?;
        }
        // Leave the cursor where the block began: that is exactly where new
        // transcript output should continue.
        write!(self.stdout, "\x1b[{};1H", self.block_top)?;
        self.last_rows = 0;
        self.last_cursor_row = 0;
        self.view_start = 0;
        self.preferred_col = None;
        Ok(())
    }

    fn finish_input(&mut self) -> io::Result<()> {
        self.clear_block()?;
        self.stdout.flush()
    }

    /// Render the full block — input box, separator, status line — with
    /// absolute cursor addressing. The block hugs the content: it starts
    /// right below the last transcript row and is pushed down as output
    /// arrives until it docks at the bottom edge of the terminal.
    fn refresh(&mut self, line: &[char], cursor: usize, status: &[String]) -> io::Result<()> {
        let (term_rows, cols) = terminal_size();
        self.cols = cols.max(1);
        let term_rows = term_rows.max(CHROME_ROWS + 1);

        // A width change re-wraps every transcript line: all cached rows and
        // on-screen pixels are void. Rebuild from the transcript replay.
        if self.last_cols != 0 && cols != self.last_cols && !self.replay.is_empty() {
            self.stdout.write_all(b"\x1b[1;1H\x1b[2J")?;
            for l in &self.replay {
                writeln!(self.stdout, "{}", l)?;
            }
            self.stdout.flush()?;
            self.block_top = 0;
            self.last_rows = 0;
            self.resync = true;
        }
        self.last_cols = cols;

        let prefix_width = Self::prefix_width(self.cols);
        let content_width = self.cols.saturating_sub(prefix_width + 1).max(1);
        let (row_ranges, cur_row, cursor_col) = Self::layout(line, cursor, content_width);
        let total_rows = row_ranges.len().max(1);

        // Pi keeps the editor to roughly 30% of the space above the chrome,
        // with a five-line minimum, and scrolls that editor only when the
        // cursor leaves its visible window.
        let viewport = (term_rows.saturating_sub(CHROME_ROWS)).saturating_mul(3) / 10;
        let viewport = viewport.max(5);
        if cur_row < self.view_start {
            self.view_start = cur_row;
        } else if cur_row >= self.view_start + viewport {
            self.view_start = cur_row + 1 - viewport;
        }
        self.view_start = self.view_start.min(total_rows.saturating_sub(viewport));
        let visible_rows = total_rows
            .saturating_sub(self.view_start)
            .min(viewport)
            .max(1);

        let block_h = visible_rows + CHROME_ROWS;
        let max_top = term_rows.saturating_sub(block_h) + 1;

        if self.resync {
            // The transcript scrolled by an unknown amount since the last
            // render; find the real content end and hug it.
            let row = self.query_cursor_row()?;
            self.block_top = row.min(term_rows);
            self.resync = false;
        }
        // Dock at the bottom edge: scroll the transcript up rather than
        // letting the block cross it.
        if self.block_top > max_top {
            let overflow = self.block_top - max_top;
            write!(self.stdout, "\x1b[{};1H", term_rows)?;
            for _ in 0..overflow {
                write!(self.stdout, "\n")?;
            }
            self.block_top = max_top;
            // Everything (including the old block pixels) moved up; the
            // targeted clear below would miss rows above block_top, but those
            // now hold scrolled transcript — never clear those.
            self.last_rows = 0;
        }
        let block_top = self.block_top.max(1);

        // Clear whatever the previous render left behind.
        let old_bottom = block_top + self.last_rows + CHROME_ROWS;
        for row in block_top..old_bottom.min(term_rows + 1) {
            write!(self.stdout, "\x1b[{};1H\x1b[K", row)?;
        }

        let prompt = if prefix_width == 2 { "> " } else { ">" };
        let indent = " ".repeat(prefix_width);
        for (offset, row) in row_ranges
            .iter()
            .enumerate()
            .skip(self.view_start)
            .take(visible_rows)
        {
            let scr = block_top + offset - self.view_start;
            write!(self.stdout, "\x1b[{};1H\x1b[K", scr)?;
            let text: String = line[row.start..row.end].iter().collect();
            let prefix = if offset == self.view_start { prompt } else { &indent };
            write!(
                self.stdout,
                "{}{}{}{}{}",
                PROMPT_COLOR,
                INPUT_COLOR,
                prefix,
                text,
                RESET,
            )?;
        }

        // Separator directly under the box.
        const DIM: &str = "\x1b[2m";
        const CYAN: &str = "\x1b[36m";
        let sep_row = block_top + visible_rows;
        let line_char = "─".repeat(self.cols.saturating_sub(3).max(1));
        write!(
            self.stdout,
            "\x1b[{};1H\x1b[K{}{}{}",
            sep_row,
            DIM,
            line_char,
            RESET,
        )?;
        // Status line under the separator.
        let bar_row = sep_row + 1;
        let body = status_body(status, self.cols);
        let pad = self.cols.saturating_sub(visible_width(&body));
        write!(
            self.stdout,
            "\x1b[{};1H\x1b[K{}{} {}{}{}",
            bar_row,
            DIM,
            CYAN,
            body,
            RESET,
            " ".repeat(pad),
        )?;

        // Park the hardware cursor inside the box at the logical cursor.
        let view_cur_row = cur_row - self.view_start;
        let cursor_column = (prefix_width + cursor_col + 1).min(self.cols);
        write!(
            self.stdout,
            "\x1b[{};{}H",
            block_top + view_cur_row,
            cursor_column,
        )?;
        self.last_rows = visible_rows;
        self.last_cursor_row = view_cur_row;
        self.block_top = block_top;
        self.stdout.flush()
    }

    fn read_line(&mut self, history: &[String], status: &[String]) -> io::Result<Option<String>> {
        let mut line = Vec::new();
        let mut cursor = 0;
        let mut history_index: Option<usize> = None;
        let mut draft = Vec::new();
        self.preferred_col = None;
        // External output (streamed transcript) may have scrolled the
        // screen since the last render; re-locate the content end.
        self.resync = true;
        take_interrupt(); // drop any stale Ctrl+C from a previous turn
        self.refresh(&line, cursor, status)?;

        loop {
            match self.read_key()? {
                Key::Char(character) if !character.is_control() => {
                    line.insert(cursor, character);
                    cursor += 1;
                    history_index = None;
                    self.preferred_col = None;
                    self.refresh(&line, cursor, status)?;
                }
                Key::Paste(text) => {
                    let normalized = text
                        .replace("\r\n", "\n")
                        .replace('\r', "\n")
                        .replace('\t', "    ");
                    let chars: Vec<char> = normalized
                        .chars()
                        .filter(|&character| character == '\n' || !character.is_control())
                        .collect();
                    let count = chars.len();
                    line.splice(cursor..cursor, chars);
                    cursor += count;
                    history_index = None;
                    self.preferred_col = None;
                    self.refresh(&line, cursor, status)?;
                }
                Key::NewLine => {
                    line.insert(cursor, '\n');
                    cursor += 1;
                    history_index = None;
                    self.preferred_col = None;
                    self.refresh(&line, cursor, status)?;
                }
                Key::Enter => {
                    let value: String = line.iter().collect();
                    self.finish_input()?;
                    return Ok(Some(value));
                }
                Key::Backspace => {
                    if cursor > 0 {
                        line.remove(cursor - 1);
                        cursor -= 1;
                        history_index = None;
                        self.preferred_col = None;
                        self.refresh(&line, cursor, status)?;
                    }
                }
                Key::Delete => {
                    if cursor < line.len() {
                        line.remove(cursor);
                        history_index = None;
                        self.preferred_col = None;
                        self.refresh(&line, cursor, status)?;
                    }
                }
                Key::Left => {
                    cursor = cursor.saturating_sub(1);
                    self.preferred_col = None;
                    self.refresh(&line, cursor, status)?;
                }
                Key::Right => {
                    cursor = (cursor + 1).min(line.len());
                    self.preferred_col = None;
                    self.refresh(&line, cursor, status)?;
                }
                Key::Home => {
                    cursor = Self::line_bounds(&line, cursor).0;
                    self.preferred_col = None;
                    self.refresh(&line, cursor, status)?;
                }
                Key::End => {
                    cursor = Self::line_bounds(&line, cursor).1;
                    self.preferred_col = None;
                    self.refresh(&line, cursor, status)?;
                }
                Key::Up => {
                    if self.move_cursor_vertically(&line, &mut cursor, -1) {
                        self.refresh(&line, cursor, status)?;
                    } else {
                        let line_start = Self::line_bounds(&line, cursor).0;
                        if !history.is_empty()
                            && (line.is_empty()
                                || history_index.is_some()
                                || cursor == line_start)
                        {
                            let index = match history_index {
                                Some(index) => index.saturating_sub(1),
                                None => {
                                    draft = line.clone();
                                    history.len() - 1
                                }
                            };
                            history_index = Some(index);
                            line = history[index].chars().collect();
                            cursor = line.len();
                            self.preferred_col = None;
                            self.refresh(&line, cursor, status)?;
                        } else {
                            cursor = line_start;
                            self.preferred_col = None;
                            self.refresh(&line, cursor, status)?;
                        }
                    }
                }
                Key::Down => {
                    if self.move_cursor_vertically(&line, &mut cursor, 1) {
                        self.refresh(&line, cursor, status)?;
                    } else if let Some(index) = history_index {
                        if index + 1 < history.len() {
                            let index = index + 1;
                            history_index = Some(index);
                            line = history[index].chars().collect();
                        } else {
                            history_index = None;
                            line = draft.clone();
                        }
                        cursor = line.len();
                        self.preferred_col = None;
                        self.refresh(&line, cursor, status)?;
                    } else {
                        cursor = Self::line_bounds(&line, cursor).1;
                        self.preferred_col = None;
                        self.refresh(&line, cursor, status)?;
                    }
                }
                Key::CtrlU => {
                    let (start, _) = Self::line_bounds(&line, cursor);
                    if cursor == start && start > 0 {
                        line.remove(start - 1);
                        cursor = start - 1;
                    } else {
                        line.drain(start..cursor);
                        cursor = start;
                    }
                    history_index = None;
                    self.preferred_col = None;
                    self.refresh(&line, cursor, status)?;
                }
                Key::CtrlK => {
                    let (_, end) = Self::line_bounds(&line, cursor);
                    if cursor < end {
                        line.drain(cursor..end);
                    } else if end < line.len() {
                        line.remove(end);
                    }
                    history_index = None;
                    self.preferred_col = None;
                    self.refresh(&line, cursor, status)?;
                }
                Key::CtrlW => {
                    let (start, _) = Self::line_bounds(&line, cursor);
                    if cursor == start && start > 0 {
                        line.remove(start - 1);
                        cursor = start - 1;
                    } else {
                        while cursor > start && line[cursor - 1].is_whitespace() {
                            line.remove(cursor - 1);
                            cursor -= 1;
                        }
                        while cursor > start && !line[cursor - 1].is_whitespace() {
                            line.remove(cursor - 1);
                            cursor -= 1;
                        }
                    }
                    history_index = None;
                    self.preferred_col = None;
                    self.refresh(&line, cursor, status)?;
                }
                Key::CtrlC => {
                    let now = std::time::Instant::now();
                    let is_double = self
                        .last_ctrl_c
                        .map(|t| now.duration_since(t).as_millis() <= 1500)
                        .unwrap_or(false);
                    self.clear_block()?;
                    if is_double {
                        writeln!(self.stdout)?;
                        self.stdout.flush()?;
                        return Ok(None); // exit the REPL
                    }
                    self.last_ctrl_c = Some(now);
                    writeln!(
                        self.stdout,
                        "{}^C press Ctrl+C again to quit{}",
                        ERROR_COLOR, RESET
                    )?;
                    self.stdout.flush()?;
                    return Ok(Some(String::new()));
                }
                Key::CtrlD if line.is_empty() => {
                    self.finish_input()?;
                    return Ok(None);
                }
                Key::CtrlD if cursor < line.len() => {
                    line.remove(cursor);
                    history_index = None;
                    self.preferred_col = None;
                    self.refresh(&line, cursor, status)?;
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod editor_tests {
    use super::{InputVisualRow, TerminalEditor};

    #[test]
    fn layout_tracks_wraps_and_explicit_newlines() {
        let wrapped: Vec<char> = "abcdef".chars().collect();
        let (rows, cursor_row, cursor_col) = TerminalEditor::layout(&wrapped, 4, 4);
        assert_eq!(
            rows,
            vec![
                InputVisualRow { start: 0, end: 4, width: 4 },
                InputVisualRow { start: 4, end: 6, width: 2 },
            ]
        );
        assert_eq!((cursor_row, cursor_col), (1, 0));

        let exact: Vec<char> = "abcd".chars().collect();
        let (rows, cursor_row, cursor_col) = TerminalEditor::layout(&exact, 4, 4);
        assert_eq!(rows.len(), 1);
        assert_eq!((cursor_row, cursor_col), (0, 4));

        let multiline: Vec<char> = "ab\ncd".chars().collect();
        let (_, cursor_row, cursor_col) = TerminalEditor::layout(&multiline, 3, 4);
        assert_eq!((cursor_row, cursor_col), (1, 0));
    }
}

impl Drop for TerminalEditor {
    fn drop(&mut self) {
        let _ = self
            .stdout
            .write_all(b"\x1b[?2004l\x1b[<u\x1b[>4;0m");
        let _ = self.stdout.flush();
        if let Ok(stdin) = self.stdin.try_clone() {
            let _ = Command::new("stty")
                .arg(&self.original_stty)
                .stdin(Stdio::from(stdin))
                .status();
        }
    }
}

fn run_chat_repl(args: &Args) {
    let config = match LlmConfig::from_env(args.base_url.clone(), args.model.clone()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("LLM config error: {}", e);
            eprintln!("Set OPENAI_API_KEY, add api_key to ~/.config/ak/config.json, or run with --tool for raw JSON tool mode.");
            std::process::exit(1);
        }
    };

    let cwd = env::current_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    let mut session = if args.new_session || args.no_session {
        let s = if args.no_session {
            Session::in_memory(cwd)
        } else {
            let mut s = Session::new(cwd.clone(), args.session_name.clone()).expect("failed to create session");
            if let Some(name) = &args.session_name {
                s.set_name(name.clone()).ok();
            }
            s
        };
        s
    } else if let Some(path) = &args.session_path {
        if path.exists() {
            let mut s = Session::from_path(path).expect("failed to open session");
            if let Some(name) = &args.session_name {
                s.set_name(name.clone()).ok();
            }
            s
        } else {
            let mut s = Session::new(cwd.clone(), args.session_name.clone()).expect("failed to create session");
            if let Some(name) = &args.session_name {
                s.set_name(name.clone()).ok();
            }
            s
        }
    } else {
        let mut s = Session::open_or_continue(cwd, None, false).expect("failed to open session");
        if let Some(name) = &args.session_name {
            s.set_name(name.clone()).ok();
        }
        s
    };

    let mut skill_dirs = skill_dirs();
    skill_dirs.extend(args.skill_dirs.iter().cloned());
    let skills = discover_skills(&skill_dirs);

    let mut messages = if session.counter > 0 {
        // Resuming: load persisted messages (skip system, we prepend it fresh).
        if let Some(path) = session.path() {
            load_messages_from_session(path).unwrap_or_default()
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    // Always prepend fresh system prompt.
    messages.insert(0, ChatMessage {
        role: "system".to_string(),
        content: Some(system_prompt(&skills)),
        tool_calls: None,
        tool_call_id: None,
        name: None,
    });

    println!("{}ak agent{}", AGENT_COLOR, RESET);
    if session.is_persisted() {
        println!("session: {} ({} turns)", session.display_name(), session.counter);
    }
    println!("type a prompt and hit enter. /clear resets history. /quit exits. /new starts fresh. /resume lists past sessions. /skill:<name> loads a skill.");

    let mut tool_state = ToolState::load();

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut history = Vec::new();
    let mut editor = match TerminalEditor::new() {
        Ok(editor) => Some(editor),
        Err(e) => {
            eprintln!("terminal line editor unavailable ({}); using basic input", e);
            None
        }
    };

    loop {
        if editor.is_some() {
            println!();
        } else {
            print!("\n{}> {}", PROMPT_COLOR, INPUT_COLOR);
        }
        let _ = stdout.flush();
        let mut status = status_segments(&config, &messages, &tool_state);
        if session.is_persisted() {
            status.push(format!("session: {}", session.display_name()));
        }
        if let Some(editor) = editor.as_mut() {
            editor.set_replay(transcript_replay(&messages));
        }
        let line = match editor.as_mut() {
            Some(editor) => match editor.read_line(&history, &status) {
                Ok(line) => line,
                Err(e) => {
                    eprintln!("terminal input error: {}", e);
                    break;
                }
            },
            None => {
                let mut line = String::new();
                if stdin.read_line(&mut line).is_err() {
                    break;
                }
                Some(line)
            }
        };
        let Some(line) = line else { break };
        let line = line.trim().to_string();
        if line == "/quit" {
            break;
        }
        if line == "/clear" {
            messages.truncate(1);
            history.clear();
            println!("history cleared.");
            continue;
        }
        if line == "/new" {
            messages.truncate(1);
            history.clear();
            let cwd = env::current_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
            let mut new_s = Session::new(cwd, None).expect("failed to create session");
            new_s.append_message(messages[0].clone()).ok(); // system
            session = new_s;
            println!("new session started.");
            continue;
        }
        if line.starts_with("/name ") {
            let name = line["/name ".len()..].trim().to_string();
            if !name.is_empty() {
                session.set_name(name.clone()).ok();
                println!("session name: {}", name);
            }
            continue;
        }
        if line == "/session" {
            println!("session: {}", session.display_name());
            if let Some(path) = session.path() {
                println!("path: {}", path.display());
            }
            println!("turns: {}", session.counter);
            continue;
        }
        if line == "/resume" {
            let cwd = env::current_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
            match Session::list(&cwd) {
                Ok(sessions) if !sessions.is_empty() => {
                    println!("sessions:");
                    for (i, (path, header)) in sessions.iter().enumerate() {
                        let name = header.name.as_ref().map(|n| n.as_str()).unwrap_or("(unnamed)");
                        println!("  {}: {} ({})", i, name, path.display());
                    }
                    println!("  (not implemented: select a session by index or path)");
                }
                _ => println!("no sessions found."),
            }
            continue;
        }
        if line.starts_with("/skill:") {
            let name = line["/skill:".len()..].trim();
            if let Some(skill) = skills.iter().find(|s| s.name == name) {
                let content = fs::read_to_string(&skill.path).unwrap_or_default();
                messages.push(ChatMessage {
                    role: "user".to_string(),
                    content: Some(format!("--- Skill: {} ---\n{}", skill.name, content)),
                    tool_calls: None,
                    tool_call_id: None,
                    name: Some("skill".to_string()),
                });
                println!("loaded skill: {}", skill.name);
            } else {
                println!("skill not found: {}", name);
                println!("available skills:");
                for s in &skills {
                    println!("  - {}", s.name);
                }
            }
            continue;
        }
        if line.is_empty() {
            continue;
        }
        // The editor erases the input box on submit; echo the sent prompt
        // so it stays visible in the transcript.
        if editor.is_some() {
            println!("{}> {}{}{}", PROMPT_COLOR, INPUT_COLOR, line, RESET);
            let _ = stdout.flush();
        }
        history.push(line.clone());
        let user_msg = ChatMessage {
            role: "user".to_string(),
            content: Some(line),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        };
        let before = messages.len();
        messages.push(user_msg);
        match process_turn(&config, &mut messages, &mut tool_state) {
            Ok(_) => {
                println!();
                // Persist every message added during this turn (assistant, tool calls, results).
                for msg in &messages[before..] {
                    session.append_message(msg.clone()).ok();
                }
            }
            Err(e) => {
                eprintln!("{}error: {}{}", ERROR_COLOR, e, RESET);
                // Persist even on error so the failed turn is recorded.
                for msg in &messages[before..] {
                    session.append_message(msg.clone()).ok();
                }
            }
        }
    }
    // Hand the terminal back with the transcript still visible.
    io::stdout().flush().ok();
}

fn run_interactive() {
    eprintln!("ak raw tool mode");
    eprintln!("tools: read, bash, write, edit, grep, find");
    eprintln!("send JSON lines like: {{\"name\":\"read\",\"args\":{{\"path\":\"Cargo.toml\"}}}}");
    eprintln!("empty line quits");

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                println!("{}", json!({"err": format!("read error: {}", e)}));
                continue;
            }
        };
        if line.trim().is_empty() {
            break;
        }
        let parsed: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                println!("{}", json!({"err": format!("json error: {}", e)}));
                continue;
            }
        };
        let name = match parsed.get("name").and_then(Value::as_str) {
            Some(n) => n,
            None => {
                println!("{}", json!({"err": "missing 'name'"}));
                continue;
            }
        };
        let args = match parsed.get("args").and_then(Value::as_object) {
            Some(a) => a.clone(),
            None => Map::new(),
        };
        let result = match execute(name, &args) {
            Ok(out) => json!({"ok": out}),
            Err(e) => json!({"err": e.to_string()}),
        };
        println!("{}", result);
        let _ = stdout.flush();
    }
}

struct Args {
    base_url: Option<String>,
    model: Option<String>,
    session_path: Option<PathBuf>,
    no_session: bool,
    new_session: bool,
    session_name: Option<String>,
    skill_dirs: Vec<PathBuf>,
    rest: Vec<String>,
}

fn parse_args() -> Args {
    let mut base_url: Option<String> = None;
    let mut model: Option<String> = None;
    let mut session_path: Option<PathBuf> = None;
    let mut no_session = false;
    let mut new_session = false;
    let mut session_name: Option<String> = None;
    let mut skill_dirs: Vec<PathBuf> = Vec::new();
    let mut rest: Vec<String> = Vec::new();
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--base-url" {
            match args.next() {
                Some(url) => base_url = Some(url),
                None => {
                    eprintln!("error: --base-url requires a value");
                    std::process::exit(1);
                }
            }
        } else if arg == "--model" {
            match args.next() {
                Some(m) => model = Some(m),
                None => {
                    eprintln!("error: --model requires a value");
                    std::process::exit(1);
                }
            }
        } else if arg == "--session" || arg == "-s" {
            match args.next() {
                Some(p) => session_path = Some(PathBuf::from(p)),
                None => {
                    eprintln!("error: --session requires a value");
                    std::process::exit(1);
                }
            }
        } else if arg == "--no-session" {
            no_session = true;
        } else if arg == "--new" || arg == "-n" {
            new_session = true;
        } else if arg == "--name" {
            match args.next() {
                Some(n) => session_name = Some(n),
                None => {
                    eprintln!("error: --name requires a value");
                    std::process::exit(1);
                }
            }
        } else if arg == "--skill" {
            match args.next() {
                Some(p) => skill_dirs.push(PathBuf::from(p)),
                None => {
                    eprintln!("error: --skill requires a value");
                    std::process::exit(1);
                }
            }
        } else {
            rest.push(arg);
        }
    }
    Args {
        base_url,
        model,
        session_path,
        no_session,
        new_session,
        session_name,
        skill_dirs,
        rest,
    }
}

fn main() {
    install_sigint_handler();
    let args = parse_args();
    if args.rest.len() == 1 && args.rest[0] == "--tool" {
        run_interactive();
    } else if !args.rest.is_empty() {
        let prompt = args.rest.join(" ");
        if let Err(e) = run_one_shot(&prompt, &args) {
            eprintln!("agent error: {}", e);
            std::process::exit(1);
        }
    } else {
        run_chat_repl(&args);
    }
}
