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

/// A semantic transcript block. Gaps between blocks are **not** stored;
/// they are inserted by `TranscriptView::render` (`ui/render.rs`) as a
/// single blank `Line` between any two blocks. This makes gutter handling
/// canonical and removes the need for ad-hoc `push_transcript_gap` /
/// `in_assistant_stream` bookkeeping at every call site.
#[derive(Debug)]
pub(crate) enum TranscriptBlock {
    User(Vec<Line<'static>>),
    Assistant(Vec<Line<'static>>),
    Tool {
        input: Line<'static>,
        output: Option<Line<'static>>,
        preview: Vec<Line<'static>>,
    },
    System(Line<'static>),
    Error(Line<'static>),
    Info(Line<'static>),
}

impl TranscriptBlock {
    /// Lines that belong to this block, in display order.
    pub(crate) fn lines(&self) -> Vec<&Line<'static>> {
        match self {
            TranscriptBlock::User(lines) => lines.iter().collect(),
            TranscriptBlock::Assistant(lines) => lines.iter().collect(),
            TranscriptBlock::Tool {
                input,
                output,
                preview,
            } => {
                let mut out = Vec::with_capacity(1 + output.is_some() as usize + preview.len());
                out.push(input);
                if let Some(o) = output {
                    out.push(o);
                }
                out.extend(preview.iter());
                out
            }
            TranscriptBlock::System(line) => vec![line],
            TranscriptBlock::Error(line) => vec![line],
            TranscriptBlock::Info(line) => vec![line],
        }
    }
}

/// The TUI application state. Rendering lives in `ui/render.rs` (`view`);
/// turn execution lives either in the local engine or, in client-server
/// mode, in `ui/remote.rs` which drives the same state from daemon events.
pub(crate) struct App {
    pub(crate) transcript: Vec<TranscriptBlock>,
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
    /// Whether the tail `Assistant` block is still open for streaming
    /// coalescence. Tracked so an initial transcript block (e.g. in tests)
    /// does not merge with the first streamed assistant turn; gaps remain
    /// canonical between blocks.
    pub(crate) assistant_open: bool,
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

pub(super) fn push_info(app: &mut App, text: String) {
    app.assistant_open = false;
    app.transcript
        .push(TranscriptBlock::Info(indent_transcript_line(Line::from(
            Span::styled(text, Style::default().fg(Color::Cyan)),
        ))));
}

/// Route a streamed console line into the transcript with the same styling
/// the local engine uses, so remote and local turns look identical.
/// Each `SinkLine` maps to one `TranscriptBlock` (or an extension of the
/// tail `Assistant` block while streaming). No empty gap `Line`s are stored;
/// `TranscriptView` inserts a single blank `Line` between any two blocks.
pub(super) fn append_sink_line(app: &mut App, sl: SinkLine) {
    match sl {
        SinkLine::Assistant(s) => {
            if s.trim().is_empty() {
                // Blank inside the current assistant message (e.g. streaming
                // blank line between paragraphs). Keep it inside the tail
                // Assistant block so the inter-block gutter remains canonical.
                if app.assistant_open {
                    if let Some(TranscriptBlock::Assistant(lines)) = app.transcript.last_mut() {
                        lines.push(Line::default());
                    }
                }
                app.autoscroll = true;
                return;
            }
            let new_lines: Vec<Line<'static>> = render::markdown_lines(s.trim_end())
                .into_iter()
                .map(indent_transcript_line)
                .collect();
            if app.assistant_open {
                if let Some(TranscriptBlock::Assistant(existing)) = app.transcript.last_mut() {
                    existing.extend(new_lines);
                    app.autoscroll = true;
                    return;
                }
            }
            app.transcript.push(TranscriptBlock::Assistant(new_lines));
            app.assistant_open = true;
            app.autoscroll = true;
            return;
        }
        SinkLine::ToolInput(s) => {
            dim_intermediate_assistant_block(app);
            app.assistant_open = false;
            let mut it = s.splitn(2, ' ');
            let name = it.next().unwrap_or("").to_string();
            let arg = it.next().unwrap_or("").to_string();
            app.active_tool = Some(name.clone());
            let input = indent_transcript_line(Line::from(vec![
                Span::styled("▸ ", Style::default().fg(Color::Yellow)),
                Span::styled(name, Style::default().fg(Color::Yellow)),
                Span::styled(format!(" {arg}"), Style::default().fg(theme::tool_input_fg())),
            ]));
            app.transcript.push(TranscriptBlock::Tool {
                input,
                output: None,
                preview: Vec::new(),
            });
        }
        SinkLine::ToolOutput {
            name: _,
            summary,
            success,
            preview,
            duration,
        } => {
            // The ▸ line above already names the tool; the └ line leads with
            // the outcome (glyph + summary) and trails timing in dim.
            let failed = !success;
            let color = if failed {
                Color::LightRed
            } else {
                Color::LightGreen
            };
            let mut spans = vec![
                Span::styled("└ ", Style::default().fg(color)),
                Span::styled(if failed { "✗ " } else { "✓ " }, Style::default().fg(color)),
                Span::styled(summary, Style::default().fg(color)),
            ];
            if duration > 0.0 {
                spans.push(Span::styled(
                    format!(" · {}", crate::core::format::format_duration(duration)),
                    Style::default().fg(theme::muted_fg()),
                ));
            }
            let output = indent_transcript_line(Line::from(spans));
            let preview_lines: Vec<Line<'static>> = preview
                .iter()
                .map(|line| {
                    indent_transcript_line(Line::from(Span::styled(
                        format!("  {line}"),
                        Style::default().fg(theme::tool_preview_fg()),
                    )))
                })
                .collect();
            app.assistant_open = false;
            // Complete the tool block started by ToolInput if it is still open.
            if let Some(TranscriptBlock::Tool {
                output: out,
                preview: prev,
                ..
            }) = app.transcript.last_mut()
            {
                if out.is_none() {
                    *out = Some(output);
                    *prev = preview_lines;
                    app.autoscroll = true;
                    return;
                }
            }
            // Fallback: no open ToolInput (e.g. replay); synthesize a block.
            app.transcript.push(TranscriptBlock::Tool {
                input: indent_transcript_line(Line::from(Span::styled(
                    "▸ tool",
                    Style::default().fg(Color::Yellow),
                ))),
                output: Some(output),
                preview: preview_lines,
            });
        }
        SinkLine::System(s) => {
            app.assistant_open = false;
            app.transcript
                .push(TranscriptBlock::System(indent_transcript_line(Line::from(
                    vec![
                        Span::styled("· ", Style::default().fg(theme::muted_fg())),
                        Span::styled(s, Style::default().fg(theme::muted_fg())),
                    ],
                ))));
        }
        // Usage updates flow into the status bar via StreamEvent::Usage in
        // the remote handler, not into the transcript.
        SinkLine::Usage(_) => {}
        SinkLine::Error(s) => {
            app.assistant_open = false;
            app.transcript
                .push(TranscriptBlock::Error(indent_transcript_line(Line::from(
                    vec![
                        Span::styled("! ", Style::default().fg(Color::Red)),
                        Span::styled(format!("error: {s}"), Style::default().fg(Color::Red)),
                    ],
                ))));
        }
    }
    app.autoscroll = true;
}

fn dim_intermediate_assistant_block(app: &mut App) {
    if let Some(TranscriptBlock::Assistant(lines)) = app.transcript.last_mut() {
        for line in lines.iter_mut() {
            for span in &mut line.spans {
                span.style = span.style.fg(theme::muted_fg());
            }
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
/// No empty gap `Line`s are stored; gutter is inserted by `TranscriptView`.
pub(super) fn render_user_prompt(app: &mut App, line: &str) {
    app.assistant_open = false;
    let user_bg = Style::default()
        .fg(theme::surface_fg())
        .bg(theme::surface_bg());
    let horizontal_pad = " ".repeat(TRANSCRIPT_INDENT);
    let edge_pad = Span::styled(" ", user_bg);
    let mut block_lines = Vec::new();
    block_lines.push(Line::from(edge_pad.clone()));
    for sub in line.split('\n') {
        block_lines.push(Line::from(vec![
            Span::styled(horizontal_pad.clone(), user_bg),
            Span::styled(sub.to_string(), user_bg),
            edge_pad.clone(),
        ]));
    }
    block_lines.push(Line::from(edge_pad));
    app.transcript.push(TranscriptBlock::User(block_lines));
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

    #[test]
    fn tool_preview_lines_are_indented_and_dimmed() {
        // Preview formatting is exercised through the Tool block produced by
        // `append_sink_line`; the stored preview lines must be indented and
        // dimmed exactly as before.
        let mut app = App {
            transcript: Vec::new(),
            input: crate::ui::input::InputField::new(),
            config: crate::llm::config::LlmConfig {
                provider: crate::core::types::Provider::OpenCode,
                api_key: String::new(),
                base_url: String::new(),
                model: "test".into(),
                available_models: vec!["test".into()],
                api: crate::core::types::ApiProtocol::Responses,
                account_id: None,
                thinking_effort: None,
                context_window: 128_000,
                permission: crate::core::types::PermissionMode::Trusted,
                max_tool_iterations: 60,
                max_prompt_tokens: 128_000,
                max_turn_seconds: 900,
                client: reqwest::blocking::Client::new(),
            },
            messages: Vec::new(),
            tool_state: crate::agent::state::ToolState::default(),
            session: crate::session::Session::in_memory("/tmp".into()),
            skills: Vec::new(),
            turn_start: 0,
            cwd: "/tmp".into(),
            git_branch: None,
            git_dirty: false,
            turn_started: None,
            active_tool: None,
            last_activity: None,
            steering_rx: None,
            followup_rx: None,
            pending_steering: Vec::new(),
            pending_followups: Vec::new(),
            cancel_requested: false,
            approval_rx: None,
            pending_approval: None,
            busy: false,
            autoscroll: true,
            scroll: 0,
            tick: 0,
            quit: false,
            history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            slash_selected: 0,
            assistant_open: false,
        };
        append_sink_line(
            &mut app,
            crate::core::types::SinkLine::ToolInput("bash echo hi".into()),
        );
        append_sink_line(
            &mut app,
            crate::core::types::SinkLine::ToolOutput {
                name: "bash".into(),
                summary: "v ok".into(),
                success: true,
                preview: vec!["src/main.rs".into(), "… +3 more lines".into()],
                duration: 0.0,
            },
        );
        let TranscriptBlock::Tool { preview, .. } = &app.transcript[0] else {
            panic!("expected Tool block");
        };
        assert_eq!(preview.len(), 2);
        for line in preview {
            assert!(line.spans.len() == 2); // indent gutter + content
            assert_eq!(line.spans[1].style.fg, Some(theme::tool_preview_fg()));
        }
        assert!(preview[0].spans[1].content.as_ref() == "  src/main.rs");
        assert!(preview[1].spans[1].content.as_ref() == "  … +3 more lines");
    }
}
