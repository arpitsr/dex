use serde_json::{Map, Value};
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[path = "tools/executor.rs"]
pub(crate) mod executor;
#[path = "tools/permissions.rs"]
pub(crate) mod permissions;
#[path = "tools/registry.rs"]
pub(crate) mod registry;

unsafe extern "C" {
    fn setpgid(pid: i32, pgid: i32) -> i32;
    fn kill(pid: i32, signal: i32) -> i32;
}

const SIGKILL: i32 = 9;
static CONFIGURED_OUTPUT_LIMIT: AtomicUsize = AtomicUsize::new(1_048_576);

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
    InvalidArgument(&'static str),
    Io(io::Error),
    EditNotUnique(usize),
    OutsideWorkspace(String),
    Unknown(String),
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

pub(crate) trait ToolExecutor {
    fn execute(&self, name: &str, args: &Map<String, Value>) -> Result<String, ToolError>;
}

pub(crate) struct WorkspaceToolExecutor;

impl ToolExecutor for WorkspaceToolExecutor {
    fn execute(&self, name: &str, args: &Map<String, Value>) -> Result<String, ToolError> {
        executor::execute(name, args)
    }
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
            Self::EditNotUnique(n) => write!(f, "edit target appears {} times (need exactly 1)", n),
            Self::OutsideWorkspace(path) => write!(f, "path is outside the workspace: {}", path),
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
    let path = base.join("ak/audit.jsonl");
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
        let _ = writeln!(file, "{}", record);
    }
}

