use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

const RESET: &str = "\x1b[0m";
const PROMPT_COLOR: &str = "\x1b[1;36m";
const INPUT_COLOR: &str = "\x1b[1;37m";
const TOOL_INPUT_COLOR: &str = "\x1b[1;33m";
const TOOL_OUTPUT_COLOR: &str = "\x1b[0;34m";
const AGENT_COLOR: &str = "\x1b[1;32m";
const ERROR_COLOR: &str = "\x1b[1;31m";

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
// A persistent one-line status bar pinned to the bottom row of the
// terminal (like codex/claude/pi). It works by installing an ANSI scroll
// region covering every row except the last: all normal output (including
// streamed LLM text and tool output) scrolls inside that region and can
// never overwrite the bar. The bar itself is drawn with absolute cursor
// positioning, so it stays stuck to the bottom no matter how much input
// or output accumulates. The terminal size is re-queried before each
// redraw, so the placement self-corrects on resize.

struct StatusBar {
    rows: usize,
    cols: usize,
}

impl StatusBar {
    /// Query the terminal geometry via `stty size`. Returns None when the
    /// size cannot be determined (not a tty, headless, etc.) so callers can
    /// degrade gracefully to plain scrolling output.
    fn new() -> Option<Self> {
        let out = Command::new("stty").arg("size").stdin(Stdio::inherit()).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut parts = text.split_whitespace();
        let rows = parts.next()?.parse::<usize>().ok()?;
        let cols = parts.next()?.parse::<usize>().ok()?;
        // Need room for: scroll region + input top/bottom borders + spacer
        // row + status bar.
        if rows < 8 || cols == 0 {
            return None; // too small to be useful
        }
        Some(Self { rows, cols })
    }

    fn query_size(&mut self) {
        if let Some(probe) = StatusBar::new() {
            self.rows = probe.rows;
            self.cols = probe.cols;
        }
    }

    /// Number of rows below the scroll region: separator line + status
    /// bar.
    const RESERVED_ROWS: usize = 2;

    /// Reserve the last two rows (separator + status bar) by limiting the
    /// scrolling region, then draw the separator without moving the caller's
    /// cursor.
    fn install(&self) {
        let region_end = self.rows - Self::RESERVED_ROWS;
        print!("\x1b7\x1b[1;{}r", region_end);
        self.draw_separator();
        print!("\x1b8");
        io::stdout().flush().ok();
    }

    /// Draw a single dim separator line directly below the scroll region.
    /// The caller owns cursor save/restore so this helper can be nested in
    /// `install` safely.
    fn draw_separator(&self) {
        const DIM: &str = "\x1b[2m";
        let line = "─".repeat(self.cols.saturating_sub(3).max(1));
        print!(
            "\x1b[{};1H\x1b[K{}{}{}",
            self.rows - 1,
            DIM,
            line,
            RESET,
        );
    }

    /// Restore the full-screen scroll region on exit.
    fn uninstall(&self) {
        print!("\x1b[r");
        io::stdout().flush().ok();
    }

    /// Redraw the status bar on the bottom row with the given segment
    /// strings, left-aligned and separated by dim dividers, truncated to
    /// fit the terminal width. The cursor is saved/restored so this is
    /// safe to call at any point.
    fn draw(&mut self, segments: &[String]) {
        self.query_size();
        // Reinstall in case of resize; cheap and idempotent.
        self.install();

        const DIM: &str = "\x1b[2m";
        const CYAN: &str = "\x1b[36m";
        let divider = format!("{} │ {}", DIM, RESET);

        // Build the visible string while tracking display width. ANSI
        // escape sequences count for zero width.
        let visible_width = |s: &str| -> usize {
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
        };
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

            if width + 4 >= self.cols {
                break;
            }
        }
        // Hard-truncate if still too wide (count only visible chars).
        if width >= self.cols {
            let budget = self.cols.saturating_sub(2);
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
            body = visible;
            width = budget.min(w);
        }
        let pad = self.cols.saturating_sub(width);

        print!(
            "\x1b7\x1b[{};1H\x1b[K{}{} {}{}{}\x1b8",
            self.rows,
            DIM,
            CYAN,
            body,
            RESET,
            " ".repeat(pad),
        );
        io::stdout().flush().ok();
    }
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

