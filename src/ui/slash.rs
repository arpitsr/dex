#![allow(dead_code)]

use std::env;
use std::fs;
use std::path::Path;

use crate::core::types::{ChatMessage, Plan, Provider};
use crate::session::Session;

use super::{push_info, App, InputField};

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
    ("/goal", "Set or show the task goal"),
    ("/plan", "Show the plan"),
    ("/plan add", "Add a plan step"),
    ("/plan done", "Mark a plan step done"),
    ("/plan clear", "Clear the plan"),
    ("/constraint add", "Add a task constraint"),
    ("/constraint clear", "Clear all constraints"),
    ("/accept add", "Add an acceptance criterion"),
    ("/accept done", "Mark an acceptance criterion checked"),
    ("/accept clear", "Clear acceptance criteria"),
    ("/budget", "Show or set the task budget"),
    ("/budget clear", "Clear the task budget"),
    ("/waive <reason>", "Waive verification with a reason"),
    ("/undo", "Undo the last recorded file change"),
    ("/help", "Show available commands"),
];

fn save_plan(app: &mut App) -> std::io::Result<()> {
    let json = app.plan.to_json();
    app.session.set_state("plan", &json)
}

pub(super) fn slash_suggestions(app: &App) -> Vec<(String, String)> {
    let input = app.input.text();
    if app.busy || !input.starts_with('/') || input.contains('\n') {
        return Vec::new();
    }
    if input.starts_with("/plan") {
        let choices = ["/plan", "/plan add ", "/plan done ", "/plan clear"];
        let q = input.to_ascii_lowercase();
        return choices
            .iter()
            .filter(|c| c.starts_with(&q))
            .map(|c| (c.to_string(), "Plan command".to_string()))
            .collect();
    }
    if input.starts_with("/constraint") {
        let choices = ["/constraint add ", "/constraint clear"];
        let q = input.to_ascii_lowercase();
        return choices
            .iter()
            .filter(|c| c.starts_with(&q))
            .map(|c| (c.to_string(), "Constraint command".to_string()))
            .collect();
    }
    if input.starts_with("/accept") {
        let choices = ["/accept add ", "/accept done ", "/accept clear"];
        let q = input.to_ascii_lowercase();
        return choices
            .iter()
            .filter(|c| c.starts_with(&q))
            .map(|c| (c.to_string(), "Acceptance command".to_string()))
            .collect();
    }
    if input.starts_with("/goal")
        && !input.starts_with("/goal ")
        && "/goal".starts_with(&input.to_ascii_lowercase())
    {
        return vec![("/goal ".to_string(), "Set the task goal".to_string())];
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
                    let session_path = session.path().map(|p| p.to_path_buf());
                    app.session = session;
                    apply_session_state(app, session_path.as_deref());
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
        "/goal" => {
            if let Some(g) = app.plan.goal.clone() {
                push_info(app, format!("goal: {g}"));
            } else {
                push_info(app, "no goal set; use /goal <text>".to_string());
            }
            if !app.plan.steps.is_empty() {
                let steps = app.plan.steps.clone();
                for (i, (s, done)) in steps.iter().enumerate() {
                    push_info(
                        app,
                        format!("  {} {} {}", if *done { "[x]" } else { "[ ]" }, i + 1, s),
                    );
                }
            }
        }
        _ if line.starts_with("/goal ") => {
            let text = line["/goal ".len()..].trim().to_string();
            if text.is_empty() {
                push_info(app, "usage: /goal <text>".to_string());
            } else {
                app.plan.goal = Some(text.clone());
                if let Err(e) = save_plan(app) {
                    push_info(app, format!("could not persist goal: {e}"));
                } else {
                    push_info(app, format!("goal set: {text}"));
                }
            }
        }
        "/plan" => {
            if app.plan.is_empty() {
                push_info(
                    app,
                    "no plan yet; use /goal, /plan add, /constraint add, /accept add".to_string(),
                );
            } else {
                if let Some(g) = app.plan.goal.clone() {
                    push_info(app, format!("Goal: {g}"));
                }
                if !app.plan.constraints.is_empty() {
                    push_info(app, "Constraints:".to_string());
                    let constraints = app.plan.constraints.clone();
                    for c in constraints {
                        push_info(app, format!("  - {c}"));
                    }
                }
                let done = app.plan.steps.iter().filter(|(_, d)| *d).count();
                let total = app.plan.steps.len();
                if total > 0 {
                    push_info(app, format!("Plan {done}/{total}"));
                    let steps = app.plan.steps.clone();
                    for (i, (s, done)) in steps.iter().enumerate() {
                        push_info(
                            app,
                            format!("  {} {} {}", if *done { "[x]" } else { "[ ]" }, i + 1, s),
                        );
                    }
                }
                if !app.plan.acceptance.is_empty() {
                    push_info(app, "Acceptance:".to_string());
                    let acceptance = app.plan.acceptance.clone();
                    for (i, (s, checked)) in acceptance.iter().enumerate() {
                        push_info(
                            app,
                            format!("  {} {} {}", if *checked { "[x]" } else { "[ ]" }, i + 1, s),
                        );
                    }
                }
                if app.plan.is_complete() {
                    push_info(app, "plan complete ✓".to_string());
                }
            }
        }
        _ if line.starts_with("/plan add ") => {
            let text = line["/plan add ".len()..].trim().to_string();
            if text.is_empty() {
                push_info(app, "usage: /plan add <step>".to_string());
            } else {
                app.plan.steps.push((text.clone(), false));
                if let Err(e) = save_plan(app) {
                    push_info(app, format!("could not persist plan: {e}"));
                } else {
                    push_info(app, format!("added step {}: {text}", app.plan.steps.len()));
                }
            }
        }
        _ if line.starts_with("/plan done ") => {
            let n = line["/plan done ".len()..]
                .trim()
                .parse::<usize>()
                .unwrap_or(0);
            if n == 0 || n > app.plan.steps.len() {
                push_info(
                    app,
                    format!("usage: /plan done <1..{}>", app.plan.steps.len()),
                );
            } else {
                app.plan.steps[n - 1].1 = true;
                if let Err(e) = save_plan(app) {
                    push_info(app, format!("could not persist plan: {e}"));
                } else {
                    push_info(app, format!("marked step {n} done"));
                }
            }
        }
        "/plan clear" => {
            app.plan = Plan::default();
            if let Err(e) = save_plan(app) {
                push_info(app, format!("could not persist plan: {e}"));
            } else {
                push_info(app, "plan cleared".to_string());
            }
        }
        _ if line.starts_with("/constraint add ") => {
            let text = line["/constraint add ".len()..].trim().to_string();
            if text.is_empty() {
                push_info(app, "usage: /constraint add <text>".to_string());
            } else {
                app.plan.constraints.push(text.clone());
                if let Err(e) = save_plan(app) {
                    push_info(app, format!("could not persist constraints: {e}"));
                } else {
                    push_info(app, format!("added constraint: {text}"));
                }
            }
        }
        "/constraint clear" => {
            app.plan.constraints.clear();
            if let Err(e) = save_plan(app) {
                push_info(app, format!("could not persist constraints: {e}"));
            } else {
                push_info(app, "constraints cleared".to_string());
            }
        }
        _ if line.starts_with("/accept add ") => {
            let text = line["/accept add ".len()..].trim().to_string();
            if text.is_empty() {
                push_info(app, "usage: /accept add <criterion>".to_string());
            } else {
                app.plan.acceptance.push((text.clone(), false));
                if let Err(e) = save_plan(app) {
                    push_info(app, format!("could not persist acceptance: {e}"));
                } else {
                    push_info(
                        app,
                        format!(
                            "added acceptance criterion {}: {text}",
                            app.plan.acceptance.len()
                        ),
                    );
                }
            }
        }
        _ if line.starts_with("/accept done ") => {
            let n = line["/accept done ".len()..]
                .trim()
                .parse::<usize>()
                .unwrap_or(0);
            if n == 0 || n > app.plan.acceptance.len() {
                push_info(
                    app,
                    format!("usage: /accept done <1..{}>", app.plan.acceptance.len()),
                );
            } else {
                app.plan.acceptance[n - 1].1 = true;
                if let Err(e) = save_plan(app) {
                    push_info(app, format!("could not persist acceptance: {e}"));
                } else {
                    push_info(app, format!("marked acceptance criterion {n} checked"));
                }
            }
        }
        "/accept clear" => {
            app.plan.acceptance.clear();
            if let Err(e) = save_plan(app) {
                push_info(app, format!("could not persist acceptance: {e}"));
            } else {
                push_info(app, "acceptance criteria cleared".to_string());
            }
        }
        "/budget" => match app.plan.budget.clone() {
            Some(b) => push_info(
                app,
                format!(
                    "budget: {}s · {} iterations · ${:.2}",
                    b.max_seconds.unwrap_or(0),
                    b.max_tool_iterations.unwrap_or(0),
                    b.max_cost_usd.unwrap_or(0.0)
                ),
            ),
            None => push_info(
                app,
                "no budget set; use /budget <seconds> [iterations] [cost]".to_string(),
            ),
        },
        "/budget clear" => {
            app.plan.budget = None;
            if let Err(e) = save_plan(app) {
                push_info(app, format!("could not persist budget: {e}"));
            } else {
                push_info(app, "budget cleared".to_string());
            }
        }
        _ if line.starts_with("/budget ") => {
            let parts: Vec<&str> = line["/budget ".len()..].split_whitespace().collect();
            if parts.is_empty() {
                push_info(
                    app,
                    "usage: /budget <seconds> [tool_iterations] [cost_usd]".to_string(),
                );
            } else {
                let max_seconds = parts[0].parse::<u64>().ok();
                if max_seconds.is_none() {
                    push_info(
                        app,
                        "usage: /budget <seconds> [tool_iterations] [cost_usd]".to_string(),
                    );
                } else {
                    let mut budget = crate::core::types::Budget {
                        max_seconds,
                        ..crate::core::types::Budget::default()
                    };
                    if let Some(iters) = parts.get(1) {
                        budget.max_tool_iterations = iters.parse::<u32>().ok();
                    }
                    if let Some(cost) = parts.get(2) {
                        budget.max_cost_usd = cost.parse::<f64>().ok();
                    }
                    app.plan.budget = Some(budget);
                    if let Err(e) = save_plan(app) {
                        push_info(app, format!("could not persist budget: {e}"));
                    } else {
                        push_info(app, "budget set (enforced this session)".to_string());
                    }
                }
            }
        }
        _ if line.starts_with("/waive ") => {
            let reason = line["/waive ".len()..].trim().to_string();
            if reason.is_empty() {
                push_info(app, "usage: /waive <reason>".to_string());
            } else {
                app.messages.push(ChatMessage {
                    role: "user".to_string(),
                    content: Some(format!("[verify waived] {reason}")),
                    tool_calls: None,
                    tool_call_id: None,
                    name: Some("waive".to_string()),
                });
                let _ = app
                    .session
                    .append_message(app.messages.last().cloned().unwrap());
                let _ = app.session.set_state(
                    "verify",
                    &serde_json::json!({
                        "disposition": "waived",
                        "reason": reason,
                        "timestamp": chrono::Utc::now().to_rfc3339(),
                    })
                    .to_string(),
                );
                push_info(app, "verification waived (recorded)".to_string());
            }
        }
        "/undo" => match crate::session::undo_last_change(&mut app.session) {
            Ok(message) => push_info(app, message),
            Err(e) => push_info(app, format!("undo: {e}")),
        },
        "/help" => {
            push_info(
                app,
                "commands: /quit /clear /new /session /resume [index|path] /permissions /name <n> /skill:<name> /model [<m>] /provider [<name>] /goal <text> /plan [add|done|clear] /constraint [add|clear] /accept [add|done|clear]"
                    .to_string(),
            );
            push_info(
                app,
                "keys: Enter send · Shift+Enter newline · ↑↓ history · PgUp/PgDn/wheel scroll"
                    .to_string(),
            );
            push_info(
                app,
                "mouse: drag to select text and copy · wheel scrolls".to_string(),
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

/// Re-apply provider/model overrides that were persisted with the session
/// (`/model` and `/provider` write `session_state` entries; later entries in
/// the JSONL win, matching the append-order semantics used for messages).
/// Each applied switch is reported to the transcript so the user can see why
/// their model changed on resume. Silently keeps the current config when the
/// session predates state entries, or when a persisted provider is no longer
/// resolvable in this environment.
pub(super) fn apply_session_state(app: &mut App, session_path: Option<&Path>) {
    let Some(path) = session_path else {
        return;
    };
    let state = match crate::session::load_session_state(path) {
        Ok(state) => state,
        Err(_) => return,
    };
    if let Some(name) = state.get("provider") {
        match Provider::parse(name) {
            Ok(provider) if provider != app.config.provider => {
                match app.config.switch_provider(provider) {
                    Ok(()) => push_info(app, format!("restored provider: {}", provider.name())),
                    Err(error) => push_info(
                        app,
                        format!("could not restore provider '{}': {}", name, error),
                    ),
                }
            }
            _ => {}
        }
    }
    if let Some(model) = state.get("model") {
        if app.config.model != *model {
            app.config.model = model.clone();
            if !app
                .config
                .available_models
                .iter()
                .any(|candidate| candidate == model)
            {
                app.config.available_models.push(model.clone());
            }
            push_info(app, format!("restored model: {}", model));
        }
    }
    if let Some(plan_json) = state.get("plan") {
        let plan = crate::core::types::Plan::from_json(plan_json);
        if !plan.is_empty() {
            app.plan = plan;
            let done = app.plan.steps.iter().filter(|(_, d)| *d).count();
            push_info(
                app,
                format!("restored plan: {} / {} steps", done, app.plan.steps.len()),
            );
            if let Some(g) = &app.plan.goal {
                push_info(app, format!("restored goal: {g}"));
            }
        }
    }
}
