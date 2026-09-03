#![allow(clippy::doc_lazy_continuation)]
mod fff;

use self::fff::{tool_fffind, tool_ffgrep};
use serde_json::{Map, Value};
use similar::TextDiff;
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::agent::state::CancellationSource;
use crate::core::format::clamp_lines;

unsafe extern "C" {
    fn setpgid(pid: i32, pgid: i32) -> i32;
    fn kill(pid: i32, signal: i32) -> i32;
    fn setsid() -> i32;
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
    /// A write/edit supplied an `expected_hash` that no longer matches the
    /// file on disk (someone else changed it since the model's last read).
    /// The caller must re-read and retry — the write is not applied.
    StaleFile {
        path: String,
        expected: String,
        actual: String,
    },
    /// A shell command ran but signalled failure (non-zero exit, killed, or
    /// timed out). `code` is `None` when the process never exited on its own.
    /// Carries the combined output so partial results still reach the model.
    Shell {
        output: String,
        code: Option<i32>,
    },
    /// An internal tool-engine failure (not a bad invocation, not a shell
    /// exit): e.g. the fff index failed to initialize.
    Internal(String),
    Unknown(String),
}

/// A tool result together with whether the call actually succeeded. Success
/// is decided where the exit status is known — never inferred from the
/// output text, which may legitimately contain markers like `[exit 1]`.
/// `diff` carries the display-only git diff for write/edit, captured before
/// the file was mutated; it never reaches the model.
#[derive(Clone, Debug)]
pub(crate) struct ToolOutcome {
    pub(crate) text: String,
    pub(crate) ok: bool,
    pub(crate) diff: Option<String>,
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
        // fff tools run in-process; only git shells out.
        "ffgrep" | "fffind" => ToolMetadata {
            read_only: true,
            mutating: false,
            idempotent: true,
            requires_shell: false,
            permission: PermissionRequirement::Read,
        },
        "git" => ToolMetadata {
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
            Self::StaleFile {
                path,
                expected,
                actual,
            } => write!(
                f,
                "file changed since it was read (expected_hash mismatch: expected {expected}, file is {actual}) — re-read {} and retry; concurrent edit wins, your write was not applied",
                path
            ),
            Self::Shell { output, code } => match code {
                Some(code) => write!(f, "{output}\n[exit {code}]"),
                None => write!(f, "{output}"),
            },
            Self::Internal(e) => write!(f, "{e}"),
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
    let mut env = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        env.push(("DEX_BIN".to_string(), exe.display().to_string()));
    }
    // git via bash without --no-pager can invoke delta/bat/less which
    // probe the terminal (OSC 10/11) and race crossterm for the reply;
    // force a non-interactive pager so the child never queries the pts.
    env.push(("GIT_PAGER".to_string(), "cat".to_string()));
    env.push(("PAGER".to_string(), "cat".to_string()));
    env
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
    let mut builder = Command::new("sh");
    builder
        .arg("-c")
        .arg(command)
        .envs(tool_runner_env())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // New session for the shell: drops the controlling tty, so tool children
    // can never write to or race the user's terminal for input. Without this,
    // a child that probes the terminal (e.g. `cargo test` running the theme
    // tests → OSC 10/11 on /dev/tty) sends queries to the TUI's pts and races
    // crossterm for the reply — the TUI can end up with half a color report
    // typed into the composer.
    // SAFETY: runs in the forked child before exec; it is not yet a process
    // group leader, so setsid() succeeds.
    unsafe {
        builder.pre_exec(|| {
            let _ = setsid();
            Ok(())
        });
    }
    let mut child = builder.spawn().map_err(ToolError::Io)?;
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

/// `read` returns line-numbered content (right-aligned number + two-space gap
/// + tab-expanded content per .editorconfig/language) so subsequent `edit`
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
    let tab_width = detect_tab_width(path);
    let numbered: Vec<String> = lines
        .iter()
        .skip(offset - 1)
        .take(limit)
        .enumerate()
        .map(|(index, line)| {
            let expanded = expand_tabs(line, tab_width);
            format!("{:>4}  {}", offset + index, expanded)
        })
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
    check_expected_hash(args, &path)?;
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

/// When a write/edit carries `expected_hash`, reject if the file on disk no
/// longer matches (stale read → 409 semantics). Absent file hashes to the
/// empty-string sentinel.
fn check_expected_hash(args: &Map<String, Value>, path: &Path) -> Result<(), ToolError> {
    let Some(expected) = args.get("expected_hash").and_then(Value::as_str) else {
        return Ok(());
    };
    if expected.is_empty() {
        return Ok(());
    }
    let actual = hash_file(&path.display().to_string());
    if actual != expected {
        return Err(ToolError::StaleFile {
            path: path.display().to_string(),
            expected: expected.to_string(),
            actual,
        });
    }
    Ok(())
}

/// FNV-1a 64-bit hex hash of a file's bytes. An absent file hashes as empty
/// content (the before-hash of a `write` creating a new file).
pub(crate) fn hash_file(path: &str) -> String {
    let bytes = fs::read(path).unwrap_or_default();
    let mut hash = 2166136261u64;
    for b in &bytes {
        hash = (hash ^ u64::from(*b)).wrapping_mul(16777619);
    }
    format!("{:016x}", hash)
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
    check_expected_hash(args, &path)?;
    let content = fs::read_to_string(&path).map_err(ToolError::Io)?;
    let (updated, note) = apply_edit(&content, &old, &new, replace_all)?;
    fs::write(&path, updated).map_err(ToolError::Io)?;
    Ok(format!("edited {}{note}", path.display()))
}

/// Git-style unified diff (4 context lines, `--- a/…` / `+++ b/…` headers,
/// `/dev/null` for new files) of a pending write/edit, shown in the
/// transcript before approval and under the tool result. Returns None when
/// the file is missing or the change is empty.
pub(crate) fn change_diff(name: &str, args: &Map<String, Value>) -> Option<String> {
    let raw_path = arg_str(args, "path").ok()?;
    let path = workspace_path(&raw_path).ok()?;
    let before = fs::read_to_string(&path).ok();
    let after = match name {
        "write" => arg_str(args, "content").ok(),
        "edit" => {
            let old = arg_str(args, "oldText").ok()?;
            let new = arg_str(args, "newText").ok()?;
            let replace_all = args
                .get("replaceAll")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            apply_edit(before.as_deref().unwrap_or(""), &old, &new, replace_all)
                .ok()
                .map(|(updated, _)| updated)
        }
        _ => return None,
    }?;
    let diff = TextDiff::from_lines(before.as_deref().unwrap_or(""), &after);
    let (old_header, new_header) = match &before {
        Some(_) => (format!("a/{raw_path}"), format!("b/{raw_path}")),
        None => ("/dev/null".to_string(), format!("b/{raw_path}")),
    };
    let out = diff
        .unified_diff()
        .context_radius(4)
        .header(&old_header, &new_header)
        .to_string();
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
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

fn expand_tabs(line: &str, width: usize) -> String {
    let width = width.clamp(1, 16);
    if !line.contains('\t') {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len() + width);
    let mut col: usize = 0;
    for ch in line.chars() {
        if ch == '\t' {
            let spaces = width - (col % width);
            out.push_str(&" ".repeat(spaces));
            col += spaces;
        } else {
            // use display width for tabstop tracking (CJK etc.), but at
            // tool layer we only need byte-column for indentation; unicode
            // width keeps generic correctness for any file.
            let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            out.push(ch);
            col += w;
        }
    }
    out
}

fn detect_tab_width(path: &Path) -> usize {
    if let Ok(raw) = env::var("DEX_TAB_WIDTH") {
        if let Ok(v) = raw.parse::<usize>() {
            if (1..=16).contains(&v) {
                return v;
            }
        }
    }
    if let Some(v) = editorconfig_tab_width(path) {
        return v;
    }
    language_tab_width(path)
}

fn editorconfig_tab_width(path: &Path) -> Option<usize> {
    let root = workspace_root().ok()?.canonicalize().ok()?;
    let mut dir = path.parent()?.canonicalize().ok()?;
    loop {
        let cfg = dir.join(".editorconfig");
        if let Ok(text) = fs::read_to_string(&cfg) {
            // Minimal parser: last matching indent_size/tab_width wins.
            // Handles `[*]`, `[*.rs]`, `[*.{js,ts}]` via simple substring/glob.
            let file_name = path.file_name()?.to_string_lossy().to_string();
            let ext = path.extension()?.to_string_lossy().to_string();
            let mut best: Option<usize> = None;
            let mut in_matching_section = true; // global pre-section
            for raw_line in text.lines() {
                let line = raw_line.trim();
                if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                    continue;
                }
                if line.starts_with('[') && line.ends_with(']') {
                    let pat = line[1..line.len() - 1].trim().to_ascii_lowercase();
                    // very small glob: * matches all, *.ext matches extension,
                    // otherwise substring check
                    in_matching_section = if pat == "*" || pat == "[*]" {
                        true
                    } else if pat.contains('*') {
                        // crude: check extension or substring
                        pat.contains(&ext.to_ascii_lowercase())
                            || pat.contains(&file_name.to_ascii_lowercase())
                    } else {
                        file_name.eq_ignore_ascii_case(&pat)
                    };
                    continue;
                }
                if !in_matching_section {
                    continue;
                }
                let lower = line.to_ascii_lowercase();
                for key in ["tab_width", "indent_size", "tabwidth"] {
                    if lower.starts_with(key) {
                        if let Some(eq) = line.find('=') {
                            let val = line[eq + 1..].trim();
                            if let Ok(v) = val.parse::<usize>() {
                                if (1..=16).contains(&v) {
                                    best = Some(v);
                                }
                            }
                        }
                    }
                }
            }
            if let Some(v) = best {
                return Some(v);
            }
        }
        if dir == root {
            break;
        }
        let parent = dir.parent()?;
        if !parent.starts_with(&root) && dir != root {
            // also check one level above root for repo-level config
            // then stop
            if parent == root.parent().unwrap_or(Path::new("/")) {
                break;
            }
        }
        dir = parent.to_path_buf();
        if dir.parent().is_none() {
            break;
        }
    }
    None
}

fn language_tab_width(path: &Path) -> usize {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name == "Makefile" || name == "makefile" || name == "GNUmakefile" {
        return 8;
    }
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
    {
        Some(ext) if matches!(ext.as_str(), "go") => 4, // gofmt uses tabs but 4 is readable; 8 is terminal-faithful, choose 4 for preview density like pi
        Some(ext)
            if matches!(
                ext.as_str(),
                "py" | "rs"
                    | "js"
                    | "ts"
                    | "jsx"
                    | "tsx"
                    | "c"
                    | "cpp"
                    | "h"
                    | "hpp"
                    | "java"
                    | "json"
                    | "toml"
                    | "yaml"
                    | "yml"
                    | "md"
                    | "sh"
                    | "bash"
                    | "rb"
                    | "php"
            ) =>
        {
            4
        }
        _ => 4,
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
        if !matches!(source_tool.as_str(), "ffgrep" | "fffind" | "chain") {
            return Err(invalid(format!(
                "'from' step {from} is '{source_tool}', which produces no file paths; use ffgrep (files mode) or fffind"
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
        "ffgrep" => tool_ffgrep(args),
        "fffind" => tool_fffind(args),
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
            diff: None,
        },
        Err(e) => ToolOutcome {
            text: format!("Error: {}", e),
            ok: false,
            diff: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::state::GlobalCancellation;
    use serde_json::json;
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
    fn bash_children_have_no_controlling_tty() {
        // A tool child must never share the user's terminal: a child that
        // probes it (e.g. cargo test → theme query → OSC 10/11 on /dev/tty)
        // would write to and race the TUI's crossterm for the same pts, and
        // half a color report could end up typed into the composer.
        let (output, code) = run_bash_with_limits(
            "if cat </dev/tty >/dev/null 2>&1; then echo HAS_TTY; else echo NO_TTY; fi",
            Duration::from_secs(5),
            4096,
            &GlobalCancellation,
        )
        .unwrap();
        assert_eq!(code, Some(0));
        assert!(
            output.contains("NO_TTY"),
            "tool child still has a controlling tty: {output}"
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
    fn fffind_rejects_unbounded_patterns() {
        let mut args = Map::new();
        args.insert("pattern".into(), Value::String("*".into()));
        assert!(matches!(
            execute("fffind", &args, &GlobalCancellation),
            Err(ToolError::InvalidArgument(_))
        ));
        let mut args = Map::new();
        args.insert("pattern".into(), Value::String("".into()));
        assert!(matches!(
            execute("ffgrep", &args, &GlobalCancellation),
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
    fn write_edit_require_expected_hash_and_reject_stale() {
        // Real workspace file under target/ (inside cwd, cleaned up).
        let cwd = std::env::current_dir().unwrap();
        fs::create_dir_all(cwd.join("target")).unwrap();
        let path = cwd.join("target/dex-stale-test.txt");
        fs::write(&path, "v1\n").unwrap();
        let rel = "target/dex-stale-test.txt";
        let h = hash_file(&path.display().to_string());

        // Correct expected_hash: edit applies.
        let mut args = Map::new();
        args.insert("path".into(), Value::String(rel.into()));
        args.insert("oldText".into(), Value::String("v1".into()));
        args.insert("newText".into(), Value::String("v2".into()));
        args.insert("expected_hash".into(), Value::String(h.clone()));
        assert!(execute("edit", &args, &GlobalCancellation).is_ok());
        assert_eq!(fs::read_to_string(&path).unwrap(), "v2\n");

        // Stale expected_hash: rejected, file untouched.
        let mut stale = Map::new();
        stale.insert("path".into(), Value::String(rel.into()));
        stale.insert("oldText".into(), Value::String("v2".into()));
        stale.insert("newText".into(), Value::String("v3".into()));
        stale.insert("expected_hash".into(), Value::String("deadbeef".into()));
        assert!(matches!(
            execute("edit", &stale, &GlobalCancellation),
            Err(ToolError::StaleFile { .. })
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), "v2\n");

        // hash_file is deterministic.
        assert_eq!(
            hash_file(&path.display().to_string()),
            hash_file(&path.display().to_string())
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn change_diff_shows_unified_diff_for_write_and_edit() {
        let cwd = std::env::current_dir().unwrap();
        fs::create_dir_all(cwd.join("target")).unwrap();
        let path = cwd.join("target/dex-preview-test.txt");
        fs::write(&path, "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n").unwrap();
        let rel = "target/dex-preview-test.txt";
        let mut args = Map::new();
        args.insert("path".into(), Value::String(rel.into()));
        args.insert("oldText".into(), Value::String("l5\n".into()));
        args.insert("newText".into(), Value::String("L5\nL5b\n".into()));
        let diff = change_diff("edit", &args).unwrap();
        assert!(diff.contains("--- a/target/dex-preview-test.txt"), "{diff}");
        assert!(diff.contains("+++ b/target/dex-preview-test.txt"), "{diff}");
        assert!(diff.contains("@@"), "{diff}");
        assert!(diff.contains("-l5"), "{diff}");
        assert!(diff.contains("+L5b"), "{diff}");
        // Context lines surround the change (4 radius) and are untouched.
        assert!(diff.lines().any(|l| l == " l4"), "{diff}");
        assert!(diff.lines().any(|l| l == " l9"), "{diff}");
        // Lines beyond the 4-line context radius stay outside the hunks.
        assert!(!diff.contains(" l10\n"), "{diff}");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn change_diff_new_file_uses_dev_null_header() {
        let cwd = std::env::current_dir().unwrap();
        let rel = "target/dex-preview-new.txt";
        let _ = fs::remove_file(cwd.join(rel));
        let mut args = Map::new();
        args.insert("path".into(), Value::String(rel.into()));
        args.insert("content".into(), Value::String("hello\n".into()));
        let diff = change_diff("write", &args).unwrap();
        assert!(diff.contains("--- /dev/null"), "{diff}");
        assert!(diff.contains("+++ b/target/dex-preview-new.txt"), "{diff}");
        assert!(diff.contains("+hello"), "{diff}");
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
        // Line numbers are now right-aligned with two spaces (no raw tab) and file
        // tabs are expanded per tab_width, so the separator is stable.
        assert_eq!(
            outcome.text,
            "   1  one\n   2  two\n   3  three\n   4  four"
        );

        args.insert("offset".into(), Value::Number(2.into()));
        args.insert("limit".into(), Value::Number(1.into()));
        let outcome = execute_outcome("read", &args, &GlobalCancellation);
        assert_eq!(outcome.text, "   2  two");

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
        assert!(outcome.text.contains("   1  alpha"), "{}", outcome.text);
        assert!(outcome.text.contains("   1  beta"), "{}", outcome.text);
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
    fn ffgrep_context_returns_surrounding_lines() {
        // Fixture at the workspace root (not target/): fff respects
        // .gitignore, so ignored fixture dirs are invisible to it.
        let root = std::env::current_dir()
            .unwrap()
            .join(format!("dex-fff-ctx-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let needle = format!("CTXNEEDLE_{}", std::process::id());
        fs::write(
            root.join("code.rs"),
            format!("top\nbefore\n{needle} here\nafter\nbottom\n"),
        )
        .unwrap();
        super::fff::rescan();

        let mut args = Map::new();
        args.insert("pattern".into(), Value::String(needle.clone()));
        args.insert("output_mode".into(), Value::String("content".into()));
        args.insert("context".into(), Value::Number(1.into()));
        let outcome = execute_outcome("ffgrep", &args, &GlobalCancellation);
        assert!(outcome.ok, "{}", outcome.text);
        assert!(outcome.text.contains("before"), "{}", outcome.text);
        assert!(outcome.text.contains("after"), "{}", outcome.text);
        assert!(!outcome.text.contains("top"), "{}", outcome.text);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ffgrep_fuzzy_fallback_recovers_typos() {
        let root = std::env::current_dir()
            .unwrap()
            .join(format!("dex-fff-typo-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("thing.rs"),
            "struct UserAccountController { field: u32 }\n",
        )
        .unwrap();
        super::fff::rescan();

        // Exact query misses (the token has a transposed 'lr'), fuzzy retry hits.
        // Assembled at runtime so the query text does not appear verbatim in
        // this source file — the exact search would hit this file otherwise.
        let mut args = Map::new();
        args.insert(
            "pattern".into(),
            Value::String(format!("UserAccountControlel{}", "r")),
        );
        args.insert("output_mode".into(), Value::String("content".into()));
        let outcome = execute_outcome("ffgrep", &args, &GlobalCancellation);
        assert!(outcome.ok, "{}", outcome.text);
        assert!(outcome.text.contains("approximate"), "{}", outcome.text);
        assert!(outcome.text.contains("UserAccountController"), "{}", outcome.text);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn chain_runs_search_then_reads_matched_files_in_one_call() {
        // Fixtures live at the workspace root (not target/): fff respects
        // .gitignore, so ignored fixture dirs are invisible to it.
        let needle = format!("TARGET_{}", "TOKEN");
        let root = std::env::current_dir()
            .unwrap()
            .join(format!("dex-chain-fx-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("one.rs"), format!("{needle} in one\n")).unwrap();
        fs::write(root.join("two.rs"), "nothing here\n").unwrap();
        super::fff::rescan();

        let args = json!({
            "steps": [
                {"tool": "ffgrep", "args": {"pattern": &needle, "output_mode": "files"}},
                {"tool": "read", "from": 0, "take": "paths", "args": {"limit": 10}}
            ]
        })
        .as_object()
        .unwrap()
        .clone();
        let outcome = execute_outcome("chain", &args, &GlobalCancellation);
        assert!(outcome.ok, "{}", outcome.text);
        assert!(
            outcome.text.contains("--- step 0: ffgrep ---"),
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
                {"tool": "ffgrep", "args": {"pattern": "x-unlikely", "output_mode": "files"}},
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
                {"tool": "ffgrep", "args": {"pattern": "x-unlikely", "output_mode": "files"}},
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
                {"tool": "ffgrep", "args": {"pattern": &needle, "output_mode": "files"}},
                {"tool": "read", "from": 0, "take": "paths", "args": {"offset": 99}}
            ]
        })
        .as_object()
        .unwrap()
        .clone();
        let outcome = execute_outcome("chain", &args, &GlobalCancellation);
        assert!(!outcome.ok, "{}", outcome.text);
        assert!(
            outcome.text.contains("step 0: ffgrep") && outcome.text.contains("step 1 failed"),
            "{}",
            outcome.text
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ffgrep_without_matches_is_success() {
        // Assembled at runtime so the needle does not appear in this source
        // file (the test greps the crate it lives in). Gibberish so the
        // fuzzy fallback has nothing approximate to land on either.
        let needle = format!("zxq{}wvut", std::process::id());
        let mut args = Map::new();
        args.insert("pattern".into(), Value::String(needle));
        let outcome = execute_outcome("ffgrep", &args, &GlobalCancellation);
        assert!(
            outcome.ok,
            "no matches (exact or fuzzy) must be ok: {}",
            outcome.text
        );
        assert!(
            outcome.text.contains("0 matches."),
            "no matches must report zero: {:?}",
            outcome.text
        );
    }

    #[test]
    fn fffind_finds_paths_fuzzily() {
        super::fff::rescan();
        let mut args = Map::new();
        args.insert("pattern".into(), Value::String("tools mod".into()));
        let outcome = execute_outcome("fffind", &args, &GlobalCancellation);
        assert!(outcome.ok, "{}", outcome.text);
        assert!(outcome.text.contains("src/tools/mod.rs"), "{}", outcome.text);
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