/// Maximum wall-clock duration for a shell command.
/// Configure with AK_TOOL_TIMEOUT_SECS (default: 120).
/// Output is capped by AK_TOOL_OUTPUT_BYTES (default: 1 MiB).
fn shell_timeout() -> Duration {
    env::var("AK_TOOL_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(120))
}

fn read_limited<R: Read>(mut reader: R, limit: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(limit.min(8192));
    let mut buffer = [0u8; 8192];
    while bytes.len() < limit {
        let chunk = (limit - bytes.len()).min(buffer.len());
        let size = reader.read(&mut buffer[..chunk]);
        match size {
            Ok(0) | Err(_) => break,
            Ok(size) => bytes.extend_from_slice(&buffer[..size]),
        }
    }
    bytes
}

pub(crate) fn cancellation_requested() -> bool {
    crate::take_cancel_requested()
}

fn run_bash(command: &str) -> Result<String, ToolError> {
    let timeout = shell_timeout();
    let max_bytes = env::var("AK_TOOL_OUTPUT_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|bytes| *bytes > 0)
        .unwrap_or_else(|| CONFIGURED_OUTPUT_LIMIT.load(Ordering::Relaxed));
    run_bash_with_limits(command, timeout, max_bytes)
}

fn run_bash_with_limits(
    command: &str,
    timeout: Duration,
    max_bytes: usize,
) -> Result<String, ToolError> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
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
        if cancellation_requested() {
            unsafe {
                let _ = kill(-(child.id() as i32), SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Ok("Error: shell command cancelled".to_string());
        }
        if Instant::now() >= deadline {
            unsafe {
                let _ = kill(-(child.id() as i32), SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Ok(format!(
                "Error: shell command timed out after {} seconds",
                timeout.as_secs()
            ));
        }
        thread::sleep(Duration::from_millis(25));
    };
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    let mut result = String::from_utf8_lossy(&stdout).into_owned();
    result.push_str(&String::from_utf8_lossy(&stderr));
    if !status.success() {
        result.push_str(&format!("\n[exit {}]", status.code().unwrap_or(-1)));
    }
    Ok(result)
}

fn tool_read(args: &Map<String, Value>) -> Result<String, ToolError> {
    fs::read_to_string(workspace_path(&arg_str(args, "path")?)?).map_err(ToolError::Io)
}

fn tool_bash(args: &Map<String, Value>) -> Result<String, ToolError> {
    run_bash(&arg_str(args, "command")?)
}

fn tool_write(args: &Map<String, Value>) -> Result<String, ToolError> {
    let path = workspace_path(&arg_str(args, "path")?)?;
    fs::write(&path, arg_str(args, "content")?).map_err(ToolError::Io)?;
    Ok(format!("wrote {}", path.display()))
}

fn tool_edit(args: &Map<String, Value>) -> Result<String, ToolError> {
    let path = workspace_path(&arg_str(args, "path")?)?;
    let old = arg_str(args, "oldText")?;
    let new = arg_str(args, "newText")?;
    let content = fs::read_to_string(&path).map_err(ToolError::Io)?;
    let updated = replace_exact(&content, &old, &new)?;
    fs::write(&path, updated).map_err(ToolError::Io)?;
    Ok(format!("edited {}", path.display()))
}

fn replace_exact(content: &str, old: &str, new: &str) -> Result<String, ToolError> {
    let count = content.matches(old).count();
    if count != 1 {
        return Err(ToolError::EditNotUnique(count));
    }
    Ok(content.replacen(old, new, 1))
}

fn tool_grep(args: &Map<String, Value>) -> Result<String, ToolError> {
    let pattern = arg_str(args, "pattern")?;
    let path = arg_str(args, "path").unwrap_or_else(|_| ".".to_string());
    let path = workspace_path(&path)?;
    run_bash(&format!(
        "grep -R -I -n -- {} {}",
        shell_escape(&pattern),
        shell_escape(&path.to_string_lossy())
    ))
}

fn tool_find(args: &Map<String, Value>) -> Result<String, ToolError> {
    let pattern = arg_str(args, "pattern")?;
    let path = arg_str(args, "path")?;
    if pattern.trim().is_empty() || pattern == "*" {
        return Err(ToolError::InvalidArgument(
            "find pattern must be targeted (not empty or '*')",
        ));
    }
    let path = workspace_path(&path)?;
    run_bash(&format!(
        "find {} -path '*{}*' -print",
        shell_escape(&path.to_string_lossy()),
        shell_escape(&pattern)
    ))
}

fn tool_git(args: &Map<String, Value>) -> Result<String, ToolError> {
    let mode = args.get("mode").and_then(Value::as_str).unwrap_or("status");
    if !matches!(mode, "status" | "diff") {
        return Err(ToolError::NotString("mode (status or diff)"));
    }
    let root = workspace_root()?;
    let output = Command::new("git")
        .arg(mode)
        .current_dir(root)
        .output()
        .map_err(ToolError::Io)?;
    let mut result = String::from_utf8_lossy(&output.stdout).into_owned();
    result.push_str(&String::from_utf8_lossy(&output.stderr));
    if !output.status.success() {
        result.push_str(&format!("\n[exit {}]", output.status.code().unwrap_or(-1)));
    }
    Ok(result)
}

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// Execute a tool using paths confined to the current workspace.
pub(crate) fn execute(name: &str, args: &Map<String, Value>) -> Result<String, ToolError> {
    if registry::metadata(name).is_none() {
        let error = ToolError::Unknown(name.to_string());
        audit(name, args, &error.to_string());
        return Err(error);
    }
    let result = match name {
        "read" => tool_read(args),
        "bash" => tool_bash(args),
        "write" => tool_write(args),
        "edit" => tool_edit(args),
        "grep" => tool_grep(args),
        "find" => tool_find(args),
        "git" => tool_git(args),
        _ => unreachable!("metadata and dispatch must stay in sync"),
    };
    let outcome = match &result {
        Ok(_) => "ok".to_string(),
        Err(error) => error.to_string(),
    };
    audit(name, args, &outcome);
    result
}

pub(crate) fn execute_to_string(name: &str, args: &Map<String, Value>) -> String {
    match WorkspaceToolExecutor.execute(name, args) {
        Ok(out) => out,
        Err(e) => format!("Error: {}", e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shell_escape_handles_quotes_and_commands() {
        assert_eq!(shell_escape("a'b; echo hacked"), "'a'\"'\"'b; echo hacked'");
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
            execute("read", &args),
            Err(ToolError::Missing("path"))
        ));
        let mut args = Map::new();
        args.insert("path".into(), Value::Bool(true));
        assert!(matches!(
            execute("read", &args),
            Err(ToolError::NotString("path"))
        ));
    }

    #[test]
    fn find_rejects_unbounded_patterns() {
        let mut args = Map::new();
        args.insert("pattern".into(), Value::String("*".into()));
        args.insert("path".into(), Value::String(".".into()));
        assert!(matches!(
            execute("find", &args),
            Err(ToolError::InvalidArgument(_))
        ));
    }

    #[test]
    fn edit_requires_exactly_one_match() {
        assert!(matches!(
            replace_exact("a a", "a", "b"),
            Err(ToolError::EditNotUnique(2))
        ));
        assert_eq!(replace_exact("a", "a", "b").unwrap(), "b");
        assert!(matches!(
            replace_exact("a", "x", "b"),
            Err(ToolError::EditNotUnique(0))
        ));
    }

    #[test]
    fn temporary_workspace_paths_are_confined() {
        let root = std::env::temp_dir().join(format!("ak-workspace-test-{}", std::process::id()));
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
        let result = run_bash_with_limits("sleep 1", Duration::from_millis(10), 1024).unwrap();
        assert!(result.contains("timed out"));
    }
}
