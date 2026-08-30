use serde_json::Value;
use std::process::Command;

pub(crate) fn truncate_text(text: &str, max_bytes: usize, max_lines: usize) -> String {
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

pub(crate) fn terminal_preview(text: &str) -> String {
    truncate_text(text, 10 * 1024, 100)
}

/// Compact single-line summary of a tool's arguments for the REPL transcript.
/// Pulls the primary arg (path/command/pattern) instead of dumping raw JSON.
pub(crate) fn short_arg(name: &str, input: &str) -> String {
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
pub(crate) fn one_line_summary(text: &str) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let line = line.trim();
    let limit = line
        .char_indices()
        .nth(120)
        .map(|(i, _)| i)
        .unwrap_or(line.len());
    line[..limit].to_string()
}

/// Human-sized result for the TUI. The full result still goes to the model;
/// the transcript only needs enough information to explain what happened.
pub(crate) fn tool_result_summary(name: &str, text: &str) -> String {
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

pub(crate) fn model_tool_result(text: &str) -> String {
    truncate_text(text, 50 * 1024, 2_000)
}

/// Git branch + dirty flag for a working directory, for status displays.
pub(crate) fn git_context(cwd: &str) -> (Option<String>, bool) {
    let branch = Command::new("git")
        .args(["-C", cwd, "branch", "--show-current"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|branch| !branch.is_empty());
    let dirty = branch.is_some()
        && Command::new("git")
            .args(["-C", cwd, "status", "--porcelain"])
            .output()
            .ok()
            .is_some_and(|output| !output.stdout.is_empty());
    (branch, dirty)
}
