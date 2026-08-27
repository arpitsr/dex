use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Mutex;

mod ui;
mod session;
mod tools;

pub(crate) use session::{load_messages_from_session, Session};
pub(crate) use tools::{execute, execute_to_string};

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
    let mut name = None;
    let mut description = None;
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        let line = line.trim();
        if let Some(val) = line.strip_prefix("name:") {
            name = Some(unquote(val.trim()));
        } else if let Some(val) = line.strip_prefix("description:") {
            description = Some(unquote(val.trim()));
        }
    }
    Some(Skill {
        name: name?,
        description: description.unwrap_or_default(),
        path: path.to_path_buf(),
    })
}

/// Strip a single layer of surrounding quotes (single or double) from a YAML
/// scalar value, so `description: "Short description"` yields the bare value.
fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 {
        let first = s.chars().next().unwrap();
        let last = s.chars().last().unwrap();
        if (first == '"' && last == '"') || (first == '\'' && last == '\'') {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
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

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
static CANCEL_REQUESTED: AtomicBool = AtomicBool::new(false);

pub(crate) fn request_cancel() {
    CANCEL_REQUESTED.store(true, Ordering::SeqCst);
}

fn take_cancel_requested() -> bool {
    CANCEL_REQUESTED.swap(false, Ordering::SeqCst)
}

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
fn take_interrupt() -> bool {
    INTERRUPTED.swap(false, Ordering::SeqCst)
}

const RESET: &str = "\x1b[0m";
const TOOL_INPUT_COLOR: &str = "\x1b[1;33m";
const TOOL_OUTPUT_COLOR: &str = "\x1b[0;34m";
const AGENT_COLOR: &str = "\x1b[1;32m";

// --- Working spinner ---
//
// Braille frames on the current line while the agent works. Transcript
// writers go through with_console(), which erases the frame before
// printing, so real output never interleaves with the animation.

const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

static CONSOLE_LOCK: Mutex<()> = Mutex::new(());
static SPINNER_RUNNING: AtomicBool = AtomicBool::new(false);
static SPINNER_DRAWN: AtomicBool = AtomicBool::new(false);

/// A single streamed line destined for the UI transcript. Plain text (no
/// ANSI) so the UI applies its own styling. Ratatui-agnostic.
#[derive(Debug, Clone)]
pub enum SinkLine {
    Assistant(String),
    ToolInput(String),
    ToolOutput { name: String, summary: String },
    System(String),
    Error(String),
}

/// When set (ratatui UI mode), the agent routes streamed output here instead
/// of printing to the console.
static CONSOLE_SINK: Mutex<Option<mpsc::Sender<SinkLine>>> = Mutex::new(None);

/// Enable/disable the console sink (used by the ratatui UI path).
pub fn set_console_sink(sink: Option<mpsc::Sender<SinkLine>>) {
    *CONSOLE_SINK.lock().unwrap_or_else(|e| e.into_inner()) = sink;
}

/// Clone of the active sink sender, if any.
pub fn console_sink() -> Option<mpsc::Sender<SinkLine>> {
    CONSOLE_SINK.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Erase the drawn spinner frame, if any. Caller holds CONSOLE_LOCK.
fn erase_spinner_frame() {
    if SPINNER_DRAWN.swap(false, Ordering::SeqCst) {
        let mut out = io::stdout();
        let _ = out.write_all(b"\r\x1b[2K");
        let _ = out.flush();
    }
}

/// Run `f` with the spinner suspended so output never interleaves with frames.
fn with_console(f: impl FnOnce()) {
    if console_sink().is_some() {
        return; // UI mode: output is routed through the sink; skip console IO
    }
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

/// Compact single-line summary of a tool's arguments for the REPL transcript.
/// Pulls the primary arg (path/command/pattern) instead of dumping raw JSON.
fn short_arg(name: &str, input: &str) -> String {
    let obj = serde_json::from_str::<Value>(input)
        .ok()
        .and_then(|v| v.as_object().cloned());
    let get = |k: &str| obj.as_ref().and_then(|o| o.get(k)).and_then(|x| x.as_str());
    let primary = match name {
        "read" | "write" | "edit" | "grep" | "glob" => get("path")
            .or_else(|| get("file"))
            .or_else(|| get("pattern"))
            .or_else(|| get("glob")),
        "bash" => get("command"),
        _ => None,
    };
    let s = primary.filter(|s| !s.is_empty()).unwrap_or(input);
    let s = s.lines().next().unwrap_or(s).trim();
    let limit = s.char_indices().nth(80).map(|(i, _)| i).unwrap_or(s.len());
    s[..limit].to_string()
}

/// First non-empty, trimmed line of a tool result, truncated — a one-line
/// confirmation for the REPL transcript instead of the full output.
fn one_line_summary(text: &str) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let line = line.trim();
    let limit = line.char_indices().nth(120).map(|(i, _)| i).unwrap_or(line.len());
    line[..limit].to_string()
}

/// Human-sized result for the TUI. The full result still goes to the model;
/// the transcript only needs enough information to explain what happened.
fn tool_result_summary(name: &str, text: &str) -> String {
    if text.starts_with("Error:") || text.contains("[exit ") {
        return format!("failed · {}", one_line_summary(text));
    }
    let lines = text.lines().filter(|line| !line.trim().is_empty()).count();
    let bytes = text.len();
    match name {
        "read" => format!("ok · {lines} lines · {bytes} bytes"),
        "grep" => format!("ok · {lines} matches"),
        "find" => format!("ok · {lines} entries"),
        "bash" => match one_line_summary(text).as_str() {
            "" => "ok".to_string(),
            first => format!("ok · {first}"),
        },
        _ => format!("ok · {}", one_line_summary(text)),
    }
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
        if let Ok(mut child) = cmd.arg("-").stdin(Stdio::piped()).spawn() {
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
        let _ = bin;
    }
    // Fallback: use a small lexer so terminals without bat still get useful
    // syntax colours (strings/comments are consumed before keywords).
    print_ansi_highlighted_code(lang, body);
}

fn print_ansi_highlighted_code(lang: &str, body: &str) {
    const KEYWORD: &str = "\x1b[1;35m";
    const STRING: &str = "\x1b[0;32m";
    const NUMBER: &str = "\x1b[0;33m";
    const COMMENT: &str = "\x1b[2;37m";
    const PUNCT: &str = "\x1b[0;36m";
    let lang = lang.to_ascii_lowercase();
    let hash_comments = matches!(lang.as_str(), "python" | "py" | "ruby" | "rb" | "bash" | "sh" | "yaml" | "yml" | "toml" | "perl");
    let keywords = match lang.as_str() {
        "rust" | "rs" => "as break const continue crate else enum extern false fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait true type unsafe use where while async await dyn",
        "python" | "py" => "and as assert async await break class continue def del elif else except False finally for from global if import in is lambda None not or pass raise return True try while with yield",
        "javascript" | "js" | "typescript" | "ts" => "as async await break case catch class const continue default delete else export extends false finally for function if import in let new null of return static super this throw true try typeof var while with yield",
        "go" | "golang" => "break case const continue default defer else fallthrough for func go goto if import interface map package range return select struct switch type var",
        _ => "class const def else false fn for function if import let match mut new null pub return static struct true try type while async await",
    };
    let is_keyword = |word: &str| keywords.split_whitespace().any(|k| k == word);

    for line in body.split_inclusive('\n') {
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if (c == '/' && i + 1 < chars.len() && chars[i + 1] == '/')
                || (c == '#' && hash_comments)
                || (c == '-' && i + 1 < chars.len() && chars[i + 1] == '-') {
                print!("{}{}{}", COMMENT, chars[i..].iter().collect::<String>(), RESET);
                break;
            } else if matches!(c, '\"' | '\'' | '`') {
                let quote = c;
                let start = i;
                i += 1;
                while i < chars.len() {
                    if chars[i] == '\\' { i += 2; continue; }
                    let closed = chars[i] == quote;
                    i += 1;
                    if closed { break; }
                }
                print!("{}{}{}", STRING, chars[start..i.min(chars.len())].iter().collect::<String>(), RESET);
            } else if c.is_ascii_digit() {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || matches!(chars[i], '.' | '_')) { i += 1; }
                print!("{}{}{}", NUMBER, chars[start..i].iter().collect::<String>(), RESET);
            } else if c.is_ascii_alphabetic() || c == '_' {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') { i += 1; }
                let word: String = chars[start..i].iter().collect();
                if is_keyword(&word) { print!("{}{}{}", KEYWORD, word, RESET); } else { print!("{}", word); }
            } else {
                i += 1;
                if "{}[]()<>;:,.=+-*/%!&|?".contains(c) { print!("{}{}{}", PUNCT, c, RESET); } else { print!("{}", c); }
            }
        }
    }
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

/// Width of a string as displayed, ignoring ANSI escape sequences.
/// dividers, truncated to fit the terminal width.
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
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ApiProtocol {
    ChatCompletions,
    Responses,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Provider {
    OpenCode,
    OpenAiCodex,
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
    provider: Option<String>,
    api_key: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
    api: Option<String>,
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

#[derive(Clone)]
struct LlmConfig {
    provider: Provider,
    api_key: String,
    base_url: String,
    model: String,
    api: ApiProtocol,
    account_id: Option<String>,
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
        let provider_name = env::var("AK_PROVIDER")
            .ok()
            .or(file.provider)
            .unwrap_or_else(|| "opencode".to_string());
        let provider = match provider_name.as_str() {
            "opencode" => Provider::OpenCode,
            "openai-codex" | "codex" => Provider::OpenAiCodex,
            other => return Err(format!(
                "unsupported provider '{}'; use opencode or openai-codex",
                other
            ).into()),
        };
        let model = model_override
            .or_else(|| env::var("OPENAI_MODEL").ok())
            .or(file.model)
            .unwrap_or_else(|| match provider {
                Provider::OpenCode => "gpt-5.6-luna".to_string(),
                Provider::OpenAiCodex => "gpt-5.6-luna".to_string(),
            });
        let env_base_url = env::var("OPENAI_BASE_URL").ok().filter(|v| !v.is_empty());
        let base_url = match provider {
            Provider::OpenCode => base_url_override
                .or(env_base_url)
                .or(file.base_url)
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            Provider::OpenAiCodex => base_url_override
                .or(env_base_url)
                .unwrap_or_else(|| "https://chatgpt.com/backend-api/codex".to_string()),
        };
        let api_name = env::var("OPENAI_API")
            .ok()
            .or(file.api)
            .unwrap_or_else(|| match provider {
                Provider::OpenCode => "openai-responses".to_string(),
                Provider::OpenAiCodex => "openai-responses".to_string(),
            });
        let api = match api_name.as_str() {
            "responses" | "openai-responses" => ApiProtocol::Responses,
            "chat" | "chat-completions" | "openai-completions" => {
                ApiProtocol::ChatCompletions
            }
            other => return Err(format!(
                "unsupported api '{}'; use openai-completions or openai-responses",
                other
            ).into()),
        };
        let (api_key, account_id) = match provider {
            Provider::OpenCode => (
                env::var("OPENAI_API_KEY")
                    .ok()
                    .or(file.api_key)
                    .ok_or("OPENAI_API_KEY not set and no api_key in config file")?,
                None,
            ),
            Provider::OpenAiCodex => load_codex_credentials()?,
        };
        Ok(Self {
            provider,
            api_key,
            base_url,
            model,
            api,
            account_id,
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

#[derive(Deserialize)]
struct CodexAuthFile {
    tokens: Option<CodexTokens>,
}

#[derive(Deserialize)]
struct CodexTokens {
    access_token: String,
    account_id: Option<String>,
}

fn load_codex_credentials() -> Result<(String, Option<String>), Box<dyn std::error::Error>> {
    if let Some(access_token) = env::var_os("CODEX_ACCESS_TOKEN") {
        let access_token = access_token.to_string_lossy().trim().to_string();
        if !access_token.is_empty() {
            return Ok((
                access_token,
                env::var("CODEX_ACCOUNT_ID").ok().filter(|id| !id.is_empty()),
            ));
        }
    }
    let path = env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
        .ok_or("HOME is not set; cannot locate Codex credentials")?
        .join("auth.json");
    let contents = fs::read_to_string(&path).map_err(|e| {
        format!("could not read Codex credentials {}: {}", path.display(), e)
    })?;
    let auth: CodexAuthFile = serde_json::from_str(&contents)
        .map_err(|e| format!("invalid Codex credentials {}: {}", path.display(), e))?;
    let tokens = auth.tokens.ok_or("Codex auth.json has no OAuth tokens; run `codex --login`")?;
    if tokens.access_token.trim().is_empty() {
        return Err("Codex auth.json contains an empty access token".into());
    }
    Ok((tokens.access_token, tokens.account_id))
}

fn authenticated_request(
    request: reqwest::blocking::RequestBuilder,
    config: &LlmConfig,
) -> reqwest::blocking::RequestBuilder {
    let request = request.bearer_auth(&config.api_key);
    if config.provider == Provider::OpenAiCodex {
        let request = request.header("originator", "codex_cli_rs");
        if let Some(account_id) = &config.account_id {
            return request.header("ChatGPT-Account-ID", account_id);
        }
        return request;
    }
    request
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
        if console_sink().is_some() {
            // Sink mode: no spinner to erase; stream directly.
            self.feed_line_inner(line);
        } else {
            with_console(|| self.feed_line_inner(line));
        }
    }

    fn feed_line_inner(&mut self, line: &str) {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            if self.in_code {
                if let Some(sink) = console_sink() {
                    sink.send(SinkLine::Assistant(format!(
                        "```{}:\n{}\n```",
                        self.code_lang, self.code_body
                    ))).ok();
                } else {
                    print_code_block(&self.code_lang, &self.code_body);
                }
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
            if let Some(sink) = console_sink() {
                sink.send(SinkLine::Assistant(line.to_string())).ok();
            } else {
                termimad::print_text(&format!("{}\n", line));
            }
        }
    }

    fn finish(self) {
        if self.in_code && !self.code_body.is_empty() {
            if let Some(sink) = console_sink() {
                sink.send(SinkLine::Assistant(format!(
                    "```{}:\n{}\n```",
                    self.code_lang, self.code_body
                ))).ok();
            } else {
                with_console(|| print_code_block(&self.code_lang, &self.code_body));
            }
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

fn call_chat_completions(
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
    };

    for attempt in 0..=MAX_RETRIES {
        let request = config
            .client
            .post(format!("{}/chat/completions", config.base_url));
        let resp = match authenticated_request(request, config).json(&req).send() {
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

        return read_stream(resp);
    }

    unreachable!()
}

fn responses_input(messages: &[ChatMessage]) -> (Option<String>, Vec<Value>) {
    let mut instructions = Vec::new();
    let mut input = Vec::new();
    for message in messages {
        if message.role == "system" {
            if let Some(content) = &message.content {
                instructions.push(content.clone());
            }
            continue;
        }
        if message.role == "tool" {
            input.push(json!({
                "type": "function_call_output",
                "call_id": message.tool_call_id.clone().unwrap_or_default(),
                "output": message.content.clone().unwrap_or_default(),
            }));
            continue;
        }
        if message.role == "assistant" {
            if let Some(content) = &message.content {
                if !content.is_empty() {
                    input.push(json!({ "role": "assistant", "content": content }));
                }
            }
            for call in message.tool_calls.as_deref().unwrap_or_default() {
                input.push(json!({
                    "type": "function_call",
                    "call_id": call.id,
                    "name": call.function.name,
                    "arguments": call.function.arguments,
                }));
            }
            continue;
        }
        input.push(json!({
            "role": message.role,
            "content": message.content.clone().unwrap_or_default(),
        }));
    }
    (if instructions.is_empty() { None } else { Some(instructions.join("\n\n")) }, input)
}

fn responses_tools() -> Vec<Value> {
    tools_schema()
        .into_iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.function.name,
                "description": tool.function.description,
                "parameters": tool.function.parameters,
            })
        })
        .collect()
}

fn response_tool_call(calls: &mut Vec<LlmToolCall>, index: usize, item: &Value) {
    while calls.len() <= index {
        calls.push(LlmToolCall {
            id: String::new(),
            call_type: "function".to_string(),
            function: FunctionCall { name: String::new(), arguments: String::new() },
        });
    }
    let call = &mut calls[index];
    if let Some(id) = item.get("call_id").or_else(|| item.get("id")).and_then(Value::as_str) {
        call.id = id.to_string();
    }
    if let Some(name) = item.get("name").and_then(Value::as_str) {
        call.function.name = name.to_string();
    }
    if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
        call.function.arguments = arguments.to_string();
    }
}

fn response_call_index(calls: &[LlmToolCall], index: usize, item: &Value) -> usize {
    item.get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
        .and_then(|id| calls.iter().position(|call| call.id == id))
        .unwrap_or(index)
}

fn read_responses_stream(response: reqwest::blocking::Response) -> Result<(ChatMessage, Option<u64>), Box<dyn std::error::Error>> {
    let mut reader = BufReader::new(response);
    let mut line = String::new();
    let mut content = String::new();
    let mut pending = String::new();
    let mut printer = StreamPrinter::new();
    let mut tool_calls = Vec::new();
    let mut response_items: HashMap<String, usize> = HashMap::new();
    let mut pending_arguments: HashMap<String, String> = HashMap::new();
    let mut usage_tokens = None;

    loop {
        if take_interrupt() {
            with_console(|| println!());
            io::stdout().flush()?;
            return Err("interrupted".into());
        }
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let Some(data) = line.strip_prefix("data:") else { continue };
        let data = data.trim();
        if data == "[DONE]" || data.is_empty() { continue; }
        let event: Value = serde_json::from_str(data)?;
        let event_type = event.get("type").and_then(Value::as_str).unwrap_or_default();
        match event_type {
            "response.output_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    content.push_str(delta);
                    pending.push_str(delta);
                    while let Some(pos) = pending.find('\n') {
                        let complete: String = pending.drain(..=pos).collect();
                        printer.feed_line(complete.trim_end_matches('\n'));
                    }
                    io::stdout().flush()?;
                }
            }
            "response.output_item.added" => {
                if event.pointer("/item/type").and_then(Value::as_str) == Some("function_call") {
                    let item = event.get("item").unwrap_or(&Value::Null);
                    let index = response_call_index(
                        &tool_calls,
                        event.get("output_index").and_then(Value::as_u64).unwrap_or(tool_calls.len() as u64) as usize,
                        item,
                    );
                    response_tool_call(&mut tool_calls, index, item);
                    if let Some(item_id) = item.get("id").and_then(Value::as_str) {
                        response_items.insert(item_id.to_string(), index);
                        if let Some(arguments) = pending_arguments.remove(item_id) {
                            tool_calls[index].function.arguments.push_str(&arguments);
                        }
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    let key = event
                        .get("item_id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            format!(
                                "output:{}",
                                event.get("output_index").and_then(Value::as_u64).unwrap_or(0)
                            )
                        });
                    if let Some(index) = event
                        .get("item_id")
                        .and_then(Value::as_str)
                        .and_then(|id| response_items.get(id).copied())
                    {
                        tool_calls[index].function.arguments.push_str(delta);
                    } else {
                        pending_arguments.entry(key).or_default().push_str(delta);
                    }
                }
            }
            "response.output_item.done" => {
                if event.pointer("/item/type").and_then(Value::as_str) == Some("function_call") {
                    let item = event.get("item").unwrap_or(&Value::Null);
                    let index = response_call_index(
                        &tool_calls,
                        event.get("output_index").and_then(Value::as_u64).unwrap_or(tool_calls.len() as u64) as usize,
                        item,
                    );
                    response_tool_call(&mut tool_calls, index, item);
                    if let Some(item_id) = item.get("id").and_then(Value::as_str) {
                        response_items.insert(item_id.to_string(), index);
                        if let Some(arguments) = pending_arguments.remove(item_id) {
                            tool_calls[index].function.arguments.push_str(&arguments);
                        }
                    }
                }
            }
            "response.completed" | "response.done" => {
                if let Some(usage) = event.pointer("/response/usage") {
                    usage_tokens = usage.get("input_tokens").and_then(Value::as_u64);
                }
            }
            _ => {}
        }
    }

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
    tool_calls.retain(|call| !call.id.is_empty() && !call.function.name.is_empty());
    Ok((ChatMessage {
        role: "assistant".to_string(),
        content: (!content.is_empty()).then_some(content),
        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
        tool_call_id: None,
        name: None,
    }, usage_tokens))
}

fn call_responses(
    config: &LlmConfig,
    messages: &[ChatMessage],
    with_tools: bool,
) -> Result<(ChatMessage, Option<u64>), Box<dyn std::error::Error>> {
    let (instructions, input) = responses_input(messages);
    let mut body = json!({
        "model": config.model,
        "input": input,
        "stream": true,
        "store": false,
    });
    if let Some(instructions) = instructions {
        body["instructions"] = json!(instructions);
    }
    if with_tools {
        body["tools"] = json!(responses_tools());
    }
    if let Some(effort) = &config.thinking_effort {
        body["reasoning"] = json!({ "effort": effort });
    }
    const MAX_RETRIES: u32 = 3;
    for attempt in 0..=MAX_RETRIES {
        let request = config.client.post(format!("{}/responses", config.base_url));
        let resp = match authenticated_request(request, config).json(&body).send() {
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
            let error_body = resp.text()?;
            let retryable = status.as_u16() == 408 || status.as_u16() == 429 || status.is_server_error();
            if retryable && attempt < MAX_RETRIES {
                let delay = Duration::from_millis(500 * 2u64.pow(attempt));
                with_console(|| eprintln!("[llm] API error {}: retrying in {:?}", status, delay));
                thread::sleep(delay);
                continue;
            }
            return Err(format!("API error: {}", error_body).into());
        }
        return read_responses_stream(resp);
    }
    unreachable!()
}

fn call_llm(
    config: &LlmConfig,
    messages: &[ChatMessage],
    with_tools: bool,
) -> Result<(ChatMessage, Option<u64>), Box<dyn std::error::Error>> {
    match config.api {
        ApiProtocol::ChatCompletions => call_chat_completions(config, messages, with_tools),
        ApiProtocol::Responses => call_responses(config, messages, with_tools),
    }
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
        .map(message_to_transcript)
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
    steering_rx: Option<&mpsc::Receiver<String>>,
    steering_accepted_tx: Option<&mpsc::Sender<String>>,
) -> Result<String, Box<dyn std::error::Error>> {
    // Spins while the agent works; erased automatically on return.
    let _working = SpinnerGuard::start("Working");
    let mut last_tools: Vec<String> = Vec::new();
    let mut last_usage: Option<u64> = state.last_usage;

    for iteration in 0..MAX_TOOL_ITERATIONS {
        if take_cancel_requested() {
            return Err("cancelled by user".into());
        }
        // Steering is consumed between turns/tool batches, while the worker
        // still owns the conversation state. This avoids concurrent mutation
        // of `messages` while allowing the UI to accept input immediately.
        if let Some(rx) = steering_rx {
            while let Ok(steering) = rx.try_recv() {
                if let Some(accepted) = &steering_accepted_tx {
                    let _ = accepted.send(steering.clone());
                }
                messages.push(ChatMessage {
                    role: "user".to_string(),
                    content: Some(steering),
                    tool_calls: None,
                    tool_call_id: None,
                    name: Some("steering".to_string()),
                });
            }
        }
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
                if let Some(sink) = console_sink() {
                    let _ = sink.send(SinkLine::ToolInput(format!(
                        "{} {}",
                        call.function.name, short_arg(&name, &input)
                    )));
                } else {
                    with_console(|| {
                        eprintln!(
                            "{}[tool input] {} {}{}",
                            TOOL_INPUT_COLOR,
                            call.function.name,
                            terminal_preview(&input),
                            RESET
                        );
                    });
                }

                let cacheable = matches!(name.as_str(), "read" | "grep" | "find");
                let mut cache_hit = false;
                let result = if repeated_count >= 3 {
                    "Error: repeated identical tool call; choose a different action or finish."
                        .to_string()
                } else if cacheable {
                    if let Some(cached) = state.cache.get(&cache_key) {
                        cache_hit = true;
                        if console_sink().is_none() {
                            with_console(|| eprintln!("{}[tool cache hit]{}", TOOL_OUTPUT_COLOR, RESET));
                        }
                        cached.clone()
                    } else {
                        state.insert(cache_key, result.clone());
                        result
                    }
                } else {
                    if matches!(name.as_str(), "write" | "edit") {
                        state.clear();
                    }
                    result
                };
                if let Some(sink) = console_sink() {
                    let mut summary = tool_result_summary(&name, &result);
                    if cache_hit {
                        summary = format!("cached · {summary}");
                    }
                    let _ = sink.send(SinkLine::ToolOutput { name: name.clone(), summary });
                } else {
                    with_console(|| {
                        eprintln!(
                            "{}[tool output] {}:\n{}{}",
                            TOOL_OUTPUT_COLOR,
                            name,
                            terminal_preview(&result),
                            RESET
                        );
                    });
                }
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
            if let Some(rx) = steering_rx {
                let steering: Vec<String> = rx.try_iter().collect();
                if !steering.is_empty() {
                    for content in steering {
                        if let Some(accepted) = &steering_accepted_tx {
                            let _ = accepted.send(content.clone());
                        }
                        messages.push(ChatMessage {
                            role: "user".to_string(),
                            content: Some(content),
                            tool_calls: None,
                            tool_call_id: None,
                            name: Some("steering".to_string()),
                        });
                    }
                    state.last_usage = last_usage;
                    continue;
                }
            }
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
    let result = process_turn(&config, &mut messages, &mut state, None, None);
    if !args.no_session {
        let cwd = env::current_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
        // A corrupt/truncated resume target must not abort the prompt — fall
        // back to a fresh session instead of propagating the error with `?`.
        let mut session = match if args.new_session {
            Session::new(cwd.clone(), args.session_name.clone())
        } else {
            Session::open_or_continue(cwd.clone(), args.session_path.as_deref(), false)
        } {
            Ok(mut s) => {
                if let Some(name) = &args.session_name {
                    s.set_name(name.clone()).ok();
                }
                s
            }
            Err(e) => {
                eprintln!("[session] could not open session ({}); starting fresh", e);
                match Session::new(cwd, args.session_name.clone()) {
                    Ok(mut s) => {
                        if let Some(name) = &args.session_name {
                            s.set_name(name.clone()).ok();
                        }
                        s
                    }
                    Err(e2) => {
                        eprintln!("[session] could not create session ({}); skipping save", e2);
                        // Continue without persistence — the prompt still ran.
                        for msg in &messages[1..] {
                            let _ = msg;
                        }
                        return result.map(|_| ());
                    }
                }
            }
        };
        for msg in &messages[1..] { // skip system
            session.append_message(msg.clone()).ok();
        }
    }
    println!();
    result.map(|_| ())
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
    } else if let Err(e) = ui::run_ratatui_repl(&args) {
        eprintln!("ui error: {}", e);
        std::process::exit(1);
    }
}
