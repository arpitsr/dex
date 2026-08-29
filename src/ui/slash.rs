use std::env;
use std::fs;

use crate::core::types::{ChatMessage, Provider};
use crate::session::Session;

use super::{push_info, push_transcript_gap, App, InputField};

const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/quit", "Exit the REPL"),
    ("/clear", "Clear conversation history"),
    ("/new", "Start a new session"),
    ("/session", "Show current session details"),
    ("/resume", "List or resume a session"),
    ("/permissions", "Show permission mode and workspace"),
    ("/name", "Rename the current session"),
    ("/model", "Show or switch the model"),
    ("/provider", "Show or switch the provider"),
    ("/help", "Show available commands"),
];

pub(super) fn slash_suggestions(app: &App) -> Vec<(String, String)> {
    let input = app.input.text();
    if app.busy || !input.starts_with('/') || input.contains('\n') {
        return Vec::new();
    }
    if let Some(query) = input.strip_prefix("/model ") {
        let query = query.to_ascii_lowercase();
        return app
            .config
            .available_models
            .iter()
            .filter(|model| model.to_ascii_lowercase().starts_with(&query))
            .map(|model| {
                (
                    format!("/model {model}"),
                    if model == &app.config.model {
                        "Current model".to_string()
                    } else {
                        "Configured model".to_string()
                    },
                )
            })
            .collect();
    }
    if let Some(query) = input.strip_prefix("/provider ") {
        let query = query.to_ascii_lowercase();
        return ["opencode", "openai-codex"]
            .into_iter()
            .filter(|provider| provider.starts_with(&query))
            .map(|provider| {
                (
                    format!("/provider {provider}"),
                    if *provider == *app.config.provider.name() {
                        "Current provider".to_string()
                    } else {
                        "Available provider".to_string()
                    },
                )
            })
            .collect();
    }
    if input.contains(' ') {
        return Vec::new();
    }
    let query = input.to_ascii_lowercase();
    let mut suggestions: Vec<(String, String)> = SLASH_COMMANDS
        .iter()
        .filter(|(command, _)| command.starts_with(&query))
        .map(|(command, description)| ((*command).to_string(), (*description).to_string()))
        .collect();
    suggestions.extend(
        app.skills
            .iter()
            .map(|skill| {
                (
                    format!("/skill:{}", skill.name),
                    "Load this skill".to_string(),
                )
            })
            .filter(|(command, _)| command.to_ascii_lowercase().starts_with(&query)),
    );
    suggestions
}

pub(super) fn complete_slash(app: &mut App) -> bool {
    let suggestions = slash_suggestions(app);
    let Some((command, _)) =
        suggestions.get(app.slash_selected.min(suggestions.len().saturating_sub(1)))
    else {
        return false;
    };
    app.input = InputField::from_text(&format!("{command} "));
    app.slash_selected = 0;
    true
}

