#![allow(dead_code)]

use std::fmt;
use std::sync::mpsc;
use std::time::Instant;

use crossterm::Command;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::agent::state::ToolState;
use crate::core::types::SinkLine;
use crate::llm::config::LlmConfig;
use crate::session::Session;

mod input;
mod remote;
mod render;
mod slash;
mod theme;
mod wrapping;

pub(crate) use remote::run_ratatui_repl_with_remote;
pub(crate) use render::view;

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

/// Raised-surface colors are resolved in `ui/theme.rs` from the terminal's
/// own palette / detected background, so they follow the terminal theme.
/// Braille spinner frames, matching the headless console spinner.
const UI_SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// The TUI application state. Rendering lives in `ui/render.rs` (`view`);
/// turn execution lives either in the local engine or, in client-server
/// mode, in `ui/remote.rs` which drives the same state from daemon events.
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

impl App {
    pub(crate) fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        if self.history_index.is_none() {
            self.history_draft = self.input.text();
        }
        let idx = self
            .history_index
            .map(|i| (i + 1).min(self.history.len() - 1))
            .unwrap_or(0);
        self.history_index = Some(idx);
        self.input = InputField::from_text(&self.history[self.history.len() - 1 - idx]);
    }

    pub(crate) fn history_down(&mut self) {
        match self.history_index {
            None => {}
            Some(0) => {
                self.history_index = None;
                self.input = InputField::from_text(&self.history_draft);
            }
            Some(idx) => {
                let new_idx = idx - 1;
                self.history_index = Some(new_idx);
                self.input = InputField::from_text(&self.history[self.history.len() - 1 - new_idx]);
            }
        }
    }

    pub(crate) fn history_push(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        if self.history.last() != Some(&text) {
            self.history.push(text);
        }
        self.history_index = None;
        self.history_draft.clear();
    }
}

pub(crate) struct PendingApproval {
    pub(crate) name: String,
    pub(crate) input: String,
    pub(crate) response: mpsc::Sender<crate::core::types::ApprovalDecision>,
    pub(crate) selected: usize,
}

pub(crate) fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

struct TerminalCleanup;

