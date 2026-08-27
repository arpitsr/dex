use serde_json::{Map, Value};
use std::fs;
use std::io;
use std::process::Command;

#[derive(Debug)]
pub(crate) enum ToolError {
    Missing(&'static str),
    NotString(&'static str),
    Io(io::Error),
    EditNotUnique(usize),
    Unknown(String),
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(k) => write!(f, "missing argument '{}'", k),
            Self::NotString(k) => write!(f, "argument '{}' must be a string", k),
            Self::Io(e) => write!(f, "io error: {}", e),
            Self::EditNotUnique(n) => write!(f, "edit target appears {} times (need exactly 1)", n),
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

fn run_bash(command: &str) -> Result<String, ToolError> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(command)
        .output()
        .map_err(ToolError::Io)?;
    let mut result = String::from_utf8_lossy(&out.stdout).into_owned();
    result.push_str(&String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        result.push_str(&format!("\n[exit {}]", out.status.code().unwrap_or(-1)));
    }
    Ok(result)
}

fn tool_read(args: &Map<String, Value>) -> Result<String, ToolError> {
    fs::read_to_string(arg_str(args, "path")?).map_err(ToolError::Io)
}

fn tool_bash(args: &Map<String, Value>) -> Result<String, ToolError> {
    run_bash(&arg_str(args, "command")?)
}

fn tool_write(args: &Map<String, Value>) -> Result<String, ToolError> {
    let path = arg_str(args, "path")?;
    fs::write(&path, arg_str(args, "content")?).map_err(ToolError::Io)?;
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
    run_bash(&format!(
        "grep -R -I -n -- {} {}",
        shell_escape(&pattern),
        shell_escape(&path)
    ))
}

fn tool_find(args: &Map<String, Value>) -> Result<String, ToolError> {
    let pattern = arg_str(args, "pattern")?;
    let path = arg_str(args, "path").unwrap_or_else(|_| ".".to_string());
    run_bash(&format!(
        "find {} -path '*{}*' -print",
        shell_escape(&path),
        pattern
    ))
}

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

pub(crate) fn execute(name: &str, args: &Map<String, Value>) -> Result<String, ToolError> {
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

pub(crate) fn execute_to_string(name: &str, args: &Map<String, Value>) -> String {
    match execute(name, args) {
        Ok(out) => out,
        Err(e) => format!("Error: {}", e),
    }
}
