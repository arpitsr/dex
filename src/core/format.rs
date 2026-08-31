use serde_json::Value;
use std::process::Command;

/// Per-line character budget: protects against minified/binary-ish content
/// whose single lines would dominate the context window.
const MAX_LINE_CHARS: usize = 2_000;

/// Head+tail output clamp shared by every tool: keeps the first and last
/// lines (errors and summaries live at the end, imports and context at the
/// start) and replaces the middle with a marker carrying the real counts, so
/// the model always knows how much it did not see. Long lines are clipped to
/// [`MAX_LINE_CHARS`].
pub(crate) fn clamp_lines(text: &str, max_lines: usize, max_bytes: usize) -> String {
    let clipped: Vec<String> = text
        .lines()
        .map(|line| {
            let limit = line
                .char_indices()
                .nth(MAX_LINE_CHARS)
                .map(|(i, _)| i)
                .unwrap_or(line.len());
            if limit < line.len() {
                format!("{}…", &line[..limit])
            } else {
                line.to_string()
            }
        })
        .collect();
    let total = clipped.len();
    let width = |lines: &[String]| -> usize { lines.iter().map(|l| l.len() + 1).sum::<usize>() };

    let fits = width(&clipped) <= max_bytes.saturating_add(total);
    if total <= max_lines && fits {
        return clipped.join("\n");
    }

    let head_budget = max_lines / 2;
    let tail_budget = max_lines - head_budget;
    let mut head: Vec<String> = Vec::new();
    let mut used = 0usize;
    for line in clipped.iter().take(head_budget) {
        if used + line.len() + 1 > max_bytes * 2 / 3 && !head.is_empty() {
            break;
        }
        used += line.len() + 1;
        head.push(line.clone());
    }
    let mut tail: Vec<String> = Vec::new();
    let mut tail_used = 0usize;
    for line in clipped.iter().rev().take(tail_budget) {
        if tail_used + line.len() + 1 > max_bytes / 3 && !tail.is_empty() {
            break;
        }
        tail_used += line.len() + 1;
        tail.push(line.clone());
    }
    tail.reverse();

    let shown = head.len() + tail.len();
    let omitted = total - shown;
    let mut out = head;
    out.push(format!("[... {omitted} of {total} lines truncated ...]"));
    out.extend(tail);
    out.join("\n")
}

pub(crate) fn truncate_text(text: &str, max_bytes: usize, max_lines: usize) -> String {
    clamp_lines(text, max_lines, max_bytes)
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
    let primary: Option<String> = match name {
        "chain" => obj
            .as_ref()
            .and_then(|o| o.get("steps"))
            .and_then(Value::as_array)
            .map(|steps| {
                let tools: Vec<&str> = steps
                    .iter()
                    .filter_map(|step| step.get("tool").and_then(Value::as_str))
                    .collect();
                format!("{} steps: {}", steps.len(), tools.join(" → "))
            }),
        "read" | "write" | "edit" | "grep" | "glob" => get("path")
            .or_else(|| get("file"))
            .or_else(|| get("pattern"))
            .or_else(|| get("glob"))
            .map(str::to_string),
        "bash" => get("command").map(str::to_string),
        _ => None,
    };
    let s = primary
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| input.to_string());
    let s = s.lines().next().unwrap_or(&s).trim();
    let s = strip_ansi(s);
    let limit = s.char_indices().nth(80).map(|(i, _)| i).unwrap_or(s.len());
    s[..limit].to_string()
}

/// First non-empty, trimmed line of a tool result, truncated — a one-line
/// confirmation for the REPL transcript instead of the full output.
pub(crate) fn one_line_summary(text: &str) -> String {
    let stripped = strip_ansi(text);
    let line = stripped
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    let line = line.trim();
    let limit = line
        .char_indices()
        .nth(120)
        .map(|(i, _)| i)
        .unwrap_or(line.len());
    line[..limit].to_string()
}

/// Drop ANSI escape sequences (colors, cursor movement) and carriage
/// returns/bells so tool output renders as plain text in the transcript.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' || c == '\x07' {
            continue;
        }
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