impl Drop for TerminalCleanup {
    fn drop(&mut self) {
        use crossterm::event::DisableMouseCapture;
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
            DisableMouseCapture,
            DisableAlternateScroll
        );
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// DECSET 1007 (alternate scroll): while in the alternate screen the
/// terminal turns mouse-wheel events into Up/Down arrow presses. Mouse
/// capture is deliberately never enabled, so click-drag stays native and
/// the terminal itself handles text selection for copy.
pub(crate) struct EnableAlternateScroll;

impl Command for EnableAlternateScroll {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[?1007h")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) struct DisableAlternateScroll;

impl Command for DisableAlternateScroll {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[?1007l")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Ok(())
    }
}

fn transcript_indent() -> String {
    " ".repeat(TRANSCRIPT_INDENT)
}

fn indent_transcript_line(mut line: Line<'static>) -> Line<'static> {
    line.spans.insert(0, Span::raw(transcript_indent()));
    line
}

fn push_transcript_gap(app: &mut App) {
    if !app
        .transcript
        .last()
        .is_some_and(|line| line.spans.is_empty())
    {
        app.transcript.push(Line::from(String::new()));
    }
}

pub(super) fn push_info(app: &mut App, text: String) {
    app.transcript
        .push(indent_transcript_line(Line::from(Span::styled(
            text,
            Style::default().fg(Color::Cyan),
        ))));
}

/// Route a streamed console line into the transcript with the same styling
/// the local engine uses, so remote and local turns look identical.
pub(super) fn append_sink_line(app: &mut App, sl: SinkLine) {
    match sl {
        SinkLine::Assistant(s) => {
            if s.trim().is_empty() {
                push_transcript_gap(app);
            } else {
                if app
                    .transcript
                    .last()
                    .is_some_and(|line| is_tool_line(line) || is_user_line(line))
                {
                    push_transcript_gap(app);
                }
                for line in render::markdown_lines(s.trim_end()) {
                    app.transcript.push(indent_transcript_line(line));
                }
            }
        }
        SinkLine::ToolInput(s) => {
            dim_intermediate_assistant_block(app);
            if app
                .transcript
                .last()
                .is_some_and(|line| !line.spans.is_empty())
            {
                push_transcript_gap(app);
            }
            let mut it = s.splitn(2, ' ');
            let name = it.next().unwrap_or("").to_string();
            let arg = it.next().unwrap_or("").to_string();
            app.active_tool = Some(name.clone());
            app.transcript.push(indent_transcript_line(Line::from(vec![
                Span::styled("▸ ", Style::default().fg(Color::Yellow)),
                Span::styled(name, Style::default().fg(Color::Yellow)),
                Span::styled(format!(" {arg}"), Style::default().fg(Color::DarkGray)),
            ])));
        }
        SinkLine::ToolOutput { name, summary } => {
            let failed = summary.starts_with("failed ·");
            let color = if failed {
                Color::LightRed
            } else {
                Color::LightGreen
            };
            app.transcript.push(indent_transcript_line(Line::from(vec![
                Span::styled("└ ", Style::default().fg(color)),
                Span::styled(if failed { "✗ " } else { "✓ " }, Style::default().fg(color)),
                Span::styled(name, Style::default().fg(Color::DarkGray)),
                Span::styled(format!(" {summary}"), Style::default().fg(color)),
            ])));
        }
        SinkLine::System(s) => app.transcript.push(indent_transcript_line(Line::from(vec![
            Span::styled("· ", Style::default().fg(Color::DarkGray)),
            Span::styled(s, Style::default().fg(Color::DarkGray)),
        ]))),
        SinkLine::Error(s) => {
            if app
                .transcript
                .last()
                .is_some_and(|line| !line.spans.is_empty())
            {
                push_transcript_gap(app);
            }
            app.transcript.push(indent_transcript_line(Line::from(vec![
                Span::styled("! ", Style::default().fg(Color::Red)),
                Span::styled(format!("error: {s}"), Style::default().fg(Color::Red)),
            ])));
        }
    }
    app.autoscroll = true;
}

fn is_tool_line(line: &Line<'static>) -> bool {
    line.spans.iter().any(|span| {
        span.content.as_ref().starts_with("▸") || span.content.as_ref().starts_with("└")
    })
}

fn is_user_line(line: &Line<'static>) -> bool {
    line.spans.iter().any(|span| span.style.bg.is_some())
}

fn is_assistant_line(line: &Line<'static>) -> bool {
    !line.spans.is_empty()
        && !is_user_line(line)
        && !line.spans.iter().any(|span| {
            let text = span.content.as_ref();
            text.starts_with("▸") || text.starts_with("└") || text.starts_with("· ")
        })
}

fn dim_intermediate_assistant_block(app: &mut App) {
    for line in app.transcript.iter_mut().rev() {
        if line.spans.is_empty() || !is_assistant_line(line) {
            break;
        }
        for span in &mut line.spans {
            span.style = span.style.fg(Color::DarkGray);
        }
    }
}

/// Send the user's approval decision for the pending tool execution.
pub(super) fn resolve_approval(app: &mut App, decision: crate::core::types::ApprovalDecision) {
    if let Some(approval) = app.pending_approval.take() {
        let _ = approval.response.send(decision);
    }
}

pub(super) fn scroll_transcript(app: &mut App, delta: i32) {
    app.autoscroll = false;
    app.scroll = (app.scroll as i32)
        .saturating_add(delta)
        .clamp(0, u16::MAX as i32) as u16;
}

/// Render the user's submitted prompt with the shared transcript grid.
pub(super) fn render_user_prompt(app: &mut App, line: &str) {
    let user_bg = Style::default()
        .fg(theme::surface_fg())
        .bg(theme::surface_bg());
    let horizontal_pad = " ".repeat(TRANSCRIPT_INDENT);
    let edge_pad = Span::styled(" ", user_bg);
    app.transcript.push(Line::from(String::new()));
    app.transcript.push(Line::from(edge_pad.clone()));
    for sub in line.split('\n') {
        app.transcript.push(Line::from(vec![
            Span::styled(horizontal_pad.clone(), user_bg),
            Span::styled(sub.to_string(), user_bg),
            edge_pad.clone(),
        ]));
    }
    app.transcript.push(Line::from(edge_pad));
    app.transcript.push(Line::from(String::new()));
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
        let (branch, dirty) = crate::core::format::git_context("/tmp/not-a-repo-12345");
        assert!(branch.is_none());
        assert!(!dirty);
    }
}
