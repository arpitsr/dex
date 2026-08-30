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

/// Drop ANSI escape sequences (colors, cursor movement) so tool output
/// renders as plain text in the transcript.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            // Consume the CSI sequence up to its final byte (@..~).
            for next in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&next) {
                    break;
                }
            }
        }
        // A lone ESC (or non-CSI sequence) is dropped.
    }
    out
}

/// A few informational lines from a tool result, rendered dim under the
/// one-line summary: enough to see *what* happened without flooding the
/// transcript. Blank lines and ANSI escapes are removed; when truncated, a
/// `… +N more lines` tail notes how much was elided. `skip_first` lets
/// callers omit the line the one-line summary already shows.
pub(crate) fn tool_result_preview(text: &str, max_lines: usize, skip_first: bool) -> Vec<String> {
    let mut lines = text
        .lines()
        .map(strip_ansi)
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty());
    if skip_first {
        lines.next();
    }
    let mut preview: Vec<String> = lines
        .by_ref()
        .map(|line| {
            let limit = line
                .char_indices()
                .nth(120)
                .map(|(i, _)| i)
                .unwrap_or(line.len());
            let mut clipped = line[..limit].to_string();
            if limit < line.len() {
                clipped.push('…');
            }
            clipped
        })
        .take(max_lines)
        .collect();
    let remaining = lines.count();
    if remaining > 0 {
        preview.push(format!(
            "… +{remaining} more line{}",
            if remaining == 1 { "" } else { "s" }
        ));
    }
    preview
}

/// Human-sized result for the TUI. The full result still goes to the model;
/// the transcript only needs enough information to explain what happened.
/// Failure is passed in by the caller (which knows the real exit status) —
/// never inferred from the text, whose content may legitimately contain
/// markers like `[exit 1]`.
pub(crate) fn tool_result_summary(name: &str, text: &str, ok: bool) -> String {
    if !ok {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_uses_explicit_status_not_text_sniffing() {
        // A successful read whose content happens to contain the shell
        // failure marker must not be reported as failed.
        assert!(tool_result_summary(
            "read",
            "use serde_json::{Map, Value};\nresult.push_str(\"[exit 1]\");\n",
            true
        )
        .starts_with("ok ·"));
        // A genuinely failed tool reports failure with its first output line.
        assert_eq!(
            tool_result_summary("bash", "ls: no such file\n[exit 2]", false),
            "failed · ls: no such file"
        );
        assert_eq!(tool_result_summary("grep", "", true), "ok · 0 matches");
    }

    #[test]
    fn preview_shows_meaningful_lines_with_more_tail() {
        let text = "\nfirst\n\nsecond\nthird\nfourth\n";
        assert_eq!(
            tool_result_preview(text, 3, false),
            vec!["first", "second", "third", "… +1 more line"]
        );
    }

    #[test]
    fn preview_can_skip_the_line_already_in_the_summary() {
        let text = "Error: boom\nat src/main.rs:1\nat src/main.rs:2\n";
        assert_eq!(
            tool_result_preview(text, 3, true),
            vec!["at src/main.rs:1", "at src/main.rs:2"]
        );
    }

    #[test]
    fn preview_strips_ansi_and_clips_long_lines() {
        let text = format!("\x1b[1;32m{}\x1b[0m", "x".repeat(200));
        let preview = tool_result_preview(&text, 1, false);
        assert_eq!(preview.len(), 1);
        assert!(preview[0].starts_with("xxx"));
        assert!(preview[0].ends_with('…'));
        assert!(preview[0].chars().count() <= 121);
        assert!(!preview[0].contains('\x1b'));
    }

    #[test]
    fn preview_of_empty_output_is_empty() {
        assert!(tool_result_preview("", 3, false).is_empty());
        assert!(tool_result_preview("\n \n", 3, true).is_empty());
    }
}
