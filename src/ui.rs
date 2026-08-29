#![allow(dead_code)]

use std::process::Command;
use std::sync::mpsc;
use std::time::Instant;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::agent::state::ToolState;
use crate::llm::config::LlmConfig;
use crate::session::Session;

mod input;
mod remote;
mod render;
mod slash;
mod wrapping;

pub(crate) use remote::run_ratatui_repl_with_remote;

use input::InputField;

const VERTICAL_GUTTER: u16 = 1;
const HORIZONTAL_GUTTER: u16 = 1;
const TRANSCRIPT_INDENT: usize = HORIZONTAL_GUTTER as usize;
const INPUT_BORDER_ROWS: u16 = 0;
const INPUT_PAD_Y: u16 = 1;
const STATUS_CONTENT_ROWS: u16 = 1;
const INPUT_MIN_ROWS: u16 = 3;
const INPUT_STATUS_GUTTER: u16 = 0;
const APPROVAL_HEIGHT: u16 = 11;

const SUBMITTED_PROMPT_BG: Color = Color::Rgb(20, 38, 54);
const INPUT_BG: Color = SUBMITTED_PROMPT_BG;

/// Braille spinner frames, matching the headless console spinner.
const UI_SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// The old local TUI App struct — kept for backwards compatibility with
/// render tests. New code should use the remote TUI (remote.rs).
#[allow(dead_code)]
pub(crate) struct App {
    pub(crate) transcript: Vec<Line<'static>>,
    pub(crate) input: InputField,
    pub(crate) config: LlmConfig,
    pub(crate) messages: Vec<crate::core::types::ChatMessage>,
    pub(crate) tool_state: ToolState,
    pub(crate) session: Session,
    pub(crate) skills: Vec<crate::core::types::Skill>,
    pub(crate) turn_start: usize,
    pub(crate) cwd: String,
    pub(crate) git_branch: Option<String>,
    pub(crate) git_dirty: bool,
    pub(crate) turn_started: Option<Instant>,
    pub(crate) active_tool: Option<String>,
    pub(crate) last_activity: Option<String>,
    pub(crate) steering_rx: Option<mpsc::Receiver<String>>,
    pub(crate) followup_rx: Option<mpsc::Receiver<String>>,
    pub(crate) pending_steering: Vec<String>,
    pub(crate) pending_followups: Vec<String>,
    pub(crate) cancel_requested: bool,
    pub(crate) approval_rx: Option<mpsc::Receiver<crate::core::types::ApprovalRequest>>,
    pub(crate) pending_approval: Option<PendingApproval>,
    pub(crate) busy: bool,
    pub(crate) autoscroll: bool,
    pub(crate) scroll: u16,
    pub(crate) tick: u16,
    pub(crate) quit: bool,
    pub(crate) history: Vec<String>,
    pub(crate) history_index: Option<usize>,
    pub(crate) history_draft: String,
    pub(crate) slash_selected: usize,
}

#[allow(dead_code)]
pub(crate) struct PendingApproval {
    pub(crate) name: String,
    pub(crate) input: String,
    pub(crate) response: mpsc::Sender<crate::core::types::ApprovalDecision>,
    pub(crate) selected: usize,
}

pub(super) fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

pub(super) fn git_context(cwd: &str) -> (Option<String>, bool) {
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

pub(super) fn push_transcript_gap(app_transcript: &mut Vec<Line<'static>>) {
    if !app_transcript
        .last()
        .is_some_and(|line| line.spans.is_empty())
    {
        app_transcript.push(Line::from(String::new()));
    }
}

#[allow(dead_code)]
pub(super) fn push_transcript_gap_app(app: &mut App) {
    push_transcript_gap(&mut app.transcript);
}

#[allow(dead_code)]
pub(super) fn push_info(app: &mut App, text: String) {
    app.transcript
        .push(indent_transcript_line(Line::from(Span::styled(
            text,
            Style::default().fg(Color::Cyan),
        ))));
}

struct TerminalCleanup;

impl Drop for TerminalCleanup {
    fn drop(&mut self) {
        use crossterm::event::DisableMouseCapture;
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
            DisableMouseCapture
        );
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

fn transcript_indent() -> String {
    " ".repeat(TRANSCRIPT_INDENT)
}

fn indent_transcript_line(mut line: Line<'static>) -> Line<'static> {
    line.spans.insert(0, Span::raw(transcript_indent()));
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indent_transcript_line_adds_gutter() {
        let line = Line::from("test");
        let indented = indent_transcript_line(line);
        assert!(indented.spans[0].content.as_ref() == " ");
    }

    #[test]
    fn git_context_returns_empty_on_non_repo() {
        let (branch, dirty) = git_context("/tmp/not-a-repo-12345");
        assert!(branch.is_none());
        assert!(!dirty);
    }
}