pub(super) fn handle_slash(app: &mut App, line: &str) -> bool {
    // Gap before command output so it reads as its own turn (pi-style).
    if app.transcript.len() > 1 {
        push_transcript_gap(app);
    }
    match line {
        "/quit" => return true,
        "/clear" => {
            app.messages.truncate(1);
            let _ = app.session.clear_messages();
            push_info(app, "history cleared.".to_string());
        }
        "/new" => {
            app.messages.truncate(1);
            let cwd = env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            match Session::new(cwd, None) {
                Ok(mut s) => {
                    s.append_message(app.messages[0].clone()).ok();
                    app.session = s;
                    push_info(app, "new session started.".to_string());
                }
                Err(e) => push_info(app, format!("could not start new session: {}", e)),
            }
        }
        "/session" => {
            push_info(app, format!("session: {}", app.session.display_name()));
            if let Some(path) = app.session.path() {
                push_info(app, format!("path: {}", path.display()));
            }
            push_info(app, format!("turns: {}", app.session.count()));
        }
        "/permissions" => {
            push_info(app, format!("permission mode: {:?}", app.config.permission));
            push_info(app, format!("workspace: {}", app.cwd));
        }
        "/resume" => match Session::list(&app.cwd) {
            Ok(sessions) if !sessions.is_empty() => {
                push_info(app, "sessions:".to_string());
                for (i, (path, header)) in sessions.iter().enumerate() {
                    let name = header.name().unwrap_or("(unnamed)");
                    push_info(app, format!("  {}: {} ({})", i, name, path.display()));
                }
            }
            _ => push_info(app, "no sessions found.".to_string()),
        },
        _ if line.starts_with("/resume ") => {
            let selector = line["/resume ".len()..].trim();
            match Session::resume(&app.cwd, selector) {
                Ok(session) => {
                    let loaded = session
                        .path()
                        .and_then(|p| crate::session::load_messages_from_session(p).ok())
                        .unwrap_or_default();
                    let system = app.messages.first().cloned();
                    app.messages = loaded;
                    if let Some(system) = system {
                        app.messages.insert(0, system);
                    }
                    app.session = session;
                    push_info(
                        app,
                        format!("resumed session: {}", app.session.display_name()),
                    );
                }
                Err(e) => push_info(app, format!("could not resume session: {}", e)),
            }
        }
        _ if line.starts_with("/name ") => {
            let name = line["/name ".len()..].trim().to_string();
            if !name.is_empty() {
                app.session.set_name(name.clone()).ok();
                push_info(app, format!("session name: {}", name));
            }
        }
        _ if line.starts_with("/skill:") => {
            let name = line["/skill:".len()..].trim();
            if let Some(skill) = app.skills.iter().find(|s| s.name == name) {
                let content = fs::read_to_string(&skill.path).unwrap_or_default();
                app.messages.push(ChatMessage {
                    role: "user".to_string(),
                    content: Some(format!("--- Skill: {} ---\n{}", skill.name, content)),
                    tool_calls: None,
                    tool_call_id: None,
                    name: Some("skill".to_string()),
                });
                let _ = app
                    .session
                    .append_message(app.messages.last().cloned().unwrap());
                push_info(app, format!("loaded skill: {}", skill.name));
            } else {
                push_info(app, format!("skill not found: {}", name));
                push_info(app, "available skills:".to_string());
                let names: Vec<String> = app.skills.iter().map(|s| s.name.clone()).collect();
                for n in names {
                    push_info(app, format!("  - {}", n));
                }
            }
        }
        "/model" => {
            push_info(app, format!("current model: {}", app.config.model));
        }
        "/provider" => {
            push_info(
                app,
                format!("current provider: {}", app.config.provider.name()),
            );
            push_info(
                app,
                "available providers: opencode, openai-codex".to_string(),
            );
        }
        "/help" => {
            push_info(
                app,
                "commands: /quit /clear /new /session /resume [index|path] /permissions /name <n> /skill:<name> /model [<m>] /provider [<name>]"
                    .to_string(),
            );
            push_info(
                app,
                "keys: Enter send · Shift+Enter newline · ↑↓ history · PgUp/PgDn/mouse scroll"
                    .to_string(),
            );
            push_info(app, "while working: Enter queues steer · Alt+Enter queues follow-up · Esc/Ctrl+C cancels and restores queued input".to_string());
        }
        _ if line.starts_with("/model ") => {
            let m = line["/model ".len()..].trim().to_string();
            if !m.is_empty() {
                app.config.model = m.clone();
                if !app
                    .config
                    .available_models
                    .iter()
                    .any(|candidate| candidate == &m)
                {
                    app.config.available_models.push(m.clone());
                }
                let _ = app.session.set_state("model", &m);
                push_info(app, format!("switched to model: {}", app.config.model));
            }
        }
        _ if line.starts_with("/provider ") => {
            let name = line["/provider ".len()..].trim();
            match Provider::parse(name) {
                Ok(provider) if provider == app.config.provider => {
                    push_info(
                        app,
                        format!("provider already selected: {}", provider.name()),
                    );
                }
                Ok(provider) => match app.config.switch_provider(provider) {
                    Ok(()) => {
                        let _ = app.session.set_state("provider", provider.name());
                        push_info(app, format!("switched to provider: {}", provider.name()));
                    }
                    Err(error) => push_info(app, format!("could not switch provider: {}", error)),
                },
                Err(error) => push_info(app, error),
            }
        }
        _ => push_info(app, format!("unknown command: {}", line)),
    }
    false
}