fn system_prompt() -> String {
    "You are a coding agent. Use the provided tools to help the user. \
     Prefer reading files before editing. \
     When editing, oldText must match exactly one occurrence in the file. \
     Stop using tools once the requested work is complete. \
     Be concise.".to_string()
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
                .or_else(|| env::var("OPENAI_BASE_URL").ok())
                .or(file.base_url)
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
            print_code_block(&self.code_lang, &self.code_body);
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
        println!();
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
                eprintln!("[llm] request failed: {}; retrying in {:?}", e, delay);
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
                eprintln!("[llm] API error {}: retrying in {:?}", status, delay);
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
            eprintln!("[history] summarization failed ({}); truncating instead", e);
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
        let (message, usage) = call_llm(config, messages, true)?;
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
                eprintln!(
                    "{}[tool input] {} {}{}",
                    TOOL_INPUT_COLOR,
                    call.function.name,
                    terminal_preview(&input),
                    RESET
                );

                let cacheable = matches!(name.as_str(), "read" | "grep" | "find");
                let result = if repeated_count >= 3 {
                    "Error: repeated identical tool call; choose a different action or finish."
                        .to_string()
                } else if cacheable {
                    if let Some(cached) = state.cache.get(&cache_key) {
                        eprintln!("{}[tool cache hit]{}", TOOL_OUTPUT_COLOR, RESET);
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
                eprintln!(
                    "{}[tool output] {}:\n{}{}",
                    TOOL_OUTPUT_COLOR,
                    name,
                    terminal_preview(&result),
                    RESET
                );
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
    base_url: Option<String>,
    model: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = LlmConfig::from_env(base_url, model)?;
    let mut messages = vec![
        ChatMessage { role: "system".to_string(), content: Some(system_prompt()), tool_calls: None, tool_call_id: None, name: None },
        ChatMessage { role: "user".to_string(), content: Some(prompt.to_string()), tool_calls: None, tool_call_id: None, name: None },
    ];
    let mut state = ToolState::load();
    process_turn(&config, &mut messages, &mut state)?;
    println!();
    Ok(())
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
    /// Number of visible rows occupied by the editor at the last refresh.
    last_rows: usize,
    /// Cursor row within the rendered editor block at the last refresh.
    last_cursor_row: usize,
    /// First visual row currently shown when the input is taller than the
    /// editor viewport (the editor scrolls internally).
    view_start: usize,
    /// Preferred visual column for consecutive vertical cursor moves.
    preferred_col: Option<usize>,
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
                "-icanon",
                "-echo",
                "-isig",
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
        // Use the same keyboard setup as pi: Kitty when supported, with
        // xterm modifyOtherKeys as the fallback.
        stdout.write_all(b"\x1b[?2004h\x1b[>7u\x1b[?u\x1b[c\x1b[>4;2m")?;
        stdout.flush()?;
        Ok(Self {
            stdin,
            stdout,
            original_stty,
            last_ctrl_c: None,
            cols: terminal_cols(),
            last_rows: 1,
            last_cursor_row: 0,
            view_start: 0,
            preferred_col: None,
        })
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
        let mut byte = [0; 1];
        self.stdin.read_exact(&mut byte)?;
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
                self.stdin.read_exact(&mut introducer)?;
                match introducer[0] {
                    // Alt+Enter is the fallback sequence used by terminals
                    // that cannot report Shift+Enter directly.
                    b'\r' | b'\n' => Ok(Key::NewLine),
                    b'[' | b'O' => {
                        let mut sequence = Vec::with_capacity(12);
                        loop {
                            let mut next = [0; 1];
                            self.stdin.read_exact(&mut next)?;
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
                                    self.stdin.read_exact(&mut next)?;
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
                self.stdin.read_exact(&mut bytes[1..width])?;
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

    fn clear_rendered_input(&mut self) -> io::Result<()> {
        // The hardware cursor is left on the logical cursor row after every
        // refresh, so use that saved row rather than assuming it is on the
        // last line of the block.
        if self.last_cursor_row > 0 {
            write!(self.stdout, "\x1b[{}A", self.last_cursor_row)?;
        }
        write!(self.stdout, "\r")?;
        for row in 0..self.last_rows {
            write!(self.stdout, "\x1b[K")?;
            if row + 1 < self.last_rows {
                write!(self.stdout, "\x1b[B\r")?;
            }
        }
        self.last_rows = 1;
        self.last_cursor_row = 0;
        self.view_start = 0;
        self.preferred_col = None;
        Ok(())
    }

    fn finish_input(&mut self) -> io::Result<()> {
        self.clear_rendered_input()?;
        write!(self.stdout, "\r\n")?;
        self.stdout.flush()
    }

    fn refresh(&mut self, line: &[char], cursor: usize) -> io::Result<()> {
        // Re-check terminal size in case of resize.
        let (term_rows, cols) = terminal_size();
        self.cols = cols.max(1);
        let prefix_width = Self::prefix_width(self.cols);
        let content_width = self.cols.saturating_sub(prefix_width + 1).max(1);
        let (row_ranges, cur_row, cursor_col) = Self::layout(line, cursor, content_width);
        let total_rows = row_ranges.len().max(1);

        // Pi keeps the editor to roughly 30% of the terminal, with a
        // five-line minimum, and scrolls that editor only when the cursor
        // leaves its visible window.
        let viewport = (term_rows.saturating_mul(3) / 10).max(5);
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

        // Return to the top of the previous block. The terminal cursor can
        // be anywhere inside it after an edit, so move up by its recorded
        // visual row, not by the block height.
        if self.last_cursor_row > 0 {
            write!(self.stdout, "\x1b[{}A", self.last_cursor_row)?;
        }
        write!(self.stdout, "\r")?;
        for row in 0..self.last_rows {
            write!(self.stdout, "\x1b[K")?;
            if row + 1 < self.last_rows {
                write!(self.stdout, "\x1b[B\r")?;
            }
        }
        if self.last_rows > 1 {
            write!(self.stdout, "\x1b[{}A", self.last_rows - 1)?;
        }
        write!(self.stdout, "\r")?;

        let prompt = if prefix_width == 2 { "> " } else { ">" };
        let indent = " ".repeat(prefix_width);
        for (offset, row) in row_ranges
            .iter()
            .enumerate()
            .skip(self.view_start)
            .take(visible_rows)
        {
            if offset > self.view_start {
                write!(self.stdout, "\r\n")?;
            }
            write!(self.stdout, "\x1b[K")?;
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

        // Rendering ends at the last visible row. Move to the cursor's row
        // and then to its exact column, as pi does for its fake cursor.
        let view_cur_row = cur_row - self.view_start;
        if visible_rows > view_cur_row + 1 {
            write!(
                self.stdout,
                "\x1b[{}A",
                visible_rows - view_cur_row - 1,
            )?;
        }
        let cursor_column = (prefix_width + cursor_col + 1).min(self.cols);
        write!(self.stdout, "\r\x1b[{}G", cursor_column)?;
        self.last_rows = visible_rows;
        self.last_cursor_row = view_cur_row;
        self.stdout.flush()
    }

    fn read_line(&mut self, history: &[String]) -> io::Result<Option<String>> {
        let mut line = Vec::new();
        let mut cursor = 0;
        let mut history_index: Option<usize> = None;
        let mut draft = Vec::new();
        self.preferred_col = None;
        self.refresh(&line, cursor)?;

        loop {
            match self.read_key()? {
                Key::Char(character) if !character.is_control() => {
                    line.insert(cursor, character);
                    cursor += 1;
                    history_index = None;
                    self.preferred_col = None;
                    self.refresh(&line, cursor)?;
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
                    self.refresh(&line, cursor)?;
                }
                Key::NewLine => {
                    line.insert(cursor, '\n');
                    cursor += 1;
                    history_index = None;
                    self.preferred_col = None;
                    self.refresh(&line, cursor)?;
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
                        self.refresh(&line, cursor)?;
                    }
                }
                Key::Delete => {
                    if cursor < line.len() {
                        line.remove(cursor);
                        history_index = None;
                        self.preferred_col = None;
                        self.refresh(&line, cursor)?;
                    }
                }
                Key::Left => {
                    cursor = cursor.saturating_sub(1);
                    self.preferred_col = None;
                    self.refresh(&line, cursor)?;
                }
                Key::Right => {
                    cursor = (cursor + 1).min(line.len());
                    self.preferred_col = None;
                    self.refresh(&line, cursor)?;
                }
                Key::Home => {
                    cursor = Self::line_bounds(&line, cursor).0;
                    self.preferred_col = None;
                    self.refresh(&line, cursor)?;
                }
                Key::End => {
                    cursor = Self::line_bounds(&line, cursor).1;
                    self.preferred_col = None;
                    self.refresh(&line, cursor)?;
                }
                Key::Up => {
                    if self.move_cursor_vertically(&line, &mut cursor, -1) {
                        self.refresh(&line, cursor)?;
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
                            self.refresh(&line, cursor)?;
                        } else {
                            cursor = line_start;
                            self.preferred_col = None;
                            self.refresh(&line, cursor)?;
                        }
                    }
                }
                Key::Down => {
                    if self.move_cursor_vertically(&line, &mut cursor, 1) {
                        self.refresh(&line, cursor)?;
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
                        self.refresh(&line, cursor)?;
                    } else {
                        cursor = Self::line_bounds(&line, cursor).1;
                        self.preferred_col = None;
                        self.refresh(&line, cursor)?;
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
                    self.refresh(&line, cursor)?;
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
                    self.refresh(&line, cursor)?;
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
                    self.refresh(&line, cursor)?;
                }
                Key::CtrlC => {
                    let now = std::time::Instant::now();
                    let is_double = self
                        .last_ctrl_c
                        .map(|t| now.duration_since(t).as_millis() <= 1500)
                        .unwrap_or(false);
                    self.clear_rendered_input()?;
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
                    self.refresh(&line, cursor)?;
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

fn run_chat_repl(base_url: Option<String>, model: Option<String>) {
    let config = match LlmConfig::from_env(base_url, model) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("LLM config error: {}", e);
            eprintln!("Set OPENAI_API_KEY, add api_key to ~/.config/ak/config.json, or run with --tool for raw JSON tool mode.");
            std::process::exit(1);
        }
    };

    let mut messages = vec![ChatMessage {
        role: "system".to_string(),
        content: Some(system_prompt()),
        tool_calls: None,
        tool_call_id: None,
        name: None,
    }];

    println!("{}ak agent{}", AGENT_COLOR, RESET);
    println!("type a prompt and hit enter. /clear resets history. /quit exits.");

    // Pin a status bar to the bottom of the terminal. All subsequent
    // output scrolls above it inside the restricted scroll region.
    let mut tool_state = ToolState::load();
    let mut status = match StatusBar::new() {
        Some(bar) => {
            bar.install();
            Some(bar)
        }
        None => None,
    };
    if let Some(bar) = status.as_mut() {
        bar.draw(&status_segments(&config, &messages, &tool_state));
    }

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
        let line = match editor.as_mut() {
            Some(editor) => match editor.read_line(&history) {
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
            if let Some(bar) = status.as_mut() {
                bar.draw(&status_segments(&config, &messages, &tool_state));
            }
            continue;
        }
        if line.is_empty() {
            continue;
        }
        history.push(line.clone());
        messages.push(ChatMessage {
            role: "user".to_string(),
            content: Some(line),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        });
        match process_turn(&config, &mut messages, &mut tool_state) {
            Ok(_) => println!(),
            Err(e) => eprintln!("{}error: {}{}", ERROR_COLOR, e, RESET),
        }
        if let Some(bar) = status.as_mut() {
            bar.draw(&status_segments(&config, &messages, &tool_state));
        }
    }
    // Clean up the display before handing the terminal back to the shell:
    // clear the screen, home the cursor, then restore the scroll region.
    print!("\x1b[2J\x1b[1;1H");
    io::stdout().flush().ok();
    if let Some(bar) = status.as_ref() {
        bar.uninstall();
    }
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

fn parse_args() -> (Option<String>, Option<String>, Vec<String>) {
    let mut base_url: Option<String> = None;
    let mut model: Option<String> = None;
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
        } else {
            rest.push(arg);
        }
    }
    (base_url, model, rest)
}

fn main() {
    let (base_url, model, args) = parse_args();
    if args.len() == 1 && args[0] == "--tool" {
        run_interactive();
    } else if !args.is_empty() {
        let prompt = args.join(" ");
        if let Err(e) = run_one_shot(&prompt, base_url, model) {
            eprintln!("agent error: {}", e);
            std::process::exit(1);
        }
    } else {
        run_chat_repl(base_url, model);
    }
}
