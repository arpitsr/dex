use serde_json::{Map, Value};
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use crate::agent::state::CancellationSource;
use crate::core::format::clamp_lines;

unsafe extern "C" {
    fn setpgid(pid: i32, pgid: i32) -> i32;
    fn kill(pid: i32, signal: i32) -> i32;
}

const SIGKILL: i32 = 9;
static CONFIGURED_OUTPUT_LIMIT: AtomicUsize = AtomicUsize::new(1_048_576);

/// Model-facing caps. Capture limits (1 MiB shell, 256 KiB read) guard
/// memory; clamp limits guard the context window. Head+tail clamping keeps
/// both the imports/context at the start and the errors/summaries at the end.
const BASH_CLAMP_LINES: usize = 400;
const BASH_CLAMP_BYTES: usize = 32 * 1024;
const READ_MAX_LINES: usize = 2_000;
const READ_MAX_BYTES: usize = 256 * 1024;
/// Multi-file read caps: enough for the "search, then read the hits" pattern
/// in one call, small enough that a fan-out cannot flood the context.
const READ_FANOUT_MAX_FILES: usize = 10;
const READ_FANOUT_GLOB_MAX_FILES: usize = 8;
const READ_FANOUT_PER_FILE_LINES: usize = 200;

pub(crate) fn set_output_limit(limit: usize) {
    if limit > 0 {
        CONFIGURED_OUTPUT_LIMIT.store(limit, Ordering::Relaxed);
    }
}

fn workspace_root() -> Result<PathBuf, ToolError> {
    env::current_dir().map_err(ToolError::Io)
}

/// Resolve a user-provided path within the current workspace. Existing path
/// components are canonicalized so symlinks cannot silently escape it; for a
/// new file, the existing parent is canonicalized instead.
fn workspace_path(raw: &str) -> Result<PathBuf, ToolError> {
    let root = workspace_root()?.canonicalize().map_err(ToolError::Io)?;
    resolve_workspace_path(&root, raw)
}

fn resolve_workspace_path(root: &Path, raw: &str) -> Result<PathBuf, ToolError> {
    let candidate = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        root.join(raw)
    };
    let resolved = if candidate.exists() {
        candidate.canonicalize().map_err(ToolError::Io)?
    } else {
        let file_name = candidate
            .file_name()
            .ok_or_else(|| ToolError::OutsideWorkspace(candidate.display().to_string()))?;
        let parent = candidate
            .parent()
            .unwrap_or(root)
            .canonicalize()
            .map_err(ToolError::Io)?;
        parent.join(file_name)
    };
    if resolved == root || resolved.starts_with(root) {
        Ok(resolved)
    } else {
        Err(ToolError::OutsideWorkspace(raw.to_string()))
    }
}