/// Outcome-first, human-sized result for the TUI transcript: the ✓/✗ glyph
/// and its color already carry success/failure, so the summary leads with
/// what actually happened (counts, diffstats, first output line). Failure
/// keeps the `failed ·` prefix: greppable, and it explains *why*. The tool
/// input JSON is consulted for write/edit so the summary can show a diffstat
/// without the caller doing extra IO.
pub(crate) fn tool_result_summary(name: &str, input: &str, text: &str, ok: bool) -> String {
    if !ok {
        return format!("failed · {}", one_line_summary(text));
    }
    let obj = serde_json::from_str::<Value>(input)
        .ok()
        .and_then(|v| v.as_object().cloned());
    let get = |k: &str| obj.as_ref().and_then(|o| o.get(k)).and_then(|x| x.as_str());
    let lines = text.lines().filter(|line| !line.trim().is_empty()).count();
    match name {
        "read" => format!("{} line{}", lines, plural(lines)),
        "grep" => {
            // Files mode (the default) lists paths, not matches.
            let files_mode = obj
                .as_ref()
                .and_then(|o| o.get("output_mode"))
                .and_then(Value::as_str)
                .unwrap_or("files")
                == "files";
            if files_mode {
                format!(
                    "{} file{} matched",
                    lines,
                    if lines == 1 { "" } else { "s" }
                )
            } else if lines == 1 {
                "1 match".to_string()
            } else {
                format!("{lines} matches")
            }
        }
        "find" => format!("{} entr{}", lines, if lines == 1 { "y" } else { "ies" }),
        "bash" => match one_line_summary(text) {
            first if first.is_empty() => "(no output)".to_string(),
            first => first,
        },
        "write" => {
            let written = get("content").unwrap_or_default().lines().count();
            format!("{} line{} written", written, plural(written))
        }
        "edit" => {
            let removed = get("oldText").unwrap_or_default().lines().count();
            let added = get("newText").unwrap_or_default().lines().count();
            format!("+{added} −{removed}")
        }
        "chain" => {
            let steps = obj
                .as_ref()
                .and_then(|o| o.get("steps"))
                .and_then(Value::as_array)
                .map(|steps| steps.len())
                .unwrap_or(0);
            // Each fan-out read emits a `==> path <==` section header.
            let files = text.matches("==> ").count();
            format!(
                "{steps} steps · {files} file{} · {lines} line{}",
                if files == 1 { "" } else { "s" },
                plural(lines)
            )
        }
        _ => match one_line_summary(text) {
            first if first.is_empty() => "(no output)".to_string(),
            first => first,
        },
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Compact wall-clock label for a tool result: sub-second precision below
/// 10s, whole seconds below a minute, then minutes.
pub(crate) fn format_duration(secs: f64) -> String {
    if secs < 0.0 {
        return String::new();
    }
    if secs < 10.0 {
        format!("{secs:.1}s")
    } else if secs < 60.0 {
        format!("{:.0}s", secs.round())
    } else {
        let total = secs.round() as u64;
        format!("{}m {:02}s", total / 60, total % 60)
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
    fn summary_is_outcome_first_without_ok_prefix() {
        // A successful read whose content happens to contain the shell
        // failure marker must not be reported as failed.
        assert_eq!(
            tool_result_summary(
                "read",
                "{}",
                "use serde_json::{Map, Value};\nresult.push_str(\"[exit 1]\");\n",
                true
            ),
            "2 lines"
        );
        // A genuinely failed tool reports failure with its first output line.
        assert_eq!(
            tool_result_summary("bash", "{}", "ls: no such file\n[exit 2]", false),
            "failed · ls: no such file"
        );
        // grep defaults to files mode: the summary counts files, not matches.
        assert_eq!(
            tool_result_summary("grep", "{}", "", true),
            "0 files matched"
        );
        assert_eq!(
            tool_result_summary(
                "grep",
                r#"{"output_mode":"content"}"#,
                "src/a.rs:1:hit",
                true
            ),
            "1 match"
        );
        // Empty successful output says so instead of a bare "ok".
        assert_eq!(tool_result_summary("bash", "{}", "", true), "(no output)");
        assert_eq!(
            tool_result_summary("bash", "{}", "hello world\nrest", true),
            "hello world"
        );
    }

    #[test]
    fn mutation_summaries_show_diffstats_from_input() {
        let write_input = r#"{"path":"src/x.rs","content":"a\nb\nc\n"}"#;
        assert_eq!(
            tool_result_summary("write", write_input, "wrote src/x.rs", true),
            "3 lines written"
        );
        let single = r#"{"path":"src/x.rs","content":"only"}"#;
        assert_eq!(
            tool_result_summary("write", single, "wrote src/x.rs", true),
            "1 line written"
        );
        let edit_input = r#"{"path":"src/x.rs","oldText":"a\nb","newText":"x\ny\nz"}"#;
        assert_eq!(
            tool_result_summary("edit", edit_input, "edited src/x.rs", true),
            "+3 −2"
        );
    }

    #[test]
    fn duration_formats_for_each_scale() {
        assert_eq!(format_duration(0.42), "0.4s");
        assert_eq!(format_duration(2.14), "2.1s");
        assert_eq!(format_duration(9.96), "10.0s");
        assert_eq!(format_duration(41.96), "42s");
        assert_eq!(format_duration(65.0), "1m 05s");
        assert_eq!(format_duration(125.4), "2m 05s");
        assert_eq!(format_duration(-1.0), "");
    }

    #[test]
    fn clamp_keeps_head_and_tail_with_real_counts() {
        let text: String = (1..=100).map(|n| format!("line {n}\n")).collect();
        let clamped = clamp_lines(text.trim_end(), 10, 1 << 20);
        assert!(clamped.starts_with("line 1\n"), "{clamped}");
        assert!(clamped.ends_with("line 100"), "{clamped}");
        assert!(
            clamped.contains("[... 90 of 100 lines truncated ...]"),
            "{clamped}"
        );

        // Within the limit the text passes through untouched.
        assert_eq!(clamp_lines("a\nb\nc", 10, 1024), "a\nb\nc");

        // Pathologically long lines are clipped, not kept whole.
        let long_line = "x".repeat(5000);
        let clamped = clamp_lines(&long_line, 10, 1 << 20);
        assert!(clamped.ends_with('…'));
        assert!(clamped.len() < 2100, "{}", clamped.len());

        // The byte budget bounds the result even below the line limit.
        let text: String = (1..=50)
            .map(|n| format!("{n} {}\n", "y".repeat(500)))
            .collect();
        let clamped = clamp_lines(text.trim_end(), 100, 4096);
        assert!(clamped.contains("lines truncated"), "{clamped}");
        assert!(clamped.len() < 8192, "{}", clamped.len());
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

    #[test]
    fn summary_and_arg_strip_escapes_and_carriage_returns() {
        assert_eq!(one_line_summary("\x1b[31mboom\x1b[0m\r\nnext"), "boom");
        assert_eq!(one_line_summary("progress\r\r\x07done"), "progressdone");
        // Fallback path: unparseable input JSON is sanitized too.
        assert_eq!(short_arg("x", "'\x1b[1mevil\x1b[0m\r"), "'evil");
    }
}
