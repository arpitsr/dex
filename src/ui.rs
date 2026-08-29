use std::process::Command;
use std::sync::mpsc;
use std::time::Instant;

use crossterm::event::DisableMouseCapture;
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, LeaveAlternateScreen};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::agent::state::ToolState;
use crate::core::types::SinkLine;
use crate::llm::config::LlmConfig;
use crate::session::Session;

mod event;
mod input;
mod render;
mod slash;
mod wrapping;
use input::InputField;

pub(crate) use event::run_ratatui_repl;
pub(crate) use render::view;

enum UiEvent {
    State {
        messages: Vec<crate::core::types::ChatMessage>,
        tool_state: ToolState,
        session: Box<Session>,
    },
    Done {
        success: bool,
    },
}

pub(crate) struct App {
    transcript: Vec<Line<'static>>,
    input: InputField,
    config: LlmConfig,
    messages: Vec<crate::core::types::ChatMessage>,
    tool_state: ToolState,
    session: Session,
    skills: Vec<crate::core::types::Skill>,
    turn_start: usize,
    cwd: String,
    git_branch: Option<String>,
    git_dirty: bool,
    turn_started: Option<Instant>,
    active_tool: Option<String>,
    last_activity: Option<String>,
    steering_rx: Option<mpsc::Receiver<String>>,
    followup_rx: Option<mpsc::Receiver<String>>,
    pending_steering: Vec<String>,
    pending_followups: Vec<String>,
    cancel_requested: bool,
    approval_rx: Option<mpsc::Receiver<crate::core::types::ApprovalRequest>>,
    pending_approval: Option<PendingApproval>,
    busy: bool,
    autoscroll: bool,
    scroll: u16,
    tick: u16,
    quit: bool,
    history: Vec<String>,
    history_index: Option<usize>,
    history_draft: String,
    slash_selected: usize,
}

struct PendingApproval {
    name: String,
    input: String,
    response: mpsc::Sender<crate::core::types::ApprovalDecision>,
    selected: usize,
}

struct TerminalCleanup;

impl Drop for TerminalCleanup {
    fn drop(&mut self) {
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        let _ = disable_raw_mode();
    }
}

impl App {
    fn history_up(&mut self) {
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

    fn history_down(&mut self) {
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

    fn history_push(&mut self, text: String) {
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

pub(super) fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn git_context(cwd: &str) -> (Option<String>, bool) {
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

pub(super) fn append_sink_line(app: &mut App, sl: SinkLine) {
    match sl {
        SinkLine::Assistant(s) => {
            if s.trim().is_empty() {
                if !app
                    .transcript
                    .last()
                    .is_some_and(|line| line.spans.is_empty())
                {
                    push_transcript_gap(app);
                }
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

pub(super) fn push_transcript_gap(app: &mut App) {
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

fn transcript_indent() -> String {
    " ".repeat(TRANSCRIPT_INDENT)
}

fn indent_transcript_line(mut line: Line<'static>) -> Line<'static> {
    line.spans.insert(0, Span::raw(transcript_indent()));
    line
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use crate::core::types::{ApiProtocol, PermissionMode, Provider};

    fn test_app() -> App {
        let cwd = "/tmp/ak-ui-test".to_string();
        App {
            transcript: vec![indent_transcript_line(Line::from(
                "hello from the transcript — this line is intentionally long enough to wrap",
            ))],
            input: InputField::new(),
            config: LlmConfig {
                provider: Provider::OpenCode,
                api_key: "test".to_string(),
                base_url: "http://localhost".to_string(),
                model: "test-model".to_string(),
                available_models: vec!["test-model".to_string()],
                api: ApiProtocol::Responses,
                account_id: None,
                thinking_effort: None,
                context_window: 128_000,
                permission: PermissionMode::Trusted,
                max_tool_iterations: 60,
                max_prompt_tokens: 128_000,
                max_turn_seconds: 900,
                client: reqwest::blocking::Client::new(),
            },
            messages: Vec::new(),
            tool_state: ToolState::default(),
            session: Session::in_memory(cwd.clone()),
            skills: Vec::new(),
            turn_start: 0,
            cwd,
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
        }
    }

    #[test]
    fn slash_suggestions_filter_by_prefix_and_include_skills() {
        let mut app = test_app();
        app.skills.push(crate::core::types::Skill {
            name: "rust".to_string(),
            description: "Rust help".to_string(),
            path: "/tmp/rust/SKILL.md".into(),
        });
        app.input = InputField::from_text("/s");
        let commands = crate::ui::slash::slash_suggestions(&app);
        let names: Vec<_> = commands.iter().map(|(name, _)| name.as_str()).collect();
        assert!(names.contains(&"/session"));
        assert!(names.contains(&"/skill:rust"));
        assert!(!names.contains(&"/model"));

        app.input = InputField::from_text("/model ");
        let models = crate::ui::slash::slash_suggestions(&app);
        assert!(models.iter().any(|(name, _)| name == "/model test-model"));

        app.input = InputField::from_text("/provider ");
        let providers = crate::ui::slash::slash_suggestions(&app);
        assert!(providers
            .iter()
            .any(|(name, _)| name == "/provider opencode"));
        assert!(providers
            .iter()
            .any(|(name, _)| name == "/provider openai-codex"));
    }

    #[test]
    fn slash_completion_replaces_input_with_selected_command() {
        let mut app = test_app();
        app.input = InputField::from_text("/mo");
        assert!(crate::ui::slash::complete_slash(&mut app));
        assert_eq!(app.input.text(), "/model ");
        assert_eq!(app.input.row, 0);
        assert_eq!(app.input.col, "/model ".len());
    }

    #[test]
    fn provider_names_are_parsed_for_runtime_switching() {
        assert_eq!(Provider::parse("codex").unwrap().name(), "openai-codex");
        assert_eq!(Provider::parse("OpenCode").unwrap().name(), "opencode");
        assert!(Provider::parse("unknown").is_err());
    }

    #[test]
    fn intermediate_assistant_block_is_dimmed_before_tool_call() {
        let mut app = test_app();
        app.transcript.clear();
        app.transcript
            .push(indent_transcript_line(Line::from(Span::styled(
                "I will inspect the project first.",
                Style::default().fg(Color::White),
            ))));
        append_sink_line(&mut app, SinkLine::ToolInput("read README.md".to_string()));
        let assistant = &app.transcript[0];
        assert_eq!(assistant.spans[1].style.fg, Some(Color::DarkGray));
    }
}