#[derive(Debug)]
pub(crate) enum ToolError {
    Missing(&'static str),
    NotString(&'static str),
    InvalidArgument(String),
    Io(io::Error),
    EditNotUnique(usize),
    OutsideWorkspace(String),
    /// A shell command ran but signalled failure (non-zero exit, killed, or
    /// timed out). `code` is `None` when the process never exited on its own.
    /// Carries the combined output so partial results still reach the model.
    Shell {
        output: String,
        code: Option<i32>,
    },
    Unknown(String),
}

/// A tool result together with whether the call actually succeeded. Success
/// is decided where the exit status is known — never inferred from the
/// output text, which may legitimately contain markers like `[exit 1]`.
#[derive(Clone, Debug)]
pub(crate) struct ToolOutcome {
    pub(crate) text: String,
    pub(crate) ok: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ToolMetadata {
    pub read_only: bool,
    pub mutating: bool,
    pub idempotent: bool,
    pub requires_shell: bool,
    pub permission: PermissionRequirement,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PermissionRequirement {
    Read,
    Write,
    Shell,
}

pub(crate) fn is_mutating(name: &str) -> bool {
    metadata(name)
        .map(|metadata| metadata.mutating)
        .unwrap_or(true)
}

pub(crate) fn metadata(name: &str) -> Option<ToolMetadata> {
    Some(match name {
        "read" => ToolMetadata {
            read_only: true,
            mutating: false,
            idempotent: true,
            requires_shell: false,
            permission: PermissionRequirement::Read,
        },
        "grep" | "find" | "git" => ToolMetadata {
            read_only: true,
            mutating: false,
            idempotent: true,
            requires_shell: true,
            permission: PermissionRequirement::Read,
        },
        "chain" => ToolMetadata {
            read_only: true,
            mutating: false,
            idempotent: false,
            requires_shell: true,
            permission: PermissionRequirement::Read,
        },
        "bash" => ToolMetadata {
            read_only: false,
            mutating: true,
            idempotent: false,
            requires_shell: true,
            permission: PermissionRequirement::Shell,
        },
        "write" | "edit" => ToolMetadata {
            read_only: false,
            mutating: true,
            idempotent: false,
            requires_shell: false,
            permission: PermissionRequirement::Write,
        },
        _ => return None,
    })
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(k) => write!(f, "missing argument '{}'", k),
            Self::NotString(k) => write!(f, "argument '{}' must be a string", k),
            Self::InvalidArgument(message) => write!(f, "{}", message),
            Self::Io(e) => write!(f, "io error: {}", e),
            Self::EditNotUnique(n) => write!(
                f,
                "oldText matches {n} locations; include more surrounding lines to make it unique, or pass replaceAll: true"
            ),
            Self::OutsideWorkspace(path) => write!(f, "path is outside the workspace: {}", path),
            Self::Shell { output, code } => match code {
                Some(code) => write!(f, "{output}\n[exit {code}]"),
                None => write!(f, "{output}"),
            },
            Self::Unknown(t) => write!(f, "unknown tool '{}'", t),
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

fn audit(name: &str, args: &Map<String, Value>, outcome: &str) {
    let Some(base) = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    else {
        return;
    };
    let path = base.join("dex/audit.jsonl");
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let record = serde_json::json!({
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "cwd": env::current_dir().ok().map(|p| p.display().to_string()),
        "tool": name,
        "args": args,
        "outcome": outcome,
    });
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        // One write syscall per record: parallel tool executions append to
        // this file concurrently, and a multi-syscall formatted write would
        // interleave mid-record.
        let mut line = record.to_string();
        line.push('\n');
        let _ = file.write_all(line.as_bytes());
    }
}

/// Maximum wall-clock duration for a shell command.
/// Configure with DEX_TOOL_TIMEOUT_SECS (default: 120).
/// Output is capped by DEX_TOOL_OUTPUT_BYTES (default: 1 MiB).
fn shell_timeout() -> Duration {
    env::var("DEX_TOOL_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(120))
}

/// Expose the running binary to shell commands as $DEX_BIN so a script can
/// call tools locally (`"$DEX_BIN" run read path=src/main.rs`) and stitch a
/// whole read-only pipeline in one call — intermediate output stays out of
/// the conversation and only the distilled result reaches the model.
/// ponytail: shell stitching only — if JSON routing in pipelines gets
/// painful, embed rquickjs and expose tools as functions (same $DEX_BIN mechanism).
fn tool_runner_env() -> Vec<(String, String)> {
    std::env::current_exe()
        .ok()
        .map(|exe| vec![("DEX_BIN".to_string(), exe.display().to_string())])
        .unwrap_or_default()
}

/// Read up to `limit` bytes; reports whether more output remained after the
/// limit was hit (the distinguishing extra read happens after EOF-or-limit,
/// so a stream that ends exactly at the limit is not flagged truncated).
fn read_limited<R: Read>(mut reader: R, limit: usize) -> (Vec<u8>, bool) {
    let mut bytes = Vec::with_capacity(limit.min(8192));
    let mut buffer = [0u8; 8192];
    loop {
        let chunk = (limit - bytes.len()).min(buffer.len());
        match reader.read(&mut buffer[..chunk]) {
            Ok(0) | Err(_) => return (bytes, false),
            Ok(size) => bytes.extend_from_slice(&buffer[..size]),
        }
        if bytes.len() >= limit {
            break;
        }
    }
    // The capture limit is reached; check whether the stream has more.
    let more = matches!(reader.read(&mut buffer), Ok(n) if n > 0);
    (bytes, more)
}

fn run_bash(
    command: &str,
    cancel: &dyn CancellationSource,
) -> Result<(String, Option<i32>), ToolError> {
    let timeout = shell_timeout();
    let max_bytes = env::var("DEX_TOOL_OUTPUT_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|bytes| *bytes > 0)
        .unwrap_or_else(|| CONFIGURED_OUTPUT_LIMIT.load(Ordering::Relaxed));
    run_bash_with_limits(command, timeout, max_bytes, cancel)
}

fn run_bash_with_limits(
    command: &str,
    timeout: Duration,
    max_bytes: usize,
    cancel: &dyn CancellationSource,
) -> Result<(String, Option<i32>), ToolError> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .envs(tool_runner_env())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(ToolError::Io)?;
    // Put the shell in its own process group so cancellation/timeout does not
    // leave descendants running.
    unsafe {
        let _ = setpgid(child.id() as i32, child.id() as i32);
    }
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let stdout_reader = thread::spawn(move || read_limited(stdout, max_bytes));
    let stderr_reader = thread::spawn(move || read_limited(stderr, max_bytes));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(ToolError::Io)? {
            break status;
        }
        if cancel.is_cancelled() {
            unsafe {
                let _ = kill(-(child.id() as i32), SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Ok(("Error: shell command cancelled".to_string(), None));
        }
        if Instant::now() >= deadline {
            unsafe {
                let _ = kill(-(child.id() as i32), SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Ok((
                format!(
                    "Error: shell command timed out after {} seconds",
                    timeout.as_secs()
                ),
                None,
            ));
        }
        thread::sleep(Duration::from_millis(25));
    };
    let (stdout, stdout_truncated) = stdout_reader.join().unwrap_or_default();
    let (stderr, stderr_truncated) = stderr_reader.join().unwrap_or_default();
    let mut result = String::from_utf8_lossy(&stdout).into_owned();
    if stdout_truncated {
        result.push_str("\n[... output exceeded capture limit ...]");
    }
    let stderr = String::from_utf8_lossy(&stderr).into_owned();
    if !stderr.trim().is_empty() {
        if !result.is_empty() && !result.ends_with('\n') {
            result.push('\n');
        }
        result.push_str("--- stderr ---\n");
        result.push_str(&stderr);
        if stderr_truncated {
            result.push_str("\n[... stderr exceeded capture limit ...]");
        }
    }
    Ok((result, status.code()))
}

/// `read` returns line-numbered content (`line\\ttext`) so subsequent `edit`
/// oldText anchors are cheap to construct. Default: first 2000 lines, capped
/// by a byte budget; paginate with offset/limit. Binary files are refused
/// rather than dumped into the context window.
///
/// Round-trip reduction: `paths` (explicit list) or `glob` (pattern fan-out)
/// read several files in ONE call — the "search, then read what it found"
/// chain collapses into a single tool call. Per-file errors are isolated and
/// the call succeeds when at least one file is readable.
fn tool_read(args: &Map<String, Value>) -> Result<String, ToolError> {
    if let Some(paths) = args.get("paths").and_then(Value::as_array) {
        return fanout_read(parse_path_list(paths)?, args);
    }
    if let Some(glob) = args.get("glob").and_then(Value::as_str) {
        return fanout_read(expand_glob(glob)?, args);
    }
    let path = workspace_path(&arg_str(args, "path")?)?;
    let (body, _more) =
        read_file_numbered(&path, read_offset(args), read_limit(args, READ_MAX_LINES))?;
    Ok(body)
}

fn read_offset(args: &Map<String, Value>) -> usize {
    args.get("offset")
        .and_then(Value::as_u64)
        .map(|n| n.max(1) as usize)
        .unwrap_or(1)
}

fn read_limit(args: &Map<String, Value>, default: usize) -> usize {
    args.get("limit")
        .and_then(Value::as_u64)
        .map(|n| n.max(1) as usize)
        .unwrap_or(default)
}

fn parse_path_list(paths: &[Value]) -> Result<Vec<PathBuf>, ToolError> {
    if paths.is_empty() {
        return Err(ToolError::InvalidArgument(
            "paths must not be empty".to_string(),
        ));
    }
    if paths.len() > READ_FANOUT_MAX_FILES {
        return Err(ToolError::InvalidArgument(format!(
            "paths accepts at most {READ_FANOUT_MAX_FILES} files per call (got {}); split into batches",
            paths.len()
        )));
    }
    paths
        .iter()
        .map(|value| {
            value.as_str().map(workspace_path).unwrap_or_else(|| {
                Err(ToolError::InvalidArgument(
                    "paths entries must be strings".to_string(),
                ))
            })
        })
        .collect()
}

/// Line-numbered content for one file, within the read budgets. Also returns
/// how many lines were omitted past the end (for pagination notes).
fn read_file_numbered(
    path: &Path,
    offset: usize,
    limit: usize,
) -> Result<(String, usize), ToolError> {
    let file = fs::File::open(path).map_err(ToolError::Io)?;
    let mut bytes = Vec::new();
    file.take((READ_MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(ToolError::Io)?;
    let over_budget = bytes.len() > READ_MAX_BYTES;
    bytes.truncate(READ_MAX_BYTES);
    if bytes.contains(&0) {
        return Err(ToolError::InvalidArgument(format!(
            "binary file; use `bash` with a targeted command such as `strings` or `hexdump` on {}",
            path.display()
        )));
    }
    let text = String::from_utf8_lossy(&bytes);

    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    if offset > total {
        return Err(ToolError::InvalidArgument(format!(
            "offset {offset} is past the end; {} has {total} lines",
            path.display()
        )));
    }
    let numbered: Vec<String> = lines
        .iter()
        .skip(offset - 1)
        .take(limit)
        .enumerate()
        .map(|(index, line)| format!("{}\t{line}", offset + index))
        .collect();
    let mut out = clamp_lines(&numbered.join("\n"), READ_MAX_LINES, READ_MAX_BYTES);
    let shown = numbered.len();
    let more = total.saturating_sub(offset - 1 + shown);
    if more > 0 && limit >= READ_MAX_LINES {
        out.push_str(&format!(
            "\n[... {more} more lines; continue with offset {} ...]",
            offset + shown
        ));
    }
    if over_budget {
        out.push_str("\n[... file exceeds the read byte budget; use offset/limit ...]");
    }
    Ok((out, more))
}

/// Multi-file read: `==> path <==` sections (grep-style), per-file limits,
/// isolated per-file errors, one shared byte budget.
fn fanout_read(paths: Vec<PathBuf>, args: &Map<String, Value>) -> Result<String, ToolError> {
    let per_file = read_limit(args, READ_FANOUT_PER_FILE_LINES);
    let offset = read_offset(args);
    let mut sections: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut used = 0usize;
    for path in &paths {
        let display = path.display().to_string();
        match read_file_numbered(path, offset, per_file) {
            Ok((body, _)) => {
                used += body.len();
                sections.push(format!("==> {display} <==\n{body}"));
            }
            Err(error) => errors.push(format!("==> {display}: error: {error}")),
        }
        if used > READ_MAX_BYTES {
            sections.push("[... read budget reached; remaining files skipped ...]".to_string());
            break;
        }
    }
    if sections.is_empty() {
        return Err(ToolError::InvalidArgument(format!(
            "no file could be read: {}",
            errors.join("; ")
        )));
    }
    let mut out = clamp_lines(&sections.join("\n"), READ_MAX_LINES, READ_MAX_BYTES);
    if !errors.is_empty() {
        out.push('\n');
        out.push_str(&errors.join("\n"));
    }
    Ok(out)
}

/// Expand a glob into workspace paths. Patterns with `/` match full paths
/// (`src/tools/*.rs`); bare patterns match basenames anywhere (`*.rs`).
fn expand_glob(glob: &str) -> Result<Vec<PathBuf>, ToolError> {
    let glob = glob.trim().trim_start_matches("./").to_string();
    if glob.is_empty() || !glob.contains(['*', '?']) {
        return Err(ToolError::InvalidArgument(
            "glob must contain wildcard characters (* or ?); use `path` for a single file"
                .to_string(),
        ));
    }
    let command = if glob.contains('/') {
        format!("find . \\( -name .git -o -name target -o -name node_modules \\) -prune -o -path './{glob}' -print")
    } else {
        format!("find . \\( -name .git -o -name target -o -name node_modules \\) -prune -o -name '{glob}' -print")
    };
    let (output, code) = run_bash(&command, &crate::agent::state::GlobalCancellation)?;
    if !matches!(code, Some(0) | Some(1)) {
        return Err(ToolError::Shell { output, code });
    }
    let mut paths: Vec<PathBuf> = output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| workspace_path(line).ok())
        .collect();
    paths.sort_unstable();
    paths.dedup();
    if paths.len() > READ_FANOUT_GLOB_MAX_FILES {
        paths.truncate(READ_FANOUT_GLOB_MAX_FILES);
    }
    if paths.is_empty() {
        return Err(ToolError::InvalidArgument(format!(
            "glob '{glob}' matched no files"
        )));
    }
    Ok(paths)
}

fn tool_bash(
    args: &Map<String, Value>,
    cancel: &dyn CancellationSource,
) -> Result<String, ToolError> {
    let (output, code) = run_bash(&arg_str(args, "command")?, cancel)?;
    match code {
        Some(0) => Ok(clamp_lines(&output, BASH_CLAMP_LINES, BASH_CLAMP_BYTES)),
        code => Err(ToolError::Shell {
            output: clamp_lines(&output, BASH_CLAMP_LINES, BASH_CLAMP_BYTES),
            code,
        }),
    }
}

fn tool_write(args: &Map<String, Value>) -> Result<String, ToolError> {
    let path = workspace_path(&arg_str(args, "path")?)?;
    let content = arg_str(args, "content")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(ToolError::Io)?;
    }
    let replaced = fs::metadata(&path).ok().map(|meta| meta.len());
    fs::write(&path, &content).map_err(ToolError::Io)?;
    Ok(match replaced {
        Some(old_bytes) => format!(
            "wrote {} (replaced {old_bytes} bytes with {})",
            path.display(),
            content.len()
        ),
        None => format!("wrote {}", path.display()),
    })
}

fn tool_edit(args: &Map<String, Value>) -> Result<String, ToolError> {
    let path = workspace_path(&arg_str(args, "path")?)?;
    let old = arg_str(args, "oldText")?;
    let new = arg_str(args, "newText")?;
    let replace_all = args
        .get("replaceAll")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if old.is_empty() {
        return Err(ToolError::InvalidArgument(
            "oldText must not be empty; use `write` to create files".to_string(),
        ));
    }
    if old == new {
        return Err(ToolError::InvalidArgument(
            "oldText and newText are identical; nothing to edit".to_string(),
        ));
    }
    let content = fs::read_to_string(&path).map_err(ToolError::Io)?;
    let (updated, note) = apply_edit(&content, &old, &new, replace_all)?;
    fs::write(&path, updated).map_err(ToolError::Io)?;
    Ok(format!("edited {}{note}", path.display()))
}

fn apply_edit(
    content: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<(String, String), ToolError> {
    let count = content.matches(old).count();
    if count == 1 || (replace_all && count > 1) {
        let updated = if replace_all {
            content.replace(old, new)
        } else {
            content.replacen(old, new, 1)
        };
        let idx = content.find(old).unwrap_or(0);
        let start_line = content[..idx].matches('\n').count() + 1;
        let end_line = start_line + old.lines().count().saturating_sub(1);
        let span = if start_line == end_line {
            format!("line {start_line}")
        } else {
            format!("lines {start_line}-{end_line}")
        };
        let note = if count > 1 {
            format!(" ({span}, {count} occurrences)")
        } else {
            format!(" ({span})")
        };
        return Ok((updated, note));
    }
    if count > 1 {
        return Err(ToolError::EditNotUnique(count));
    }

    // Exact match failed: retry with a whitespace-insensitive line-window
    // comparison (Codex-style fuzzy fallback). Handles the common case of
    // the model reproducing content with different indentation or trailing
    // whitespace. Whole lines are replaced, so the match must cover them.
    let old_lines: Vec<&str> = old.lines().collect();
    let window = old_lines.len();
    let content_lines: Vec<&str> = content.lines().collect();
    let matches_at: Vec<usize> = (0..content_lines.len().saturating_sub(window - 1))
        .filter(|&start| {
            content_lines[start..start + window]
                .iter()
                .zip(&old_lines)
                .all(|(c, o)| c.trim() == o.trim())
        })
        .collect();
    if matches_at.is_empty() {
        return Err(ToolError::InvalidArgument(
            "oldText not found; read the file to confirm the exact text (whitespace must match)"
                .to_string(),
        ));
    }
    if matches_at.len() > 1 && !replace_all {
        return Err(ToolError::EditNotUnique(matches_at.len()));
    }

    // Replace windows from the end so earlier indices stay valid. Each
    // replacement line inherits the indentation of the old line it replaces
    // when it carries none of its own — models frequently resend matched
    // text without the file's leading whitespace.
    let mut updated_lines: Vec<String> = content_lines.iter().map(|s| s.to_string()).collect();
    for &start in matches_at.iter().rev() {
        let replacement: Vec<String> = new
            .lines()
            .enumerate()
            .map(|(index, line)| {
                let old_indent = content_lines
                    .get(start + index)
                    .map(|old_line| &old_line[..old_line.len() - old_line.trim_start().len()])
                    .unwrap_or_default();
                if !old_indent.is_empty() && !line.is_empty() && line.trim_start() == line {
                    format!("{old_indent}{line}")
                } else {
                    line.to_string()
                }
            })
            .collect();
        updated_lines.splice(start..start + window, replacement);
    }
    let mut updated = updated_lines.join("\n");
    if content.ends_with('\n') {
        updated.push('\n');
    }
    let note = if matches_at.len() == 1 {
        format!(
            " (lines {}-{}, whitespace-insensitive)",
            matches_at[0] + 1,
            matches_at[0] + window
        )
    } else {
        format!(" ({} sites, whitespace-insensitive)", matches_at.len())
    };
    Ok((updated, note))
}

/// Directories that are noise for a coding agent and slow (`target/` alone
/// can be gigabytes) enough to cause spurious tool timeouts when searched.
const SKIP_DIRS: &str = "--exclude-dir=.git --exclude-dir=target --exclude-dir=node_modules";
const SKIP_GLOBS: &str = "--glob '!target' --glob '!node_modules' --glob '!.git'";

fn has_ripgrep() -> bool {
    static RG: OnceLock<bool> = OnceLock::new();
    *RG.get_or_init(|| {
        Command::new("rg")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GrepMode {
    Files,
    Content,
    Count,
}

impl GrepMode {
    fn parse(value: Option<&str>) -> Result<Self, ToolError> {
        match value {
            None | Some("files") => Ok(Self::Files),
            Some("content") => Ok(Self::Content),
            Some("count") => Ok(Self::Count),
            Some(other) => Err(ToolError::InvalidArgument(format!(
                "unknown output_mode '{other}' (files, content, count)"
            ))),
        }
    }

    /// Ripgrep flags for each output mode.
    fn rg_flags(self) -> &'static str {
        match self {
            Self::Files => "-l",
            Self::Content => "--no-heading -n",
            Self::Count => "--count-matches",
        }
    }

    /// GNU grep fallback flags for each output mode.
    fn grep_flags(self) -> &'static str {
        match self {
            Self::Files => "-R -I -l",
            Self::Content => "-R -I -n",
            Self::Count => "-R -I -c",
        }
    }

    fn default_limit(self) -> usize {
        match self {
            Self::Files => 100,
            Self::Content => 200,
            Self::Count => 50,
        }
    }

    fn unit(self) -> &'static str {
        match self {
            Self::Files => "files",
            Self::Content => "lines",
            Self::Count => "entries",
        }
    }
}

/// Cap search output at `head_limit` lines with a marker that states the
/// real totals, so the model knows to narrow the pattern instead of paging.
fn shape_search_output(output: &str, mode: GrepMode, head_limit: usize) -> String {
    let lines: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() <= head_limit {
        return lines.join("\n");
    }
    let shown: Vec<&str> = lines.iter().take(head_limit).copied().collect();
    format!(
        "{}\n[... {} of {} {} matched; raise head_limit or narrow the search ...]",
        shown.join("\n"),
        lines.len() - head_limit,
        lines.len(),
        mode.unit()
    )
}

fn tool_grep(
    args: &Map<String, Value>,
    cancel: &dyn CancellationSource,
) -> Result<String, ToolError> {
    let pattern = arg_str(args, "pattern")?;
    if pattern.trim().is_empty() {
        return Err(ToolError::InvalidArgument(
            "grep pattern must not be empty (it would match every line)".to_string(),
        ));
    }
    let path = arg_str(args, "path").unwrap_or_else(|_| ".".to_string());
    let path = workspace_path(&path)?;
    let mode = GrepMode::parse(args.get("output_mode").and_then(Value::as_str))?;
    let head_limit = args
        .get("head_limit")
        .and_then(Value::as_u64)
        .map(|n| n.max(1) as usize)
        .unwrap_or_else(|| mode.default_limit());

    let escaped = shell_escape(&pattern);
    let target = shell_escape(&path.to_string_lossy());
    // Context lines (content mode) fold the follow-up "read around the
    // match" call into this one.
    let context = args
        .get("context")
        .and_then(Value::as_u64)
        .map(|n| n.min(10))
        .unwrap_or(0);
    let context_flag = if context > 0 {
        format!(" -C {context}")
    } else {
        String::new()
    };
    let command = if has_ripgrep() {
        format!(
            "rg {}{context_flag} -S {SKIP_GLOBS} -- {escaped} {target}",
            mode.rg_flags()
        )
    } else {
        format!(
            "grep {}{context_flag} {SKIP_DIRS} -- {escaped} {target}",
            mode.grep_flags()
        )
    };
    let (output, code) = run_bash(&command, cancel)?;
    // Exit 1 means "no matches" for both grep and ripgrep — a normal result.
    match code {
        Some(0) | Some(1) => Ok(shape_search_output(&output, mode, head_limit)),
        code => Err(ToolError::Shell { output, code }),
    }
}

fn tool_find(
    args: &Map<String, Value>,
    cancel: &dyn CancellationSource,
) -> Result<String, ToolError> {
    let pattern = arg_str(args, "pattern")?;
    // Optional, matching the advertised schema (only `pattern` is required).
    let path = arg_str(args, "path").unwrap_or_else(|_| ".".to_string());
    if pattern.trim().is_empty() || pattern == "*" {
        return Err(ToolError::InvalidArgument(
            "find pattern must be targeted (not empty or '*')".to_string(),
        ));
    }
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n.max(1) as usize)
        .unwrap_or(100);
    let path = workspace_path(&path)?;
    let (output, code) = run_bash(
        &format!(
            "find {} \\( -name .git -o -name target -o -name node_modules \\) -prune -o -path '*{}*' -print",
            shell_escape(&path.to_string_lossy()),
            shell_escape(&pattern)
        ),
        cancel,
    )?;
    match code {
        Some(0) => {
            let mut paths: Vec<&str> = output
                .lines()
                .filter(|line| !line.trim().is_empty())
                .collect();
            paths.sort_unstable();
            paths.dedup();
            if paths.len() <= limit {
                return Ok(paths.join("\n"));
            }
            Ok(format!(
                "{}\n[... {} of {} paths matched; raise limit or narrow the pattern ...]",
                paths[..limit].join("\n"),
                paths.len() - limit,
                paths.len()
            ))
        }
        code => Err(ToolError::Shell { output, code }),
    }
}

fn tool_git(
    args: &Map<String, Value>,
    cancel: &dyn CancellationSource,
) -> Result<String, ToolError> {
    let mode = args.get("mode").and_then(Value::as_str).unwrap_or("status");
    if !matches!(mode, "status" | "diff") {
        return Err(ToolError::NotString("mode (status or diff)"));
    }
    // Routed through run_bash so git shares the shell timeout, cancellation,
    // capture limits, and clamping instead of running unbounded.
    let (output, code) = run_bash(&format!("git --no-pager {mode}"), cancel)?;
    match code {
        Some(0) | Some(1) => Ok(clamp_lines(&output, BASH_CLAMP_LINES, BASH_CLAMP_BYTES)),
        code => Err(ToolError::Shell {
            output: clamp_lines(&output, BASH_CLAMP_LINES, BASH_CLAMP_BYTES),
            code,
        }),
    }
}

/// Maximum steps in one chain: enough for search → read → search → read,
/// small enough to stay predictable.
const CHAIN_MAX_STEPS: usize = 4;

/// A bounded, read-only chain executed in ONE LLM round trip — dex's scoped
/// take on programmatic tool calling. The model declares steps; routing
/// between steps is mechanical (`from` a search step, `take: "paths"` into a
/// read fan-out), never semantic: the model cannot branch or transform
/// mid-chain, and mutation/shell tools are refused. On a step failure the
/// earlier steps' outputs ship with the error, so the round trip still
/// carries information.
fn tool_chain(
    args: &Map<String, Value>,
    cancel: &dyn CancellationSource,
) -> Result<String, ToolError> {
    let steps = args
        .get("steps")
        .and_then(Value::as_array)
        .ok_or(ToolError::Missing("steps"))?;
    if steps.len() < 2 {
        return Err(ToolError::InvalidArgument(
            "chain needs at least 2 steps; a single tool call does not need a chain".to_string(),
        ));
    }
    if steps.len() > CHAIN_MAX_STEPS {
        return Err(ToolError::InvalidArgument(format!(
            "chain supports at most {CHAIN_MAX_STEPS} steps (got {})",
            steps.len()
        )));
    }

    let mut completed: Vec<(String, String)> = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        match run_chain_step(step, index, &completed, cancel) {
            Ok(pair) => completed.push(pair),
            Err(error) => {
                let mut text = render_chain_steps(&completed);
                text.push_str(&format!("\n--- step {index} failed ---\nError: {error}\n"));
                return Err(ToolError::Shell {
                    output: clamp_lines(&text, READ_MAX_LINES, READ_MAX_BYTES),
                    code: None,
                });
            }
        }
    }
    Ok(clamp_lines(
        &render_chain_steps(&completed),
        READ_MAX_LINES,
        READ_MAX_BYTES,
    ))
}

fn render_chain_steps(steps: &[(String, String)]) -> String {
    let mut out = String::new();
    for (index, (tool, output)) in steps.iter().enumerate() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("--- step {index}: {tool} ---\n{output}"));
    }
    out
}

fn run_chain_step(
    step: &Value,
    index: usize,
    completed: &[(String, String)],
    cancel: &dyn CancellationSource,
) -> Result<(String, String), ToolError> {
    let invalid = |message: String| ToolError::InvalidArgument(format!("step {index}: {message}"));
    let obj = step
        .as_object()
        .ok_or_else(|| invalid("must be an object".to_string()))?;
    let tool = obj
        .get("tool")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("is missing 'tool'".to_string()))?;
    let meta = metadata(tool).ok_or_else(|| invalid(format!("unknown tool '{tool}'")))?;
    if meta.permission != PermissionRequirement::Read {
        return Err(invalid(format!(
            "chain is read-only; '{tool}' must run as its own approved call"
        )));
    }

    let mut step_args = match obj.get("args") {
        Some(Value::Object(map)) => map.clone(),
        None => Map::new(),
        Some(_) => return Err(invalid("'args' must be an object".to_string())),
    };

    if let Some(from) = obj.get("from") {
        let from = from
            .as_u64()
            .map(|n| n as usize)
            .ok_or_else(|| invalid("'from' must be the index of an earlier step".to_string()))?;
        if from >= index {
            return Err(invalid("'from' must reference an earlier step".to_string()));
        }
        if obj.get("take").and_then(Value::as_str) != Some("paths") {
            return Err(invalid(
                "'take' must be \"paths\" (routes a search step's matched files into read)"
                    .to_string(),
            ));
        }
        if tool != "read" {
            return Err(invalid("'from' routing requires the read tool".to_string()));
        }
        let (source_tool, source_output) = &completed[from];
        if !matches!(source_tool.as_str(), "grep" | "find" | "chain") {
            return Err(invalid(format!(
                "'from' step {from} is '{source_tool}', which produces no file paths; use grep (files mode) or find"
            )));
        }
        let max_files = obj
            .get("max_files")
            .and_then(Value::as_u64)
            .map(|n| (n as usize).clamp(1, READ_FANOUT_MAX_FILES))
            .unwrap_or(5);
        let paths = extract_search_paths(source_output, max_files, from)?;
        step_args.remove("path");
        step_args.insert(
            "paths".to_string(),
            Value::Array(
                paths
                    .into_iter()
                    .map(|path| Value::String(path.display().to_string()))
                    .collect(),
            ),
        );
    }

    let output = execute(tool, &step_args, cancel)?;
    Ok((tool.to_string(), output))
}

/// Pull file paths out of a search step's output (grep files-mode lines,
/// find output) and resolve them within the workspace.
fn extract_search_paths(
    output: &str,
    max_files: usize,
    from: usize,
) -> Result<Vec<PathBuf>, ToolError> {
    let mut paths: Vec<PathBuf> = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        // Skip truncation markers and chain section labels.
        if line.is_empty() || line.starts_with('[') || line.starts_with("---") {
            continue;
        }
        if let Ok(path) = workspace_path(line) {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        if paths.len() >= max_files {
            break;
        }
    }
    if paths.is_empty() {
        return Err(ToolError::InvalidArgument(format!(
            "step {from} output contained no resolvable file paths; widen the search or raise head_limit"
        )));
    }
    Ok(paths)
}

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// Execute a tool using paths confined to the current workspace.
pub(crate) fn execute(
    name: &str,
    args: &Map<String, Value>,
    cancel: &dyn CancellationSource,
) -> Result<String, ToolError> {
    if metadata(name).is_none() {
        let error = ToolError::Unknown(name.to_string());
        audit(name, args, &error.to_string());
        return Err(error);
    }
    let result = match name {
        "read" => tool_read(args),
        "bash" => tool_bash(args, cancel),
        "write" => tool_write(args),
        "edit" => tool_edit(args),
        "grep" => tool_grep(args, cancel),
        "find" => tool_find(args, cancel),
        "git" => tool_git(args, cancel),
        "chain" => tool_chain(args, cancel),
        _ => unreachable!("metadata and dispatch must stay in sync"),
    };
    let outcome = match &result {
        Ok(_) => "ok".to_string(),
        Err(error) => error.to_string(),
    };
    audit(name, args, &outcome);
    result
}

/// Execute a tool, reporting success explicitly. Callers must not re-derive
/// success from the output text: tool output can legitimately contain
/// strings like `[exit 1]` (shell markers appear in source files and logs).
pub(crate) fn execute_outcome(
    name: &str,
    args: &Map<String, Value>,
    cancel: &dyn CancellationSource,
) -> ToolOutcome {
    match execute(name, args, cancel) {
        Ok(out) => ToolOutcome {
            text: out,
            ok: true,
        },
        Err(e) => ToolOutcome {
            text: format!("Error: {}", e),
            ok: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::state::GlobalCancellation;
    use serde_json::json;
    #[test]
    fn shell_escape_handles_quotes_and_commands() {
        assert_eq!(shell_escape("a'b; echo hacked"), "'a'\"'\"'b; echo hacked'");
    }

    #[test]
    fn bash_exposes_the_binary_for_local_stitching() {
        let (output, code) = run_bash_with_limits(
            "printf '%s' \"$DEX_BIN\"",
            Duration::from_secs(5),
            4096,
            &GlobalCancellation,
        )
        .unwrap();
        assert_eq!(code, Some(0));
        assert_eq!(
            output,
            std::env::current_exe().unwrap().display().to_string()
        );
    }
    #[test]
    fn metadata_classifies_tools() {
        assert!(metadata("read").unwrap().read_only);
        assert!(metadata("write").unwrap().mutating);
        assert!(!metadata("bash").unwrap().idempotent);
    }

    #[test]
    fn tool_arguments_are_validated() {
        let args = Map::new();
        assert!(matches!(
            execute("read", &args, &GlobalCancellation),
            Err(ToolError::Missing("path"))
        ));
        let mut args = Map::new();
        args.insert("path".into(), Value::Bool(true));
        assert!(matches!(
            execute("read", &args, &GlobalCancellation),
            Err(ToolError::NotString("path"))
        ));
    }

    #[test]
    fn find_rejects_unbounded_patterns() {
        let mut args = Map::new();
        args.insert("pattern".into(), Value::String("*".into()));
        args.insert("path".into(), Value::String(".".into()));
        assert!(matches!(
            execute("find", &args, &GlobalCancellation),
            Err(ToolError::InvalidArgument(_))
        ));
    }

    #[test]
    fn edit_requires_exactly_one_match() {
        assert!(matches!(
            apply_edit("a a", "a", "b", false),
            Err(ToolError::EditNotUnique(2))
        ));
        let (updated, _) = apply_edit("a", "a", "b", false).unwrap();
        assert_eq!(updated, "b");
        assert!(matches!(
            apply_edit("a", "x", "b", false),
            Err(ToolError::InvalidArgument(_))
        ));
    }

    #[test]
    fn edit_replace_all_replaces_every_occurrence() {
        let (updated, note) = apply_edit("a b a", "a", "c", true).unwrap();
        assert_eq!(updated, "c b c");
        assert!(note.contains("2 occurrences"), "{note}");
        // Without replaceAll the duplicate is an error, not a silent partial.
        assert!(apply_edit("a b a", "a", "c", false).is_err());
    }

    #[test]
    fn edit_fuzzy_fallback_handles_whitespace_drift() {
        // oldText with different indentation still matches exactly one
        // line-window and replaces whole lines.
        let content = "fn main() {\n    let x = 1;\n    println!(x);\n}\n";
        let old = "let x = 1;\nprintln!(x);";
        let new = "let y = 2;\nprintln!(y);";
        let (updated, note) = apply_edit(content, old, new, false).unwrap();
        assert!(updated.contains("let y = 2;"), "{updated}");
        assert!(updated.contains("    println!(y);"), "{updated}");
        assert!(note.contains("whitespace-insensitive"), "{note}");
        // Ambiguous fuzzy matches are rejected, not guessed.
        assert!(apply_edit("a\nb\na\nb", "a\nb", "c", false).is_err());
        // A genuinely absent match reports actionable guidance.
        let err = apply_edit("x", "nope", "c", false).unwrap_err();
        assert!(err.to_string().contains("read the file"), "{err}");
    }

    #[test]
    fn temporary_workspace_paths_are_confined() {
        let root = std::env::temp_dir().join(format!("dex-workspace-test-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        assert!(resolve_workspace_path(&root, "inside.txt")
            .unwrap()
            .starts_with(&root));
        assert!(matches!(
            resolve_workspace_path(&root, "../outside.txt"),
            Err(ToolError::OutsideWorkspace(_))
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn shell_timeout_terminates_long_running_command() {
        let (result, code) = run_bash_with_limits(
            "sleep 1",
            Duration::from_millis(10),
            1024,
            &GlobalCancellation,
        )
        .unwrap();
        assert!(result.contains("timed out"));
        assert_eq!(code, None);
    }

    #[test]
    fn shell_exit_code_is_reported_separately_from_output() {
        let (output, code) = run_bash_with_limits(
            "echo partial-results; exit 3",
            Duration::from_secs(5),
            1024,
            &GlobalCancellation,
        )
        .unwrap();
        assert_eq!(code, Some(3));
        assert_eq!(output, "partial-results\n");
    }

    #[test]
    fn stderr_is_labeled_and_capture_limit_is_marked() {
        let (output, _) = run_bash_with_limits(
            "echo out; echo err 1>&2",
            Duration::from_secs(5),
            1024,
            &GlobalCancellation,
        )
        .unwrap();
        assert!(output.contains("out\n"), "{output:?}");
        assert!(output.contains("--- stderr ---\nerr"), "{output:?}");

        let (clipped, _) =
            run_bash_with_limits("seq 1 100", Duration::from_secs(5), 16, &GlobalCancellation)
                .unwrap();
        assert!(clipped.contains("capture limit"), "{clipped:?}");
    }

    #[test]
    fn read_is_line_numbered_and_paginates() {
        // Fixtures live under target/ so the workspace path confinement
        // accepts them (and the directory is already ignored).
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("dex-read-test-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("sample.txt");
        fs::write(&path, "one\ntwo\nthree\nfour\n").unwrap();

        let mut args = Map::new();
        args.insert("path".into(), Value::String(path.display().to_string()));
        let outcome = execute_outcome("read", &args, &GlobalCancellation);
        assert!(outcome.ok, "{}", outcome.text);
        assert_eq!(outcome.text, "1\tone\n2\ttwo\n3\tthree\n4\tfour");

        args.insert("offset".into(), Value::Number(2.into()));
        args.insert("limit".into(), Value::Number(1.into()));
        let outcome = execute_outcome("read", &args, &GlobalCancellation);
        assert_eq!(outcome.text, "2\ttwo");

        args.insert("offset".into(), Value::Number(9.into()));
        let outcome = execute_outcome("read", &args, &GlobalCancellation);
        assert!(!outcome.ok, "offset past end must fail: {}", outcome.text);

        // Binary content is refused instead of dumped into the context.
        fs::write(root.join("blob.bin"), [0u8, 1, 2, 3]).unwrap();
        let mut args = Map::new();
        args.insert(
            "path".into(),
            Value::String(root.join("blob.bin").display().to_string()),
        );
        let outcome = execute_outcome("read", &args, &GlobalCancellation);
        assert!(!outcome.ok, "{}", outcome.text);
        assert!(outcome.text.contains("binary file"), "{}", outcome.text);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn read_fanout_reads_many_files_in_one_call() {
        // Fixtures live at the workspace root (not target/) because search
        // and glob rules deliberately exclude target/.
        let root = std::env::current_dir()
            .unwrap()
            .join(format!("dex-fanout-test-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "alpha\n").unwrap();
        fs::write(root.join("b.txt"), "beta\n").unwrap();

        // Explicit list; a missing file is isolated, not fatal.
        let mut args = Map::new();
        args.insert(
            "paths".into(),
            Value::Array(vec![
                Value::String(root.join("a.txt").display().to_string()),
                Value::String(root.join("missing.txt").display().to_string()),
                Value::String(root.join("b.txt").display().to_string()),
            ]),
        );
        let outcome = execute_outcome("read", &args, &GlobalCancellation);
        assert!(outcome.ok, "{}", outcome.text);
        assert!(outcome.text.contains("==> "), "{}", outcome.text);
        assert!(outcome.text.contains("1\talpha"), "{}", outcome.text);
        assert!(outcome.text.contains("1\tbeta"), "{}", outcome.text);
        assert!(outcome.text.contains("error:"), "{}", outcome.text);

        // Glob fan-out, sorted, capped.
        let mut args = Map::new();
        args.insert("glob".into(), Value::String("*.txt".into()));
        let outcome = execute_outcome("read", &args, &GlobalCancellation);
        assert!(outcome.ok, "{}", outcome.text);
        assert!(outcome.text.contains("a.txt"), "{}", outcome.text);
        assert!(outcome.text.contains("b.txt"), "{}", outcome.text);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn grep_context_returns_surrounding_lines() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("dex-grep-ctx-test-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("code.rs"),
            "top\nbefore\nNEEDLE here\nafter\nbottom\n",
        )
        .unwrap();

        let mut args = Map::new();
        args.insert("pattern".into(), Value::String("NEEDLE".into()));
        args.insert(
            "path".into(),
            Value::String(root.join("code.rs").display().to_string()),
        );
        args.insert("output_mode".into(), Value::String("content".into()));
        args.insert("context".into(), Value::Number(1.into()));
        let outcome = execute_outcome("grep", &args, &GlobalCancellation);
        assert!(outcome.ok, "{}", outcome.text);
        assert!(outcome.text.contains("before"), "{}", outcome.text);
        assert!(outcome.text.contains("after"), "{}", outcome.text);
        assert!(!outcome.text.contains("top"), "{}", outcome.text);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn chain_runs_search_then_reads_matched_files_in_one_call() {
        // Fixtures live at the workspace root (not target/) because search
        // rules deliberately exclude target/.
        let needle = format!("TARGET_{}", "TOKEN");
        let root = std::env::current_dir()
            .unwrap()
            // Prefix must NOT match .gitignore entries: the chain's grep respects
        // ignore files, so an ignored fixture dir is invisible to it.
        .join(format!("dex-chain-fx-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("one.rs"), format!("{needle} in one\n")).unwrap();
        fs::write(root.join("two.rs"), "nothing here\n").unwrap();

        let args = json!({
            "steps": [
                {"tool": "grep", "args": {"pattern": &needle, "path": ".", "output_mode": "files"}},
                {"tool": "read", "from": 0, "take": "paths", "args": {"limit": 10}}
            ]
        })
        .as_object()
        .unwrap()
        .clone();
        let outcome = execute_outcome("chain", &args, &GlobalCancellation);
        assert!(outcome.ok, "{}", outcome.text);
        assert!(
            outcome.text.contains("--- step 0: grep ---"),
            "{}",
            outcome.text
        );
        assert!(
            outcome.text.contains("--- step 1: read ---"),
            "{}",
            outcome.text
        );
        assert!(
            outcome.text.contains(&format!("{needle} in one")),
            "{}",
            outcome.text
        );

        // Mutation and shell tools are refused inside chains.
        let args = json!({
            "steps": [
                {"tool": "grep", "args": {"pattern": "x-unlikely", "output_mode": "files"}},
                {"tool": "bash", "args": {"command": "echo hi"}}
            ]
        })
        .as_object()
        .unwrap()
        .clone();
        let outcome = execute_outcome("chain", &args, &GlobalCancellation);
        assert!(!outcome.ok, "{}", outcome.text);
        assert!(outcome.text.contains("read-only"), "{}", outcome.text);

        // `from` must reference an earlier step.
        let args = json!({
            "steps": [
                {"tool": "grep", "args": {"pattern": "x-unlikely", "output_mode": "files"}},
                {"tool": "read", "from": 1, "take": "paths"}
            ]
        })
        .as_object()
        .unwrap()
        .clone();
        let outcome = execute_outcome("chain", &args, &GlobalCancellation);
        assert!(!outcome.ok, "{}", outcome.text);
        assert!(outcome.text.contains("earlier step"), "{}", outcome.text);

        // A failing second step still ships the first step's output.
        let args = json!({
            "steps": [
                {"tool": "grep", "args": {"pattern": &needle, "path": ".", "output_mode": "files"}},
                {"tool": "read", "from": 0, "take": "paths", "args": {"offset": 99}}
            ]
        })
        .as_object()
        .unwrap()
        .clone();
        let outcome = execute_outcome("chain", &args, &GlobalCancellation);
        assert!(!outcome.ok, "{}", outcome.text);
        assert!(
            outcome.text.contains("step 0: grep") && outcome.text.contains("step 1 failed"),
            "{}",
            outcome.text
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn grep_without_matches_is_success() {
        // Assembled at runtime so the needle does not appear in this source
        // file (the test greps the crate it lives in).
        let needle = format!("dex-no-such-token-{}", "xyz");
        let mut args = Map::new();
        args.insert("pattern".into(), Value::String(needle));
        let outcome = execute_outcome("grep", &args, &GlobalCancellation);
        assert!(
            outcome.ok,
            "grep exit 1 (no matches) must be ok: {}",
            outcome.text
        );
        assert!(
            outcome.text.trim().is_empty(),
            "no matches must produce empty output: {:?}",
            outcome.text
        );
    }

    #[test]
    fn find_defaults_path_to_workspace() {
        let mut args = Map::new();
        args.insert("pattern".into(), Value::String("mod.rs".into()));
        args.insert("path".into(), Value::String("src".into()));
        let with_path = execute_outcome("find", &args, &GlobalCancellation);
        assert!(with_path.ok, "{}", with_path.text);
        let mut args = Map::new();
        args.insert("pattern".into(), Value::String("mod.rs".into()));
        let without_path = execute_outcome("find", &args, &GlobalCancellation);
        assert!(
            without_path.ok,
            "find without path must default to '.': {}",
            without_path.text
        );
        assert!(without_path.text.contains("src/tools/mod.rs"));
    }

    #[test]
    fn failed_shell_command_keeps_output_and_exit_marker() {
        let mut args = Map::new();
        args.insert("command".into(), Value::String("echo boom; exit 2".into()));
        let outcome = execute_outcome("bash", &args, &GlobalCancellation);
        assert!(!outcome.ok);
        assert!(outcome.text.contains("boom"));
        assert!(outcome.text.contains("[exit 2]"));
    }
}
